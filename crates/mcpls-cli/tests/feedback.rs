//! Feedback regressions through real stdio MCP requests and isolated state.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}

#[allow(
    clippy::unwrap_used,
    reason = "test fixture failures must panic with the underlying error"
)]
impl Client {
    fn new(root: &Path, config: &str) -> Self {
        Self::with_executable(root, config, Path::new(env!("CARGO_BIN_EXE_mcpls")))
    }

    fn with_executable(root: &Path, config: &str, executable: &Path) -> Self {
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, config).unwrap();
        let mut child = Command::new(executable)
            .args(["--config", config_path.to_str().unwrap()])
            .current_dir(root)
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env_remove("MCPLS_CONFIG")
            .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            input,
            output,
            id: 0,
        };
        let initialized = client.request("initialize", &json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"feedback-test","version":"1"}}));
        assert!(initialized.get("result").is_some(), "{initialized}");
        writeln!(
            client.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        client
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                return response;
            }
        }
    }

    fn feedback(&mut self, name: &str, args: &Value) -> Value {
        self.request("tools/call", &json!({"name":name,"arguments":args}))
    }
}

// A valid receipt must identify the launched release, while feedback retains
// the same receiving-service identity and leaves historical caller text intact.
#[cfg(target_os = "linux")]
#[test]
fn runtime_provenance_matches_release_receipt_and_preserves_feedback_history() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let release = root.path().join("release");
    std::fs::create_dir(&release).unwrap();
    let executable = release.join("mcpls");
    std::fs::copy(env!("CARGO_BIN_EXE_mcpls"), &executable).unwrap();
    let output = Command::new("sha256sum").arg(&executable).output().unwrap();
    assert!(output.status.success());
    let digest = String::from_utf8(output.stdout).unwrap()[..64].to_owned();
    let revision = "a".repeat(40);
    let expected = json!({"status":"verified","revision":revision,"executable_sha256":digest});
    std::fs::write(
        release.join("build.json"),
        json!({"revision":revision,"sha256":digest,"features":["transport-http"]}).to_string(),
    )
    .unwrap();
    // The working directory's receipt must have no authority over this release.
    std::fs::write(
        root.path().join("build.json"),
        json!({"revision":"b".repeat(40),"sha256":digest}).to_string(),
    )
    .unwrap();
    let state = root.path().join("state/mcpls");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = state.join("feedback.jsonl");
    let old = b"{\"observation\":\"old\"}\n";
    std::fs::write(&file, old).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut client = Client::with_executable(root.path(), "[workspace]\nroots = []\n", &executable);
    let support = client.feedback("get_tool_support", &json!({}));
    assert_eq!(
        support["result"]["structuredContent"]["runtime_provenance"],
        expected
    );
    let observation = format!("  build_sha={} λ\n", "b".repeat(40));
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"get_hover","observation":observation}),
    ));
    let bytes = std::fs::read(file).unwrap();
    assert!(bytes.starts_with(old));
    let record: Value = serde_json::from_slice(&bytes[old.len()..]).unwrap();
    assert_eq!(record["observation"], observation);
    assert_eq!(record["runtime_provenance"], expected);
    drop(client);
    // A digest mismatch must retain the measured digest and unknown revision.
    std::fs::write(
        release.join("build.json"),
        json!({"revision":revision,"sha256":"0".repeat(64)}).to_string(),
    )
    .unwrap();
    let mut client = Client::with_executable(root.path(), "[workspace]\nroots = []\n", &executable);
    let support = client.feedback("get_tool_support", &json!({}));
    assert_eq!(
        support["result"]["structuredContent"]["runtime_provenance"],
        json!({"status":"unknown","revision":"unknown","executable_sha256":digest})
    );
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn success(response: &Value) {
    assert_eq!(
        response["result"]["structuredContent"],
        json!({"stored":true}),
        "{response}"
    );
}

