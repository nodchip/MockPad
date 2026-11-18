"""Run on Linux: python3 -m unittest discover -s deploy/nighthawk -p test_health.py."""
import hashlib
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
HTML = b'<html>staged release</html>'
DIGEST = hashlib.sha256(HTML).hexdigest()


class ProxyHealthTest(unittest.TestCase):
    def check(self):
        installer.proxy_health_check('https://example.test/mockpad/', 'example.test:443:127.0.0.1', DIGEST, timeout=2)

    @patch.object(installer.time, 'sleep')
    @patch.object(installer.time, 'monotonic', side_effect=[0, 0, 1])
    def test_old_worker_404_is_retried(self, clock, sleep):
        responses = [subprocess.CompletedProcess([], 22, b'', b'HTTP 404'),
                     subprocess.CompletedProcess([], 0, HTML, b'')]
        with patch.object(installer.subprocess, 'run', side_effect=responses) as curl:
            self.check()
        self.assertEqual(curl.call_count, 2)
        self.assertIn('--noproxy', curl.call_args.args[0])

    @patch.object(installer.time, 'sleep')
    @patch.object(installer.time, 'monotonic', side_effect=[0, 0, 3])
    def test_persistent_http_error_reports_reason(self, clock, sleep):
        with patch.object(installer.subprocess, 'run', return_value=subprocess.CompletedProcess([], 22, b'', b'HTTP 502')):
            with self.assertRaisesRegex(RuntimeError, 'HTTP 502'):
                self.check()

    @patch.object(installer.time, 'sleep')
    @patch.object(installer.time, 'monotonic', side_effect=[0, 0, 3])
    def test_successful_wrong_site_is_rejected(self, clock, sleep):
        with patch.object(installer.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, b'WordPress', b'')):
            with self.assertRaisesRegex(RuntimeError, 'does not match'):
                self.check()


if __name__ == '__main__':
    unittest.main()
