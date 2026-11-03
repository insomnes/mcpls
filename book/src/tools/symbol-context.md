# Symbol Context

`get_symbol_context` collects source and selected semantic information for one
primary-file target. It is available in both `agent` and `full` profiles.

```json
{
  "file_path": "/work/app/main.rs",
  "symbol_name": "process",
  "sections": ["source", "hover", "outgoing_calls"]
}
```

Address the target with either `line` and `character`, or `symbol_name` with
optional `symbol_kind` and `container`. Positions are 1-based UTF-16 coordinates.
Names use the existing qualification and ambiguity rules. Unknown fields,
incomplete or mixed addressing, empty sections, duplicate sections and unknown
sections are rejected. There are no budget overrides.

The default sections are `source` and `hover`. Optional sections are
`outgoing_calls`, `diagnostics`, `inlay_hints` and `highlights`. Only requested
sections appear. Each section reports its own outcome:

| Status | Meaning |
|---|---|
| `ok` | Available data fit, with no known section limitation. Analysis can still be incomplete. |
| `empty` | The provider returned no matching information within the known scope. |
| `partial` | Usable data remain, with an explicit limitation or omission. |
| `unsupported` | A required provider capability is absent. |
| `unavailable` | Current state, range fidelity or a limit prevents a reliable result. |
| `error` | The section failed; other sections can remain useful. |

Every limited or failed section has a nonempty `reason`. Limit reasons include
`response_budget`, `item_budget`, `file_budget`, `source_budget` and `text_budget`.
A bounded provider response retains `provider_truncated`. Outer `truncated`
identifies omitted data caused by limits; uncertainty alone does not set it.

The result includes `target.file_path`, `target.position` and
`target.range_fidelity`. A reliable hierarchical declaration range gives
`range_fidelity: "symbol"` and `target.range`. The declaration's selection range
must contain the target point. Flat, malformed, conflicting or degraded symbols
and body-only positions give `range_fidelity: "unknown"`. Name addressing also
retains `target.resolved_symbol`. The tool never follows definitions to guess a
range.

## Source observation

The sole source fragment contains `file_path`, `text`, `start_line`, `origin`,
`sha256`, `completeness` and `redacted`. The fragment preserves original whole
lines, UTF-8, line separators and final-newline state, except configured secret
redaction. Source never contains inserted hints or highlight markers.

A reliable range permits `completeness: "complete_symbol"`. Boundary lines can
include adjacent text. An end at column 1 excludes that final line. A text limit
retains a whole-line prefix only when it includes the target line. Otherwise,
source is unavailable. Without a reliable range, source uses the existing
up-to-five-line window and reports `partial` with `range_unavailable`.

`origin` is `tracked` or `disk`; tracked observations take precedence. `sha256`
identifies the complete unredacted observed file, rather than the excerpt.
`redacted: true` means displayed text differs through secret redaction;
completeness still describes coverage. `source_acquisition_bytes` reports the
existing bounded acquisition charge, including failed-read reservations.
Workspace containment applies to source access, including symlink targets.

## Semantic scope and uncertainty

Hover retains provider contents and ranges. A null hover is `empty`. Outgoing
calls retain direct callees, call-site ranges and advisory external-location
flags; callee source is never acquired. Traversal requires one complete,
nondegraded preparation item. No item is empty; multiple or incomplete items
remain unavailable with `selection_required` or `preparation_incomplete`.

Diagnostics and highlights use intersection with the reliable half-open symbol
range. Zero-length diagnostics belong when their point is inside it. Hint
positions must be inside the range. These sections are unavailable with
`range_unavailable` when scope is unknown. Hints query only that range.
Highlights query the primary position once. Missing highlight kinds remain
`text`, never inferred `read` or `write`.

Diagnostics preserve origin, availability and indexing signals. Pending or
evicted diagnostics are unavailable. Cache results after a failed pull remain
partial with `pull_failed`, including empty lists. Position-degradation and
provider indexing signals remain visible.