#[test]
fn feedback_preserves_explicit_text_and_repeated_reports_in_both_profiles() {
    for profile in ["agent", "full"] {
        let root = TempDir::new().unwrap();
        let mut client = Client::new(
            root.path(),
            &format!(
                "[mcp]\ntool_profile = {profile:?}\ntool_prefix = 'test'\n[workspace]\nroots = []\n"
            ),
        );
        let discovered = client.request("tools/list", &json!({}));
        let tool = discovered["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "test_feedback_add")
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["annotations"]["idempotentHint"], false);
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert!(!root.path().join("state/mcpls").exists());
        let args = json!({"operation":" get_hover ","observation":"λ\nsecond line","category":"friction","workaround":" retry "});
        success(&client.feedback("test_feedback_add", &args));
        success(&client.feedback("test_feedback_add", &args));
        let text = std::fs::read_to_string(root.path().join("state/mcpls/feedback.jsonl")).unwrap();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        for record in records {
            for key in ["operation", "observation", "category", "workaround"] {
                assert_eq!(record[key], args[key]);
            }
            assert_eq!(record["version"], env!("CARGO_PKG_VERSION"));
            assert!(record["timestamp"].as_str().unwrap().ends_with('Z'));
            assert_eq!(record.as_object().unwrap().len(), 7);
            let support = client.feedback("test_get_tool_support", &json!({}));
            assert_eq!(
                record["runtime_provenance"],
                support["result"]["structuredContent"]["runtime_provenance"]
            );
        }
    }
}

#[test]
fn invalid_feedback_never_creates_storage() {
    let root = TempDir::new().unwrap();
    let mut client = Client::new(root.path(), "[workspace]\nroots = []\n");
    for args in [
        json!({"operation":" ","observation":"failure"}),
        json!({"operation":"hover","observation":"\n"}),
        json!({"operation":"é".repeat(65),"observation":"failure"}),
        json!({"operation":"hover","observation":"a".repeat(2049)}),
        json!({"operation":"hover","observation":"failure","category":"other"}),
        json!({"operation":"hover","observation":"failure","category":null}),
        json!({"operation":"hover","observation":"failure","workaround":null}),
        json!({"operation":"hover","observation":"failure","workaround":""}),
        json!({"operation":"hover","observation":"failure","workspace_path":"/tmp"}),
        json!({"operation":"hover","observation":"failure","runtime_provenance":{}}),
    ] {
        let response = client.feedback("feedback_add", &args);
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{response}"
        );
        assert!(!root.path().join("state/mcpls").exists());
    }
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"é".repeat(64),"observation":"é".repeat(1024)}),
    ));
}

#[test]
fn separate_processes_append_complete_records_without_losing_successes() {
    let root = TempDir::new().unwrap();
    // Initialize sequentially so the shared fixture config is never rewritten
    // while another process is loading it. Appends run concurrently.
    let clients: Vec<_> = (0..6)
        .map(|_| Client::new(root.path(), "[workspace]\nroots = []\n"))
        .collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(clients.len()));
    let threads: Vec<_> = clients.into_iter().enumerate().map(|(index, mut client)| {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            for report in 0..8 {
                success(&client.feedback("feedback_add", &json!({"operation":format!("{index}:{report}"),"observation":"λ\n".repeat(512)})));
            }
        })
    }).collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let text = std::fs::read_to_string(root.path().join("state/mcpls/feedback.jsonl")).unwrap();
    let operations: std::collections::HashSet<String> = text
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line).unwrap();
            record["operation"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(text.lines().count(), 48);
    assert_eq!(operations.len(), 48);
}

#[cfg(unix)]
#[test]
fn feedback_storage_is_private_and_refuses_unsafe_destinations() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = TempDir::new().unwrap();
    let mut client = Client::new(root.path(), "[workspace]\nroots = []\n");
    let args = json!({"operation":"hover","observation":"failure"});
    success(&client.feedback("feedback_add", &args));
    let dir = root.path().join("state/mcpls");
    let file = dir.join("feedback.jsonl");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let original = std::fs::read(&file).unwrap();
    assert!(
        client
            .feedback("feedback_add", &args)
            .get("error")
            .is_some()
    );
    assert_eq!(std::fs::read(&file).unwrap(), original);
    std::fs::remove_file(&file).unwrap();
    let target = root.path().join("target");
    std::fs::write(&target, "untouched").unwrap();
    symlink(&target, &file).unwrap();
    assert!(
        client
            .feedback("feedback_add", &args)
            .get("error")
            .is_some()
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
}

