# Local MCP Tool Usage

mcpls records aggregate usage for MCP `tools/call` requests over stdio and
HTTP. Each call that reaches the tool handler counts once, including a
diagnostics batch. Discovery, initialization, and language server traffic do
not contribute to tool usage.

Set `MCPLS_USAGE=0` before starting mcpls to disable collection for that
process. A disabled process leaves existing history intact.

## Database location

mcpls stores `mcpls/usage.sqlite` under the platform user state directory. It
uses the local application data directory when the platform has no state
directory.

| Platform | Typical path |
|----------|--------------|
| Linux | `$XDG_STATE_HOME/mcpls/usage.sqlite`, or `~/.local/state/mcpls/usage.sqlite` by default |
| macOS | `~/Library/Application Support/mcpls/usage.sqlite` |
| Windows | `%LOCALAPPDATA%\mcpls\usage.sqlite` |

The database stores UTC time buckets, canonical workspace roots, unprefixed
tool names, outcomes, counts, and total and maximum handler durations in
microseconds. It stores no tool arguments, per-file paths, responses, or
error messages. Unrecognized names count under `unknown_tool`.

Dispatcher mode uses the selected worktree or approved non-Git workspace
root. Standalone mode uses the most specific configured root containing the
validated path. Symlink aliases contribute to the same canonical root. A
standalone runtime-wide tool can use the sole configured root. Process-wide
calls and calls without a unique validated or selected workspace use
`unattributed`.

The time bucket reflects when the call completed or stopped. Outcomes are
`success` for ordinary results, `tool_error` for results with
`isError: true`, `protocol_error` for outer MCP errors, and `stopped` for
cancellation or transport shutdown before the handler finished.

## Query the history

Open the database with a SQLite client. For example, on Linux with the
default state directory, run `sqlite3 -readonly ~/.local/state/mcpls/usage.sqlite`.

Use `usage_hourly` to inspect the last 24 UTC hours, including the current
hour.

```sql
SELECT datetime(hour_utc, 'unixepoch') AS hour_utc,
       workspace, tool, outcome, call_count, total_duration_us, max_duration_us
FROM usage_hourly
WHERE hour_utc >= (unixepoch() / 3600 - 23) * 3600
ORDER BY hour_utc DESC, workspace, tool, outcome;
```

Use `usage_daily` for daily totals. Each row sums the hourly call counts and
durations and takes the greatest hourly maximum for that day, workspace,
tool, and outcome.

```sql
SELECT date(day_utc, 'unixepoch') AS day_utc,
       workspace, tool, outcome, call_count, total_duration_us, max_duration_us
FROM usage_daily
ORDER BY day_utc DESC, workspace, tool, outcome;
```

## Persistence

mcpls flushes pending counts about every five seconds and attempts a final
flush on clean shutdown. Processes that share the database add their
aggregates in transactions. Storage failures leave tool responses unaffected.
The runtime retries uncommitted updates while it continues to serve.

A crash can lose counts accumulated since the last successful commit,
typically the current flush interval. A storage outage can extend that
window and can prevent the final flush. History remains until you remove the
database. Stop processes that write to it before removing it.