Every bundle reports `analysis_freshness: "unknown"` and `atomic_snapshot: false`.
Source and semantic sections can describe different revisions during concurrent
edits. A source hash proves observation identity only. Request position
degradation means the provider may have queried a different symbol; it cannot
produce an unqualified `ok` section.

## Fixed per-call limits

| Quantity | Limit |
|---|---|
| Compact UTF-8 MCP CallToolResult JSON, including text and structured representations | 131072 bytes |
| Displayed source text after redaction | 16384 bytes |
| Returned items across all sections | 256 |
| Distinct participating paths or URIs, including the target | 16 |
| Source files attempted | One primary file |
| Complete observed source file | 1048576 bytes |
| Source acquisition/copy accounting, including lookahead and failed reads | 4194304 bytes |
| Foreground logical semantic operations | 8 |
| Foreground semantic wire attempts, including existing retries | 32 |

Each source fragment, hover object, outgoing call, call-site range, preparation
item, diagnostic, hint and highlight counts once. Metadata does not count.
File identities are lexically normalized; non-file URIs also count.

Admission priority is source, hover, outgoing calls, diagnostics, hints, then
highlights, regardless of request order. Lists retain provider-order prefixes.
Limits can remove data, but preserve the target, requested status records and
uncertainty. Oversized files report `file_too_large` without discarding
independent sections.

At most two document-symbol operations, one hover, one call preparation, one
outgoing query, one diagnostics pull, one hints query and one highlights query
participate. The tool adds no retry loop. These limits exclude initialization,
protocol replies, document synchronization, normal background diagnostics and
existing coordinate-conversion acquisition. They do not bound server computation
or the complete upstream response before existing normalization. Existing finite
document tracking and conversion limits still apply. There is no latency or
atomic-snapshot guarantee.

## Copy a portable target

`workspace_symbol_search`, `get_document_outline`, and `get_definition` accept
`include_targets: true`. It defaults to false. Each retained entry then contains
`symbol_target` or `symbol_target_unavailable.reason`. Workspace symbols and
outline entries put these fields in `symbols[]`; definitions use `locations[]`.
`get_symbol_context` always adds the same envelope when it issues a trustworthy
reference. Its existing descriptive `target` and `symbol_ref` fields remain.

Copy the complete object into any of `get_symbol_context`, `get_hover`,
`get_definition`, `get_references`, `get_incoming_calls`, or `get_outgoing_calls`:

```json
{
  "symbol_target": {
    "file_path": "/work/app/main.rs",
    "symbol_ref": "<unchanged emitted reference>"
  }
}
```

The envelope has exactly two required strings: absolute `file_path` and
`symbol_ref`. Unknown envelope members fail. Omit top-level `file_path`,
`symbol_ref`, `line`, `character`, `symbol_name`, `symbol_kind`, `container`, and
hierarchy `item`. Mixing any non-null legacy selector with an envelope fails,
even when they agree. With a valid `symbol_target`, optional null selectors count
as absent. Non-address
options, including `sections`, `limit`, `include_source`, and
`include_declaration`, retain their meaning. Edit tools, standalone hierarchy
preparation, type hierarchy, implementation and type-definition tools do not
accept envelopes. Unchanged hierarchy items remain valid continuation inputs.

A target binds the returned exact UTF-16 point and observed complete file bytes.
Outline targets use hierarchical selection starts. Workspace symbols and
definitions use their location starts, including destinations in another file.
The envelope does not certify analyzer freshness or an identifier-token boundary.
Equal names never cause reselection. If reuse fails, perform discovery again.

List enrichment adds no source acquisition or semantic operations when disabled.
When enabled, it considers retained entries in result order after existing
filtering and pagination. It attempts at most 16 entries and 16 distinct files.
It acquires at most 4194304 bytes, including lookahead and failed-read reservations,
with at most 1048576 complete source bytes per file. Files share one bounded
observation per call. A two-second best-effort acquisition deadline also applies.
There is no caller budget override. The existing 8192-byte reference limit applies.
Target enrichment adds no semantic lookup. Existing coordinate conversion and
separately requested source or enclosing context work have independent costs.
Enrichment never removes or reorders ordinary results.

