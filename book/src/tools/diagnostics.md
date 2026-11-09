# Diagnostics and Formatting

In this chapter you learn how to read the errors and warnings a language server reports, and how to ask for formatting and structural ranges. Diagnostics are what turn an assistant's "this should compile" into a checked fact.

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## get_diagnostics

Returns the errors, warnings and hints for one file, merged from a fresh pull request and the notifications the server has pushed (for example clippy results from rust-analyzer).

```json
{ "file_path": "/work/app/src/main.rs" }
```

```json
{
  "diagnostics": [
    {
      "range": {
        "start": { "line": 10, "character": 8 },
        "end": { "line": 10, "character": 24 }
      },
      "severity": "error",
      "message": "cannot find value `undefined_variable` in this scope",
      "code": "E0425"
    }
  ],
  "availability": "published",
  "origin": "pull",
  "indexing_in_progress": false,
  "push_notifications_degraded": false
}
```

Severity is `error`, `warning`, `information` or `hint`. Add `context: "enclosing_symbol"` to see which function each diagnostic is in.

Read the extra fields before you trust an empty list:

| Field | Values and meaning |
|-------|--------------------|
| `availability` | `published`: the server reported on the file, so an empty list means clean. `pending`: nothing was published yet. `evicted`: a publish was lost, either dropped to bound the cache or unreadable (oversized, malformed or undecodable). An empty list next to `pending` or `evicted` is not a clean file |
| `origin` | `pull` when a `textDocument/diagnostic` request answered, `push_cache` when the server has no pull support and the cache alone answered, `cache_after_failed_pull` when the pull failed and the cache answered |
| `indexing_in_progress` | `true` if the server was still indexing during the read, so early errors may be false and real ones may be missing |
| `push_notifications_degraded` | `true` if the server crashed and restarted during this session, so push-only diagnostics may be missing until mcpls restarts |

**After an edit.** Once mcpls has re-synced a changed file, a pushed diagnostic from an older version of the file is not listed when the server's pull for the current version no longer reports it, so an error you just fixed does not linger for the 250 to 350 ms a server such as pyright needs to publish again. Only items the same server's pull reported before are dropped: pushed diagnostics that no pull ever reports (clippy results from rust-analyzer, for example) stay. If the pull failed or the server answers only by push, nothing is dropped and the latest push is the answer. Limits: a pushed item without a `source` that no earlier pull covered, and items of a server whose pulls never reported that `source` yet (for example its first pull came back empty), stay until the server publishes again.

Diagnostics from the two sources are deduplicated by severity, code and proximity (within 3 lines), without comparing messages. Two distinct errors with the same code that start within 3 lines appear once, with the pulled message.

### Several files in one request

Use exactly one selector: `file_path` for the existing single-file response, or
`file_paths` for 1 through 32 absolute paths in one authorized workspace.
Null selectors, both selectors, and neither selector are invalid.
Each batch path is at most 4096 UTF-8 bytes. Their compact JSON string encodings
must total at most 16384 bytes. Selection checks every input before starting analysis.
Missing or unsupported files inside the workspace produce individual errors.

```json
{ "file_paths": ["/work/app/src/main.rs", "/work/app/src/lib.rs"], "context": "enclosing_symbol" }
```

The response has `results` in input order and an outer `truncated` flag.
Each occurrence, including duplicates, has `input_index`, the original `file_path`,
`status` (`ok` or `error`), and `truncated`. Successful items have `report`, with the
same fields as the single-file response, and `omitted_diagnostics`. Failed items
have `error` with numeric `code`, string `message`, and boolean `retryable`.
A failed item does not erase successful reports. Envelope and workspace-selection
failures reject the whole call.

At most four reads, including optional context work, run concurrently. Cancellation
stops queued reads; runtime invalidation rejects the whole batch. Already-sent
operations can finish. Batching reduces client calls; it does not guarantee less
language-server work, fresher results, or a clean workspace.

Batch output retains at most 100 diagnostics per file and 256 in total, allocated
in input order. Each retained complete diagnostic uses at most 8192 compact JSON
UTF-8 bytes. The complete structured result is at most 131072 bytes. Diagnostics
that exceed a limit are omitted whole; later fitting diagnostics can remain.
`omitted_diagnostics` counts all diagnostics removed from that report. Error objects
are limited to 1024 bytes; shortened error messages also set `truncated`.
The outer flag is true when any item is truncated. These limits apply to returned
output, not language-server computation. Single-file output keeps its existing limits.

An empty published report means clean only when the item is untruncated.
Pending, evicted, failed, or truncated outcomes do not establish a clean file.
Inspect origin and degradation fields for each report separately.

## get_cached_diagnostics

Returns what mcpls already holds for a file, with no new analysis: the diagnostics the server pushed plus the last full report a `get_diagnostics` call stored. It is fast and takes only `file_path`.

While the file's server is still starting it returns the retryable `ServerInitializing` error, and if the server failed to start it returns that failure, instead of an empty list. An empty list therefore always means "no diagnostics". The same cache backs the `lsp-diagnostics://` resource; see [Diagnostics and Resources](../advanced/diagnostics.md).

## format_document

Returns a one-document `changes` array with a readable diff against observed source.

Arguments: `file_path`, optional `full` (default `false`), `tab_size` (1 to 32, default 4) and `insert_spaces` (default `true`).

```json
{ "file_path": "/work/app/src/main.rs", "tab_size": 4, "insert_spaces": true }
```

