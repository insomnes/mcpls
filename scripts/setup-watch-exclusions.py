#!/usr/bin/env python3
"""Explicitly enroll one root in Watchman exclusions (Python 3.11+)."""
from __future__ import annotations

import argparse
import json
import os
import stat
import sys
import tempfile
import tomllib
from pathlib import Path

BASELINE = [
    ".agnt-tsks", "target", ".venv", "venv", "node_modules", "__pycache__",
    ".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox", ".nox", ".uv-cache",
    ".uv_cache", ".hypothesis", ".next", ".nuxt", ".turbo", ".pnpm-store",
]


def relative_path(value: str) -> str:
    parts = value.split("/")
    if (any(part in ("", ".", "..") for part in parts)
            or any(character in value for character in "\\\0*?[]")
            or parts[0] == ".git"):
        raise argparse.ArgumentTypeError(
            f"invalid root-relative path {value!r}: use literal components; .git is protected")
    return value


def checked_root(value: str) -> Path:
    root = Path(value)
    if not root.is_absolute():
        raise ValueError("--root must be an existing absolute directory")
    current = Path(root.anchor)
    for part in root.parts[1:]:
        current /= part
        if current.is_symlink():
            raise ValueError(f"--root must not contain symlink components: {current}")
    if not root.is_dir():
        raise ValueError(f"--root is not an existing directory: {root}")
    return root.resolve()


def check_profile(path: Path) -> None:
    with path.open("rb") as stream:
        profile = tomllib.load(stream)
    workspace = profile.get("workspace")
    watching = workspace.get("file_watching") if isinstance(workspace, dict) else None
    if not isinstance(watching, dict) or watching.get("backend") != "watchman":
        raise ValueError("--profile must set workspace.file_watching.backend = 'watchman'")


def reject_constant(value: str) -> None:
    raise ValueError(f"invalid JSON constant: {value}")


def setup(args: argparse.Namespace) -> dict:
    check_profile(Path(args.profile))
    root = checked_root(args.root)
    conflict = set(args.exclude) & set(args.include)
    if conflict:
        raise ValueError(f"paths supplied to both --exclude and --include: {sorted(conflict)}")
    config = root / ".watchmanconfig"
    original = None
    mode = 0o600
    policy = {}
    try:
        metadata = config.lstat()
    except FileNotFoundError:
        pass
    else:
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError(f"configuration must be a regular file, not a symlink: {config}")
        mode = stat.S_IMODE(metadata.st_mode)
        original = config.read_bytes()
        policy = json.loads(original, parse_constant=reject_constant)
        if not isinstance(policy, dict):
            raise TypeError(f"configuration must contain a JSON object: {config}")
    if "ignore_dirs" in policy:
        existing = policy["ignore_dirs"]
        if not isinstance(existing, list) or any(not isinstance(p, str) for p in existing):
            raise ValueError(f"ignore_dirs must be an array of strings: {config}")
        effective = list(existing)
    else:
        effective = list(BASELINE)
    for path in args.exclude:
        if path not in effective:
            effective.append(path)
    effective = [path for path in effective if path not in args.include]
    changed = "ignore_dirs" not in policy or policy["ignore_dirs"] != effective
    receipt = {"root": str(root), "changed": changed, "applied": False,
               "ignore_dirs": effective, "backup": None}
    if not changed or not args.apply:
        return receipt
    policy["ignore_dirs"] = effective
    content = (json.dumps(policy, indent=2, ensure_ascii=True) + "\n").encode("utf-8")
    if original is not None:
        # A private parent protects original permissions, including world-readable modes.
        state_home = Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state")))
        if not state_home.is_absolute():
            raise ValueError("XDG_STATE_HOME must be an absolute directory for backups")
        backup_parent = (state_home / "mcpls/watch-config-backups").resolve()
        if backup_parent.is_relative_to(root):
            raise ValueError("backup location is inside --root; set XDG_STATE_HOME to an outside directory")
        backup_parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        backup_directory = Path(tempfile.mkdtemp(prefix="mcpls-watch-exclusions-", dir=backup_parent))
        backup = backup_directory / ".watchmanconfig"
        with backup.open("xb") as stream:
            stream.write(original)
            stream.flush()
            os.fsync(stream.fileno())
            os.fchmod(stream.fileno(), mode)
        receipt["backup"] = str(backup)
    with tempfile.NamedTemporaryFile(prefix=".watchmanconfig-", dir=root, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
            os.fchmod(stream.fileno(), mode)
            stream.close()
            temporary.replace(config)
        finally:
            temporary.unlink(missing_ok=True)
    receipt["applied"] = True
    return receipt


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", required=True, help="explicit Watchman-enabled TOML workspace profile")
    parser.add_argument("--root", required=True, help="one existing absolute root without symlink components")
    parser.add_argument("--exclude", action="append", type=relative_path, default=[], metavar="RELPATH")
    parser.add_argument("--include", action="append", type=relative_path, default=[], metavar="RELPATH")
    parser.add_argument("--apply", action="store_true", help="write changes; default is a dry run")
    args = parser.parse_args()
    try:
        receipt = setup(args)
    except (OSError, TypeError, ValueError) as error:
        parser.exit(1, f"setup-watch-exclusions: {error}\n")
    print(json.dumps(receipt))
    return 0


if __name__ == "__main__":
    sys.exit(main())
