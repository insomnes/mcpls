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