Set `full: true` to include exact normalized raw edits. Empty successful edits still return one document.
See [observed-source previews](refactoring.md#observed-source-previews) for states, source identity, unknown freshness, redaction, and shared limits.

## format_range

Returns a one-document preview for formatting a range. Set `full: true` for exact normalized raw edits; the default is `false`. Arguments: `file_path`, the four range fields, `tab_size` and `insert_spaces`. A line past the end of the file is rejected.

```json
{
  "file_path": "/work/app/src/main.rs",
  "start_line": 12, "start_character": 1,
  "end_line": 20, "end_character": 1
}
```

Not every server supports range formatting; rust-analyzer does not advertise it.

## get_selection_ranges

Available with `[mcp].tool_profile = "full"`.

Returns the ranges that enclose a position, innermost first (expression, statement, block, function, and so on). Pass one of them to `get_code_actions` or `format_range`.

```json
{ "file_path": "/work/app/src/main.rs", "line": 14, "character": 9 }
```

## get_folding_ranges

Available with `[mcp].tool_profile = "full"`.

Returns the foldable regions of a file with 1-based lines, a `kind` and collapsed text, sorted by start line with the longest first. Argument `kind` is `all` (default), `comment`, `imports` or `region`, in any case.

```json
{ "file_path": "/work/app/src/lib.rs", "kind": "imports" }
```

It is a compact way to see the shape of a large file.

## What's Next

Continue with [Refactoring](refactoring.md) to turn what you found into changes.

## diagnostics_delta

Capture one file's normalized merged diagnostics, then compare the returned
caller-held baseline with another observation. Both tool profiles expose this tool.

```json
{"file_path":"/work/app/src/main.rs","action":"capture","mode":"refresh"}
```

The response has `status: "baseline_captured"`, `baseline`, and `truncated`.
Pass the entire returned `baseline` object unchanged to compare:

```javascript
const captured = await callTool("diagnostics_delta", {
  file_path: "/work/app/src/main.rs", action: "capture"
});
const delta = await callTool("diagnostics_delta", {
  file_path: "/work/app/src/main.rs", action: "compare",
  baseline: captured.baseline
});
```

`mode` defaults to `refresh`, which synchronizes the document and uses the same
pull/probe and push-merge policy as `get_diagnostics`. `cached` reads resident
observations without starting a language server, synchronizing, or pulling.
An unstarted route returns an ordinary unavailable or initializing error.

Matching uses exact normalized range, severity, code, message, and source.
Positions are 1-based UTF-16 coordinates. Changed messages or shifted ranges
produce unmatched old and new observations. Each current occurrence consumes
the lowest unmatched equal baseline index. Existing push/pull deduplication
still determines the visible population; delta does not restore discarded items.

`added` contains unmatched current diagnostics. `remaining`, `resolved`, and
`uncertain_removed` contain ascending, zero-based baseline indices. For example,
a baseline `[A, B]` followed by `[B, C]` gives `remaining: [1]`, `resolved: [0]`,
and `added: [C]` when both observations are complete. With incomplete evidence,
`resolved` is empty and `[0]` appears in `uncertain_removed` instead. With an
incomplete baseline, `added` only means absent from its retained items.

`comparison_complete` requires complete published observations without indexing,
degradation, position uncertainty, or loss. `status` is `compared` when complete,
otherwise `incomplete`. `resolved` means absent from comparable observed reports.
It does not prove that the source is fixed or the program compiles.

The report exposes availability, origin, pull kind, result ID, tracked sync
version, server-supplied report version, cached origins, completeness, uncertainty
reasons, and omitted diagnostic count. `source_freshness` is always `unknown`.
An unchanged pull reuses retained full materialization only with matching report
continuity. A result ID or local sync version does not prove analysis completion.
A delayed push can continue to expose the previous report after a source edit.
Missing or mixed report versions remain null.

Generated opaque identity fields are limited to 256 UTF-8 bytes. A configured
server identifier beyond that limit makes `diagnostics_delta` return a typed
unavailable error; existing diagnostics tools and configuration remain usable.

Baselines have no TTL or server-side storage. Canonical file/workspace identity,
runtime generation, server ID, and server incarnation must match. Server or
runtime replacement invalidates them. A well-formed mismatch returns only
`status: "incompatible_baseline"` and its reason: `schema_mismatch`,
`file_mismatch`, `workspace_mismatch`, `runtime_mismatch`, `server_mismatch`, or
`server_restarted`, in that precedence. Concurrent invalidation can return an
ordinary typed runtime error. Capture again to advance the baseline.

Paths are absolute and at most 4096 UTF-8 bytes. Baselines contain at most 256
diagnostics and 65536 compact JSON UTF-8 bytes. Each diagnostic is at most 8192
bytes; whole responses are at most 131072 bytes. Output limits omit whole
items, retain later fitting items, and set `truncated: true` with incomplete
evidence. Known upstream loss also sets truncation. `omitted_diagnostics` is
exact when known, otherwise null. Result IDs longer than 1024 bytes, or changed
by secret redaction, are omitted and cannot establish continuity.

Malformed envelopes, unknown fields, explicit optional nulls, malformed nested
records, invalid positions, and oversized baselines return JSON-RPC `-32602`
before starting or querying a language server. Existing path authorization,
unsupported-language, startup, cancellation, and no-observation pull errors remain
ordinary MCP errors. Caller-held baselines are unauthenticated evidence.

Known loss of an alias or cached source remains incomplete across later source
publications. If bounded eviction history forgets which files lost sources,
completeness becomes unknown until the analysis context is recreated. This
uncertainty does not claim that every observed file lost diagnostics.
