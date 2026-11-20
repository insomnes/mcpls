//! Tool usage through the stdio protocol and its SQLite reporting interface.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn stdio_flushes_only_tool_calls_with_validated_scope() {
    let root = TempDir::new().unwrap();
    let workspace = root.path().join("workspace");
    let nested = workspace.join("nested");
    let file = nested.join("sentinel-file.txt");
    fs::create_dir_all(&nested).unwrap();
    fs::write(&file, "").unwrap();

    let config = root.path().join("mcpls.toml");
    fs::write(
        &config,
        format!(
            "lsp_servers = []\n[workspace]\nroots = {}\n[mcp]\ntool_prefix = \"probe\"\n",
            json!([workspace, nested])
        ),
    )
    .unwrap();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "usage-test", "version": "1"}
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "probe_get_tool_support", "arguments": {"file_path": file}
        }}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "probe_preview_file_move", "arguments": {
                "file_path": file, "new_file_path": nested.join("new.txt")
            }
        }}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "probe_unknown", "arguments": {}
        }}),
    ];
    let state = root.path().join("state");
    let real_state = root.path().join("local-state");
    fs::create_dir(&real_state).unwrap();
    symlink(real_state, &state).unwrap();
    let run = |enabled| run_stdio(root.path(), &config, &state, &requests, enabled);
    let db = state.join("mcpls/usage.sqlite");
    run(false).await.unwrap();
    assert!(!db.exists());
    let responses = run(true).await.unwrap();
    for (response, key) in responses
        .iter()
        .zip(["result", "result", "result", "error", "error"])
    {
        assert!(
            response.get(key).is_some(),
            "request {} expected {key}",
            response["id"]
        );
    }
    let root_name = nested.to_str().unwrap();
    let expected = [
        (
            root_name.into(),
            "get_tool_support".into(),
            "success".into(),
            1,
        ),
        (
            root_name.into(),
            "preview_file_move".into(),
            "protocol_error".into(),
            1,
        ),
        (
            "unattributed".into(),
            "unknown_tool".into(),
            "protocol_error".into(),
            1,
        ),
    ];
    assert_eq!(history(&db).unwrap(), expected);
    run(false).await.unwrap();
    assert_eq!(history(&db).unwrap(), expected);
}

#[tokio::test]
async fn runtime_calls_use_one_distinct_canonical_workspace() {
    let root = TempDir::new().unwrap();
    let workspace = root.path().join("workspace");
    let alias = root.path().join("alias");
    let other = root.path().join("other");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&other).unwrap();
    symlink(&workspace, &alias).unwrap();
    let config = root.path().join("mcpls.toml");
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "usage-test", "version": "1"}
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "get_server_logs", "arguments": {}
        }}),
    ];
    let canonical = workspace.canonicalize().unwrap();
    for (second, expected) in [
        (&alias, canonical.to_str().unwrap()),
        (&other, "unattributed"),
    ] {
        fs::write(
            &config,
            format!(
                "lsp_servers = []\n[workspace]\nroots = {}\n",
                json!([workspace, second])
            ),
        )
        .unwrap();
        let state = tempfile::tempdir_in(root.path()).unwrap();
        let responses = run_stdio(root.path(), &config, state.path(), &requests, true)
            .await
            .unwrap();
        assert!(responses[0].get("result").is_some());
        let result = responses[1].get("result").unwrap();
        assert_ne!(result["isError"], true);
        let db = state.path().join("mcpls/usage.sqlite");
        assert_eq!(
            history(&db).unwrap(),
            [(
                expected.into(),
                "get_server_logs".into(),
                "success".into(),
                1
            )]
        );
    }
}

async fn run_stdio(
    root: &Path,
    config: &Path,
    state: &Path,
    requests: &[Value],
    enabled: bool,
) -> anyhow::Result<Vec<Value>> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcpls"));
    command
        .arg("--config")
        .arg(config)
        .current_dir(root)
        .env("XDG_STATE_HOME", state)
        .env_remove("MCPLS_LISTEN")
        .env_remove("MCPLS_USAGE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if !enabled {
        command.env("MCPLS_USAGE", "0");
    }
    let mut child = command.spawn()?;
    let mut input = child.stdin.take().context("missing fixture stdin")?;
    let output = child.stdout.take().context("missing fixture stdout")?;
    let mut output = BufReader::new(output).lines();
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut responses = Vec::new();
        for request in requests {
            input.write_all(format!("{request}\n").as_bytes()).await?;
            if let Some(id) = request.get("id") {
                loop {
                    let line = output
                        .next_line()
                        .await?
                        .context("MCP fixture closed stdout")?;
                    let response: Value = serde_json::from_str(&line)?;
                    if response.get("id") == Some(id) {
                        responses.push(response);
                        break;
                    }
                }
            }
        }
        drop(input);
        let status = child.wait().await?;
        if !status.success() {
            bail!("MCP fixture exited with {status}");
        }
        Ok(responses)
    })
    .await
    .context("MCP fixture timed out")?
}

fn history(db: &Path) -> rusqlite::Result<Vec<(String, String, String, i64)>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut rows = conn
        .prepare("SELECT workspace, tool, outcome, call_count FROM usage_hourly ORDER BY tool")?;
    let rows = rows.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    rows.collect()
}
