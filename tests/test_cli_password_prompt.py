"""Exercise real masked terminal input; fixture passwords are public test data."""
import json
import os
import pathlib
import pty
import select
import signal
import subprocess
import sys
import tempfile
import termios
import time
import unittest

BINARY = str(pathlib.Path(sys.argv.pop(1)).resolve()) if len(sys.argv) > 1 else str(pathlib.Path('target/debug/deep-obsidian-cli').resolve())
ROOT = pathlib.Path(__file__).resolve().parents[1]
PASSWORD = "correct $:! 'quoted' \\password"


class PasswordPrompt(unittest.TestCase):
    def run_terminal(self, args, env, answers):
        pid, master = pty.fork()
        if pid == 0:
            os.execve(BINARY, [BINARY, *args], env)
        output = b''
        status = None
        deadline = time.monotonic() + 45
        try:
            for prompt, answer in answers:
                while prompt.encode() not in output:
                    self.assertLess(time.monotonic(), deadline, 'password prompt timed out: ' + output.decode(errors='replace'))
                    child, raw = os.waitpid(pid, os.WNOHANG)
                    if child:
                        status = os.waitstatus_to_exitcode(raw)
                        self.fail('CLI exited before prompt: ' + output.decode(errors='replace'))
                    if select.select([master], [], [], .1)[0]:
                        output += os.read(master, 65536)
                # rpassword prints the prompt before disabling echo. Wait for the
                # actual terminal mode, rather than racing it with an early paste.
                while termios.tcgetattr(master)[3] & termios.ECHO:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.01)
                os.write(master, answer.encode() + b'\n')
                output = output.replace(prompt.encode(), b'<answered>', 1)
            while status is None:
                if select.select([master], [], [], .1)[0]:
                    try:
                        output += os.read(master, 65536)
                    except OSError:
                        pass
                child, raw = os.waitpid(pid, os.WNOHANG)
                if child:
                    status = os.waitstatus_to_exitcode(raw)
                self.assertLess(time.monotonic(), deadline, 'CLI did not finish')
            text = output.decode(errors='replace')
            for _, answer in answers:
                self.assertNotIn(answer, text, 'masked password was echoed')
            self.assertEqual(status, 0, text)
            return text
        finally:
            if status is None:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            os.close(master)

    def test_masked_secret_input_preserves_special_characters(self):
        with tempfile.TemporaryDirectory(prefix='cli-password-') as directory:
            base = pathlib.Path(directory)
            vault = base / 'vault'
            vault.mkdir()
            config = base / 'config.json'
            config.write_text(json.dumps({'indexDir': str(base / 'index'), 'experimental': {'couchdbVaults': True}, 'mounts': [
                {'id': 'vault', 'mountAt': '', 'backend': {'kind': 'couchdb', 'url': 'http://localhost:1', 'database': 'test', 'username': 'laura', 'passwordRef': {'kind': 'encryptedFile', 'id': 'test-password'}}}
            ]}))
            env = {**os.environ, 'XDG_CONFIG_HOME': str(base / 'config'), 'HOME': directory}
            self.run_terminal(['--config', str(config), 'secrets', 'set', '--mount', 'vault'], env, [('CouchDB password', PASSWORD)])
            check = subprocess.run([BINARY, '--config', str(config), 'secrets', 'check'], env=env, capture_output=True, text=True)
            self.assertEqual(check.returncode, 0, check.stderr)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'mount creation uses the OS keyring on macOS; run the retry scenario in isolated Linux CI')
    def test_wrong_password_is_cleaned_up_and_retry_can_succeed(self):
        with tempfile.TemporaryDirectory(prefix='cli-retry-') as directory:
            base = pathlib.Path(directory)
            vault = base / 'vault'
            vault.mkdir()
            config = base / 'config.json'
            config.write_text(json.dumps({'vaultPath': str(vault), 'indexDir': str(base / 'index'), 'experimental': {'multiVault': True, 'couchdbVaults': True}}))
            env = {**os.environ, 'XDG_CONFIG_HOME': str(base / 'config'), 'HOME': directory, 'MOCK_COUCH_PASSWORD': PASSWORD}
            mock = subprocess.Popen(['node', 'test/mock-couch-server.mjs', '--vault', 'small'], cwd=ROOT / 'sidecar/livesync-sidecar', env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            try:
                remote = json.loads(mock.stdout.readline())
                scripted = subprocess.run([BINARY, '--config', str(config), 'mounts', 'add', 'couchdb', '--id', 'private', '--mount-at', 'Private', '--url', remote['url'], '--database', remote['database'], '--username', 'laura', '--password-stdin', '--yes'], env=env, input='wrong-fixture-password\n', text=True, capture_output=True, timeout=30)
                self.assertNotEqual(scripted.returncode, 0)
                self.assertNotIn('Retry 1/2', scripted.stderr)
                self.assertNotIn('mounts', json.loads(config.read_text()))
                text = self.run_terminal(['--config', str(config), 'mounts', 'add', 'couchdb', '--id', 'private', '--mount-at', 'Private', '--url', remote['url'], '--database', remote['database'], '--username', 'laura', '--yes'], env,
                    [('CouchDB password for laura', 'wrong-fixture-password'), ('CouchDB password for laura', PASSWORD)])
                self.assertIn('Retry 1/2', text)
                self.assertEqual(len(json.loads(config.read_text())['mounts']), 2)
            finally:
                mock.stdin.close()
                mock.wait(timeout=5)


if __name__ == '__main__':
    unittest.main()
