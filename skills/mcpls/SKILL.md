---
name: mcpls
description: >-
  Install, configure, and run the mcpls CLI — a Rust binary bridging MCP to LSP that gives
  an agent compiler-grade code intelligence (hover, definitions, references, diagnostics,
  rename). Use when setting up or registering mcpls as an MCP server, writing or debugging
  an mcpls.toml, choosing CLI flags or MCPLS_* environment variables, diagnosing why
  mcpls or one of its language servers fails to start, or choosing focused code-navigation
  requests with existing mcpls tools, or explicitly reporting and triaging local mcpls
  feedback. Feedback guidance does not manage general project tasks.
license: MIT OR Apache-2.0
compatibility: >-
  Wraps the `mcpls` Rust binary (Rust 1.99+ / edition 2024 to build from source). Requires at
  least one LSP server on PATH (rust-analyzer, pyright, gopls, clangd, …). HTTP transport
  requires building with the non-default `transport-http` feature.
metadata:
  repository: "https://github.com/bug-ops/mcpls"
  docs: "https://bug-ops.github.io/mcpls/"
---

# mcpls

## Purpose

mcpls is a single Rust binary that bridges the Model Context Protocol (MCP) to the
Language Server Protocol (LSP). It spawns and speaks LSP to real language servers
(rust-analyzer, pyright, gopls, clangd, …) and exposes their capabilities to an AI
agent through MCP tools for hover, go-to-definition, references, diagnostics, rename with internal preparation, completions, symbols, formatting (document and range), call and type
hierarchy, document highlights, a `get_tool_support` report of which tools work for which
languages, and more.

Secrets configured for a server (secret-named `env` values, flags and options) are replaced by
`[redacted:NAME]` markers in the display text of tool results. This is partial: edits, URIs,
identifiers and command arguments are returned unmodified.

