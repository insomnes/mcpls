# Shared service for Git worktrees

Dispatcher mode lets one HTTP endpoint serve separate Git worktrees. Each resident workspace owns its language servers, document tracking, diagnostics, subscriptions, and background tasks. Requests do not combine indexes across worktrees. Static HTTP and stdio remain available when dispatcher mode is absent or disabled.

## Approvals and service profiles

Use an explicit service-controlled configuration:

```toml
[mcp]
tool_profile = "agent"

[dispatcher]
enabled = true
max_resident_workspaces = 4
max_concurrent_starts = 2
idle_timeout_seconds = 600
reconcile_interval_seconds = 30

[[dispatcher.repositories]]
path = "/srv/projects/example"
profile = "/home/user/.config/mcpls/profiles/example.toml"

[[dispatcher.workspaces]]
path = "/home/user/.config"
profile = "/home/user/.config/mcpls/profiles/config.toml"
```

A repository approval captures the canonical Git common-directory identity at startup. It authorizes that repository's registered main and linked worktrees, including detached worktrees. New worktrees are discovered from incoming absolute paths without restarting mcpls. Nested repositories require separate approval. Copies of a checkout do not inherit approval.

An explicit non-Git approval covers that directory and its ordinary descendants. A nested Git repository does not inherit it. Converting the approved directory into a Git repository also requires a repository approval. Symlink aliases resolve to the same canonical workspace. Missing, inaccessible, replaced, and moved identities fail closed.

Entry paths and profile paths must be absolute. Limits and intervals must be positive and fit their runtime types. Duplicate canonical approvals, conflicting profiles, missing profiles, and unknown dispatcher keys are configuration errors. Empty explicit and automatic approval lists authorize no workspace. Enabled dispatcher mode rejects top-level `[workspace]` and `[[lsp_servers]]`. Disabled dispatcher entries are not inspected or started.

A profile uses the existing workspace and LSP-server fields:

```toml
[workspace]
roots = ["."]
max_concurrent_server_starts = 1

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["*.rs"]
initialization_options = { linkedProjects = ["rust/Cargo.toml"] }
env = { CARGO_TARGET_DIR = "target/mcpls" }
```

Profiles reject `[mcp]` and `[dispatcher]`. Roots default to `["."]`. Relative roots must resolve to directories inside the selected workspace. Rust `linkedProjects` string entries resolve inside the selected worktree; absolute strings and escaping paths are rejected. Object-valued entries keep their server-defined meaning. Use only the manifests that belong to that repository.

Trusted children run with the selected worktree as their current directory. Commands with a relative path resolve from that worktree. Bare commands use executable lookup. Arguments and other settings pass through unchanged. mcpls does not recursively rewrite JSON or interpolate shell or environment expressions. Cargo resolves relative `CARGO_TARGET_DIR` from the child's actual current directory.

`--workspace-trust untrusted` preserves the existing safe child directory outside analyzed roots. Profile roots and `linkedProjects` still resolve from the selected worktree. Untrusted execution restrictions apply to initial starts and restarts, including relative executables and wrappers. Project-local mcpls configuration is never loaded during discovery. A service profile does not grant trust to worktree content.

Approval entries and dispatcher limits change only after daemon restart. Profiles are checked during dispatch and reconciliation. A changed profile retires only its affected runtimes. Invalid profiles fail closed. Repairing a profile permits a fresh runtime on the next demand.

## Automatic temporary workspaces

Add optional parent policies to authorize projects without registering each checkout:

```toml
[[dispatcher.auto_workspaces]]
path = "/srv/scratch"
profile = "/home/user/.config/mcpls/profiles/automatic.toml"
```

The parent directory must exist. Paths and profiles must be absolute. mcpls captures each parent's canonical directory identity at startup and rejects duplicate canonical parents. Overlapping parents use the most specific policy. Policy changes require a daemon restart; new projects within a parent do not.

Automatic profiles analyze the complete selected root. Omit `workspace.roots` or set it to exactly `["."]`. Rust `initialization_options.linkedProjects` is forbidden, including empty arrays and object values. Explicit profiles retain their existing roots and Rust manifest selections. Service trust, executable restrictions, and configuration-location checks also apply to automatic parents. Discovery never loads project-local `mcpls.toml`.

