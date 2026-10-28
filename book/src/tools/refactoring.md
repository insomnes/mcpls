# Refactoring

In this chapter you learn the tools that propose changes: renames and code actions. They return edits and never write to disk, so the assistant stays in control of what changes and you stay in control of what is approved.

## Prerequisites

- You know how to [address code](overview.md#addressing-code).

## rename_symbol

Resolves the target once, checks preparation when the rename server advertises it, then returns a read-only rename proposal.
Preparation and rename use the same server, document and position. A server with rename support but explicitly unsupported preparation receives rename directly.
Unknown capability information and operational errors never enable this fallback.

The result has one of two `status` values:

| `status` | Fields and meaning |
|----------|--------------------|
| `proposed` | Queried `position`, `preparation`, and `changes`; successful empty edits remain a proposal |
| `not_renameable` | Queried `position` and optional redacted `server_message`; no changes, dropped counters or preparation fields |

For a proposal, `preparation.status` is `renameable` (with `range` and optional `placeholder`), `default_behavior` (without a range), or `unsupported`.
Both variants include `resolved_symbol` for name-addressed requests and can include `positions_degraded` warnings.
All positions and ranges use 1-based UTF-16 columns. A request warning takes precedence over a response warning.
Preparation rejection stops before rename. Timeout, cancellation, invalid position and other operational errors remain errors.

**Migration:** The standalone MCP `prepare_rename` tool was removed. Call `rename_symbol` once, inspect the result, then apply complete edits through the client.

Arguments: `file_path`, `new_name`, and either `line` with `character` or `symbol_name` (with optional `symbol_kind` and `container`).

```json
{
  "file_path": "/work/app/src/data.rs",
  "symbol_name": "process_data",
  "new_name": "handle_data"
}
```

```json
{
  "status": "proposed",
  "position": { "line": 10, "character": 4 },
  "preparation": { "status": "default_behavior" },
  "changes": [
    {
      "uri": "file:///work/app/src/data.rs",
      "edits": [
        {
          "range": {
            "start": { "line": 10, "character": 4 },
            "end": { "line": 10, "character": 16 }
          },
          "new_text": "handle_data"
        }
      ]
    }
  ]
}
```

Check for `dropped` before applying anything. When some edits were withheld, because they point outside every workspace root, use an edit shape mcpls does not translate, or exceed the per-file cap (`exceeds_item_cap`), the result carries a per-reason count:

```json
{ "status": "proposed", "position": { "line": 10, "character": 4 }, "preparation": { "status": "unsupported" }, "changes": [], "dropped": { "out_of_workspace": 1 } }
```

A non-empty `dropped` means the rename is incomplete even when `changes` is not empty. `dropped` is omitted when nothing was withheld. Do not apply an incomplete proposal. With `positions_degraded: "request"`, do not trust the target; with `"response"`, verify exact edit ranges against the source before applying.

An ambiguous `symbol_name` never produces edits; mcpls returns the candidates instead.

## get_code_actions

Returns the quick fixes, refactorings and source actions available for a range, with their edits.

Arguments: `file_path`, the four range fields, and optional `kind_filter` (`quickfix`, `refactor`, `source`, or a more specific kind, in any case).

```json
{
  "file_path": "/work/app/src/lib.rs",
  "start_line": 10, "start_character": 5,
  "end_line": 10, "end_character": 15,
  "kind_filter": "quickfix"
}
```

Use the range of a diagnostic from `get_diagnostics`, or a range from `get_selection_ranges`. The end line must exist in the file. An action's `edit.dropped`, when present and non-empty, means some of its changes were withheld, as for renames. The list is capped; `truncated: true` means some actions, diagnostics or edits were left out.

## What's Next

Continue with [Hierarchies and Navigation](hierarchies.md) to explore how code connects.