List unavailability reasons are:

| Reason | Meaning |
|---|---|
| `budget_exhausted` | The entry or acquisition allowance is exhausted. |
| `approximate_position` | A flat outline lacks an exact selection start. |
| `positions_degraded` | Position conversion cannot establish exact coordinates. |
| `non_file_uri` | The location has no usable local file URI. |
| `source_unverifiable` | Source is inaccessible, outside authorized roots, invalid UTF-8, oversized, changed during acquisition, or unavailable before the acquisition deadline. |
| `position_unverifiable` | The point is outside source or splits a UTF-16 character. |
| `reference_encoding` | The existing reference cannot be encoded. |
| `reference_too_large` | The encoded reference exceeds 8192 UTF-8 bytes. |

Context envelopes mirror the existing reference's unavailability reason, including
`source_not_requested`, `source_unavailable`, `source_changed`, and the shared
position, acquisition, and encoding reasons. They require no additional reads
beyond existing reference issuance. All consumers retain the reference validation,
typed errors, runtime lifecycle, and concurrency limits described below.

## Reuse an observed target

When source is requested, a trustworthy observation adds an opaque `symbol_ref`
to the result. Keep this string unchanged. Pass it with `file_path` to
`get_symbol_context`, `get_hover`, `get_definition`, or `get_references`:

```json
{
  "file_path": "/work/app/main.rs",
  "symbol_ref": "<symbol_ref from get_symbol_context>"
}
```

Use the reference instead of position or name addressing. Do not combine it
with `line`, `character`, `symbol_name`, `symbol_kind`, `container`, or a cursor.
Other supported options keep their meanings. Edit tools do not accept references.
Both tool profiles expose the supported reference consumers.

The reference binds an exact 1-based UTF-16 position, the authorized file and
runtime context, and SHA-256 of the complete unredacted observed source. It
contains no source text. Display redaction does not change content identity.
An unknown declaration range does not prevent issuance at an exact valid point.
Invalid UTF-16 boundaries, degraded positions, unavailable source, or a fresh
source observation that disagrees with the displayed observation prevent issuance.
The result then contains `symbol_ref_unavailable.reason`. When source was omitted,
the reason is `source_not_requested`; other sections retain independent outcomes.

Each consumer reads complete current source before its semantic request. It
checks content independently of file size, timestamps, and tracked-text caches.
Typed JSON-RPC errors have `data.kind` and an explanatory message:

| `data.kind` | Meaning |
|---|---|
| `stale_target` | Current source bytes differ from the observation. |
| `invalid_symbol_ref` | The string is malformed, structurally invalid, unsupported, empty, or longer than 8192 UTF-8 bytes. |
| `symbol_ref_context_mismatch` | The file or runtime context differs. |
| `unverifiable_target` | Complete current UTF-8 source or exact coordinates cannot be established. |

Existing authorization errors still apply. Resolve the target afresh after
rejection. Consumers do not relocate or rebase the reference automatically.

References have no clock expiry or per-reference eviction. They remain valid
while their runtime context and exact source bytes remain available. Restoring
the original bytes permits reuse. Restarting an LSP child preserves references;
replacing or evicting its workspace backend context, or restarting mcpls,
invalidates them. References grant no path access and are not portable between
mcpls processes.

Each complete source observation is bounded to 1 MiB. Issuance performs an
additional fresh contained disk observation to confirm the source section's
identity. A source section's `source_acquisition_bytes` also includes reference
validation and issuance reads. Existing source accounting and response limits
still apply. Read,
encoding, size, or detected acquisition-race failures prevent trustworthy reuse.

Content identity does not prove analyzer freshness. Validation observes bytes
before semantic dispatch; a change after that observation can produce a result
or a typed failure. Collection is not atomic with filesystem writes or LSP
analysis. No guarantee covers undetectable concurrent rewrites or other files.
Existing freshness and position-degradation signals remain authoritative.
