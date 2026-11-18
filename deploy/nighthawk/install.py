#!/usr/bin/env python3
"""First installation on nighthawk; --check is read-only and does not need sudo."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import subprocess
import tempfile
import time
import urllib.request

STAGE = Path(__file__).resolve().parent
SITE = Path('/etc/nginx/sites-enabled/default')
LIMITS = Path('/etc/nginx/conf.d/mockpad-limits.conf')
LOCATION = Path('/etc/nginx/snippets/mockpad.conf')
UNIT = Path('/etc/systemd/system/mockpad.service')
CURRENT = Path('/opt/mockpad/current')
INCLUDE = '\tinclude /etc/nginx/snippets/mockpad.conf;\n\n'
ANCHOR = '\tlocation = /qmaclone {'


def run(*args):
    subprocess.run(args, check=True)


def atomic_write(path, data):
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix='.mockpad-', delete=False) as temporary:
        os.fchmod(temporary.fileno(), 0o644)
        temporary.write(data)
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, path)


def verify():
    manifest = json.loads((STAGE / 'manifest.json').read_text())
    if not re.fullmatch(r'\d{8}T\d{6}Z-[0-9a-f]{12}', manifest['release']):
        raise RuntimeError('Invalid release name')
    for relative, expected in manifest['files'].items():
        file = STAGE / relative
        if file.is_symlink() or not file.resolve().is_relative_to(STAGE):
            raise RuntimeError(f'Unsafe release file: {relative}')
        if hashlib.sha256(file.read_bytes()).hexdigest() != expected:
            raise RuntimeError(f'Checksum mismatch: {relative}')
    required = {'mockpad', 'dist/index.html', 'install.py', 'mockpad.service', 'nginx-limits.conf', 'nginx-location.conf'}
    if not required.issubset(manifest['files']):
        raise RuntimeError('Incomplete release')
    original = SITE.read_bytes()
    if hashlib.sha256(original).hexdigest() != manifest['expected_nginx_sha256']:
        raise RuntimeError('Existing nginx configuration changed; re-review before installing')
    text = original.decode()
    if text.count(ANCHOR) != 1 or INCLUDE in text:
        raise RuntimeError('nginx insertion point is not unique or MockPad is already installed')
    for file in (LIMITS, LOCATION, UNIT, CURRENT):
        if file.exists() or file.is_symlink():
            raise RuntimeError(f'Refusing to overwrite an existing installation: {file}')
    return manifest, original, text.replace(ANCHOR, INCLUDE + ANCHOR, 1).encode()


def health_check(url):
    for _ in range(30):
        try:
            with urllib.request.urlopen(url, timeout=2) as response:
                if response.status == 200 and b'/mockpad/assets/' in response.read():
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.5)
    raise RuntimeError('MockPad did not become healthy')


def proxy_health_check(url, resolve, expected_sha256, timeout=30):
    # nginx -s reload acknowledges the signal before new workers serve traffic.
    # Use a fresh connection each time while waiting for the exact staged HTML.
    deadline = time.monotonic() + timeout
    last_error = 'No response'
    while time.monotonic() < deadline:
        result = subprocess.run(
            ['curl', '--fail', '--silent', '--show-error', '--noproxy', '*',
             '--max-time', '3', '--resolve', resolve, url], capture_output=True)
        if result.returncode == 0:
            if hashlib.sha256(result.stdout).hexdigest() == expected_sha256:
                return
            last_error = 'Response HTML does not match the staged release'
        else:
            last_error = result.stderr.decode(errors='replace').strip()
        time.sleep(0.5)
    raise RuntimeError(f'nginx route did not become healthy: {last_error}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    manifest, original, configured = verify()
    print(f"Verified {manifest['release']}: {len(manifest['files'])} checksums; nginx change is limited to one include")
    if args.check:
        print('Ready: local assets, binary, service and /mockpad/ proxy configuration are staged')
        return
    if os.geteuid() != 0:
        raise SystemExit('Run this final installation with sudo')
    run('/usr/sbin/nginx', '-t')
    release = Path('/opt/mockpad/releases') / manifest['release']
    if release.exists():
        raise RuntimeError('Release directory already exists; inspect the previous attempt')
    backup = Path('/var/backups') / ('mockpad-' + manifest['release'])
    backup.mkdir(mode=0o700)
    (backup / 'nginx-default').write_bytes(original)
    (backup / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    try:
        try:
            pwd.getpwnam('mockpad')
        except KeyError:
            run('/usr/sbin/useradd', '--system', '--user-group', '--no-create-home', '--shell', '/usr/sbin/nologin', 'mockpad')
        release.mkdir(parents=True)
        shutil.copy2(STAGE / 'mockpad', release / 'mockpad')
        (release / 'mockpad').chmod(0o755)
        shutil.copytree(STAGE / 'dist', release / 'dist')
        for directory, _, filenames in os.walk(release):
            Path(directory).chmod(0o755)
            for filename in filenames:
                file = Path(directory) / filename
                file.chmod(0o755 if file.name == 'mockpad' else 0o644)
        CURRENT.symlink_to(release)
        atomic_write(UNIT, (STAGE / 'mockpad.service').read_bytes())
        atomic_write(LIMITS, (STAGE / 'nginx-limits.conf').read_bytes())
        atomic_write(LOCATION, (STAGE / 'nginx-location.conf').read_bytes())
        atomic_write(SITE, configured)
        run('/usr/sbin/nginx', '-t')
        run('systemd-analyze', 'verify', str(UNIT))
        run('systemctl', 'daemon-reload')
        run('systemctl', 'enable', '--now', 'mockpad.service')
        health_check('http://127.0.0.1:3001/mockpad/')
        run('systemctl', 'reload', 'nginx')
        proxy_health_check('https://kishibe.dyndns.tv/mockpad/', 'kishibe.dyndns.tv:443:127.0.0.1',
                           manifest['files']['dist/index.html'])
    except BaseException:
        atomic_write(SITE, original)
        subprocess.run(['systemctl', 'disable', '--now', 'mockpad.service'], check=False)
        for file in (UNIT, LIMITS, LOCATION, CURRENT):
            if file.is_symlink() or file.exists():
                file.unlink()
        subprocess.run(['systemctl', 'daemon-reload'], check=False)
        subprocess.run(['/usr/sbin/nginx', '-t'], check=False)
        subprocess.run(['systemctl', 'reload', 'nginx'], check=False)
        print(f'Installation failed; prior nginx configuration restored. Backup: {backup}')
        raise
    print(f"Deployed {manifest['public_url']}")
    print(f'Backup: {backup}')
    run('systemctl', 'is-active', 'mockpad.service', 'nginx.service')


if __name__ == '__main__':
    main()
