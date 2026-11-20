# Tools Overview

In this chapter you learn the conventions every mcpls tool shares: how code is addressed, what the advisory flags in results mean, and how to read errors. The next chapters then cover the tools by task. Learn these rules once and every tool becomes predictable.

## Prerequisites

- A [connected client](../getting-started/connect-client.md).

## Tools at a glance

| Task | Tools |
|------|-------|
| [Read and navigate code](code-intelligence.md) | `get_symbol_context`, `get_hover`, `get_definition`, `get_references`, `get_completions`, `get_document_symbols`, `get_document_outline`, `workspace_symbol_search`, `get_document_highlights`, `get_signature_help`, `get_inlay_hints` |
| [Diagnostics and formatting](diagnostics.md) | `get_diagnostics`, `get_workspace_diagnostics`, `diagnostics_delta`, `get_cached_diagnostics`, `format_document`, `format_range`, `get_folding_ranges`, `get_selection_ranges` |
| [Refactor](refactoring.md) | `rename_symbol`, `preview_file_move`, `get_code_actions` |
| [Hierarchies and navigation](hierarchies.md) | `go_to_implementation`, `go_to_type_definition`, `go_to_declaration`, `get_incoming_calls`, `get_outgoing_calls`, `get_call_graph`, `prepare_type_hierarchy`, `get_supertypes`, `get_subtypes` |
| [Manage feedback](#local-feedback) | `feedback_add`, `feedback_list`, `feedback_resolve`, `feedback_reopen` |
| [Monitor and control servers](server-control.md) | `get_server_logs`, `get_server_messages`, `get_tool_support`, `restart_server` |

`feedback_add` appends explicit feedback to local storage. `feedback_resolve` and `feedback_reopen` update its local resolution state. `restart_server` stops and starts language server processes. Other tools are read-only. Tools that produce edits (`rename_symbol`, `preview_file_move`, `format_document`, `format_range`, `get_code_actions`) return them without applying them.

If you set `mcp.tool_prefix`, every name above gains the prefix, as in `{tool_prefix}_get_hover`.

The table includes full-profile-only tools: `get_folding_ranges`, `get_selection_ranges`, and `prepare_type_hierarchy`.

The default `[mcp].tool_profile = "agent"` exposes 36 tools. Set `tool_profile = "full"` to restore all 39 tools, including `get_folding_ranges`, `get_selection_ranges`, and `prepare_type_hierarchy`. Other values are rejected. The catalog is fixed for the session; tools remain visible when a selected server lacks the required capability. `tools/list`, dispatch, and `get_tool_support` use the same catalog and configured prefix. Shared tool schemas are identical in both profiles.

## Choose the smallest useful answer

Start with the question: use a [filtered outline](code-intelligence.md#choose-filters-from-the-question)
for in-file discovery, [selected context sections](symbol-context.md#choose-sections-for-the-next-question)
for source/types/direct calls, and [reference locations or files](code-intelligence.md#get_references)
according to whether you need positions or file selection. Reuse a known valid
definition position or [portable target](symbol-context.md#copy-a-portable-target)
when continuing inspection. Optional source and enclosing context should answer
the next question. Smaller output does not imply less provider work or fresher
analysis.

Tool/schema discovery establishes which calls a client can send.
`get_tool_support` establishes support for the selected language/provider.
Follow governing instructions that require this check before first use of each
language; reuse valid session knowledge instead of repeating it before each call.
A catalog entry can still be unsupported. Missing optional route details alone
do not establish missing support. For transient initialization/indexing errors,
allow at most two retries after short waits, honoring retry hints within the task
budget, then use an available fallback and state the limitation. Unsupported
operations need a fallback, not retries. Navigation grants no edit, restart, or
source-access authority beyond the existing task and workspace boundaries.

## Addressing code

Most tools take a `file_path`, which must be an absolute path to a file under a workspace root.

Position-based tools use two integers:

- `line`: the line number, starting at 1.
- `character`: the column, starting at 1, counted in UTF-16 code units. For ASCII text this is the character count. See [Positions and Encodings](../advanced/positions.md) for the rest.

Range tools take `start_line`, `start_character`, `end_line` and `end_character` the same way. The end line must exist in the file.

Positions are 1-based everywhere, so `0` is invalid. A flat `line` or `character` of `0`, or above 1,000,000, is rejected with `-32602` before any server is asked. Inside a nested object, such as the `range` of a call or type hierarchy `item` you pass back, a `0` fails deserialization instead and the error text comes from the parser, not from mcpls.

### Addressing a symbol by name

`get_symbol_context`, `get_hover`, `get_definition`, `get_references`, `go_to_implementation`, `go_to_type_definition`, `get_incoming_calls`, `get_outgoing_calls`, `get_call_graph`, `get_supertypes`, `get_subtypes` and `rename_symbol` accept a symbol name instead of a position:

| Parameter | Meaning |
|-----------|---------|
| `symbol_name` | A symbol defined in the file; may be qualified, such as `Type::method` or `Type.method` |
| `symbol_kind` | Optional filter by kind (`function`, `method`, `struct`, ...) or numeric LSP `SymbolKind` |
| `container` | Optional filter: only symbols directly inside a type, impl, class or module of this name |

Give exactly one form. A request with both, neither, or half a position is rejected with `-32602`.

```json
{
  "file_path": "/work/app/src/parser.rs",
  "symbol_name": "Parser::parse",
  "symbol_kind": "method"
}
```

The result carries `resolved_symbol` with the `name`, `kind`, `container`, the position that was queried and `position_source` (`selection_range` or `inferred`). mcpls never guesses. A name that matches several symbols, none, or whose identifier cannot be located fails with `-32602` and a structured `data.resolution`:

| `resolution` | Meaning |
|--------------|---------|
| `ambiguous` | Several symbols match; every candidate is listed with its position |
| `not_found` | No symbol has that name |
| `not_defined_in_file` | The name is an import or a plain reference; use `workspace_symbol_search` or a position |
| `position_unverified` | The identifier could not be located unambiguously |

`rename_symbol` never produces edits for an ambiguous name. `get_signature_help` and `get_completions` take a position only.

## Reading results

### Truncation

List-returning tools are capped at a fixed maximum. When more exist, the result has `truncated: true`.

### Advisory flags

These flags describe locations and position reliability. Direct call and type queries stop before automatic traversal when preparation reports degraded positions.

- **`out_of_workspace`** is set on each location that falls outside every workspace root, such as the standard library or a dependency. The location is valid; it is simply not under your roots.
- **`positions_degraded`** appears when a column could not be converted exactly for a server that does not use UTF-16. `"request"` means the position you sent reached the server unconverted, so the result may describe a different symbol and should not be trusted. `"response"` means only returned `character` offsets may be inexact. The flag is omitted when everything converted exactly.

### Indexing and diagnostics flags

Results that depend on the server's index carry `indexing_in_progress`, which is `true` when the server was still indexing during the call, so an empty or partial answer may be incomplete.

### Enclosing-symbol context

`get_references`, `get_definition`, `go_to_implementation`, `go_to_type_definition`, `go_to_declaration` and `get_diagnostics` accept `context`: `"none"` (the default, no extra work) or `"enclosing_symbol"`. With the second value, each location or diagnostic gains an `enclosing_symbol` naming the innermost symbol that contains it. The five location tools also accept `"symbol_source"`, which adds the symbol's source text (see [symbol source](#symbol-source)); `get_diagnostics` accepts only the first two values.

```json
{
  "uri": "file:///work/app/src/parser.rs",
  "range": { "start": { "line": 15, "character": 4 }, "end": { "line": 15, "character": 8 } },
  "enclosing_symbol": {
    "status": "resolved",
    "name_path": ["Parser", "parse"],
    "kind": 6,
    "range": { "start": { "line": 12, "character": 1 }, "end": { "line": 30, "character": 2 } },
    "fidelity": "hierarchical"
  }
}
```

| `status` | Meaning |
|----------|---------|
| `resolved` | The innermost containing symbol, with `name_path` (outermost first), LSP numeric `kind`, `range` and `fidelity` |
| `top_level` | The file's symbols were read and none contains the item |
| `not_computed` | Skipped, with a `reason`: `file_cap`, `out_of_workspace`, `tracker_limit` or `deadline` |
| `unavailable` | Attempted and failed, with a `reason`: `capability_absent`, `request_failed` or `timed_out` |

`not_computed` and `unavailable` mean nothing is known; never read them as top level. The lookup costs one `documentSymbol` request per distinct file, capped at 16 files (or `max_documents / 4` when `workspace.max_documents` is below 64) and a 30 second budget. An `enrichment` object reports `files_enriched`, `files_skipped` and `cut_short`. The primary result is never affected.

### Symbol source

With `context: "symbol_source"`, each location from `get_references`, `get_definition`, `go_to_implementation`, `go_to_type_definition` and `go_to_declaration` carries the enclosing-symbol lookup above plus a `source` taken from the document text mcpls already tracks, so it adds no request; the text comes from that tracked document, and converting a position may read a line from disk, as for any location:

| `source.status` | Meaning |
|-----------------|---------|
| `included` | `text` of the symbol, lines joined with `\n`, with `truncated: true` when it was cut at 200 lines or 16 KiB |
| `same_as` | The span equals that of the earlier item `item` (an index into `locations`), which carries the text; 30 references inside one function cost one copy |
| `not_computed` | No text, with a `reason`: `no_span`, `call_budget`, `document_changed` or `symbol_lookup_skipped` |

For `get_references` the span is the symbol the reference sits in; a top-level reference has `no_span`. For the go-to tools it is the symbol the location names, else the `targetRange` of the server's link answer (local variables and parameters), else `no_span`. One call returns at most 64 KiB of `included` text and cuts at most 256 distinct spans. It stops starting new cuts after scanning 16 MiB of document text. One cut can cross that limit. New spans that exceed a limit report `call_budget`. A smaller later span can still fit the output allowance. `document_changed` means the document changed after the symbol lookup, so the ranges no longer describe its text. `symbol_lookup_skipped` means the file's symbol lookup was skipped or failed; the item's `enclosing_symbol` says why. The text is server-influenced content, passed through verbatim.

### Key naming

Keys that mcpls defines are `snake_case`. Objects passed through from the language server keep LSP's casing, such as the `Diagnostic` items and `selectionRange` on call hierarchy items.

### Arguments

A tool's `outputSchema` lists a field as `required` when mcpls always serializes it, even when its value can be `null`, so a schema-validating client can rely on the key being present.

A tool accepts only the arguments its schema declares. An argument name the tool does not declare, such as `kinds` instead of `kind_filter`, is rejected with an error that names the first unknown field and lists the accepted ones, instead of being ignored. Every `tools/list` input schema carries `additionalProperties: false`, so a schema-validating client sees the same rule. The `data` field of a call or type hierarchy item stays open, because it is opaque by contract.

Kind filters (`kind_filter` of `workspace_symbol_search` and `get_code_actions`, `symbol_kind` of the symbol-addressed tools, `kind` of `get_folding_ranges`) accept their names in any case, and an unknown or over-long value is rejected as `-32602` with the valid values.

## Errors

Failures are returned as MCP errors with a code and a message, except malformed arguments, which come back as a tool result with `isError: true`. Common ones:

| Situation | What to do |
|-----------|-----------|
| No server for the file's language | Add an `[[lsp_servers]]` entry, or install the server |
| `-32602` invalid params | Fix the path or parameters; a path outside every root is rejected, and so is a text argument that is blank or too long (`new_name` or `query` above 1,000 bytes, `symbol_name` above 256 bytes, `trigger` above 8 bytes). The message names the offending field, for example `` `query` `` |
| Tool result with `isError: true` and `failed to deserialize parameters: ...` (unknown field, wrong type, `null` for a defaulted argument) | Use only the arguments the tool declares, with the declared types; the message names the field and lists the accepted ones |
| Server still starting (`ServerInitializing`, `-32051`, retryable) | Wait and retry |
| Server still indexing (`-32050`, retryable) | Wait and retry, or raise `workspace.indexing_ready_timeout_seconds` |
| Server restarted while a request was in flight (`-32054`, retryable) | Retry the request |
| `capability_not_advertised` | The server lacks the feature; see `get_tool_support` |

A path, resource URI or method name that you send and that appears in an error message is escaped and shortened, so control characters and very long input never reach the log or your terminal unchanged.

See [Troubleshooting](../guide/troubleshooting.md) for the fixes.

## What's Next

Start with [Code Intelligence](code-intelligence.md), the tools you will use most.

Navigation tools optionally attach bounded source with `include_source:true` (default false).
See [Optional navigation source](code-intelligence.md#optional-navigation-source) for outcomes, limits, and observed-byte hashes.

## Local feedback

Use `feedback_add` to report concrete mcpls failures, missing capabilities, and useful workarounds once per distinct observation. Skip routine success and duplicate reports. Do not report feedback-tool failures recursively. A reporting failure must not block the user's task.

Supply `operation` (nonblank, at most 128 UTF-8 bytes) and `observation` (nonblank, at most 2,048 bytes). Optional `category` is `friction`, `missing`, or `useful`; optional `workaround` must be nonblank and at most 2,048 bytes. Supplied text is preserved. Do not include secrets or source code. No source, arguments, or workspace context is captured automatically.

The tool needs no workspace or language server. It creates storage only after a valid submission and returns `{"stored": true}` after appending a record. Each record includes a UTC RFC3339 `timestamp`, the mcpls package `version`, and the receiving process's `runtime_provenance`. Provenance matches a local installation receipt against the measured executable; it is not source-build attestation. Legacy records can omit provenance. Listing and lifecycle changes preserve the original metadata and its absence. Repeated submissions append separate records.

On Linux, records are stored in `$XDG_STATE_HOME/mcpls/feedback.jsonl`, or `~/.local/state/mcpls/feedback.jsonl` when `XDG_STATE_HOME` is unset. Platforms without a state directory use the local data directory. The tool fails when neither directory is available. On Unix, the mcpls directory is created with mode `0700` and the file with mode `0600`. Existing storage must be private, owned by the current user, and a regular file without symlinks or extra hard links. Unsafe existing destinations are rejected without changing their permissions.

Inspect records with:

```bash
jq . "${XDG_STATE_HOME:-$HOME/.local/state}/mcpls/feedback.jsonl"
```

Use `feedback_list` to find reports that need investigation. Its default `state` is `open`; choose `resolved` or `all` explicitly to include addressed reports. Results follow newest original append order, regardless of timestamps or resolution changes. `limit` defaults to 20 (1 through 100), and `offset` defaults to zero. Filtering occurs before pagination. Counts describe the filtered reports; separate pages are not a shared snapshot. Reading an absent store creates nothing.

Copy the opaque, case-sensitive `feedback_id` from a list item. Existing JSONL reports need no migration and initially appear open. Identical legacy lines are separate reports with distinct stable IDs. IDs survive supported additions, transitions, and process restarts; manual file rewriting, replacement, reordering, truncation, or rotation is unsupported.

Call `feedback_resolve` with `feedback_id` and a nonblank `resolution` (at most 2,048 UTF-8 bytes). Optional `task_id` and `note_id` are nonblank literal references (at most 128 bytes each). Either, both, or neither can be supplied. Text is preserved exactly. References cause no lookup or journal change and confer no completion, acceptance, or verification authority. The returned resolution includes its UTC millisecond timestamp and null references when omitted. Resolved reports disappear from the default open list.

Call `feedback_reopen` with the exact `feedback_id` to return a resolved report to open and clear its active resolution and references. IDs must be nonblank and at most 256 bytes. Optional arguments must be omitted rather than null; unknown fields, including workspace/project selectors, are rejected. Resolving a resolved report or reopening an open report returns `feedback_conflict`, including an exact repeated request. Unknown IDs return `feedback_not_found`. After an ambiguous failure, list current state before retrying.

Lifecycle changes append to `feedback-state.jsonl` in the same private directory and never alter original `feedback.jsonl` bytes. The sidecar uses the same file privacy checks and mode `0600`. Malformed or truncated reports/state, unsafe storage, and unreadable files cause explicit `feedback_storage_failed` errors; no records are silently dropped or automatically repaired.

Concurrent processes use the original report file's exclusive lock for additions, reads, and transitions. Successful concurrent operations persist without overwriting one another. Writes are not synced to disk. An interrupted write can leave a partial final record; an ambiguous add failure can produce duplicates on retry. There is no crash recovery, exactly-once guarantee, rotation, deduplication, or automatic task creation.