fn content(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "{response}");
    &response["result"]["structuredContent"]
}

#[test]
fn feedback_lifecycle_preserves_legacy_identity_and_original_bytes() {
    let root = TempDir::new().unwrap();
    let config = "[workspace]\nroots = []\n";
    let mut client = Client::new(root.path(), config);
    let empty = client.feedback("feedback_list", &json!({}));
    assert_eq!(content(&empty)["total_count"], 0);
    assert!(!root.path().join("state/mcpls").exists());
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"legacy", "observation":"same"}),
    ));
    let path = root.path().join("state/mcpls/feedback.jsonl");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, original.repeat(2)).unwrap();
    let before = client.feedback("feedback_list", &json!({"state":"all"}));
    let ids: Vec<_> = content(&before)["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["feedback_id"].clone())
        .collect();
    assert_ne!(ids[0], ids[1]);
    let bytes = std::fs::read(&path).unwrap();
    let resolved = client.feedback("feedback_resolve", &json!({"feedback_id":ids[0], "resolution":" λ fixed ", "task_id":" nonexistent task ", "note_id":"note:missing"}));
    assert_eq!(content(&resolved)["resolution"]["text"], " λ fixed ");
    assert_eq!(
        content(&resolved)["resolution"]["task_id"],
        " nonexistent task "
    );
    let conflict = client.feedback(
        "feedback_resolve",
        &json!({"feedback_id":ids[0], "resolution":"again"}),
    );
    assert_eq!(conflict["error"]["data"]["code"], "feedback_conflict");
    assert_eq!(
        content(&client.feedback("feedback_list", &json!({})))["total_count"],
        1
    );
    drop(client);
    let mut client = Client::new(root.path(), config);
    let after = client.feedback("feedback_list", &json!({"state":"resolved"}));
    assert_eq!(content(&after)["items"][0]["feedback_id"], ids[0]);
    let reopened = client.feedback("feedback_reopen", &json!({"feedback_id":ids[0]}));
    assert_eq!(content(&reopened)["resolution"], Value::Null);
    assert_eq!(
        content(&client.feedback("feedback_list", &json!({})))["total_count"],
        2
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let appended = client.feedback(
        "feedback_add",
        &json!({"operation":"new", "observation":"new"}),
    );
    success(&appended);
    let all = client.feedback("feedback_list", &json!({"state":"all", "offset":1}));
    assert_eq!(content(&all)["items"][0]["feedback_id"], ids[0]);
    assert_eq!(content(&all)["items"][1]["feedback_id"], ids[1]);
}

#[test]
fn feedback_rejected_targets_and_malformed_stores_have_no_effects() {
    let root = TempDir::new().unwrap();
    let mut client = Client::new(root.path(), "[workspace]\nroots = []\n");
    let missing = client.feedback("feedback_reopen", &json!({"feedback_id":"missing"}));
    assert_eq!(missing["error"]["data"]["code"], "feedback_not_found");
    assert!(!root.path().join("state/mcpls").exists());
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"one", "observation":"test"}),
    ));
    let listed = client.feedback("feedback_list", &json!({}));
    let id = &content(&listed)["items"][0]["feedback_id"];
    for args in [
        json!({"feedback_id":id,"resolution":" "}),
        json!({"feedback_id":id,"resolution":"fixed","task_id":null}),
        json!({"feedback_id":id,"resolution":"fixed","project":"/tmp"}),
    ] {
        let response = client.feedback("feedback_resolve", &args);
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{response}"
        );
    }
    assert!(
        !root
            .path()
            .join("state/mcpls/feedback-state.jsonl")
            .exists()
    );
    let path = root.path().join("state/mcpls/feedback.jsonl");
    std::fs::write(&path, b"{\"operation\":").unwrap();
    for (name, args) in [
        ("feedback_list", json!({})),
        (
            "feedback_resolve",
            json!({"feedback_id":id,"resolution":"fixed"}),
        ),
        ("feedback_reopen", json!({"feedback_id":id})),
    ] {
        let result = client.feedback(name, &args);
        assert_eq!(result["error"]["data"]["code"], "feedback_storage_failed");
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"operation\":");
}

