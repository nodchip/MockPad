import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, writeFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';

const port = 3187;
const base = `http://127.0.0.1:${port}`;
const executable = process.platform === 'win32' ? './target/debug/mockpad.exe' : './target/debug/mockpad';
let server;
let browser;
const results = [];
const errors = [];
const check = name => { results.push(name); console.log(`PASS ${name}`); };
async function startServer() {
  server = spawn(executable, [], {
    env: { ...process.env, MOCKPAD_ADDR: `127.0.0.1:${port}`, MOCKPAD_PUBLIC_ORIGIN: base },
    stdio: ['ignore', 'pipe', 'inherit'], windowsHide: true,
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.once('exit', code => reject(new Error(`Server exited before startup: ${code}`)));
    server.stdout.once('data', resolve);
  });
}
async function stopServer() {
  if (server && server.exitCode === null) {
    const done = once(server, 'exit');
    server.kill();
    await done;
  }
}
// Monaco reuses line DOM nodes; DOM insertion order is not document line order.
const code = page => page.locator('.view-line').evaluateAll(nodes => nodes
  .sort((a, b) => a.getBoundingClientRect().top - b.getBoundingClientRect().top)
  .map(node => node.textContent.replaceAll('\u00a0', ' ').replaceAll('\u200b', '')).join('\n'));
