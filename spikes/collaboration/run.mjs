import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';
import { chromium } from 'playwright-core';

const root = fileURLToPath(new URL('.', import.meta.url));
const executable = fileURLToPath(new URL('./rust/target/debug/mockpad-collaboration-spike.exe', import.meta.url));
const rust = spawn(executable, [], { stdio: ['pipe', 'pipe', 'inherit'], windowsHide: true });
const pending = [];
const lines = createInterface({ input: rust.stdout });
lines.on('line', line => pending.shift()?.resolve(JSON.parse(line)));
rust.on('error', error => { for (const item of pending.splice(0)) item.reject(error); });
rust.on('exit', code => { for (const item of pending.splice(0)) item.reject(new Error(`Rust exited: ${code}`)); });
function exchange(payload) {
  return new Promise((resolve, reject) => {
    pending.push({ resolve, reject });
    rust.stdin.write(JSON.stringify(payload) + '\n');
  });
}
const server = await createServer({
  root, server: { host: '127.0.0.1', port: 0 },
  resolve: { alias: [{
    find: 'monaco-editor/esm/vs/editor/editor.api.js', replacement: 'monaco-editor/editor',
  }] },
  plugins: [{ name: 'rust-spike-bridge', configureServer(vite) {
    vite.middlewares.use('/sync', async (req, res) => {
      try {
        const chunks = [];
        for await (const chunk of req) chunks.push(chunk);
        const result = await exchange(JSON.parse(Buffer.concat(chunks).toString()));
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify(result));
      } catch (error) { res.statusCode = 500; res.end(String(error)); }
    });
  } }],
});
let browser;
const results = [];
const check = (name, detail) => { results.push({ name, detail }); console.log(`PASS ${name}: ${detail}`); };
try {
  await server.listen();
  const url = server.resolvedUrls.local[0];
  browser = await chromium.launch({ channel: 'msedge', headless: true });
  const contextA = await browser.newContext();
  const contextB = await browser.newContext();
  contextA.setDefaultTimeout(20000);
  contextB.setDefaultTimeout(20000);
  const a = await contextA.newPage();
  const b = await contextB.newPage();
  for (const page of [a, b]) {
    page.on('pageerror', error => console.error('PAGE ERROR', error.message));
    await page.goto(url);
    await page.waitForFunction(() => Boolean(window.spike));
  }
  const call = (page, method, ...args) => page.evaluate(({ method, args }) => window.spike[method](...args), { method, args });
  const settle = async () => { for (const page of [a, b, a]) await call(page, 'sync'); };
  await call(a, 'insert', '// 模擬面接 🦀\n');
  await settle();
  assert.equal(await call(a, 'value'), await call(b, 'value'));
  check('Yjs–Yrs round trip', 'Japanese and emoji survive Rust synchronization');

  await call(a, 'insert', 'const interviewer = 1;\n');
  await call(b, 'insert', 'const candidate = 2;\n');
  await settle();
  const merged = await call(a, 'value');
  assert.equal(merged, await call(b, 'value'));
  assert.ok(merged.includes('interviewer') && merged.includes('candidate'));
  check('Concurrent edits', 'Independent editors converge and preserve both insertions');

  await call(a, 'clearUndo');
  await call(a, 'insert', '// own edit\n');
  await call(b, 'insert', '// remote edit\n');
  await settle();
  await call(a, 'focusEnd');
  await a.keyboard.press('Control+z');
  await settle();
  const undone = await call(b, 'value');
  assert.ok(!undone.includes('own edit') && undone.includes('remote edit'));
  await a.keyboard.press('Control+y');
  await settle();
  assert.ok((await call(b, 'value')).includes('own edit'));
  check('Local Undo/Redo', 'Ctrl+Z preserves remote edits; Ctrl+Y restores only own edit');

  await call(a, 'configure', 'plaintext', 2);
  await settle();
  assert.deepEqual(await call(b, 'config'), { language: 'plaintext', tabSize: 2 });
  await call(a, 'configure', 'javascript', 4);
  await settle();
  assert.deepEqual(await call(b, 'config'), { language: 'javascript', tabSize: 4 });
  check('Shared settings', 'Language and tab size propagate through Rust to the other editor');

  await contextB.setOffline(true);
  await call(a, 'insert', '// online\n');
  await call(a, 'sync');
  await call(b, 'insert', '// offline\n');
  await contextB.setOffline(false);
  await settle();
  const resumed = await call(a, 'value');
  assert.equal(resumed, await call(b, 'value'));
  assert.ok(resumed.includes('// online') && resumed.includes('// offline'));
  check('Reconnect data merge', 'Edits made during a network outage converge after explicit resync');

  const presence = await call(a, 'presence', '面接官', '#3366cc');
  await call(b, 'receivePresence', presence);
  assert.ok((await call(b, 'participants')).some(p => p.user?.name === '面接官' && p.user?.color === '#3366cc' && p.selection));
  assert.ok(await call(b, 'remoteSelections') > 0);
  check('Presence binding', 'Name, color and selection transfer; Monaco creates remote-selection decorations');

  await call(a, 'insert', 'if (true) {');
  await call(a, 'focusEnd');
  await a.keyboard.press('Enter');
  assert.ok((await call(a, 'value')).endsWith('\n    '));
  await a.keyboard.press('Tab');
  assert.ok((await call(a, 'value')).endsWith('\n        '));
  assert.ok(!(await call(a, 'value')).includes('\t'));
  check('Indentation', 'JavaScript Enter adds four spaces; Tab advances with spaces');

  await writeFile(new URL('./results.json', import.meta.url), JSON.stringify({
    timestamp: new Date().toISOString(), browser: await browser.version(),
    scope: 'Compatibility spike; HTTP/stdin bridge, not production WebSocket or room lifecycle', results,
  }, null, 2) + '\n');
} finally {
  await browser?.close();
  await server.close();
  rust.stdin.end();
  lines.close();
  rust.kill();
}
