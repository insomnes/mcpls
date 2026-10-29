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

Arguments: `file_path`, `new_name`, optional `full` (default `false`), and either `line` with `character` or `symbol_name` (with optional `symbol_kind` and `container`).

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
      "edit_count": 1,
      "analysis_freshness": "unknown",
      "preview": {
        "status": "complete",
        "diff": "--- observed\n+++ proposed\n@@ -10 +10 @@\n-process_data()\n+handle_data()\n"
      }
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

## Observed-source previews

`rename_symbol`, `format_document`, `format_range`, and `get_code_actions` return readable previews by default.
Set `full: true` to also receive exact normalized raw `edits`, including unredacted replacement text.
Default responses omit `edits`. mcpls never writes files or executes action commands.

Every retained document has `uri`, `edit_count`, `preview`, and `analysis_freshness: "unknown"`.
Formatting returns a one-document `changes` array, including successful empty edit sets.
Code actions retain their commands and optional `edit.changes`; command-only actions have no edit preview. Execute such commands outside this MCP interface; mcpls has no command execution API.

`preview.status` is `complete`, `truncated`, or `unavailable`.
Available `diff` text uses unified diff syntax with three context lines and observed/proposed headers.
A `reason` explains unavailable or shortened output. Preview truncation does not drop edits.
Configured secrets are redacted in displayed diffs; `redacted: true` records a changed display.
Raw edits requested with `full: true` retain their exact normalized ranges and replacement text.

When source is available, `preview_source` contains `origin` (`tracked` or `disk`) and a lowercase `sha256` of the exact unredacted observed bytes.
This identifies an observation, not the source analyzed by the server. No freshness check is performed.
A selected text-document edit with a version field also has `supplied_version: {"version": 7}` or `{"version": null}`.
Absent fields and edits from the selected changes map omit `supplied_version`. A supplied version does not establish freshness.

One response shares these limits across all documents and actions: 32 canonical files, 8 MiB of source plus constructed text, and 64 KiB of displayed UTF-8 diff text after redaction.
Source acquisition has a two-second best-effort deadline checked between reads and bounded copies; an in-progress OS operation can overrun.
Diff algorithms run off the async executor with a shared 100 ms deadline; a less compact diff can result.
Aliases reuse one observation while preserving returned URI groups. Sources outside the workspace are withheld.
Oversized eligible tracked sources do not fall back to disk.
Invalid or overlapping edits and unreliable positions suppress the diff while retaining permitted metadata and raw edits.
No additional documents are opened on the language server solely for previews.

## get_code_actions

Returns the quick fixes, refactorings and source actions available for a range, with their edits.

Arguments: `file_path`, the four range fields, and optional `full` (default `false`) and `kind_filter` (`quickfix`, `refactor`, `source`, or a more specific kind, in any case).

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
