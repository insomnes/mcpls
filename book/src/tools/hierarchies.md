# Hierarchies and Navigation

In this chapter you learn the tools that follow relationships: implementations, type definitions, declarations, call graphs and type hierarchies. They answer "what implements this", "who calls this" and "what does this extend".

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## Jump tools

These three take a position (and the first two also accept `symbol_name`) and return a list of locations with `uri`, `range` and `out_of_workspace`. All accept `context: "enclosing_symbol"`.

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
