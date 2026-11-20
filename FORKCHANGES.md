# Fork changes

This fork is version `0.10.0+insomnes.1`. It merges the complete upstream
[v0.10.0](https://github.com/bug-ops/mcpls/releases/tag/v0.10.0) release at
`accbb51bc2dbf4ada6b0773f34f976fd85325685` with fork commit
`80a6dcc6c95912f673087e4c60277055b1d4dc08`. Both histories remain in the merge.

## Upstream behavior included

- The five location tools support `context:"symbol_source"`. The symbol-body pass
  uses bounded tracked text and deduplicates spans within the retained locations.
  The 16 MiB scan budget permits one cut to cross the limit.
- The bridge uses typed positions, URIs, workspace queries, parameter errors, and
  a shared five-level log and message enumeration.
- The LSP client limits `workspace/configuration` replies to 256 items and 4 MiB.
  The fork counts the serialized array, including brackets and commas, before
  copying settings.
- Permanent path failures skip diagnostic URI retries. Clears and lost publications
  still invalidate cached evidence through the resolver's fallback. Resource updates
  resume after a delivery task panic while the subscription registry remains valid.
- Both lockfiles select `rmcp` and `rmcp-macros` 3.10.0. The upstream release also
  includes bounded panic reporting, runtime log changes, and CI checks.

## Deliberate fork differences

- Oversized LSP requests with complete bounded ID and method tokens receive `-32600` even when params never closes. Oversized responses retain their closed-object attribution rule. Existing framing, notification silence and diagnostic-loss behavior remain.
- Client tool declarations retain finite reference-source alternatives, degradation literals and resolved-symbol fields through local schema normalization. Body and excerpt outcomes have distinct tags and retain their declared fields.

- Unix LSP launch remains native: accepted environment keys and values, command paths (including `=`), argv, working directory and stdio are preserved. Missing commands, nonexecutable commands and missing shebang interpreters retain native startup errors and restart reason `spawn_failed`. Abrupt parent death before watchdog binding can leave servers or descendants alive; this startup gap remains. After binding, the existing watchdog cleans up attributable and already-recorded descendants, including `setsid` descendants. Ancestry must remain attributable at discovery: a kernel orphaned-group SIGHUP/SIGCONT transition can kill the native leader before the first snapshot and leave reparented helpers unknown. An independent leader exit can cause the same limit. These gaps remain; native signal behavior is unchanged. Anchor and watchdog failure fallbacks retain their documented limits.

- The public Rust field `LspServerConfig.initialization_options` remains `Option<serde_json::Value>`. Private boundary adapters reject nested TOML datetimes. Settings and parent configuration Debug output redact values; initialization and configuration transmission preserve valid values.
- Every completion response is ranked before count or byte cuts, including responses with at most 100 candidates. Defaults remain 20 items and no documentation. Existing 65536-byte compact output and field-clipping limits, omission metadata, and provider completeness remain independent.
- Diagnostic deltas qualify removals only from complete comparable evidence. Lost pushes remain visible as evicted evidence even when an older pull remains cached; uncertain removals stay conservative. Fresh recovery can restore ordinary comparisons.
- `get_workspace_diagnostics` pulls fresh workspace diagnostics from one
  configured server. It returns bounded results for authorized files and
  distinguishes incomplete, unsupported, and failed reports.
- `get_references` accepts `include_targets:true` to attach portable occurrence targets or unavailable reasons to retained locations. Files mode omits portable targets. Copy each complete target into definition or context navigation without reconstructing coordinates.
- Portable symbol targets retain their existing format, consumers, workspace and source validation, runtime binding, and acquisition budgets. Multi-server symbols normalize with each producer's encoding before target acquisition.
- The fork retains its exact `globset = 0.4.20` pin and Unicode regex matching for Watchman registration patterns.
- `preview_file_move` proposes dependent edits for a single file move within one workspace. It uses the source language's `file_move` route and checks the server's static `workspace/willRenameFiles` filters.
- Shared HTTP workspaces, workspace isolation, Watchman, call graphs, bounded navigation and edit previews, private local feedback, and both tool profiles are retained. The static agent and full profiles expose 36 and 39 tools respectively. The unprefixed catalogue allows 170000 compact bytes total, 9000 per tool, and 35000 for schema descriptions.

## Navigation source options

- Use `{"include_source":true}` for the legacy location excerpt in `source`.
- Use `{"broader_context":true}` for the same bounded excerpt in `broader_context`.
  When both flags are true, the excerpt appears once in `broader_context`.
- Use `{"context":"symbol_source","broader_context":true}` for the bounded body
  outcome in `source` and the location excerpt in `broader_context`.
  `include_source:true` also selects the excerpt when combined with symbol source.
- Reference file summaries support excerpts on selected examples within the
  65,536-byte summary limit. Symbol bodies require `mode:"locations"` and
  `same_as.item` indexes refer to the final locations array.

Location excerpts observe tracked or contained disk text, use the starting line
and up to two lines on either side, and apply configured redaction. The go-to
tools include the SHA-256 of the complete observed file with text excerpts.
A body or excerpt can be unavailable independently. Neither source mode verifies
analyzed freshness. See [Code Intelligence](book/src/tools/code-intelligence.md#optional-navigation-source)
for details and limits.

Release and registry publication are disabled for this fork. Existing workflow files remain available. The package version identifies the adopted release and fork revision; installation receipts use the full commit SHA and binary digest for exact provenance.

Subsequent accepted fork releases increment `insomnes.N`. The upstream base version changes only after deliberate adoption of a complete upstream release.
