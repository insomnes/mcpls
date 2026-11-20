//! Tool usage through the stdio protocol and its SQLite reporting interface.

#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn stdio_flushes_only_tool_calls_with_validated_scope() {
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
    let mut stdin = String::new();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    let state = root.path().join("state");
    let real_state = root.path().join("local-state");
    fs::create_dir(&real_state).unwrap();
    symlink(real_state, &state).unwrap();
    let run = |usage: Option<&str>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcpls"));
        child
            .args(["--config", config.to_str().unwrap()])
            .current_dir(root.path())
            .env("XDG_STATE_HOME", &state)
            .env_remove("MCPLS_LISTEN")
            .env_remove("MCPLS_USAGE");
        if let Some(value) = usage {
            child.env("MCPLS_USAGE", value);
        }
        child
            .timeout(Duration::from_secs(15))
            .write_stdin(stdin.as_str())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    let db = state.join("mcpls/usage.sqlite");
    run(Some("0"));
    assert!(!db.exists());
    let responses = run(None);
    let responses: Vec<Value> = String::from_utf8(responses)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (id, key) in [
        (1, "result"),
        (2, "result"),
        (3, "result"),
        (4, "error"),
        (5, "error"),
    ] {
        assert!(
            responses
                .iter()
                .any(|response| response["id"] == id && response.get(key).is_some()),
            "{responses:?}"
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
    assert_eq!(history(&db), expected);
    run(Some("0"));
    assert_eq!(history(&db), expected);
}

fn history(db: &Path) -> Vec<(String, String, String, i64)> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut rows = conn
        .prepare("SELECT workspace, tool, outcome, call_count FROM usage_hourly ORDER BY tool")
        .unwrap();
    rows.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}
