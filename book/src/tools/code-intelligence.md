# Code Intelligence

In this chapter you learn the tools for reading code: types, definitions, usages, outlines and completions. They answer "what is this, where does it come from, and who uses it". Addressing and result flags are explained in [Tools Overview](overview.md).

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## get_hover

Returns the type signature and documentation for the symbol at a position.

Arguments: `file_path`, and `line` with `character` or `symbol_name` (with optional `symbol_kind`, `container`).

```json
{ "file_path": "/work/app/src/models.rs", "line": 10, "character": 5 }
```

```json
{
  "contents": "```rust\nstruct User {\n    id: u64,\n    name: String,\n}\n```\n\nUser information structure.",
  "range": {
    "start": { "line": 10, "character": 5 },
    "end": { "line": 10, "character": 9 }
  }
}
```

The result is `null` when the server has no hover information for that position. Hover works best with statically typed languages.

## get_definition

Returns where a symbol is defined, across files and crates.

Arguments: the same addressing as `get_hover`, plus optional `context` (see [enclosing-symbol context](overview.md#enclosing-symbol-context)).

```json
{ "file_path": "/work/app/src/billing.rs", "symbol_name": "process_payment" }
```

```json
[
  {
    "uri": "file:///work/app/src/billing.rs",
    "range": {
      "start": { "line": 23, "character": 0 },
      "end": { "line": 23, "character": 14 }
    },
    "out_of_workspace": false
  }
]
```

A symbol with several definitions returns several locations; an unknown symbol returns an empty array.

## get_references

Returns references reported by the language server, in server order, including duplicates.

Arguments: addressing as above, `include_declaration` (default `false`), optional `context`,
`limit` (default 100, integer 1 through 10000), optional absolute `path_filter`,
and `include_source` (default `true`). Relative filters are rejected.

```json
{
  "file_path": "/work/app/src/errors.rs",
  "symbol_name": "Timeout",
  "container": "ApiError",
  "limit": 20,
  "path_filter": "/work/app/src/handlers",
  "context": "enclosing_symbol"
}
```

The filter selects that exact file or descendants by lexical path components.
It decodes file URIs and collapses `.` and `..` without resolving symlinks.
It supports no globs. Non-file URIs do not match a filter.
Filtering scans the complete received response before applying the limit.
`matched_count` counts matching locations in that response, independently of `limit`.
`truncated: true` means matching locations were omitted. Neither field guarantees workspace-wide coverage.

Each location preserves `uri`, `range`, advisory `out_of_workspace`, and optional `enclosing_symbol`.
With source enabled, `source` has one of these shapes:

```json
{"status":"text","text":"fn usage() {\n    call();\n}\n","start_line":10,"origin":"disk","shortened":false}
```

```json
{"status":"unavailable","reason":"access_denied"}
```

Text includes the reference's starting line and up to two lines on either side.
`start_line` is 1-based. Unicode and LF, CRLF, and CR line endings are preserved.
`origin` is `tracked` for text last synchronized to the server, or `disk` for a contained file read.
These snapshots are not atomic with the server's locations.
External paths and symlink escapes disclose no source, regardless of `out_of_workspace`.
Source enrichment does not open LSP documents or use a persistent cache.
`include_source: false` omits `source` and source excerpt reads and copies.

Fixed source limits per request are 100 candidate locations, 16 distinct files,
1 MiB per file, 4 MiB total read/copy allowance, 2 KiB per excerpt, and 64 KiB total excerpt text.
Each selected file is read or copied once, including duplicate references.
Failed reads and lookahead consume the allowance. A two-second best-effort deadline
stops further enrichment; an uninterruptible OS call can exceed it.
When a full window exceeds the remaining text allowance, the reference line alone
is returned with `shortened: true`. An oversized reference line is unavailable.
Failures and exhausted limits preserve locations with explicit reasons:
`candidate_limit`, `file_limit`, `source_budget`, `text_budget`, `deadline`,
`access_denied`, `read_failed`, `file_too_large`, `invalid_utf8`, or `line_unavailable`.

Only an originally empty server response carries `index_coverage`.
A filter-empty result from a nonempty response does not carry this note.
`synced_documents` counts documents currently synced to the server process;
`server_restarted` stays true after a restart or respawn.
Servers can index only opened documents, so an empty answer is not proof that a symbol is unused.

## get_document_symbols

Returns the outline of one file as a hierarchy of symbols (types, functions, fields) with their ranges.

```json
{ "file_path": "/work/app/src/models.rs" }
```

Use it to understand a file before reading it, or to find the exact name for a `symbol_name` argument. Each symbol has a numeric `kind` such as `5` (class or struct), `6` (method), `8` (field), `11` (interface or trait) and `12` (function).

## workspace_symbol_search

Searches symbol names across the whole workspace, with partial and fuzzy matching. It needs no file, so it is the way in when you only know a name.

Arguments: `query` (required), `limit` (default 100, capped by the server) and `kind_filter` (a kind name such as `function` or `class`, or the numeric LSP kind).

```json
{ "query": "SessionManager", "kind_filter": "struct" }
```

This tool has no document to route on. mcpls sends it to the first server that explicitly claims `workspace_symbols` in `handles`, otherwise to the first catch-all server.

When the server itself answers with no symbols, the result carries the same `index_coverage` note as `get_references`. It is not added when `limit` or `kind_filter` emptied a non-empty server answer.

## get_document_highlights

Lists the occurrences of the symbol at a position inside the same file, each marked `read`, `write` or `text`. It is the file-local subset of `get_references` and is faster.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 42, "character": 9 }
```

## get_signature_help

Returns the signatures and parameter documentation for the call at a position, with the active signature and parameter. Position it inside the parentheses of a call.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 57, "character": 31 }
```

## get_completions

Returns API-discovery completion candidates at a position. Add `trigger` (for example `.`, `:` or `->`) to mimic typing a trigger character.

```json
{ "file_path": "/work/app/src/billing.rs", "line": 57, "character": 18, "trigger": "." }
```

Each item has a `label`, a numeric `kind` (2 method, 3 function, 5 field, 6 variable, 7 class, 9 module), a `detail`, and optional `documentation`. Results do not include insertion text, snippets, or completion edit payloads. Completion requests are capped at 10 seconds whatever `request_timeout_seconds` says.

## get_inlay_hints

Returns the inferred type and parameter-name annotations an editor would draw inline, for a range.

```json
{
  "file_path": "/work/app/src/billing.rs",
  "start_line": 40, "start_character": 1,
  "end_line": 60, "end_character": 1
}
```

Use it to see inferred types of `let` bindings without hovering over each one.

## What's Next

Continue with [Diagnostics and Formatting](diagnostics.md) to see what the compiler reports.

## get_document_outline

Returns a compact, flat page of navigation entries. Use `get_document_symbols` when you need complete ranges and children.

Arguments: required absolute `file_path`; optional `query`, `kind_filter`, `offset` and `limit`.
`query` matches a case-sensitive substring of each symbol's own name, up to 1,000 bytes.
`kind_filter` accepts the existing symbol kind names or numeric LSP kinds.
`offset` defaults to 0 after filtering. `limit` defaults to 100 and accepts 1 through 200.

```json
{ "file_path": "/work/app/src/models.rs", "query": "User", "offset": 0, "limit": 100 }
```

```json
{
  "symbols": [{ "name_path": ["models", "User"], "kind": 23, "position": { "line": 10, "character": 8 } }],
  "fidelity": "hierarchical",
  "truncated": false,
  "next_offset": null
}
```

Hierarchical responses use depth-first server order, preserve ancestor names, and navigate to the symbol's selection start.
Flat responses retain server order and the supplied container name as one path segment.
Their `fidelity` is `flat`, and their range-start navigation positions are approximate.
Duplicate names remain separate entries. Positions use 1-based lines and UTF-16 characters.
When `positions_degraded` is present, follow the [position warning](overview.md#reading-results).

Use `next_offset` for the next page when `truncated` is true. Restart pagination after document changes.
The limit bounds returned entries; it does not bound the language server's original response.
This tool returns navigation information and does not apply edits.