async function editorEnd(page) {
  await page.locator('.monaco-editor').click({ position: { x: 150, y: 30 } });
  await page.keyboard.press('Control+End');
}
async function append(page, text) { await editorEnd(page); await page.keyboard.insertText(text); }
async function prepend(page, text) {
  await editorEnd(page);
  await page.keyboard.press('Control+Home');
  await page.keyboard.insertText(text);
}
async function expectCode(page, pattern) {
  for (let i = 0; i < 100; i++) {
    if (pattern.test(await code(page))) return;
    await page.waitForTimeout(50);
  }
  throw new Error(`Code does not match ${pattern}: ${await code(page)}`);
}
async function joined(page) {
  await page.getByRole('status').filter({ hasText: /^接続済み$/ }).waitFor();
}
try {
  await startServer();
  browser = await chromium.launch(process.platform === 'win32'
    ? { channel: 'msedge', headless: true }
    : { headless: true, executablePath: process.env.CHROMIUM_PATH });
  const host = await browser.newContext({ viewport: { width: 1360, height: 900 }, permissions: ['clipboard-read', 'clipboard-write'] });
  const guest = await browser.newContext({ viewport: { width: 1360, height: 900 } });
  host.setDefaultTimeout(20000);
  guest.setDefaultTimeout(20000);
  const a = await host.newPage();
  const b = await guest.newPage();
  for (const page of [a, b]) page.on('pageerror', error => errors.push(error.message));
  let blocked = false;
  let route;
  let upstream;
  await b.routeWebSocket('**/api/rooms/*/ws', ws => {
    if (blocked) { ws.close({ code: 1013 }); return; }
    route = ws;
    upstream = ws.connectToServer();
  });
  await a.goto(base);
  await a.getByLabel('表示名').fill('面接官');
  await a.getByRole('button', { name: '部屋を作成する' }).click();
  await joined(a);
  const url = a.url();
  const roomId = url.split('/').at(-1);
  assert.match(roomId, /^[a-f0-9]{32}$/);
  await b.goto(url);
  await b.getByLabel('表示名').fill('受験者');
  await b.getByRole('button', { name: '部屋に参加する' }).click();
  await joined(b);
  await b.getByText('面接官', { exact: true }).waitFor();
  assert.equal(await a.getByRole('button', { name: '部屋を終了', exact: true }).count(), 1);
  assert.equal(await b.getByRole('button', { name: '部屋を終了', exact: true }).isVisible(), false);
  check('Create, invite, join and host-only controls');
  await a.getByRole('button', { name: '参加URLをコピー', exact: true }).click();
  assert.equal(await a.evaluate(() => navigator.clipboard.readText()), url);
  await a.getByText('参加URLをコピーしました', { exact: true }).waitFor();
  check('Invite copies only the participant URL');

  const forbidden = await guest.request.post(`${base}/api/rooms/${roomId}/close`, { headers: { Origin: base } });
  assert.equal(forbidden.status(), 403);
  const foreign = await host.request.post(`${base}/api/rooms/${roomId}/close`, { headers: { Origin: 'https://other.example' } });
  assert.equal(foreign.status(), 403);
  check('Guest close and foreign-origin close are rejected');

  await append(a, '// 模擬面接 🦀\n');
  await expectCode(b, /模擬面接 🦀/);
  await Promise.all([prepend(a, '// interviewer\n'), append(b, '// candidate\n')]);
  for (const page of [a, b]) { await expectCode(page, /interviewer/); await expectCode(page, /candidate/); }
  assert.equal(await code(a), await code(b));
  check('Real WebSocket concurrent editing, Japanese and emoji');

  await append(a, '// own-change\n');
  await expectCode(b, /own-change/);
  await append(b, '// remote-change\n');
  await expectCode(a, /remote-change/);
  await a.getByRole('button', { name: '元に戻す', exact: true }).click();
  await expectCode(b, /remote-change/);
  await expectCode(a, /^(?![\s\S]*own-change)/);
  assert.ok((await code(a)).includes('remote-change'));
  await a.getByRole('button', { name: 'やり直す', exact: true }).click();
  await expectCode(b, /own-change/);
  await editorEnd(a);
  await a.keyboard.press('Control+z');
  await expectCode(a, /^(?![\s\S]*own-change)/);
  await a.keyboard.press('Control+Shift+z');
  await expectCode(b, /own-change/);
  check('Local-only Undo/Redo buttons and keyboard commands');

  await append(a, '// context-own\n');
  await expectCode(b, /context-own/);
  await append(b, '// context-remote\n');
  await expectCode(a, /context-remote/);
  await a.locator('.monaco-editor').click({ button: 'right', position: { x: 190, y: 70 } });
  // Monaco intentionally ignores mouse-up for the first 100ms after opening a menu.
  await a.waitForTimeout(150);
  await a.getByRole('menuitem', { name: '元に戻す', exact: true }).click();
  await expectCode(a, /^(?![\s\S]*context-own)/);
  assert.ok((await code(a)).includes('context-remote'));
  await a.locator('.monaco-editor').click({ button: 'right', position: { x: 190, y: 70 } });
  await a.waitForTimeout(150);
  await a.getByRole('menuitem', { name: 'やり直す', exact: true }).click();
  await expectCode(b, /context-own/);
  check('Context-menu Undo/Redo also preserves remote edits');

  await a.getByLabel('言語', { exact: true }).selectOption('javascript');
  await a.getByLabel('タブサイズ', { exact: true }).selectOption('2');
  await b.waitForFunction(() => document.querySelector('#language').value === 'javascript' && document.querySelector('#tab-size').value === '2');
  await b.getByLabel('タブサイズ', { exact: true }).selectOption('4');
  await a.waitForFunction(() => document.querySelector('#tab-size').value === '4');
  check('Shared language and indentation settings in both directions');

  for (const [language, prefix] of [['javascript', 'if (true) {'], ['python', 'if True:'], ['rust', 'fn main() {']]) {
    await a.getByLabel('言語', { exact: true }).selectOption(language);
    await editorEnd(a);
    await a.keyboard.press('Control+a');
    await a.keyboard.insertText(prefix);
    await a.keyboard.press('Enter');
    await a.keyboard.press('Tab');
    await a.keyboard.insertText('indented');
    await expectCode(a, /\n {8}indented/);
    await expectCode(b, /indented/);
    assert.ok(await a.locator('.view-line span[class*="mtk"]').count() > 1);
  }
  check('JavaScript, Python and Rust highlighting and Enter/Tab spaces');

  await editorEnd(a);
  await a.keyboard.press('Home');
  await a.keyboard.press('Shift+ArrowRight');
  await a.keyboard.press('Shift+ArrowRight');
  await b.locator('.yRemoteSelectionHead').first().waitFor();
  const label = await b.locator('.yRemoteSelectionHead').first().evaluate(el => getComputedStyle(el, '::after').content);
  assert.ok(label.includes('面接官'));
  check('Remote cursor, selection and name label');

  blocked = true;
  route.close({ code: 1011, reason: 'simulated network interruption' });
  upstream.close();
  await b.getByRole('status').filter({ hasText: /^再接続中$/ }).waitFor();
  assert.equal(await b.getByLabel('言語', { exact: true }).isDisabled(), true);
  await append(a, '\n// changed-while-disconnected');
  blocked = false;
  await joined(b);
  await expectCode(b, /changed-while-disconnected/);
  await append(b, '\n// resumed');
  await expectCode(a, /resumed/);
  check('Automatic reconnect, disabled offline controls and resynchronization');

  await b.reload();
  await b.getByLabel('表示名').fill('受験者');
  await b.getByRole('button', { name: '部屋に参加する' }).click();
  await joined(b);
  await expectCode(b, /changed-while-disconnected/);
  assert.equal(await b.getByLabel('言語', { exact: true }).inputValue(), 'rust');
  check('Reload restores current code and shared settings');

  await mkdir('test-results', { recursive: true });
  await a.screenshot({ path: 'test-results/room.png', fullPage: true });
  await a.getByRole('button', { name: '部屋を終了', exact: true }).click();
  await a.getByRole('button', { name: 'キャンセル', exact: true }).click();
  await joined(a);
  await a.getByRole('button', { name: '部屋を終了', exact: true }).click();
  await a.getByRole('button', { name: '終了する', exact: true }).click();
  for (const page of [a, b]) {
    await page.getByRole('heading', { name: 'この部屋は利用できません' }).waitFor();
    assert.equal(await page.locator('.monaco-editor').count(), 0);
  }
  assert.equal((await host.request.get(`${base}/api/rooms/${roomId}`)).status(), 404);
  await b.goto(url);
  await b.getByRole('heading', { name: 'この部屋は利用できません' }).waitFor();
  check('Cancel then close removes code for both participants and rejects old URL');

  await a.goto(base);
  await a.getByLabel('表示名').fill('面接官');
  await a.getByRole('button', { name: '部屋を作成する' }).click();
  await joined(a);
  const oldUrl = a.url();
  await stopServer();
  await startServer();
  await a.getByRole('heading', { name: 'この部屋は利用できません' }).waitFor();
  await b.goto(oldUrl);
  await b.getByRole('heading', { name: 'この部屋は利用できません' }).waitFor();
  check('Server restart invalidates old rooms without recreating them');
  assert.deepEqual(errors, []);
  check('No uncaught browser errors');
  await writeFile('test-results/e2e.json', JSON.stringify({ timestamp: new Date().toISOString(), browser: await browser.version(), results }, null, 2) + '\n');
} catch (error) {
  console.error(error);
  if (browser) {
    await mkdir('test-results', { recursive: true });
    for (const [i, page] of browser.contexts().flatMap(c => c.pages()).entries()) {
      await page.screenshot({ path: `test-results/failure-${i}.png` }).catch(() => {});
      console.error(`PAGE ${i}:`, (await page.locator('body').innerText().catch(() => '')).slice(0, 4000));
    }
  }
  process.exitCode = 1;
} finally {
  await browser?.close();
  await stopServer();
}
