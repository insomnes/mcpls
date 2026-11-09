#!/usr/bin/env python3
"""Install an exact mcpls HTTP release into an existing systemd user service."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import selectors
import re
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request


def run(*args: str, capture: bool = False) -> str:
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def atomic_write(path: Path, content: bytes, mode: int = 0o600) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
            temporary.chmod(mode)
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def save(path: Path, backup: Path, name: str) -> dict:
    record = {"path": str(path), "kind": "absent"}
    if path.is_symlink():
        record.update(kind="symlink", target=os.readlink(path))
    elif path.is_dir():
        shutil.copytree(path, backup / name, symlinks=True)
        record.update(kind="directory", snapshot=name)
    elif path.is_file():
        shutil.copy2(path, backup / name)
        record.update(kind="file", snapshot=name, mode=path.stat().st_mode & 0o777)
    elif path.exists():
        raise RuntimeError(f"unsupported destination type: {path}")
    return record


def restore(record: dict, backup: Path) -> None:
    path = Path(record["path"])
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.is_dir():
        shutil.rmtree(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if record["kind"] == "symlink":
        path.symlink_to(record["target"])
    elif record["kind"] == "file":
        atomic_write(path, (backup / record["snapshot"]).read_bytes(), record["mode"])
    elif record["kind"] == "directory":
        shutil.copytree(backup / record["snapshot"], path, symlinks=True)


def readiness(endpoint: str, timeout: float) -> None:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "mcpls-install-readiness", "version": "1"}}}).encode()
    deadline = time.monotonic() + timeout
    while True:
        session = None
        try:
            request = urllib.request.Request(endpoint, data=body, headers={
                "Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
            with urllib.request.urlopen(request, timeout=3) as response:
                payload = response.read(1024 * 1024).decode()
                session = response.headers.get("Mcp-Session-Id")
            if payload.lstrip().startswith("{"):
                messages = [json.loads(payload)]
            else:
                messages = []
                for event in payload.replace("\r\n", "\n").split("\n\n"):
                    lines = event.splitlines()
                    event_type = next((line[6:].strip() for line in lines if line.startswith("event:")), "message")
                    data = "\n".join(line[5:].removeprefix(" ") for line in lines if line.startswith("data:"))
                    if event_type == "message" and data.strip():
                        messages.append(json.loads(data))
            message = next(message for message in messages if isinstance(message, dict) and message.get("id") == 1)
            if message.get("result", {}).get("serverInfo", {}).get("name") != "mcpls":
                raise RuntimeError("readiness response is not an mcpls initialize result")
            return
        except (OSError, urllib.error.URLError, ValueError, StopIteration, RuntimeError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"MCP readiness failed at {endpoint}") from error
            time.sleep(0.25)
        finally:
            if session:
                delete = urllib.request.Request(endpoint, method="DELETE", headers={"Mcp-Session-Id": session})
                with urllib.request.urlopen(delete, timeout=3):
                    pass


def rollback(backup: Path) -> None:
    state = json.loads((backup / "state.json").read_text())
    unit = state["unit"]
    run("systemctl", "--user", "stop", unit)
    for record in reversed(state["files"]):
        restore(record, backup)
    run("systemctl", "--user", "daemon-reload")
    if state["was_active"]:
        run("systemctl", "--user", "restart", unit)
        readiness(state["endpoint"], state["readiness_timeout"])
    print(f"Restored service state from {backup}")


def preflight(binary: Path, parsed: dict, source_profiles: Path, args: argparse.Namespace) -> None:
    """Validate profile/runtime configuration without altering the installed service."""
    # TOML is rewritten through the narrow input references, not a general template.
    with tempfile.TemporaryDirectory(prefix="mcpls-install-check-") as temporary:
        stage = Path(temporary)
        config = args.config.expanduser().resolve(strict=True).read_text()
        replacements = {}
        for entries in (parsed["dispatcher"].get("repositories", []), parsed["dispatcher"].get("workspaces", []), parsed["dispatcher"].get("auto_workspaces", [])):
            for entry in entries:
                original = entry["profile"]
                destination = stage / Path(original).name
                destination.write_bytes((source_profiles / destination.name).read_bytes())
                replacements[original] = str(destination)

        def replace_profile_literal(match):
            try:
                value = tomllib.loads("value=" + match.group())["value"]
            except tomllib.TOMLDecodeError:
                return match.group()
            return json.dumps(replacements[value], ensure_ascii=False) if value in replacements else match.group()

        config = re.sub(r"\"(?:[^\"\\]|\\.)*\"|'[^']*'", replace_profile_literal, config)
        config_path = stage / "daemon.toml"
        config_path.write_text(config)
        command = [str(binary.resolve()), "--config", str(config_path), "--workspace-trust", args.workspace_trust]
        for server in args.allow_server:
            command.extend(["--allow-server", server])
        with (stage / "stderr.log").open("w") as errors:
            child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True)
            try:
                message = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "mcpls-install-check", "version": "1"}}}
                child.stdin.write(json.dumps(message) + "\n")
                child.stdin.flush()
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    if not selector.select(args.readiness_timeout):
                        raise RuntimeError("candidate configuration preflight timed out")
                    line = child.stdout.readline()
                    if not line:
                        raise RuntimeError("candidate exited before initialize response")
                    response = json.loads(line)
                if response.get("result", {}).get("serverInfo", {}).get("name") != "mcpls":
                    raise RuntimeError("candidate configuration preflight failed")
            except (RuntimeError, ValueError, BrokenPipeError) as error:
                # Keep bounded daemon diagnostics outside the temporary staging
                # directory so configuration failures remain actionable.
                errors.flush()
                stderr = (stage / "stderr.log").read_bytes()[-8192:].decode(errors="replace")
                evidence = Path(tempfile.mkdtemp(prefix="mcpls-install-preflight-failure-"))
                evidence.chmod(0o700)
                atomic_write(evidence / "stderr.log", stderr.encode())
                raise RuntimeError(
                    f"configuration preflight failed: {error}; stderr: {stderr.strip() or '(empty)'}; "
                    f"diagnostic log: {evidence / 'stderr.log'}"
                ) from error
            finally:
                if child.poll() is None:
                    child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
                    raise RuntimeError("candidate preflight did not stop within 15 seconds") from None
        print("Candidate configuration preflight passed without service activation", flush=True)


def unit_quote(value: str) -> str:
    # systemd performs its own specifier expansion, including inside quotes.
    return '"' + value.replace("%", "%%").replace("\\", "\\\\").replace('"', '\\"').replace("$", "$$") + '"'


def install(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[1]
    os.chdir(repository)
    revision = run("git", "rev-parse", "HEAD", capture=True)
    if revision != args.revision or len(args.revision) != 40:
        raise RuntimeError("--revision must equal the current full HEAD commit")
    run("git", "diff", "--quiet", "HEAD", "--", "Cargo.toml", "Cargo.lock", "crates", "scripts/install-shared-service.py")
    endpoint = urllib.parse.urlsplit(args.endpoint)
    if endpoint.scheme != "http" or endpoint.hostname not in {"127.0.0.1", "localhost", "::1"} or not endpoint.port or endpoint.query or endpoint.fragment or endpoint.username:
        raise RuntimeError("--endpoint must be a loopback HTTP URL with an explicit port")
    if "/" in args.unit or not args.unit.endswith(".service"):
        raise RuntimeError("--unit must be a systemd service name")
    source_config = args.config.expanduser().resolve(strict=True)
    parsed = tomllib.loads(source_config.read_text())
    if not parsed.get("dispatcher", {}).get("enabled"):
        raise RuntimeError("the supplied configuration must explicitly enable dispatcher mode")
    source_profiles = args.profiles_dir.expanduser().resolve(strict=True)
    profiles = sorted(source_profiles.glob("*.toml"))
    if any(path.is_symlink() or not path.is_file() for path in profiles):
        raise RuntimeError("profile inputs must be regular TOML files")
    config_path = args.config_path.expanduser().absolute()
    profiles_path = (args.profiles_path or config_path.parent / "profiles").expanduser().absolute()
    if profiles_path.is_symlink():
        raise RuntimeError(f"profile destination must not be a symlink: {profiles_path}")
    for entry in parsed["dispatcher"].get("repositories", []) + parsed["dispatcher"].get("workspaces", []) + parsed["dispatcher"].get("auto_workspaces", []):
        destination = Path(entry["profile"])
        supplied = source_profiles / destination.name
        if destination.parent != profiles_path or supplied not in profiles:
            raise RuntimeError(f"profile input does not match destination: {destination}")
        tomllib.loads(supplied.read_text())
    print(f"Building {revision} with transport-http", flush=True)
    run("cargo", "build", "--release", "--locked", "--features", "transport-http", "-p", "mcpls")
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", os.environ.get("CARGO_BUILD_TARGET_DIR", "target")))
    binary = target_root / "release" / "mcpls"
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if args.expected_binary_sha256 and digest != args.expected_binary_sha256:
        raise RuntimeError("release binary digest differs from the verified candidate")
    preflight(binary, parsed, source_profiles, args)
    if args.check:
        print(f"Checked {revision}\nBinary SHA256: {digest}")
        return
    release = args.release_root.expanduser().absolute() / revision / "mcpls"
    if release.exists():
        if hashlib.sha256(release.read_bytes()).hexdigest() != digest:
            raise RuntimeError(f"immutable release already exists with different bytes: {release}")
    else:
        atomic_write(release, binary.read_bytes(), 0o755)
    receipt = release.parent / "build.json"
    if not receipt.exists():
        atomic_write(receipt, (json.dumps({"revision": revision, "sha256": digest, "features": ["transport-http"]}, indent=2) + "\n").encode())
    user_config = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    override = user_config / "systemd" / "user" / f"{args.unit}.d" / "50-mcpls-release.conf"
    cli = args.cli_path.expanduser().absolute()
    unit_path = Path(run("systemctl", "--user", "show", args.unit, "--property=FragmentPath", "--value", capture=True))
    if not unit_path.is_file():
        raise RuntimeError(f"existing service unit not found: {args.unit}")
    was_active = subprocess.run(["systemctl", "--user", "is-active", "--quiet", args.unit]).returncode == 0
    backup_root = args.backup_root.expanduser().absolute()
    backup_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    backup = Path(tempfile.mkdtemp(prefix=f"{time.strftime('%Y%m%dT%H%M%S')}-{revision[:12]}-", dir=backup_root))
    files = [save(path, backup, name) for path, name in [
        (config_path, "daemon.toml"), (profiles_path, "profiles"), (override, "service-override.conf"), (cli, "cli")]]
    shutil.copy2(unit_path, backup / "original-unit.service")
    state = {"unit": args.unit, "endpoint": args.endpoint, "was_active": was_active,
             "readiness_timeout": args.readiness_timeout, "files": files}
    atomic_write(backup / "state.json", (json.dumps(state, indent=2) + "\n").encode())
    rollback_command = [sys.executable, str(Path(__file__).resolve()), "--rollback", str(backup)]
    print(f"Backup: {backup}\nRollback: {shlex.join(rollback_command)}", flush=True)
    stage = "stop service"
    try:
        run("systemctl", "--user", "stop", args.unit)
        stage = "stage profiles and daemon configuration"
        for profile in profiles:
            atomic_write(profiles_path / profile.name, profile.read_bytes())
        atomic_write(config_path, source_config.read_bytes())
        stage = "update service and CLI"
        listen_host = "127.0.0.1" if endpoint.hostname == "localhost" else endpoint.hostname
        listen = f"[{listen_host}]:{endpoint.port}" if listen_host == "::1" else f"{listen_host}:{endpoint.port}"
        command = [str(release), "--config", str(config_path), "--listen", listen, "--http-path", endpoint.path or "/mcp", "--workspace-trust", args.workspace_trust]
        for server in args.allow_server:
            command.extend(["--allow-server", server])
        command.extend(args.service_arg)
        override_text = "[Service]\nExecStart=\nExecStart=" + " ".join(map(unit_quote, command)) + "\nKillMode=control-group\nTimeoutStopSec=60\n"
        atomic_write(override, override_text.encode())
        cli.parent.mkdir(parents=True, exist_ok=True)
        temporary_link = cli.with_name(f".{cli.name}.{os.getpid()}.new")
        temporary_link.unlink(missing_ok=True)
        temporary_link.symlink_to(release)
        temporary_link.replace(cli)
        stage = "reload and restart service"
        run("systemctl", "--user", "daemon-reload")
        run("systemctl", "--user", "restart", args.unit)
        stage = "MCP readiness"
        readiness(args.endpoint, args.readiness_timeout)
        effective = run("systemctl", "--user", "show", args.unit, "--property=ExecStart", "--value", capture=True)
        if str(release) not in effective:
            raise RuntimeError("systemd ExecStart does not use the immutable candidate")
    except BaseException:
        print(f"Installation failed during {stage}. Restoring {backup}", file=sys.stderr, flush=True)
        try:
            rollback(backup)
        except BaseException as error:
            print(f"Automatic rollback failed: {error}. Retry: {shlex.join(rollback_command)}", file=sys.stderr)
        raise
    print(f"Installed {revision}\nBinary SHA256: {digest}\nEndpoint: {args.endpoint}\nRollback retained: {backup}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="build and validate supplied configuration without changing the installed service")
    parser.add_argument("--rollback", type=Path)
    parser.add_argument("--revision")
    parser.add_argument("--config", type=Path)
    parser.add_argument("--profiles-dir", type=Path)
    parser.add_argument("--config-path", type=Path, default=Path("~/.config/mcpls/shared-http.toml"))
    parser.add_argument("--profiles-path", type=Path)
    parser.add_argument("--release-root", type=Path, default=Path("~/.local/libexec/mcpls/releases"))
    parser.add_argument("--cli-path", type=Path, default=Path("~/.cargo/bin/mcpls"))
    parser.add_argument("--backup-root", type=Path, default=Path("~/.local/state/mcpls/install"))
    parser.add_argument("--unit", default="mcpls.service")
    parser.add_argument("--endpoint", default="http://127.0.0.1:39287/mcp")
    parser.add_argument("--readiness-timeout", type=float, default=30)
    parser.add_argument("--workspace-trust", choices=["trusted", "untrusted"], default="trusted")
    parser.add_argument("--allow-server", action="append", default=[])
    parser.add_argument("--service-arg", action="append", default=[], help="additional daemon argument; repeat to preserve configured HTTP policies")
    parser.add_argument("--expected-binary-sha256")
    args = parser.parse_args()
    if args.rollback:
        rollback(args.rollback.expanduser().resolve(strict=True))
    elif not args.revision or not args.config or not args.profiles_dir:
        parser.error("installation requires --revision, --config, and --profiles-dir")
    elif args.readiness_timeout <= 0:
        parser.error("--readiness-timeout must be positive")
    else:
        install(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
