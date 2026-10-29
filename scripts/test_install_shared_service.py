"""Exercise installer rejection and preflight through its command-line boundary."""
import json
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


if __name__ == "__main__":
    unittest.main()
