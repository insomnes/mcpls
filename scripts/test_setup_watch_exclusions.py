"""Exercise explicit Watchman policy setup through the subprocess boundary."""
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("setup-watch-exclusions.py").resolve()
BASELINE = [
    ".agnt-tsks", "target", ".venv", "venv", "node_modules", "__pycache__",
    ".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox", ".nox", ".uv-cache",
    ".uv_cache", ".hypothesis", ".next", ".nuxt", ".turbo", ".pnpm-store",
]


class WatchExclusionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="mcpls-watch-setup-test-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name).resolve()
        self.root = self.directory / "project"
        self.root.mkdir()
        self.profile = self.directory / "profile.toml"
        self.profile.write_text('[workspace.file_watching]\nbackend="watchman"\n')
        self.state = self.directory / "state"
        self.config = self.root / ".watchmanconfig"

    def invoke(self, *arguments, root=None, profile=None, state=None):
        return subprocess.run(
            [sys.executable, "-B", str(SCRIPT), "--profile", str(profile or self.profile),
             "--root", str(root or self.root), *arguments],
            check=False, env=os.environ | {"XDG_STATE_HOME": str(state or self.state)},
            text=True, capture_output=True, timeout=10)

    def success(self, *arguments, **kwargs):
        result = self.invoke(*arguments, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_new_root_dry_run_apply_and_byte_identical_repeat(self):
        receipt = self.success()
        self.assertEqual(receipt, {"root": str(self.root), "changed": True, "applied": False,
                                   "ignore_dirs": BASELINE, "backup": None})
        self.assertEqual(list(self.root.iterdir()), [])
        self.assertFalse(self.state.exists())
        self.assertTrue(self.success("--apply")["applied"])
        original = self.config.read_bytes()
        receipt = self.success("--apply")
        self.assertFalse(receipt["changed"])
        self.assertFalse(receipt["applied"])
        self.assertIsNone(receipt["backup"])
        self.assertEqual(self.config.read_bytes(), original)
        self.assertFalse(self.state.exists())
        self.config.unlink()  # New policy rolls back by removal.
        self.assertFalse(self.config.exists())

    def test_existing_list_preserves_duplicates_values_mode_and_persistent_overrides(self):
        original = b'{ "ignore_dirs" : ["legacy", "target", "target", "legacy"], "other": {"a": true} }\n'
        self.config.write_bytes(original)
        self.config.chmod(0o640)
        arguments = ("--exclude", "packages/web/node_modules", "--exclude", "legacy",
                     "--include", "target", "--apply")
        receipt = self.success(*arguments)
        expected = ["legacy", "legacy", "packages/web/node_modules"]
        self.assertEqual(receipt["ignore_dirs"], expected)
        self.assertEqual(json.loads(self.config.read_bytes())["other"], {"a": True})
        self.assertEqual(stat.S_IMODE(self.config.stat().st_mode), 0o640)
        backup = Path(receipt["backup"])
        self.assertFalse(backup.is_relative_to(self.root))
        self.assertEqual(backup.read_bytes(), original)
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o640)
        self.assertEqual(stat.S_IMODE(backup.parent.stat().st_mode), 0o700)
        changed_bytes = self.config.read_bytes()
        for args in (arguments, ("--apply",)):
            receipt = self.success(*args)
            self.assertFalse(receipt["changed"])
            self.assertIsNone(receipt["backup"])
            self.assertEqual(self.config.read_bytes(), changed_bytes)
        shutil.copy2(backup, self.config)
        self.assertEqual(self.config.read_bytes(), original)
        self.assertEqual(stat.S_IMODE(self.config.stat().st_mode), 0o640)

    def test_seed_only_when_key_absent_and_include_beats_seed(self):
        self.config.write_text('{"other": [1, "two", null]}')
        receipt = self.success("--apply", "--include", "target")
        self.assertEqual(receipt["ignore_dirs"], [p for p in BASELINE if p != "target"])
        self.assertEqual(json.loads(self.config.read_text())["other"], [1, "two", None])
        self.assertFalse(self.success("--apply")["changed"])
        self.config.write_bytes(b'{ "ignore_dirs": [] }\n')
        self.assertFalse(self.success("--apply")["changed"])
        self.assertEqual(self.config.read_bytes(), b'{ "ignore_dirs": [] }\n')

    def test_independent_worktree_and_nested_path_need_no_directory_scan(self):
        other = self.directory / "worktree"
        other.mkdir()
        (other / ".git").write_text("gitdir: /unrelated/repository/worktrees/other\n")
        outside = self.directory / "outside"
        outside.mkdir()
        (other / "node_modules").symlink_to(outside, target_is_directory=True)
        receipt = self.success("--apply", "--exclude", "packages/web/node_modules", root=other)
        self.assertIn("packages/web/node_modules", receipt["ignore_dirs"])
        self.assertFalse(self.config.exists())
        self.assertEqual(list(outside.iterdir()), [])
        self.assertEqual((other / ".git").read_text(), "gitdir: /unrelated/repository/worktrees/other\n")

    def test_invalid_paths_and_conflict_leave_existing_policy_unchanged(self):
        original = b'{"ignore_dirs": ["target"]}\n'
        self.config.write_bytes(original)
        invalid = ["", "/absolute", ".", "..", "a/./b", "a/../b", "a//b", "a/",
                   "a\\b", "*", "a?", "a[0]", ".git", ".git/objects"]
        for option in ("--exclude", "--include"):
            for value in invalid:
                with self.subTest(option=option, value=value):
                    result = self.invoke("--apply", option, value)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertTrue(result.stderr)
                    self.assertEqual(self.config.read_bytes(), original)
        result = self.invoke("--apply", "--include", "target", "--exclude", "target")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.config.read_bytes(), original)
        self.assertFalse(self.state.exists())

    def test_invalid_profiles_fail_before_root_mutation(self):
        for content in ('[', '', '[workspace.file_watching]\nbackend="disabled"',
                        '[workspace]\nfile_watching=1', '[workspace.file_watching]\nbackend="other"'):
            with self.subTest(content=content):
                self.profile.write_text(content)
                self.assertNotEqual(self.invoke("--apply").returncode, 0)
                self.assertFalse(self.config.exists())
        self.profile.unlink()
        self.assertNotEqual(self.invoke("--apply").returncode, 0)
        self.assertFalse(self.config.exists())

    def test_malformed_or_nonregular_policy_fails_without_replacement(self):
        for original in (b'{', b'[]', b'{"ignore_dirs":null}', b'{"ignore_dirs":[1]}',
                         b'{"other":NaN}', b'\xff'):
            with self.subTest(original=original):
                self.config.write_bytes(original)
                self.assertNotEqual(self.invoke("--apply").returncode, 0)
                self.assertEqual(self.config.read_bytes(), original)
        self.config.unlink()
        self.config.mkdir()
        self.assertNotEqual(self.invoke("--apply").returncode, 0)
        self.assertTrue(self.config.is_dir())
        self.assertFalse(self.state.exists())

    def test_symlink_config_root_and_ancestor_are_rejected(self):
        outside = self.directory / "outside.json"
        outside.write_bytes(b'{"ignore_dirs": []}')
        self.config.symlink_to(outside)
        self.assertNotEqual(self.invoke("--apply").returncode, 0)
        self.assertTrue(self.config.is_symlink())
        self.assertEqual(outside.read_bytes(), b'{"ignore_dirs": []}')
        self.config.unlink()
        link = self.directory / "link"
        link.symlink_to(self.root, target_is_directory=True)
        for root in (link, link / ".." / "project"):
            self.assertNotEqual(self.invoke("--apply", root=root).returncode, 0)
        self.assertFalse(self.config.exists())
        self.assertFalse(self.state.exists())

    def test_backup_failure_and_inside_root_backup_preserve_original(self):
        original = b'{"ignore_dirs": []}\n'
        self.config.write_bytes(original)
        blocked = self.directory / "blocked"
        blocked.write_text("not a directory")
        for state in (blocked, self.root / "private-state"):
            with self.subTest(state=state):
                result = self.invoke("--apply", "--exclude", "target", state=state)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(result.stderr)
                self.assertEqual(self.config.read_bytes(), original)
        self.assertFalse((self.root / "private-state").exists())


if __name__ == "__main__":
    unittest.main()
