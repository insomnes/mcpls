# Hierarchies and Navigation

In this chapter you learn the tools that follow relationships: implementations, type definitions, declarations, call graphs and type hierarchies. They answer "what implements this", "who calls this" and "what does this extend".

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## Jump tools

These three take a position (and the first two also accept `symbol_name`) and return a list of locations with `uri`, `range` and `out_of_workspace`. All accept `context: "enclosing_symbol"`. Set `include_source:true` to attach bounded source (default false); see [Optional navigation source](code-intelligence.md#optional-navigation-source).

| Tool | Answers | Typical use |
|------|---------|-------------|
| `go_to_implementation` | Where is this trait method or interface member implemented? | Find implementors of a trait |
| `go_to_type_definition` | What type does this expression have, and where is it defined? | Jump from a variable to its type, not to its binding |
| `go_to_declaration` | Where is this symbol declared? | C and C++ headers, interface members; servers without the concept may return the definition |

```json
{ "file_path": "/work/app/src/store.rs", "symbol_name": "Storage::save" }
```

An empty result is valid. Lists are capped (`truncated: true`).

## Call hierarchy

`get_incoming_calls` finds callers. `get_outgoing_calls` finds callees. Both accept a direct file target, so no separate preparation request is needed:

```json
{ "file_path": "/work/app/src/data.rs", "line": 12, "character": 8 }
```

Positions are 1-based; `character` counts UTF-16 code units. You can also use a symbol name with optional `symbol_kind` and `container` qualifiers:

```json
{ "file_path": "/work/app/src/data.rs", "symbol_name": "Store::initialize", "symbol_kind": "method" }
```

A name must resolve unambiguously before preparation. Name ambiguity and missing names keep the usual structured [addressing errors](overview.md#addressing-a-symbol-by-name).

Direct queries resolve the target, prepare callable items, and traverse one level only when preparation returns exactly one item with complete, reliable positions.

| `status` | Meaning |
|----------|---------|
| `ok` | Traversal succeeded. `calls` contains callers (`from`) or callees (`to`), with `from_ranges`. Empty `calls` is valid. |
| `no_item` | Complete preparation returned no callable item. The result has `items: []` and no `calls`. |
| `selection_required` | Preparation returned multiple items or was truncated. Choose a returned `items` entry explicitly; no traversal occurred. |
| `preparation_incomplete` | Preparation reported `positions_degraded`, even if empty, multiple or truncated. The result contains `items` and no `calls`; no traversal occurred. |

All direct results include `indexing_in_progress`. When true, empty or partial results may reflect an incomplete index; `no_item` does not prove absence. Name queries include `resolved_symbol`. Lists remain bounded and report `truncated: true` when results were omitted. Degraded preparation takes precedence over selection or absence. Traversal warnings remain visible on successful queries.

To choose a candidate from `selection_required`, pass one `items` entry unchanged to either call tool: `{ "item": <the selected object> }`. To follow another level, pass a returned `from` or `to` item the same way. Preserve every item field, including opaque nested `data`.

Give either `item` or a direct target, never both. Item requests return `calls` without direct-query `status` or `indexing_in_progress`. Items are meaningful only to the server that produced them; both tools share the `call_hierarchy` route. These are separate operations, not an atomic snapshot. An external result marked `out_of_workspace` does not permit a later request outside the configured workspace.

**Migration:** the standalone MCP `prepare_call_hierarchy` tool was removed intentionally. Replace `prepare_call_hierarchy(target)` followed by a call tool with `get_incoming_calls(target)` or `get_outgoing_calls(target)`. Direct queries prepare internally. Use the item form to choose candidates or follow returned relationships. Internal LSP preparation remains available to bridge clients.

## Bounded call graphs

`get_call_graph` follows several levels in one request. Set `direction` to
`incoming` or `outgoing`. Use the same position, name, portable `symbol_target`,
or unchanged hierarchy `item` selectors as the one-level call tools.

```json
{ "file_path": "/work/app/src/data.rs", "symbol_name": "Store::initialize", "direction": "outgoing", "depth": 2, "max_nodes": 30 }
```

`depth` defaults to 2 and accepts 1 through 8. `max_nodes` defaults to 30 and
accepts 1 through 256, including the root. Explicit null limits are invalid.
The tool appears in both catalogs and uses the `call_hierarchy` route.

Preparation uses the same status precedence: degraded preparation returns
`preparation_incomplete`; multiple or truncated candidates return
`selection_required`; complete empty preparation returns `no_item`. These results
have `root:null`, empty `nodes` and `edges`, and bounded `items`. Select an
unchanged reusable candidate explicitly. An oversized root returns
`preparation_incomplete` with a budget reason.

A successful graph has `status:"ok"`, `root:"n0"`, first-seen ordered `nodes`,
and first-seen ordered `edges`. Traversal is sequential and breadth-first. Each
node has a request-local ID, an `item`, minimum hop `depth`, and
`expansion_status`. Every edge points from `caller` to `callee`, including an
incoming traversal. Its `from_ranges` belong to the caller node's `item.uri`.
Repeated identities expand once; repeated directed relationships merge distinct
call sites in first-seen order. Opaque `data` participates in identity.

| `expansion_status` | Meaning |
|---|---|
| `expanded` | The complete returned response was examined. Analyzer completeness remains unknown. |
| `depth_boundary` | The node is at the requested hop limit. No expansion was sent. |
| `excluded_scope` | The node lies outside the root's canonical workspace. Its content was not read or synchronized. |
| `truncated` | A budget interrupted this node's returned relationships. |
| `failed` | The node's expansion or coordinate conversion failed. Other branches may succeed. |
| `unexpanded_budget` | An eligible node did not start because a budget or connection lifetime ended. |

`positions_encoding` and `positions_base` accompany every node and edge.
Normalized coordinates use UTF-16 with base 1. When reliable conversion is
unavailable, original LSP coordinates retain their negotiated encoding and base 0.
Raw items have no normalized-item continuation token and are not expanded.
A degraded preparation returns its entire stopped `items` list in raw coordinates,
with top-level `positions_encoding` and `positions_base:0`. This also applies
when an excluded candidate prevents safe normalization of a mixed list.
External, sibling-workspace, unsupported-URI, and symlink-escape nodes remain
explicit scope boundaries when they fit.

`limits` reports the effective public options and fixed aggregate limits:
512 edges, 2,048 examined call-site ranges, 4,096 examined preparation items and
relationship records, 64 attempted analysis requests, 1 MiB of retained opaque
data, and 256 KiB of final compact structured JSON. Analysis requests include
name resolution, preparation, failures, and retries. Duplicates consume examined
record and range budgets. A relationship keeps all its ranges or is omitted
whole. Reaching an aggregate capacity stops further record processing and
eligible expansion, including when the root alone fills `max_nodes`.

`usage` reports retained nodes and edges, examined ranges and records, attempted
analysis requests, retained opaque-data bytes, exact final `output_bytes`, and
`omitted_relationships`. The latter counts only encountered rejected records;
it does not estimate unseen descendants or unexamined records. Final bytes include
dispatcher authentication metadata and the byte counter itself, after the existing
payload-preserving hierarchy redaction policy. MCP framing and duplicated text
content are excluded. These limits do not bound analyzer internal work or parsing
of the already received LSP frame. Existing transport limits and timeouts apply.

`stop_reasons` and `warnings` preserve observed budget, scope, degradation,
indexing, branch-failure, and connection-replacement signals. `child_truncated`
means bounded admission interrupted a child response. Depth and scope boundaries
alone do not set `truncated`. `truncated:false` never establishes completeness.
A changed connection stops queued expansion with `server_changed`; stale items
are never replayed or authenticated against a replacement.

Each node includes `source_identity`: a SHA-256 observation of already obtained
complete trusted bytes, or `status:"unavailable"`. No source is acquired only
for hashing. Every result states `analysis_freshness:"unknown"` and
`atomic_snapshot:false`. Source identity does not establish analyzer freshness.

## Type hierarchy

`get_supertypes` returns base classes and implemented interfaces. `get_subtypes` returns derived classes and implementors. Both accept an absolute `file_path` with `line` and `character`, or `symbol_name` with optional `symbol_kind` and `container` qualifiers. Positions use 1-based UTF-16 columns.

```json
{ "file_path": "/work/app/model.py", "symbol_name": "Store", "symbol_kind": "class" }
```

Each direct query prepares internally and traverses one level only for one complete item with reliable positions. Results contain `items`, `truncated` (including false), `indexing_in_progress`, and `status`. Name queries also include `resolved_symbol`.

- `ok`: traversal succeeded; empty related items are valid.
- `no_item`: preparation found no items.
- `selection_required`: preparation was truncated or returned several candidates; select one unchanged item explicitly.
- `preparation_incomplete`: preparation positions were degraded. This takes precedence over truncation, multiple candidates, and absence. No traversal occurs.

Preserve position warnings and indexing signals. Empty results while indexing do not prove absence. Give either an item or a file target, never both. Missing names, ambiguous names, invalid targets, unsupported capabilities, and server faults remain errors.

Pass a returned item unchanged as `{ "item": <selected object> }` to either tool for further traversal. Preserve opaque `data`. Item responses keep the legacy shape without direct status, indexing, or resolved-symbol fields. Workspace boundaries still apply to later requests.

The default `agent` profile omits standalone `prepare_type_hierarchy`. Set `[mcp].tool_profile = "full"` for the previous prepare-then-traverse workflow. Direct queries work in both profiles. `get_tool_support` is an optional diagnostic aid for checking server support.

## What's Next

Finish the tool tour with [Server Monitoring and Control](server-control.md).

## Portable symbol targets

Copy an emitted `symbol_target` unchanged into the supported read-only consumers.
See [portable target inputs, producers, limits, and errors](symbol-context.md#copy-a-portable-target).