Existing explicit repository and non-Git approvals take priority. An invalid explicit profile or replaced approved identity fails closed without using an automatic profile. An automatic policy authorizes actual canonical paths inside its parent. A symlink into an outside directory is rejected. A linked worktree outside the parent needs its own approval, even when its repository is inside the parent. Registered worktrees inside the parent can use Git metadata outside it; this does not authorize outside source files.

For a file request, root selection starts at its containing directory. For `workspace_path`, selection starts at that directory. mcpls checks ancestors through the policy parent, stopping at the nearest valid Git boundary inside it. The nearest directory containing any `project_markers` from any server in the policy profile becomes the root. Without a marker, mcpls uses that Git root, or the starting directory when no Git root is inside the parent. Sibling directories cannot select a root. Discovery checks marker existence without reading manifests or recursively searching subtrees. Malformed Git boundaries fail closed.

For automatic contexts in untrusted mode, every automatic parent remains part of the execution trust boundary, including parents outside the selected project and broader overlapping policies. These contexts cannot run executables, wrapped programs, or executable helper arguments inside any parent, even with `--allow-server`. In untrusted mode, all service profiles must resolve outside every automatic parent. These checks do not broaden analyzed roots. Trusted children still run from the selected project root.

Markers select automatic roots; they do not suppress servers for the requested language. A standalone Python file can therefore start its configured server without a manifest. Explicit workspaces retain recursive marker filtering. Language servers can still require their own project configuration.

Automatic contexts share the normal resident capacity, startup limits, backoff, idle timeout, subscription pins, and cleanup rules. Concurrent clients selecting the same canonical root share one runtime. Automatic registrations disappear after cleanup. A status-only discovery returns a transient snapshot and creates no lasting registration. Cleanup and recreation invalidate old continuation data.

## Select a workspace

File tools keep their absolute `file_path` parameter. These fileless tools require an absolute `workspace_path` directory in dispatcher mode:

- `workspace_symbol_search`
- `get_server_logs`
- `get_server_messages`
- `restart_server`

```json
{"query":"Server","workspace_path":"/srv/worktrees/feature"}
```

`restart_server` still requires either `servers` or `all: true`. Both selectors affect only the selected runtime. Static mode permits calls without `workspace_path`; an explicit selector must lie within a configured static root and does not narrow that static runtime.

`get_tool_support` accepts `file_path`, `workspace_path`, or neither. When both are supplied, they must select the same workspace. Without a selector, dispatcher mode returns `basis: "configured"`, configured language routes, and unknown LSP capability coverage. It starts no backend. A selector returns `basis: "runtime"`, `workspace_path`, `runtime_generation`, and actual server capabilities. File selection starts only backends for the selected language.

No operation selects a workspace from the daemon's current directory, the previous request, or MCP roots. MCP roots are optional hints and grant no approval. A returned external dependency location remains marked according to the normal `out_of_workspace` behavior. That result does not authorize a later standalone source request outside approved roots.

The catalog is fixed by daemon mode, tool profile, and prefix. Dispatcher mode adds `get_workspace_status`: 28 agent tools or 31 full tools. Static mode keeps 27 or 30. Discovery and eviction do not alter these catalogs.

## Status and lifecycle

Call `get_workspace_status` with `{}` for known entries, or with `workspace_path` to discover and inspect one identity. It starts no backend and does not refresh idle activity. Initialization, tool listing, unscoped support, and resource listing also start no backend.

The status result includes `mode`, `limits`, and sorted `workspaces`. Each workspace reports `temporary` (`true` for automatic contexts and `false` for explicit approvals), its canonical path, opaque identity, Git common/admin paths or null, service profile, generation or null, state, last activity, active request count, subscription count, and servers. Activity timestamps use RFC3339 UTC. Servers follow profile declaration order and report their id, state, live PID or null, failure count, bounded redacted error, and retry delay in milliseconds or null. This is a point-in-time snapshot rather than an atomic OS census.

Workspace states are `discovered`, `starting`, `ready`, `draining`, and `failed`. Server states are `not_started`, `starting`, `ready`, `draining`, and `failed`. A discovered entry consumes no resident capacity. Starts and draining runtimes consume capacity until cleanup finishes.

The first semantic request starts applicable backends lazily. Concurrent callers share a start. Canceling one waiter does not cancel another waiter. The global start limit also bounds profile starts; a smaller profile concurrency limit further restricts that runtime.

