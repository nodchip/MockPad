import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';
import * as encoding from 'lib0/encoding';
import * as syncProtocol from 'y-protocols/sync';

const base = 'http://127.0.0.1:3192';
const results = [];
const check = name => { results.push(name); console.log(`PASS ${name}`); };
const executable = process.platform === 'win32' ? './target/debug/mockpad.exe' : './target/debug/mockpad';
let server, browser;
const errors = [];
let stderr = '';

function presence(json) {
  const body = encoding.createEncoder();
  for (const value of [1, 4242, 1]) encoding.writeVarUint(body, value);
  encoding.writeVarString(body, json);
  const message = encoding.createEncoder();
  encoding.writeVarUint(message, 1);
  encoding.writeVarUint8Array(message, encoding.toUint8Array(body));
  return encoding.toUint8Array(message);
}

async function open(page, room) {
  return page.evaluate(async room => {
    window.probes ??= [];
    const socket = new WebSocket(`${location.origin.replace('http', 'ws')}/api/rooms/${room}/ws`);
    const state = { socket, close: null, received: 0 };
    socket.onclose = event => { state.close = event.code; };
    socket.onmessage = () => { state.received++; };
    const index = window.probes.push(state) - 1;
    await new Promise((resolve, reject) => {
      socket.onopen = resolve;
      socket.onerror = () => reject(new Error('WebSocket refused'));
    });
    return index;
  }, room);
}

async function rejected(page, room, bytes) {
  const index = await open(page, room);
  await page.evaluate(({ index, bytes }) => window.probes[index].socket.send(new Uint8Array(bytes)), { index, bytes: [...bytes] });
  await page.waitForFunction(index => window.probes[index].close !== null, index);
  assert.equal(await page.evaluate(index => window.probes[index].close, index), 4002);
}

