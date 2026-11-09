# Process Lifetime

In this chapter you learn what happens to language server processes when mcpls exits, whether normally or not, and the platform caveats. Language servers can start heavy children such as `cargo check` or build daemons, and a bridge that leaks them would slowly eat your machine.

## Prerequisites

- You know how servers [start and stop](lifecycle.md).

## The guarantee

When mcpls exits for any reason, including `SIGKILL` and out-of-memory kills, bound language servers and their attributable descendants are killed. This includes processes that are meant to outlive their server, such as shared build daemons like Gradle or Bloop that the server started. On Unix this guarantee starts after watchdog binding; abrupt parent death before binding can leave servers or descendants alive.

The mechanism differs by platform.

## Unix

Each server has its own lifeline: a small anchor process leads the server's process group, and a watchdog sits outside the group. After binding, when mcpls exits or the server is shut down or restarted, the watchdog freezes the tree, finds descendants that left the group with `setsid()` or `setpgid()` (for example the `cargo check` that rust-analyzer's flycheck runs), and kills attributable or already-recorded targets. A crashed server's attributable leftovers are killed when the crash is noticed.

Configured commands launch natively. Accepted environment entries and values, command paths (including `=`), argv, working directory and stdio retain their original behavior. Missing commands, nonexecutable commands and missing shebang interpreters keep native startup error classifications and restart reason `spawn_failed`. Failed initial startup remains unavailable and does not retry automatically.

Caveats:

- The sweep needs `ps` and `awk`. Without `ps`, mcpls logs a warning and escaped descendants survive.
- Ancestry must remain attributable when the watchdog discovers a descendant, or the target must already be recorded. Binding alone does not preserve ancestry. An orphaned-process-group transition can deliver kernel SIGHUP/SIGCONT; if SIGHUP kills the native leader before the first snapshot, a `setsid` helper can be reparented before discovery and survive. An independent leader exit can cause the same limit. This risk remains; native server signal handling is unchanged. A `pkill -9` that matches `lsp-lifeline-watchdog` kills the watchdog and prevents the sweep.
- Servers that exit only on stdin EOF wait out the 3 second shutdown grace and are then killed. A bound watchdog is indicated by `processId: null` in the LSP `initialize` request. The server need not answer initialization for binding to be established.
- Servers run in their own process group, so Ctrl-C in the terminal no longer reaches them directly. A descendant that reads `/dev/tty`, such as an ssh or git credential prompt, may be stopped by `SIGTTIN` when mcpls runs in an interactive terminal.
- If the anchor cannot be started, the server runs unbound in its own process group. If only the watchdog fails, the group is killed during Rust teardown but escaped descendants are not swept. Abrupt parent death is not protected by either fallback. Ctrl-C does not reach servers in either case.

## Windows

Servers run in a job object without breakaway, and the whole tree is killed. A descendant that requests `CREATE_BREAKAWAY_FROM_JOB` fails to spawn. If the job cannot be created or assigned, the server spawn fails.

## Why this matters to you

- After watchdog binding, killing mcpls or your client triggers cleanup of `rust-analyzer` and its attributable children. The startup window before binding remains unprotected.
- Shared daemons in a bound server's attributable tree are killed with it. If a server needs a daemon that outlives it, start that daemon yourself, outside mcpls.
- `restart_server` uses the same machinery: it kills the whole tree before starting a replacement.

## What's Next

Finish Part 3 with [Limits and Known Constraints](limits.md), a checklist of the boundaries to plan around.
