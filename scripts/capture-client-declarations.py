#!/usr/bin/env python3
"""Capture actual Codex code-mode MCP declarations through a fresh client.

The output is metadata supplied by the client, not a locally rendered schema.
Use one output directory per candidate and profile. Compare its declarations
with tools/list from the same endpoint before making compatibility claims.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="candidate MCP HTTP endpoint")
    parser.add_argument(
        "--binary", required=True, type=Path, help="binary serving that endpoint"
    )
    parser.add_argument(
        "--output", required=True, type=Path, help="new private evidence directory"
    )
    parser.add_argument(
        "--auth-file",
        type=Path,
        help="optional Codex authentication file; copied temporarily",
    )
    parser.add_argument("--timeout", type=int, default=180)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    output = args.output.resolve()
    metadata = output / "declarations.json"
    prompt = f"""Capture client-provided MCP metadata only. Do not read repository files,
configuration, authentication, or environment. Do not execute any MCP tool.
Use functions.exec to compute JSON.stringify(ALL_TOOLS.filter(t =>
t.name.includes("lsp")), null, 2), without reconstructing its contents.
Persist the computed string verbatim to {str(metadata)!r} through nested
exec_command. This is your only permitted file write. Shell-quote each argument
with single quotes and replace every embedded apostrophe with '\\'"'"'\\'.
Truncate the output first. Append chunks of at most 2000 Unicode code points
(at most 8000 UTF-8 bytes) using printf '%s'. Check each command's exit_code.
Check that the output file exists and is nonempty, then finish. Never print
metadata as prose or author declarations yourself."""
    (output / "prompt.txt").write_text(prompt)
    config = (
        """model = "gpt-6.1-sol"
model_reasoning_effort = "medium"
approval_policy = "never"
[features]
code_mode = true
code_mode_host = true
code_mode_only = true
multi_agent = false
memories = false
apps = false
plugins = false
hooks = false
[mcp_servers.lsp]
url = """
        + json.dumps(args.url)
        + "\n"
    )
    result = None
    try:
        with tempfile.TemporaryDirectory(prefix="mcpls-client-") as private:
            home = Path(private)
            (home / "config.toml").write_text(config)
            if args.auth_file:
                shutil.copyfile(args.auth_file, home / "auth.json")
                (home / "auth.json").chmod(0o600)
            environment = dict(os.environ, CODEX_HOME=str(home))
            with (output / "events.jsonl").open("w") as events:
                result = subprocess.run(
                    [
                        "codex",
                        "exec",
                        "--ephemeral",
                        "--skip-git-repo-check",
                        "--ignore-rules",
                        "--json",
                        "-C",
                        str(output),
                        prompt,
                    ],
                    env=environment,
                    stdout=events,
                    stderr=subprocess.STDOUT,
                    timeout=args.timeout,
                    check=False,
                )
        if result.returncode:
            raise RuntimeError(f"Codex exited with {result.returncode}")
        declarations = json.loads(metadata.read_text())
        names = [tool["name"] for tool in declarations]
        if not names or len(names) != len(set(names)):
            raise RuntimeError("Missing or duplicate tool declarations")
        if any(not tool.get("description") for tool in declarations):
            raise RuntimeError("A tool has no client declaration")
    finally:
        facts = {
            "url": args.url,
            "binary": str(args.binary.resolve()),
            "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
            "client_version": subprocess.check_output(
                ["codex", "--version"], text=True
            ).strip(),
            "exit_code": None if result is None else result.returncode,
        }
        (output / "capture.json").write_text(json.dumps(facts, indent=2) + "\n")


if __name__ == "__main__":
    main()