This skill covers focused code navigation and operating the **binary**: installing it, choosing CLI flags and
environment variables, registering it with an MCP client, and writing `mcpls.toml`.
For the complete MCP tool and parameter reference, see
[Tools Reference](https://bug-ops.github.io/mcpls/tools/overview.html).
If the bridge is configured with `mcp.tool_prefix`, every tool name listed there gains
that prefix (`{tool_prefix}_{tool}`).

The default `[mcp].tool_profile = "agent"` exposes 37 tools. Set `tool_profile = "full"` to restore all 40 tools, including `get_folding_ranges`, `get_selection_ranges`, and `prepare_type_hierarchy`. Other values are rejected. The catalog is fixed for the session; tools remain visible when a selected server lacks the required capability. `tools/list`, dispatch, and `get_tool_support` use the same catalog and configured prefix. Shared tool schemas are identical in both profiles.

`rename_symbol` resolves a name or position and prepares on the rename server when supported.
Its `proposed` result includes the queried `position`, `preparation`, and `changes`.
A `not_renameable` result has no edits. The standalone MCP `prepare_rename` tool was removed.
Request a proposal, inspect it, then apply complete edits through the client.

`preview_file_move` uses absolute `file_path` and `new_file_path` in one workspace. For example, preview moving `/work/app/lib.ts` to `/work/app/users.ts` before applying any import edits.
The source must be a regular file, and the destination must be absent in an existing directory. The server selected by the `file_move` route must statically register `workspace/willRenameFiles` for the source.

Edit tools (`rename_symbol`, `preview_file_move`, `format_document`, `format_range`, `get_code_actions`) default to bounded readable previews.
Set `full: true` to include exact normalized raw `edits`; previews never change raw replacement text.
Each edit document has `uri`, `edit_count`, `preview`, optional `preview_source` and `supplied_version`, and `analysis_freshness: "unknown"`.
Formatting returns one document in `changes`, including empty successful edit sets. Command-only actions have no preview and require execution outside this MCP interface; mcpls provides no command execution API.
`preview.status` is `complete`, `truncated`, or `unavailable`; inspect `reason` when output is limited.
Diffs have three context lines. Configured secret redaction sets `redacted: true` when it changes displayed text.
`preview_source.origin` is `tracked` or `disk`; `sha256` hashes the exact unredacted observed bytes.
The observation does not prove the server's analyzed baseline. `supplied_version.version` retains an integer or explicit null only when supplied by the selected text-document edit; absent and map versions are omitted.
One response shares 32 files, 8 MiB source plus constructed text, a two-second best-effort acquisition deadline, 100 ms off-executor diff work, and 64 KiB of displayed diff text after redaction.
An OS read already in progress can overrun the deadline. Unreliable positions, invalid ranges, and overlaps suppress diffs without dropping otherwise retained edits.
Previews never write files, execute commands, or add language-server opens solely for referenced source.

Do not apply proposals with non-empty `dropped` or unreliable positions.

`get_diagnostics` accepts exactly one of `file_path` or `file_paths` (1 through 32 absolute paths in one authorized workspace). Null selectors and conflicts fail. Batch paths are limited to 4096 UTF-8 bytes each and 16384 compact JSON string bytes in total. Preflight checks all inputs before analysis starts. Missing or unsupported files produce item errors.

Batch `results` preserve input order and duplicates. Each item has `input_index`, original `file_path`, `status`, and `truncated`; success adds `report` and exact `omitted_diagnostics`, while failure adds `error` with `code`, `message`, and `retryable`. Reports retain the single-file provenance and degradation fields. At most four reads, including optional `context: "enclosing_symbol"`, run concurrently. Cancellation stops queued work; invalidation rejects the whole batch.

Batch output retains at most 100 diagnostics per file, 256 overall, 8192 compact JSON UTF-8 bytes per complete diagnostic, and 131072 bytes per structured result. Budgets allocate in input order and omit whole diagnostics. Error objects use at most 1024 bytes; shortening sets item `truncated`. Outer `truncated` means at least one item was shortened. An empty published report means clean only when untruncated; pending, evicted, failed, or truncated outcomes do not establish cleanliness. One batch call does not guarantee less backend work or stronger freshness. Existing single-file output stays compatible.

When the affected files are unknown, use `get_workspace_diagnostics` to query one
configured server, including for unopened files. Pass its name as `server`, or its
language ID if unnamed. Dispatcher mode also requires an absolute, approved
`workspace_path`. Interpret ranges as zero-based LSP positions using the returned
`position_encoding`. Check `status` and `truncated` before interpreting an empty
report. Even a complete report covers only that provider's current settings.

`get_completions` returns API-discovery candidates ranked by typed-prefix match and sortText, without insertion text or snippet edits. `limit` accepts integers 1 through 100 (default 20); `include_documentation` defaults to false. Null or invalid option values fail. Disabled or absent documentation is null; enabled empty documentation remains empty.

Set `include_completion_refs: true` to select one candidate for deferred detail
or documentation. Pass its exact `completion_ref` and the original absolute
`file_path` to `resolve_completion` in the same stdio or initialized HTTP session.
Duplicate labels have distinct references. The reference may be absent for an
oversized provider item or degraded query position. Reference bytes count
toward the list budget. Listing makes zero resolve requests.

`resolve_completion` returns bounded display fields when `status` is `resolved`.
An `unsupported` provider returns a null item. Provider failures return MCP
errors. The tool runs no completion search and applies no edits or commands.
List again after a stale-reference error, which can follow a provider restart,
detected document change, session change, or bounded memory eviction.

Completion labels, details, and documentation are clipped at 512, 2048, and 4096 UTF-8 bytes after existing display redaction. Labels retain identifier-payload redaction semantics. Clipping keeps a Unicode-scalar prefix without an ellipsis; a clipped label is partial, not an exact source replacement. Each item lists clipped fields in `truncated_fields`, in label/detail/documentation order.

The compact structured JSON is at most 65536 bytes, including escaping and metadata; the legacy text encodes the same result. The longest fitting ranked prefix is returned. `omitted_count` counts excluded candidates. `truncated` means candidates were omitted or retained fields clipped; disabled documentation alone does not set it. `provider_is_incomplete` is the provider's explicit boolean, or null for bare arrays/null responses. Empty responses preserve this distinction. Provider incompleteness and local truncation are independent; false does not prove complete API coverage. Existing `positions_degraded` behavior is unchanged. Output bounds do not bound the original LSP response, provider work, process memory, or whole MCP envelope. The existing completion timeout remains 10 seconds.

## Local feedback

Use `feedback_add` for concrete mcpls failures, missing capabilities, or useful
workarounds once per distinct observation. Skip routine success and duplicates.
Verify the observed `runtime_provenance` before attributing a report to a build;
use its verified full revision when available. If provenance is unknown, say so.
Include `build_sha=<verified full revision>` in `observation` for the observed
build; use `build_sha=unknown` when unavailable. The receiving service's metadata
can describe a different build. Never guess `build_sha` from a checkout's HEAD.
Preserve the original trigger and observation; do not include secrets or source code.

Use `feedback_list` to investigate unresolved reports. Its default state is
`open`; request `resolved` or `all` explicitly when needed. Copy opaque
`feedback_id` values exactly. Skip resolved reports unless new evidence requires
investigation.

Resolve only feedback addressed by the current task, after verifying its original
trigger against the relevant build. Use `feedback_resolve` with a resolution that
states the fix, verification evidence, and full verified Git SHA. Optional
`task_id` and `note_id` are literal context only; they cause no lookup and confer
no completion, acceptance, or verification authority.

Use `feedback_reopen` when contradictory evidence invalidates a resolution.
After an ambiguous failure, inspect current state before retrying. Never edit raw
feedback JSONL files. Feedback failures must not block the user's task; do not
report feedback-tool failures recursively.

See [Local feedback](https://bug-ops.github.io/mcpls/tools/overview.html#local-feedback)
for schemas, pagination, privacy, and durability limits.

## Choose a navigation request

Choose the request from the question, then reuse the established target.
Tool/schema discovery and language-provider support are separate checks. Follow
owner instructions that require `get_tool_support` before first use of each
language. Reuse valid session capability knowledge within that policy; a visible
tool can still be unsupported. Missing optional route detail alone does not
establish missing support. For temporary initialization/indexing errors, allow
at most two retries after short waits, following retry hints within the task
budget. Then use an available fallback and state the limitation. Do not retry an
unsupported operation or restart a server without authority.

| Question | Focused request |
|---|---|
| Which functions or methods are in this file? | `get_document_outline` with the provider's `kind_filter` and a small page. |
| Where is a handler with a known name fragment? | Outline `query`, optionally combined with its reported kind. |
| What is the source of this known declaration? | `get_symbol_context` with `sections:["source"]`, in direct mode. |
| What is its type or documentation? | `get_hover`, or context with `sections:["hover"]`. |
| What does it call directly? | `get_outgoing_calls`, or context with `sections:["outgoing_calls"]`. |
| What are its source and type together? | One context request; the defaults are source plus hover. |
| Where does this use point lead? | `get_definition` for locations, or context with `resolve:"definition"` for selected definition context. |
| Where are a few exact occurrences? | `get_references` in default `locations` mode; no preliminary files query. |
| Which file should I inspect next? | References with `mode:"files"` to compare reported counts per file. |

For a Neovim-style Lua module, list provider-reported functions:

```json
{"file_path":"/work/editor/lua/navigation.lua","kind_filter":"function","limit":20}
```

For a ConTree-style handler lookup, combine an own-name fragment with the reported
method kind:

```json
{"file_path":"/work/service/handlers.py","query":"handle_","kind_filter":"method","limit":10}
```

These are `get_document_outline` arguments. Match the provider's kinds:
`function` is 12 and `method` is 6. One filter cannot select both kinds; omit it
or issue separate requests when both matter. `query` is a case-sensitive
substring of the symbol's own name, never its ancestors. Follow `next_offset`
while `truncated` only if further matches are needed. Preserve the first page's
returned `runtime_generation` and include it when `offset` is greater than zero.
Restart from offset zero after document/runtime changes or `stale_continuation`.
Use `get_document_symbols` for full ranges/hierarchy. Empty filtered output does
not prove absence. Confirm flat or degraded coordinates against current source.

For mcpls/Zhournal-style symbol inspection, use an already established definition
position directly, without another definition lookup:

```json
{"file_path":"/work/app/src/index.rs","line":24,"character":8,"sections":["source"]}
```

These are `get_symbol_context` arguments; positions are 1-based UTF-16.
Select only the sections the next step needs. Bundles can avoid separate calls
for a combined question; they are not always smaller than standalone tools.
Navigation excerpts do not guarantee complete bodies. Section failures are
independent; inspect each status and reason. Context reports
`analysis_freshness:"unknown"` and `atomic_snapshot:false`: source and semantic
observations can describe different revisions. Source hashes identify observed
bytes, not the analyzer's snapshot.

Reuse a valid definition position or copy an emitted `symbol_target` unchanged
into a supported consumer. Request `include_targets:true` from discovery only
when a later operation benefits. Do not fabricate or decode the opaque reference,
or combine the envelope with legacy selectors. Failed reuse requires renewed
discovery from current evidence. Reuse proves neither analyzer freshness nor
access to another workspace. See [portable target inputs and errors](https://bug-ops.github.io/mcpls/tools/symbol-context.html#copy-a-portable-target).

Start with locations when you only need positions. Use the
[source options](#optional-navigation-source) when the question needs text, or
`context:"enclosing_symbol"` when you only need the enclosing symbol's name.
Files-mode reference summaries add metadata and can be larger than a small
locations response. Use them to choose files, then narrow `path_filter` for
exact occurrences if needed. Query locations directly for a small set of known
occurrences. Increase `limit` or narrow the filter when output is cut.
Counts preserve duplicates and provider order; incomplete counts are lower
bounds for the received response, never proof of complete workspace coverage.
Smaller output does not establish less provider work. Read-only navigation
provides no authority to edit, acquire unauthorized source, or restart servers.

### Small reference queries

`get_references` defaults to 10 locations with both excerpt flags off. It
preserves provider order and duplicates. Set `limit` (integer 1 through 10000)
for a larger answer. Set `broader_context:true` for bounded excerpts.
Use `"limit":100,"include_source":true` to request the previous default behavior.

An absolute `path_filter` selects one file or a directory subtree by lexical path
components before the limit. Relative filters fail; globs and symlink resolution
are not supported. `matched_count` counts matching locations in the received
provider array; `truncated` means matching locations were omitted. Only an
originally empty provider array carries `index_coverage`. These results do not
prove workspace coverage or provide a representative usage audit. Continuation
is deferred; use a larger limit or narrower filter for follow-up queries.
Smaller output does not guarantee less LSP backend work.

With both excerpt flags off, requests skip excerpt reads and copies. With
`context` omitted or `"none"`, requests skip enclosing-symbol enrichment.
Required synchronization and position normalization still occur.
Excerpt outcomes have status `text` or `unavailable`. Text includes `start_line`
(1-based), `origin` (`tracked` or `disk`), and `shortened`. Source snapshots and
LSP locations are not atomic. External paths and symlink escapes disclose no source.

Reference excerpts have fixed limits of 100 candidate locations, 16 files,
1 MiB per file, 4 MiB aggregate source read/copy allowance, 2 KiB per excerpt,
64 KiB total excerpt text, and a two-second best-effort deadline. Exhausted limits
preserve locations with unavailable reasons or shortened context. An in-flight
OS call can exceed the deadline. See the book's `get_references` reference for details.


Use `mode:"files"` to choose the next file to inspect. The default mode is
`"locations"`; explicit `null` and unknown modes fail. In files mode, `locations`
is empty and `file_summary.files` contains groups in first-seen provider order.
Each group has the exact provider `uri`, `count` including duplicates, and the
first matching `example` with the usual location, warning, context, and optional
source fields. Excerpt flags apply to the selected `example` locations.
`context:"symbol_source"` requires `mode:"locations"` and fails in files mode.
Different URI spellings remain separate, even for the same file.

`limit` bounds groups in this mode, with the same default 10 and range 1..10000.
Filtering precedes grouping. Counts include all matching locations in the
processed prefix, even when their groups are omitted from output. The summary
reports `received_count`, `processed_count` (including nonmatching locations),
`file_count`, `omitted_file_count`, `counts_complete`, `output_truncated`,
`counting_basis:"received_locations_including_duplicates"`, and `limits`.

Processing stops before exceeding 100000 locations, 8388608 aggregate UTF-8 URI
bytes, or 10000 distinct matching URIs. With `counts_complete:false`, group
counts, `matched_count`, and `file_count` are lower bounds for the received array.
These counts never guarantee complete workspace coverage. Group limits alone do
not make counts incomplete. `output_truncated:true` means known groups were
omitted by `limit` or the byte budget. Top-level `truncated:true` also covers
incomplete processing. An empty provider response retains `index_coverage`;
a nonempty response emptied by filtering does not.

The compact UTF-8 JSON for the entire `file_summary`, including example context
and source, is limited to 65536 bytes. Output keeps a prefix of whole groups.
Only selected examples receive normalization and optional enrichment. An excerpt
outcome counts toward the byte limit even when it reports `unavailable`. Final
byte trimming can omit selected groups. Existing source and context limits apply.
The `limits` fields are `max_processed_locations`, `max_uri_bytes`, `max_files`,
and `max_output_bytes`, with the values above. This bounds additional local work
and output; it does not bound the already received LSP response or backend work.

For a supported file URI, `file_path` is an absolute lexical path. When file
selection is the question, query files first, then pass the chosen `file_path` as `path_filter` with `mode:"locations"`
to inspect details. For example, use `{"mode":"files","limit":5}` with your
symbol address, then add `{"path_filter":"/work/app/src/usage.rs"}` to a new
locations query. URI aliases can map to the same filter path. Unsupported URIs
remain visible with `file_path:null` and
`path_filter_unavailable:"unsupported_uri"`. This also applies when the decoded
path cannot be represented as a UTF-8 JSON string. Retained examples report
requested excerpt failures. Source and positions do not form an atomic snapshot.

### Optional navigation source

`get_definition`, `get_references`, `go_to_implementation`,
`go_to_type_definition`, and `go_to_declaration` support these options. Both
boolean excerpt flags, `broader_context` and `include_source`, default to `false`.
`go_to_declaration` requires a position. The other four also accept symbol names.

Choose the smallest response that answers the question. Reuse returned text
when it is sufficient and skip a follow-up source read.

**Locations only.** Use `get_definition` without source options to find where a
use leads.

```json
{"file_path":"/work/app/src/checkout.rs","line":42,"character":9}
```

**Nearby lines.** Use `get_references` with `broader_context:true` to inspect
what happens near a few call sites.

```json
{"file_path":"/work/app/src/billing.rs","symbol_name":"process_payment","limit":3,"broader_context":true}
```

**Symbol body.** Use `go_to_implementation` with `context:"symbol_source"` to
inspect the bounded implementation body.

```json
{"file_path":"/work/app/src/checkout.rs","line":42,"character":9,"context":"symbol_source"}
```

**Body and nearby lines.** Add `broader_context:true` to `go_to_implementation`
only if you also need lines around the returned implementation location.

```json
{"file_path":"/work/app/src/checkout.rs","line":42,"character":9,"context":"symbol_source","broader_context":true}
```

| `context` | Excerpt flags | Fields on the location |
|---|---|---|
| Default (`"none"`) or `"enclosing_symbol"` | Both off | No source fields |
| Default or `"enclosing_symbol"` | Legacy `include_source:true` alone | Bounded excerpt in `source` |
| Default or `"enclosing_symbol"` | `broader_context:true`, including when both flags are true | One bounded excerpt in `broader_context` |
| `"symbol_source"` | Both off | Bounded body outcome in `source` |
| `"symbol_source"` | Either or both flags true | Body outcome in `source` and one excerpt outcome in `broader_context` |

For references, the body comes from the symbol that encloses the occurrence.
A top-level reference has no body span. For go-to tools, mcpls uses the returned
target symbol's span or falls back to the server's target range when available.
Each requested outcome can be unavailable independently. For bodies, check
`source.status` for `included`, `same_as`, or `not_computed`. Check `truncated`
on included text, and follow `same_as.item` to the earlier location that holds
the text.

An excerpt has status `text` or `unavailable`. See
[Small reference queries](#small-reference-queries) for reference excerpt limits.

#### Go-to excerpt details

The following fields, limits, and guarantees apply to excerpts from the four
go-to tools. A text outcome includes `text`, `start_line` (1-based),
`origin` (`tracked` or `disk`), `sha256`, and `shortened`. The lowercase SHA-256
identifies the complete unredacted observed file bytes, including bytes outside
the excerpt. It does not identify an analyzed snapshot or prove current disk
freshness. Tracked source takes precedence over a contained disk read.
Duplicate canonical files share one observation.

The preferred window includes the location's starting line and up to two lines
on each side. Columns and the location's ending line do not select source.
Original UTF-8 and LF, CRLF, and CR separators remain, except configured secret
redaction. No final newline is added. Empty source has line 1; a final separator
permits a trailing empty line. File boundaries alone do not set `shortened`.
When the window cannot fit, the whole target line is tried with `shortened:true`.
The excerpt never guarantees a complete definition body.

Limits per request are 100 candidate positions, 16 distinct attempted files,
1 MiB per complete file, 4 MiB aggregate acquisition/copy allowance including
charged growth lookahead, 2 KiB per excerpt, 64 KiB total finally redacted text,
and 2 MiB added compact JSON. Failed reads can reserve allowance conservatively.
The best-effort two-second deadline stops new acquisition and read chunks;
an in-flight OS operation can overrun it. These limits do not bound the original
LSP request or total response time. Final configured secret redaction preserves
the same text limits, with whole-line fallback or an unavailable outcome.

Unavailable excerpts contain only `status` and `reason`: `candidate_limit`,
`file_limit`, `source_budget`, `text_budget`, `deadline`, `access_denied`,
`read_failed`, `file_too_large`, `invalid_utf8`, or `line_unavailable`.
They respectively mean the candidate cap, file cap, acquisition allowance,
display allowance, deadline, containment authorization, file I/O, file size,
UTF-8 decoding, or source line prevented text. Candidate positions beyond 100
always report `candidate_limit`. Partial successes and failures coexist.

The selected workspace context authorizes excerpts. External paths, non-file URIs,
and symlink escapes yield `access_denied`. Lexical `out_of_workspace` remains
advisory. Excerpt failures preserve locations, order, duplicates, ranges,
`resolved_symbol`, enclosing context, `positions_degraded`, and `truncated`.
With both excerpt flags off, mcpls skips excerpt acquisition, copying, and hashing.
Excerpt enrichment alone adds no LSP opens or semantic requests. Source and LSP
locations are separate observations that can describe different revisions.

`get_tool_support` also provides diagnostics; this does not waive owner-required
first-language capability checks described above.

## Prerequisites

mcpls does not implement language analysis itself — it forwards to a real LSP server
that must already be installed and on `PATH` (e.g. `rust-analyzer`, `pyright-langserver`,
`gopls`, `clangd`, `typescript-language-server`). mcpls runs multiple LSP servers
concurrently, and one failing to start does not affect the others (graceful
degradation) — but a language with no server configured, or whose server isn't on
`PATH`, simply has no code intelligence available for it.

See [Language Server Setup](https://bug-ops.github.io/mcpls/guide/language-servers.html)
for per-language install commands.

## Installation

**From crates.io (recommended):**

```bash
cargo install mcpls
```

**Pre-built binaries:** download the archive for your platform from
[GitHub Releases](https://github.com/bug-ops/mcpls/releases), extract it, and move
the `mcpls` binary onto your `PATH`. See
[Pre-Built Binaries](https://bug-ops.github.io/mcpls/getting-started/installation.html)
for per-platform archive names. Each archive ships with a `.sha256` sidecar; verify
before extracting:

```bash
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-<target>.tar.gz
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-<target>.tar.gz.sha256
shasum -a 256 -c mcpls-<target>.tar.gz.sha256
```

**From source (this repository):**

```bash
cargo install --path crates/mcpls-cli
```

This builds only the default feature set — no HTTP transport (see below). To build
with HTTP transport support, add `--features transport-http`.

**Verify:**

```bash
mcpls --version
```

## CLI Reference

Each mcpls-specific option below accepts an equivalent `MCPLS_*` environment
variable; the flag takes precedence when both are set. `--version`/`--help` have
no environment variable equivalent.

| Flag | Short | Env var | Default | Notes |
|---|---|---|---|---|
| `--config <FILE>` | `-c` | `MCPLS_CONFIG` | auto-detect | Always trusted, even a *relative* path set via the env var — naming a path is treated as consent, so this bypasses the project-config trust gate entirely (see [Config trust model](#config-trust-model)). Hard-errors at startup if the file doesn't exist — unlike auto-detection, it never falls back to defaults. |
| `--trust-project-config` | — | `MCPLS_TRUST_PROJECT_CONFIG` | `false` | See [Config trust model](#config-trust-model) below. The env var accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive); any other value is a startup parse error. |
| `--workspace-trust <MODE>` | — | — | `trusted` | `untrusted` starts only the servers named with `--allow-server` and refuses a config file or server executable inside the workspace; conflicts with `--trust-project-config`. Not a sandbox. |
| `--allow-server <ID>` | — | — | none | Repeatable; the server's `name`, else its `language_id`. Requires `--workspace-trust untrusted`. |
| `--log-level <LEVEL>` | `-l` | `MCPLS_LOG` | `info` | Any `tracing-subscriber` `EnvFilter` directive works, e.g. `mcpls=debug,info`. An invalid value does **not** error — it silently falls back to `info`. |
| `--log-json` | — | `MCPLS_LOG_JSON` | `false` | Output logs in JSON format for structured logging. The env var accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive). |
| `--listen <ADDR>` | — | `MCPLS_LISTEN` | unset | HTTP transport bind address (e.g. `127.0.0.1:3000`). Only exists when built with `--features transport-http` — see [HTTP transport caveats](#registering-with-an-mcp-client). |
| `--http-path <PATH>` | — | `MCPLS_HTTP_PATH` | `/mcp` | URL path the MCP service mounts at. Must start with `/`, must not be `/`, and may use only ASCII letters, digits and `-._~` per segment; an invalid value exits with code 2 before any server starts, even without `--listen`. Only used with `--listen`; same `transport-http` feature gate. HTTP requests carrying an `Origin` other than a loopback origin on the bound port or one listed with `--http-allowed-origin` get `403`. |
| `--http-allowed-origin <ORIGIN>` | — | `MCPLS_HTTP_ALLOWED_ORIGINS` | none | Extra browser origin (`http(s)://host[:port]`, repeatable or comma-separated) accepted besides loopback origins on the bound port. The request's `Host` must be allowed too (`--http-allowed-host`). |
| `--http-allowed-host <HOST>` | — | `MCPLS_HTTP_ALLOWED_HOSTS` | none | Extra `Host` header value (`host[:port]`, no wildcards, repeatable or comma-separated) accepted besides `localhost`, `127.0.0.1`, `::1` and the bound IP address; without a port any port matches; ports 80, 443 and 0 are rejected (clients omit the default ports). Needed when reaching mcpls by name or when bound to `0.0.0.0`. |

Plus standard `--version` / `--help`. No subcommands.

By default, mcpls records aggregate tool-call counts and durations in a local
SQLite database for both stdio and HTTP. To disable collection for one process,
set `MCPLS_USAGE=0` in its environment before startup. Existing history is preserved.

## Registering with an MCP client

**stdio (default, works with any build):**

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

**Per-project, trusting that project's `mcpls.toml`:**

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": ["--trust-project-config"]
    }
  }
}
```

Only pass `--trust-project-config` for repositories you trust — see
[Config trust model](#config-trust-model). Prefer the `args` form above (scoped to
this one client config entry) over `MCPLS_TRUST_PROJECT_CONFIG=true` in a shell
profile or `.envrc` — the env var is a blanket grant for every `mcpls` process
launched in that shell, including future untrusted checkouts.

**HTTP transport:** requires a binary built with `--features transport-http`
(`cargo install mcpls --features transport-http`, or the equivalent `cargo install
--path` form). This asymmetry matters when diagnosing startup failures:

- On a build **without** the feature, passing `--listen` on the command line is a
  startup **parse error** (`unexpected argument '--listen'`) — clap doesn't know the
  flag exists.
- On that same build, setting `MCPLS_LISTEN`/`MCPLS_HTTP_PATH` as environment
  variables produces **no error at all** — the fields they'd bind to are compiled
  out, so mcpls silently ignores them and serves stdio as usual.

If mcpls appears to ignore `MCPLS_LISTEN`, or `--listen` errors as unrecognized, the
binary was built without `transport-http` — reinstall with the feature enabled.

## Configuration

### Search order and paths

mcpls resolves configuration in this order; the first match wins:

1. `--config <FILE>` / `$MCPLS_CONFIG` — an explicit path is always trusted and
   loaded via a strict path that hard-errors if the file is missing (it never falls
   back or creates a default).
2. `./mcpls.toml` in the current directory — only loaded when trusted (see below).
3. The platform user-config path (table below).
4. Built-in defaults (covers 30 languages out of the box).

| Platform | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml` |
| macOS | `~/Library/Application Support/mcpls/mcpls.toml` |
| Windows | `%APPDATA%\mcpls\mcpls.toml` |

`~/.config/mcpls/mcpls.toml` is **not** read on macOS — only
`~/Library/Application Support/mcpls/mcpls.toml` is.

**First run writes a default config.** If auto-detection (tiers 2–4 above) finds no
existing file, mcpls writes one to the platform path in the table, populated with all
30 default language mappings, and continues running. If the write fails (e.g.
read-only filesystem), it degrades gracefully to in-memory defaults with a warning —
it does not crash. This auto-create behavior only applies to auto-detection; passing
`--config <path>` explicitly never creates a file, it only reads one.

### Config trust model

A project-local `./mcpls.toml` can set the `command`/`args` mcpls spawns as an LSP
server — so honoring one automatically from an unfamiliar checkout would be arbitrary
code execution. mcpls therefore **ignores** a discovered `./mcpls.toml` by default.
Pass `--trust-project-config` (or `MCPLS_TRUST_PROJECT_CONFIG=true`) only for
repositories you trust. Note that the env var grants trust process-wide, not
per-project — setting it in a shell profile trusts every `mcpls` invocation that
shell ever launches, not just the one project you meant to trust.

Trusting the config is not trusting the workspace: language servers run workspace code
(build scripts, procedural macros, tsconfig plugins) and mcpls does not sandbox them, so
analyze an untrusted checkout only inside a container or disposable VM. For the TypeScript
server, mcpls pins `tsserver` to the one bundled with `typescript-language-server` on npm symlink
and `.cmd`/`.ps1` shim installs, pnpm global installs and `node`/`nodejs`/`bun` (also versioned, such as
`node22`) running an absolute `cli.mjs`; Volta, asdf, mise and `npx`/`bunx`/`deno npm:` launchers are not covered, and
`initialization_options.tsserver.path` overrides the pin. TypeScript 7 ships no `tsserver`, so the server needs `typescript@6` next to it (or
configure `tsc --lsp --stdio` explicitly). See [SECURITY.md](https://github.com/bug-ops/mcpls/blob/main/SECURITY.md).

When a project config is found but ignored, mcpls does not just log a `tracing::warn!`
to stderr (which a stdio-based agent typically can't see) — it also appends a NOTE to
the `instructions` field of the MCP `initialize` response (`ServerInfo.instructions`,
populated by `McplsServer::get_info`). **Check the server instructions surfaced at
connection time**: if you see a note about an ignored project config, the file wasn't
malicious-by-default — it just needs an explicit trust decision.

The rest of `instructions` is not guaranteed to be mcpls's built-in capability
blurb: a configured `[mcp].instructions` (see [Configuration
Reference](https://bug-ops.github.io/mcpls/reference/config.html#mcp))
**replaces** that text entirely. Read whatever `instructions` actually contains at
connection time rather than assuming the built-in wording — the ignored-config NOTE
above is still appended after it regardless.

### Starter config

A minimal `mcpls.toml` — expand per-language via
[Configuration Reference](https://bug-ops.github.io/mcpls/reference/config.html):

A `[workspace]` table is deliberately omitted here: `roots` defaults to `[]`, which
already auto-resolves to the current directory, and adding `[workspace]` for that
alone would zero out the 30 built-in `language_extensions` mappings (see
[`language_extensions`](references/configuration.md#workspace-fields)) unless you
list them all back explicitly.

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
name = "rust-analyzer"
timeout_seconds = 30
request_timeout_seconds = 30

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
name = "pyright"
timeout_seconds = 30
request_timeout_seconds = 30
```

For the full field reference (`handles` routing, `initialization_options`, `env`
allowlist, heuristics), see
[references/configuration.md](references/configuration.md).

## Task recipes

| Goal | How |
|---|---|
| Add support for a new language | Add a `[[lsp_servers]]` entry with `language_id`, `command`, `file_patterns`. See [Multi-Language Configuration](https://bug-ops.github.io/mcpls/guide/language-servers.html#monorepos). |
| Route specific tools to a specialized server | Set `handles` on each `[[lsp_servers]]` entry to claim only the tools that server should serve; see `handles` in [references/configuration.md](references/configuration.md). |
| Fix a slow/timing-out LSP request | Raise that server's `request_timeout_seconds` (per-request) and/or `timeout_seconds` (handshake) in `mcpls.toml`. |
| Handle a "still initializing" error on a large project | This is `Error::ServerInitializing` — the server is up but hasn't finished its `initialize` handshake (common with rust-analyzer on a large repo). It returns immediately, it does not time out, so raising `request_timeout_seconds` does nothing here. Wait and retry the call instead. |
| Debug why mcpls won't start | Run with `--log-level debug` (or `trace`) and inspect stderr; see [Troubleshooting](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#read-the-logs-first). |
| Use a trusted project's own `mcpls.toml` | Pass `--trust-project-config` — see [Config trust model](#config-trust-model). |

## Troubleshooting

For symptom-driven debugging, see
[Troubleshooting Guide](https://bug-ops.github.io/mcpls/guide/troubleshooting.html):

- ["command not found: mcpls"](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#command-not-found-mcpls) — `PATH` doesn't include Cargo's bin directory.
- [mcpls not showing up in an MCP client](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#mcpls-does-not-appear-in-the-client) — verify the client config points at an absolute binary path.
- ["LSP server not available for file type"](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#lsp-server-not-available-for-file-type) — no `[[lsp_servers]]` entry matches the file's extension.
- ["Configuration file not found" / unexpected config used](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#configuration-file-not-found-or-the-wrong-one-is-used) — check the [search order and paths](#search-order-and-paths) above first.
- [High memory or CPU usage](https://bug-ops.github.io/mcpls/guide/troubleshooting.html#queries-are-slow-or-time-out) — usually an over-broad `workspace.roots` or too many configured servers.

## Further reference

[references/configuration.md](references/configuration.md) — full `mcpls.toml` schema
tables (`[workspace]` fields, `[[lsp_servers]]` fields, the `handles` routing map, the
`env` allowlist), for cases the starter config above doesn't cover.
