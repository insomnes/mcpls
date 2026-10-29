"""Exercise installer rejection and preflight through its command-line boundary."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("install-shared-service.py").resolve()


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="mcpls-installer-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.executable("git", "#!/bin/sh\nif [ \"$1\" = rev-parse ]; then printf '%040d\\n' 0; fi\n")
        self.executable("cargo", "#!/bin/sh\nexit 0\n")
        self.target = self.root / "target"
        release = self.target / "release"
        release.mkdir(parents=True)
        binary = release / "mcpls"
        binary.write_text("#!/usr/bin/env python3\nimport sys\nsys.stderr.write('invalid dispatcher profile: forbidden mcp table\\n')\nsys.exit(1)\n")
        binary.chmod(0o755)
        self.profiles = self.root / "source"
        self.profiles.mkdir()
        (self.profiles / "profile.toml").write_text('[mcp]\ntool_profile="agent"\n')
        self.destination = self.root / "installed-profiles"
        self.config = self.root / "daemon.toml"
        self.config.write_text('[dispatcher]\nenabled=true\n[[dispatcher.workspaces]]\npath=' + json.dumps(str(self.root)) + '\nprofile=' + json.dumps(str(self.destination / "profile.toml")) + '\n')

    def executable(self, name, content):
        path = self.bin / name
        path.write_text(content)
        path.chmod(0o755)

    def check(self):
        environment = os.environ | {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"], "CARGO_TARGET_DIR": str(self.target)}
        return subprocess.run([sys.executable, "-B", str(SCRIPT), "--check", "--revision", "0" * 40,
                               "--config", str(self.config), "--profiles-dir", str(self.profiles),
                               "--profiles-path", str(self.destination)], env=environment, text=True,
                              capture_output=True, timeout=30)

    def test_symlink_directory_is_rejected_before_build_or_mutation(self):
        original = self.root / "original"
        original.mkdir()
        profile = original / "profile.toml"
        profile.write_text("old profile bytes")
        self.destination.symlink_to(original, target_is_directory=True)
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("profile destination must not be a symlink", result.stderr)
        self.assertNotIn("Building", result.stdout)
        self.assertTrue(self.destination.is_symlink())
        self.assertEqual(profile.read_text(), "old profile bytes")

    def test_preflight_eof_reports_stage_actual_stderr_and_retained_log(self):
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("configuration preflight failed", result.stderr)
        self.assertIn("invalid dispatcher profile: forbidden mcp table", result.stderr)
        self.assertNotIn("Expecting value", result.stderr)
        log = Path(re.search(r"diagnostic log: ([^\n]+)", result.stderr).group(1))
        self.addCleanup(shutil.rmtree, log.parent)
        self.assertEqual(log.read_text(), "invalid dispatcher profile: forbidden mcp table\n")
        self.assertLessEqual(log.stat().st_size, 8192)
        self.assertFalse(self.destination.exists())


    def test_install_readiness_skips_sse_priming_and_repeats_with_rollback_assets(self):
        binary = self.target / "release/mcpls"
        binary.write_text("#!/usr/bin/env python3\nimport json,sys,time\njson.loads(sys.stdin.readline())\nprint(json.dumps({'jsonrpc':'2.0','id':1,'result':{'serverInfo':{'name':'mcpls'}}}),flush=True)\ntime.sleep(30)\n")
        observed = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                observed.append(request)
                data = (': priming\n\ndata: \nid: 0\nretry: 3000\n\n'
                        'event: ping\ndata: ignored-non-message\n\n'
                        'data: {"jsonrpc":"2.0","method":"notifications/keepalive"}\n\n'
                        'data: ' + json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': {'serverInfo': {'name': 'mcpls'}}}) + '\n\n').encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Mcp-Session-Id', 'fixture-session')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            def do_DELETE(self):
                observed.append({'deleted': self.headers.get('Mcp-Session-Id')})
                self.send_response(202)
                self.end_headers()
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        unit = self.root / 'existing.service'
        unit.write_text('[Service]\nExecStart=/previous/mcpls\n')
        release = self.root / 'releases' / ('0' * 40) / 'mcpls'
        self.executable('systemctl', '#!/usr/bin/env python3\nimport sys\nif "--property=FragmentPath" in sys.argv: print(' + repr(str(unit)) + ')\nif "--property=ExecStart" in sys.argv: print(' + repr(str(release)) + ')\nif "is-active" in sys.argv: sys.exit(3)\n')
        installed = self.root / 'installed.toml'
        installed.write_text('# previous configuration\n')
        cli = self.root / 'mcpls'
        cli.write_text('previous executable')
        environment = os.environ | {'PATH': str(self.bin) + os.pathsep + os.environ['PATH'], 'CARGO_TARGET_DIR': str(self.target), 'XDG_CONFIG_HOME': str(self.root / 'config')}
        command = [sys.executable, '-B', str(SCRIPT), '--revision', '0' * 40,
                   '--config', str(self.config), '--profiles-dir', str(self.profiles), '--profiles-path', str(self.destination),
                   '--config-path', str(installed), '--cli-path', str(cli), '--release-root', str(self.root / 'releases'),
                   '--backup-root', str(self.root / 'backups'), '--endpoint', f'http://127.0.0.1:{server.server_port}/mcp', '--readiness-timeout', '2']
        for _ in range(2):
            result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('Rollback retained:', result.stdout)
            self.assertEqual(cli.resolve(), release)
        backups = sorted((self.root / 'backups').glob('*/state.json'))
        self.assertEqual(len(backups), 2)
        self.assertTrue(any((path.parent / 'daemon.toml').read_text() == '# previous configuration\n' for path in backups))
        self.assertEqual(sum(item.get('method') == 'initialize' for item in observed), 2)
        self.assertEqual(sum(item.get('deleted') == 'fixture-session' for item in observed), 2)


if __name__ == "__main__":
    unittest.main()
