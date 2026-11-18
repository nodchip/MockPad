import { cp, mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../../', import.meta.url));
const binary = path.join(root, 'target/x86_64-unknown-linux-musl/release/mockpad');
const bytes = await readFile(binary);
if (!bytes.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70]))) throw new Error('Expected a Linux ELF binary');
const html = await readFile(path.join(root, 'dist/index.html'), 'utf8');
if (!html.includes('src="/mockpad/assets/')) throw new Error('Build dist with MOCKPAD_BASE_PATH=/mockpad first');
const digest = data => createHash('sha256').update(data).digest('hex');
const release = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d{3}Z$/, 'Z') + '-' + digest(bytes).slice(0, 12);
const directory = path.join(root, '.cache/deploy', release);
await mkdir(path.dirname(directory), { recursive: true });
await mkdir(directory);
await cp(binary, path.join(directory, 'mockpad'));
await cp(path.join(root, 'dist'), path.join(directory, 'dist'), { recursive: true });
for (const file of ['install.py', 'mockpad.service', 'nginx-limits.conf', 'nginx-location.conf']) {
  await cp(path.join(root, 'deploy/nighthawk', file), path.join(directory, file));
}
const files = {};
async function scan(dir, prefix = '') {
  for (const item of await readdir(dir, { withFileTypes: true })) {
    const relative = prefix + item.name;
    const full = path.join(dir, item.name);
    if (item.isSymbolicLink()) throw new Error('Release cannot contain symlinks');
    if (item.isDirectory()) await scan(full, relative + '/');
    else files[relative] = digest(await readFile(full));
  }
}
await scan(directory);
const manifest = {
  release, source_head: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(),
  source_dirty: Boolean(execFileSync('git', ['status', '--porcelain'], { cwd: root, encoding: 'utf8' }).trim()),
  public_url: 'https://kishibe.dyndns.tv/mockpad/', base_path: '/mockpad',
  expected_nginx_sha256: '90ebcce2e2bf2a943de0bf06d9ebd44157f77f8864711662aace83f783c98188',
  files,
};
await writeFile(path.join(directory, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
const archive = `${directory}.tar.gz`;
execFileSync('tar', ['-czf', archive, '-C', path.dirname(directory), release], { cwd: root });
console.log(JSON.stringify({ release, directory, archive, archive_sha256: digest(await readFile(archive)) }, null, 2));