#[test]
fn malformed_legacy_report_fields_fail_without_modifying_storage() {
    let root = TempDir::new().unwrap();
    let mut client = Client::new(root.path(), "[workspace]\nroots = []\n");
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"valid", "observation":"report"}),
    ));
    let listed = client.feedback("feedback_list", &json!({}));
    let id = &content(&listed)["items"][0]["feedback_id"];
    let path = root.path().join("state/mcpls/feedback.jsonl");
    let valid: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (field, value) in [
        ("timestamp", json!("not-a-timestamp")),
        ("operation", json!(" ")),
        ("operation", json!("é".repeat(65))),
        ("observation", json!("\n")),
        ("observation", json!("é".repeat(1025))),
        ("workaround", json!(" ")),
        ("workaround", json!("x".repeat(2049))),
        ("extra", json!(true)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        let bytes = format!("{invalid}\n").into_bytes();
        std::fs::write(&path, &bytes).unwrap();
        for (tool, arguments) in [
            ("feedback_list", json!({})),
            (
                "feedback_resolve",
                json!({"feedback_id":id,"resolution":"fixed"}),
            ),
            ("feedback_reopen", json!({"feedback_id":id})),
        ] {
            let result = client.feedback(tool, &arguments);
            assert_eq!(
                result["error"]["data"]["code"], "feedback_storage_failed",
                "{field}: {result}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(
                !root
                    .path()
                    .join("state/mcpls/feedback-state.jsonl")
                    .exists()
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn feedback_lifecycle_retains_runtime_metadata_and_legacy_absence() {
    let root = TempDir::new().unwrap();
    let release = root.path().join("release");
    std::fs::create_dir(&release).unwrap();
    let executable = release.join("mcpls");
    std::fs::copy(env!("CARGO_BIN_EXE_mcpls"), &executable).unwrap();
    let output = Command::new("sha256sum").arg(&executable).output().unwrap();
    assert!(output.status.success());
    let digest = String::from_utf8(output.stdout).unwrap()[..64].to_owned();
    let revision = "a".repeat(40);
    std::fs::write(
        release.join("build.json"),
        json!({"revision":revision,"sha256":digest}).to_string(),
    )
    .unwrap();
    let config = "[workspace]\nroots = []\n";
    let mut client = Client::with_executable(root.path(), config, &executable);
    success(&client.feedback(
        "feedback_add",
        &json!({"operation":"current", "observation":"report"}),
    ));
    let path = root.path().join("state/mcpls/feedback.jsonl");
    let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let expected = json!({"status":"verified","revision":revision,"executable_sha256":digest});
    assert_eq!(record["runtime_provenance"], expected);
    let mut legacy = record.clone();
    legacy.as_object_mut().unwrap().remove("runtime_provenance");
    legacy["operation"] = json!("legacy");
    let original = format!("{legacy}\n{legacy}\n{record}\n").into_bytes();
    std::fs::write(&path, &original).unwrap();
    drop(client);
    let mut client = Client::with_executable(root.path(), config, &executable);
    let listed = client.feedback("feedback_list", &json!({"state":"all"}));
    let items = content(&listed)["items"].as_array().unwrap();
    assert_eq!(items[0]["runtime_provenance"], expected);
    assert!(items[1].get("runtime_provenance").is_none());
    assert!(items[2].get("runtime_provenance").is_none());
    assert_ne!(items[1]["feedback_id"], items[2]["feedback_id"]);
    let id = items[0]["feedback_id"].clone();
    let resolved = client.feedback(
        "feedback_resolve",
        &json!({"feedback_id":id,"resolution":"addressed"}),
    );
    assert_eq!(content(&resolved)["state"], "resolved");
    drop(client);
    let mut client = Client::with_executable(root.path(), config, &executable);
    let listed = client.feedback("feedback_list", &json!({"state":"resolved"}));
    assert_eq!(content(&listed)["items"][0]["runtime_provenance"], expected);
    let reopened = client.feedback("feedback_reopen", &json!({"feedback_id":id}));
    assert_eq!(content(&reopened)["state"], "open");
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one public MCP fixture table covers valid and malformed historical provenance"
)]
fn stored_runtime_provenance_validates_values_without_reauthenticating_history() {
    for (metadata, valid) in [
        (None, true),
        (
            Some(
                json!({"status":"verified","revision":"a".repeat(40),"executable_sha256":"b".repeat(64)}),
            ),
            true,
        ),
        (
            Some(
                json!({"status":"unknown","revision":"unknown","executable_sha256":"b".repeat(64)}),
            ),
            true,
        ),
        (
            Some(json!({"status":"unknown","revision":"unknown","executable_sha256":"unknown"})),
            true,
        ),
        (Some(Value::Null), false),
        (
            Some(
                json!({"status":"verified","revision":"not-a-revision","executable_sha256":"not-a-digest"}),
            ),
            false,
        ),
        (
            Some(
                json!({"status":"unknown","revision":"a".repeat(40),"executable_sha256":"b".repeat(64)}),
            ),
            false,
        ),
        (
            Some(
                json!({"status":"verified","revision":"A".repeat(40),"executable_sha256":"b".repeat(64)}),
            ),
            false,
        ),
        (
            Some(
                json!({"status":"verified","revision":"a".repeat(40),"executable_sha256":"B".repeat(64)}),
            ),
            false,
        ),
        (
            Some(
                json!({"status":"unknown","revision":"unknown","executable_sha256":"b".repeat(63)}),
            ),
            false,
        ),
    ] {
        let root = TempDir::new().unwrap();
        let mut client = Client::new(root.path(), "[workspace]\nroots = []\n");
        success(&client.feedback(
            "feedback_add",
            &json!({"operation":"test", "observation":"historical"}),
        ));
        let listed = client.feedback("feedback_list", &json!({}));
        let id = content(&listed)["items"][0]["feedback_id"].clone();
        let path = root.path().join("state/mcpls/feedback.jsonl");
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if let Some(metadata) = &metadata {
            record["runtime_provenance"] = metadata.clone();
        } else {
            record.as_object_mut().unwrap().remove("runtime_provenance");
        }
        let original = format!("{record}\n").into_bytes();
        std::fs::write(&path, &original).unwrap();
        for (tool, arguments) in [
            ("feedback_list", json!({})),
            (
                "feedback_resolve",
                json!({"feedback_id":id,"resolution":"verified"}),
            ),
            ("feedback_reopen", json!({"feedback_id":id})),
        ] {
            let response = client.feedback(tool, &arguments);
            if valid {
                let result = content(&response);
                if tool == "feedback_list" {
                    assert_eq!(
                        result["items"][0].get("runtime_provenance"),
                        metadata.as_ref()
                    );
                } else {
                    assert_eq!(result["feedback_id"], id);
                    let state = if tool == "feedback_resolve" {
                        "resolved"
                    } else {
                        "open"
                    };
                    assert_eq!(result["state"], state);
                    let current = client.feedback("feedback_list", &json!({"state":state}));
                    assert_eq!(content(&current)["total_count"], 1);
                    assert_eq!(
                        content(&current)["items"][0].get("runtime_provenance"),
                        metadata.as_ref()
                    );
                }
            } else {
                assert_eq!(
                    response["error"]["data"]["code"], "feedback_storage_failed",
                    "{metadata:?}: {response}"
                );
                assert!(
                    !root
                        .path()
                        .join("state/mcpls/feedback-state.jsonl")
                        .exists()
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
}
