# Fork changes

This fork is version `0.9.1+insomnes.1`. Its upstream release is [v0.9.1](https://github.com/bug-ops/mcpls/releases/tag/v0.9.1), commit `7fa196aade37e472c067865bd0779ca647604294`. The complete release and its dependency resolution are included, with the fork dependencies needed for retained features. Fork history is merged without rewriting commits. The two later upstream fuzz dependency changes are excluded.

## Upstream behavior included

- PRs [#849](https://github.com/bug-ops/mcpls/pull/849) and [#859](https://github.com/bug-ops/mcpls/pull/859) add concurrent workspace-symbol fanout, producer identities, per-server outcomes and support reporting, completion ranking, and bounded stdio and LSP recovery.
- PRs [#868](https://github.com/bug-ops/mcpls/pull/868), [#869](https://github.com/bug-ops/mcpls/pull/869), and [#874](https://github.com/bug-ops/mcpls/pull/874) classify lost diagnostic publications, bound per-server loss and eviction tracking, and prevent stale pushed diagnostics from appearing current.
- Direct clangd commands receive missing reference, result, rename-file, and log defaults. Explicit options win. Wrappers, version-suffixed commands, and response-file arguments are excluded. Startup, restart, respawn, and trust checks remain active.
- Stdio accepts lines through 4 MiB. Recoverable oversized LSP bodies above 10 MiB through 256 MiB fail only the affected request; larger announced bodies close the connection. Existing scanner and stalled-frame limits still apply.

## Deliberate fork differences

- Unix LSP launch remains native: accepted environment keys and values, command paths (including `=`), argv, working directory and stdio are preserved. Missing commands, nonexecutable commands and missing shebang interpreters retain native startup errors and restart reason `spawn_failed`. Abrupt parent death before watchdog binding can leave servers or descendants alive; this startup gap remains. After binding, the existing watchdog cleans up the attributable tree, including `setsid` descendants. Anchor and watchdog failure fallbacks retain their documented limits.

- The public Rust field `LspServerConfig.initialization_options` remains `Option<serde_json::Value>`. Private boundary adapters reject nested TOML datetimes. Settings and parent configuration Debug output redact values; initialization and configuration transmission preserve valid values.
- Every completion response is ranked before count or byte cuts, including responses with at most 100 candidates. Defaults remain 20 items and no documentation. Existing 65536-byte compact output and field-clipping limits, omission metadata, and provider completeness remain independent.
- Diagnostic deltas qualify removals only from complete comparable evidence. Lost pushes remain visible as evicted evidence even when an older pull remains cached; uncertain removals stay conservative. Fresh recovery can restore ordinary comparisons.
- Portable symbol targets retain their existing format, consumers, workspace and source validation, runtime binding, and acquisition budgets. Multi-server symbols normalize with each producer's encoding before target acquisition.
- The fork retains its exact `globset = 0.4.20` pin and Unicode regex matching for Watchman registration patterns. The deliberate security update selects `rmcp` and `rmcp-macros` 3.9.0, with their required new SHA-256 dependencies, in both lockfiles. This addresses RUSTSEC-2026-0462 in the optional HTTP client dependency. Other release dependency versions remain selected.
- Shared HTTP workspaces, workspace isolation, Watchman, call graphs, bounded navigation and edit previews, private local feedback, and both tool profiles are retained. The agent profile exposes 31 tools; the full profile exposes 34. Tool-schema budgets remain 160000 compact bytes total, 9000 per tool, and 35000 description bytes.

Release and registry publication are disabled for this fork. Existing workflow files remain available. The package version identifies the adopted release and fork revision; installation receipts use the full commit SHA and binary digest for exact provenance.

Subsequent accepted fork releases increment `insomnes.N`. The upstream base version changes only after deliberate adoption of a complete upstream release.