Only workspaces without active requests or live subscriptions can be idle-evicted. An open HTTP session alone does not pin a runtime. Capacity pressure can evict an eligible idle runtime immediately. If all slots are busy, the request returns `workspace_capacity`. Failures and demand-driven retries remain isolated to the affected workspace/server. Automatic retries are separated by at least five seconds after failure, with delays no greater than thirty seconds. Explicit restart keeps its existing cooldown.

Identity replacement, removal, movement, profile changes, and eviction invalidate old runtime state. Pending results cannot publish as current after invalidation. Cleanup finishes before a replacement starts. Normal killable children and their owned descendants are stopped within ten seconds of committed invalidation or eviction. Daemon shutdown stops all owned runtimes within the systemd stop bound.

## Resources and continuation data

Diagnostic resource reads and subscriptions select through the decoded absolute file path in `lsp-diagnostics:///...`. A successful live subscription pins its selected runtime. Re-subscribe is idempotent. Unsubscribe, session closure, or invalidation releases pins. Failed or canceled subscriptions leave no new pin. Subscriptions never transfer to a replacement generation. Resource listing can include known open documents across runtimes and does not grant source access.

Keep `runtime_generation` from the first `get_document_outline` page. Include it when `offset` is greater than zero. Restart pagination after document changes. A mismatching or missing generation returns `stale_continuation` instead of paging a replacement.

Hierarchy items carry opaque `dispatcher_context`. Reuse the complete item unchanged for item-based call/type traversal. Missing, modified, mismatched, or stale context is rejected before forwarding opaque data. Server restart invalidates items from that server incarnation. Resource-list cursors are also bound to their included runtime generations.

## Dispatcher errors

Errors use the existing MCP envelope. Structured `data` contains `code`, `retryable`, and an optional `retry_after_ms`. Backoff errors include a nonnegative delay.

| JSON-RPC code | `data.code` | Retryable | Meaning |
|---|---|---|---|
| -32602 | `workspace_required` | false | Required workspace selector is absent |
| -32602 | `workspace_invalid` | false | Relative, missing, malformed, or conflicting selection |
| -32602 | `workspace_not_approved` | false | Repository or non-Git root lacks approval |
| -32602 | `stale_continuation` | false | Continuation does not identify its live generation/origin |
| -32602 | `profile_invalid` | false | Service profile failed validation |
| -32055 | `workspace_capacity` | true | No resident slot can be reclaimed |
| -32056 | `workspace_invalidated` | true | Identity/profile changed before publication |
| -32057 | `workspace_unavailable` | true | Startup, cleanup, or backoff prevents dispatch |

Normal static errors, language-server errors, and cancellation retain their existing behavior.

## Install, restart, and roll back

On Linux with a systemd user service, use `scripts/install-shared-service.py`. It builds the selected exact commit with `transport-http`, retains the previous executable/configuration, installs an immutable release, updates a dedicated service override and CLI symlink, reloads systemd, restarts the service, and checks the MCP endpoint.

Prepare the daemon configuration and profiles first. Preserve any extra daemon flags with repeated `--service-arg=VALUE` options. Their absolute profile paths must match the chosen destination. The destination profile directory must not be a symlink. Preserve language settings, pinned server commands, environment settings, and repository-specific Rust manifest selections.

```bash
python3 scripts/install-shared-service.py \
  --revision FULL_COMMIT_SHA \
  --config /private/staged/shared-http.toml \
  --profiles-dir /private/staged/profiles \
  --config-path "$HOME/.config/mcpls/shared-http.toml" \
  --endpoint http://127.0.0.1:39287/mcp
```

Add `--check` to build and validate the supplied configuration without changing the installed service. The script uses an isolated MCP handshake for this preflight and starts no language server. A failed preflight identifies the stage, displays bounded daemon stderr, and retains its diagnostic log.

The script prints its backup directory and retained rollback command. Repeat invocation preserves immutable releases and takes a new configuration snapshot. A failed activation reports its stage and restores the saved service state. To restore a selected backup explicitly:

```bash
python3 scripts/install-shared-service.py --rollback /absolute/backup/directory
```

Keep the previous release until installed behavior is verified. From two independent HTTP sessions, request real Rust semantics in the same worktree and compare the `get_workspace_status` PID. Check Python and Go separately when configured. A responsive protocol endpoint alone does not establish language-server correctness.
