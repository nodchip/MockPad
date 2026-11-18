#!/usr/bin/env python3
"""Run the staged release through an isolated, unprivileged nginx for validation."""
import argparse
from pathlib import Path
import socket
import subprocess


def run(*args):
    subprocess.run(args, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['start', 'stop'])
    parser.add_argument('stage', type=Path)
    args = parser.parse_args()
    stage = args.stage.resolve()
    preview = stage / 'preview'
    config = preview / 'nginx.conf'
    if args.action == 'stop':
        if (preview / 'nginx.pid').exists():
            run('/usr/sbin/nginx', '-p', str(preview) + '/', '-c', str(config), '-e', str(preview / 'error.log'), '-s', 'quit')
        subprocess.run(['systemctl', '--user', 'stop', 'mockpad-preview.service'], check=False)
        return
    if (preview / 'nginx.pid').exists():
        raise RuntimeError('Preview already exists; inspect it before starting again')
    for port in [3001, 3189]:
        with socket.socket() as check:
            check.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            check.bind(('127.0.0.1', port))
    state = subprocess.run(['systemctl', '--user', 'show', 'mockpad-preview.service', '--property=LoadState', '--value'], capture_output=True, text=True)
    if state.stdout.strip() not in ['', 'not-found']:
        raise RuntimeError('Refusing to replace an existing preview unit')
    preview.mkdir(exist_ok=True)
    config.write_text(f'''worker_processes 1;
pid {preview}/nginx.pid;
error_log {preview}/error.log;
events {{ worker_connections 1024; }}
http {{
    access_log {preview}/access.log;
    client_body_temp_path {preview}/client-body;
    proxy_temp_path {preview}/proxy;
    fastcgi_temp_path {preview}/fastcgi;
    uwsgi_temp_path {preview}/uwsgi;
    scgi_temp_path {preview}/scgi;
    map $http_upgrade $connection_upgrade {{ default upgrade; '' close; }}
    include {stage}/nginx-limits.conf;
    server {{
        listen 127.0.0.1:3189;
        server_name kishibe.dyndns.tv;
        absolute_redirect off;
        include {stage}/nginx-location.conf;
    }}
}}
''')
    run('/usr/sbin/nginx', '-p', str(preview) + '/', '-c', str(config), '-e', str(preview / 'error.log'), '-t')
    run('systemd-run', '--user', '--collect', '--unit=mockpad-preview',
        f'--property=WorkingDirectory={stage}', '--setenv=MOCKPAD_ADDR=127.0.0.1:3001',
        '--setenv=MOCKPAD_PUBLIC_ORIGIN=http://127.0.0.1:3189', '--setenv=MOCKPAD_TRUSTED_PROXY=127.0.0.1',
        '--setenv=MOCKPAD_BASE_PATH=/mockpad', str(stage / 'mockpad'))
    try:
        run('/usr/sbin/nginx', '-p', str(preview) + '/', '-c', str(config), '-e', str(preview / 'error.log'))
    except BaseException:
        subprocess.run(['systemctl', '--user', 'stop', 'mockpad-preview.service'], check=False)
        raise


if __name__ == '__main__':
    main()
