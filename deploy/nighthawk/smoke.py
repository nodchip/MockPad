#!/usr/bin/env python3
"""Validate the staged Linux executable and actual nginx proxy on server loopback."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import time

ORIGIN = 'http://127.0.0.1:3189'
BASE = '/mockpad/'
TEXT = '// nighthawk 共同編集 🦀'


def request(method, path, cookie=None):
    connection = http.client.HTTPConnection('127.0.0.1', 3189, timeout=5)
    headers = {'Origin': ORIGIN}
    if cookie:
        headers['Cookie'] = cookie
    connection.request(method, path, headers=headers)
    response = connection.getresponse()
    result = response.status, dict(response.getheaders()), response.read()
    connection.close()
    return result


def uint(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def string(value):
    data = value.encode()
    return uint(len(data)) + data


class WebSocket:
    def __init__(self, room):
        import base64
        self.socket = socket.create_connection(('127.0.0.1', 3189), timeout=5)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (f'GET {BASE}api/rooms/{room}/ws HTTP/1.1\r\nHost: 127.0.0.1:3189\r\nOrigin: {ORIGIN}\r\n'
                   f'Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n')
        self.socket.sendall(request.encode())
        self.buffer = b''
        while b'\r\n\r\n' not in self.buffer:
            self.buffer += self.socket.recv(4096)
        header, self.buffer = self.buffer.split(b'\r\n\r\n', 1)
        assert header.startswith(b'HTTP/1.1 101 '), header.split(b'\r\n')[0]

    def exact(self, length):
        while len(self.buffer) < length:
            received = self.socket.recv(max(4096, length - len(self.buffer)))
            if not received:
                raise RuntimeError('Unexpected WebSocket EOF')
            self.buffer += received
        result, self.buffer = self.buffer[:length], self.buffer[length:]
        return result

    def send(self, payload, opcode=2):
        length = len(payload)
        header = bytes([128 | opcode])
        if length < 126:
            header += bytes([128 | length])
        else:
            header += bytes([128 | 126]) + struct.pack('!H', length)
        mask = os.urandom(4)
        self.socket.sendall(header + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(payload)))

    def binary(self):
        for _ in range(10):
            first, second = self.exact(2)
            length = second & 127
            if length == 126:
                length = struct.unpack('!H', self.exact(2))[0]
            elif length == 127:
                length = struct.unpack('!Q', self.exact(8))[0]
            assert length <= 3 * 1024 * 1024
            payload = self.exact(length)
            if first & 15 == 9:
                self.send(payload, opcode=10)
            elif first & 15 == 2:
                return payload
            elif first & 15 == 8:
                raise RuntimeError('WebSocket closed before a binary message')
        raise RuntimeError('No binary message received')


def main():
    stage = Path(sys.argv[1]).resolve()
    preview = Path(__file__).with_name('preview.py')
    sockets = []
    started = False
    try:
        subprocess.run([sys.executable, str(preview), 'start', str(stage)], check=True)
        started = True
        status = None
        for _ in range(30):
            try:
                status, headers, body = request('GET', BASE)
                if status == 200:
                    break
            except OSError:
                pass
            time.sleep(0.2)
        if status != 200:
            print(f'Preview HTTP status: {status}', flush=True)
            subprocess.run(['systemctl', '--user', 'show', 'mockpad-preview.service', '--property=WorkingDirectory', '--property=Environment'], check=False)
            for direct_path in ['/mockpad/', '/mockpad/index.html', '/mockpad/rooms/test', '/', '/mockpad/api/rooms/test']:
                direct = http.client.HTTPConnection('127.0.0.1', 3001, timeout=2)
                direct.request('GET', direct_path)
                print(f'Direct {direct_path}: {direct.getresponse().status}', flush=True)
                direct.close()
        assert status == 200 and b'MockPad' in body
        assert request('GET', '/mockpad')[0] == 308
        assert headers['Referrer-Policy'] == 'no-referrer'
        assert 'frame-ancestors' in headers['content-security-policy']
        manifest = json.loads((stage / 'manifest.json').read_text())
        for asset in re.findall(r'(?:src|href)="(/mockpad/assets/[^\"]+)"', body.decode()):
            status, _, content = request('GET', asset)
            assert status == 200
            relative = 'dist/' + asset.removeprefix(BASE)
            assert hashlib.sha256(content).hexdigest() == manifest['files'][relative]
        status, headers, body = request('POST', BASE + 'api/rooms')
        assert status == 201
        room = json.loads(body)['id']
        assert request('GET', BASE + f'r/{room}')[0] == 200
        cookie = headers['set-cookie']
        assert f'Path={BASE}api/rooms/{room}' in cookie and 'HttpOnly' in cookie
        cookie = cookie.split(';')[0]
        status, _, body = request('GET', BASE + f'api/rooms/{room}', cookie)
        assert status == 200 and json.loads(body)['isHost']
        status, _, body = request('GET', BASE + f'api/rooms/{room}')
        assert status == 200 and not json.loads(body)['isHost']
        alice, bob = WebSocket(room), WebSocket(room)
        sockets.extend([alice, bob])
        alice.binary()
        bob.binary()
        # A normal Yjs text insertion: one client, one ContentString in root code.
        update = uint(1) + uint(1) + uint(20261025) + uint(0) + b'\x04\x01' + string('code') + string(TEXT) + uint(0)
        alice.send(b'\x00\x02' + uint(len(update)) + update)
        assert TEXT.encode() in bob.binary()
        rejoined = WebSocket(room)
        sockets.append(rejoined)
        rejoined.binary()
        rejoined.send(b'\x00\x00\x01\x00')
        assert TEXT.encode() in rejoined.binary()
        assert request('POST', BASE + f'api/rooms/{room}/close')[0] == 403
        assert request('POST', BASE + f'api/rooms/{room}/close', cookie)[0] == 204
        assert request('GET', BASE + f'api/rooms/{room}')[0] == 404
        print('PASS Linux + nginx: subpath assets/checksums, security headers, scoped management cookie, WebSocket editing, resync, host-only close')
    finally:
        for connection in sockets:
            connection.socket.close()
        if started:
            subprocess.run([sys.executable, str(preview), 'stop', str(stage)], check=False)


if __name__ == '__main__':
    main()