try {
  server = spawn(executable, [], {
    env: { ...process.env, MOCKPAD_ADDR: '127.0.0.1:3192', MOCKPAD_PUBLIC_ORIGIN: base },
    stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true,
  });
  server.stderr.on('data', chunk => { stderr += chunk.toString(); });
  await Promise.race([
    once(server.stdout, 'data'),
    once(server, 'error').then(([error]) => { throw error; }),
    once(server, 'exit').then(([code]) => { throw new Error(`Server exited: ${code}`); }),
  ]);
  browser = await chromium.launch(process.platform === 'win32' ? { channel: 'msedge', headless: true } : { headless: true, executablePath: process.env.CHROMIUM_PATH });
  const context = await browser.newContext();
  context.setDefaultTimeout(10000);
  const victim = await context.newPage();
  victim.on('pageerror', error => errors.push(error.message));
  victim.on('console', message => {
    if (message.type() === 'error' && /content security policy|violates.*directive/i.test(message.text())) errors.push(message.text());
  });
  const response = await victim.goto(base);
  const headers = response.headers();
  assert.equal(headers['x-frame-options'], 'DENY');
  assert.equal(headers['x-content-type-options'], 'nosniff');
  assert.equal(headers['referrer-policy'], 'no-referrer');
  assert.ok(headers['content-security-policy'].includes("frame-ancestors 'none'"));
  assert.ok(headers['content-security-policy'].includes("script-src 'self'"));
  assert.equal(headers['permissions-policy'], 'camera=(), microphone=(), geolocation=()');
  check('Security headers on actual HTTP responses');

  await victim.getByLabel('表示名').fill('review-victim');
  await victim.getByRole('button', { name: '部屋を作成する' }).click();
  await victim.locator('#connection').filter({ hasText: /^接続済み$/ }).waitFor();
  const room = victim.url().split('/').at(-1);
  const attacker = await browser.newPage();
  attacker.setDefaultTimeout(10000);
  await attacker.goto(base);
  let victimReconnects = 0;
  // The existing socket was opened before the listener; a reconnect would still be observable.
  victim.on('websocket', () => { victimReconnects++; });
  await rejected(attacker, room, presence('{"user":{"name":{"toString":null},"color":"#237b65"}}'));
  await rejected(attacker, room, presence('{"selection":{"anchor":{},"head":{"item":{"client":1,"clock":"bad"}}}}'));
  assert.equal(victimReconnects, 0);
  assert.equal(await victim.locator('#connection').textContent(), '接続済み');
  check('Malformed name and selection disconnect only the sender');

  const update = encoding.createEncoder();
  for (const value of [1, 1, 123, 0]) encoding.writeVarUint(update, value);
  encoding.writeUint8(update, 0x28);
  encoding.writeVarUint(update, 1);
  encoding.writeVarString(update, 'settings');
  encoding.writeVarString(update, 'language');
  encoding.writeVarUint(update, 1);
  for (let i = 0; i < 4096; i++) { encoding.writeUint8(update, 117); encoding.writeVarUint(update, 1); }
  encoding.writeUint8(update, 126);
  encoding.writeVarUint(update, 0);
  const packet = encoding.createEncoder();
  encoding.writeVarUint(packet, 0);
  syncProtocol.writeUpdate(packet, encoding.toUint8Array(update));
  await rejected(attacker, room, encoding.toUint8Array(packet));
  assert.equal((await fetch(`${base}/api/rooms/${room}`)).status, 200);
  check('4096 nested values cannot crash the server');

  const rapid = await open(attacker, room);
  await attacker.evaluate(index => {
    for (let i = 0; i < 121; i++) window.probes[index].socket.send(new Uint8Array());
  }, rapid);
  await attacker.waitForFunction(index => window.probes[index].close !== null, rapid);
  // A TCP reset is also possible when the sender keeps writing during server close.
  assert.ok([4002, 1006].includes(await attacker.evaluate(index => window.probes[index].close, rapid)));
  check('WebSocket message rate limit disconnects the sender');

  const peers = [];
  for (let i = 0; i < 7; i++) peers.push(await open(attacker, room));
  await assert.rejects(open(attacker, room), /WebSocket refused/);
  await attacker.evaluate(indices => { for (const i of indices) window.probes[i].socket.close(); }, peers);
  check('Ninth participant is refused');

  // One room was created by the victim; nine more exhaust this IP's minute budget.
  for (let i = 0; i < 9; i++) {
    assert.equal((await fetch(`${base}/api/rooms`, { method: 'POST', headers: { Origin: base } })).status, 201);
  }
  const limited = await fetch(`${base}/api/rooms`, {
    method: 'POST', headers: { Origin: base, 'X-Forwarded-For': '192.0.2.123' },
  });
  assert.equal(limited.status, 429);
  assert.equal(limited.headers.get('x-frame-options'), 'DENY');
  check('Creation limit cannot be bypassed with X-Forwarded-For');

  const assets = (await readdir('dist/assets')).filter(name => name.endsWith('.js'));
  const bundle = (await Promise.all(assets.map(name => readFile(`dist/assets/${name}`, 'utf8')))).join('\n');
  assert.ok(/(?:DOMPurify |version\s*=\s*["'`])3\.4\.17/.test(bundle));
  assert.ok(!/(?:DOMPurify |version\s*=\s*["'`])3\.4\.15/.test(bundle));
  check('Production bundle uses DOMPurify 3.4.17 instead of the Monaco vendor copy');
  await victim.locator('.monaco-editor').click({ position: { x: 150, y: 30 } });
  await victim.keyboard.insertText('あ'.repeat(88_000));
  await victim.getByRole('heading', { name: 'この部屋は利用できません' }).waitFor();
  assert.ok((await victim.locator('#reason').textContent()).includes('上限'));
  check('Oversized paste exits cleanly without a reconnect loop');
  assert.deepEqual(errors, []);
  assert.equal(stderr, '');
  check('No browser exceptions, CSP violations or server errors');
  await mkdir('test-results', { recursive: true });
  await writeFile('test-results/security.json', JSON.stringify({ results }, null, 2) + '\n');
} finally {
  await browser?.close();
  if (server && server.exitCode === null && server.signalCode === null) {
    const done = once(server, 'exit');
    server.kill();
    await done;
  }
}
