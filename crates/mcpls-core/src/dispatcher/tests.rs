//! Regression oracles run through real MCP transports, Git, and an external LSP.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::Dispatcher;
use crate::config::ServerConfig;
use crate::mcp::McplsServer;

type Client = RunningService<rmcp::RoleClient, ()>;

struct Fixture {
    _temp: TempDir,
    main: PathBuf,
    linked: PathBuf,
    profile: PathBuf,
    events: PathBuf,
    config: ServerConfig,
}
impl Fixture {
    fn new(max: usize) -> Self {
        let temp = TempDir::new().unwrap();
        let main = temp.path().join("main");
        let linked = temp.path().join("linked tree");
        std::fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        git(&["worktree", "add", "--detach", linked.to_str().unwrap()]);
        for (root, marker) in [(&main, "AUTHORED MAIN"), (&linked, "AUTHORED LINKED")] {
            std::fs::write(root.join("marker"), marker).unwrap();
            std::fs::write(root.join("file.py"), "x = 1\n").unwrap();
        }
        let profile = temp.path().join("profile.toml");
        let events = temp.path().join("events.jsonl");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dispatcher_lsp.py");
        std::fs::write(&profile,format!("[[lsp_servers]]\nlanguage_id='python'\ncommand='/usr/bin/python3'\nargs=[{script:?},{events:?}]\nfile_patterns=['*.py']\n")).unwrap();
        let config:ServerConfig=toml::from_str(&format!("[dispatcher]\nenabled=true\nmax_resident_workspaces={max}\nreconcile_interval_seconds=1\nidle_timeout_seconds=1\n[[dispatcher.repositories]]\npath={main:?}\nprofile={profile:?}\n")).unwrap();
        Self {
            _temp: temp,
            main,
            linked,
            profile,
            events,
            config,
        }
    }
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.events)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    async fn wait_event(&self, event: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if self.events().iter().filter(|e| e["event"] == event).count() >= count {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn server(&self, dispatcher: Arc<Dispatcher>) -> McplsServer {
        self.static_server().with_dispatcher(dispatcher)
    }
    fn static_server(&self) -> McplsServer {
        McplsServer::new(
            Arc::new(crate::bridge::Translator::new()),
            Arc::new(tokio::sync::Mutex::new(
                crate::bridge::NotificationCache::new(),
            )),
            crate::bridge::WorkspaceRoots::default(),
            crate::mcp::SubscriptionRegistry::new(),
            self.config.project_config_status,
            self.config.mcp.clone(),
        )
    }
}
async fn connect(server: McplsServer) -> (Client, tokio::task::JoinHandle<()>) {
    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        if let Ok(server) = server.serve(tokio::io::split(server_io)).await {
            let _ = server.waiting().await;
        }
    });
    let client = ().serve(tokio::io::split(client)).await.unwrap();
    (client, task)
}
async fn call(
    client: &Client,
    name: &str,
    args: Value,
) -> std::result::Result<Value, rmcp::ServiceError> {
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new(name.to_string())
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await?;
    Ok(result
        .structured_content
        .unwrap_or_else(|| json!({"content":result.content,"isError":result.is_error})))
}
fn hover(root: &Path) -> Value {
    json!({"file_path":root.join("file.py"),"line":1,"character":1})
}
fn error_code(error: rmcp::ServiceError, expected: &str) {
    match error {
        rmcp::ServiceError::McpError(error) => assert_eq!(error.data.unwrap()["code"], expected),
        other => panic!("expected {expected}, got {other:?}"),
    }
}

/// A catalog/status request must not start LSP, and identical relative paths must use their own worktree cwd.
#[tokio::test]
async fn lazy_catalog_and_worktree_isolation() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let before = client.peer().list_all_tools().await.unwrap();
    assert_eq!(before.len(), 31);
    let support = call(&client, "get_tool_support", json!({})).await.unwrap();
    assert_eq!(support["basis"], "configured");
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.linked}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["state"], "discovered");
    assert_eq!(fixture.events(), [] as [serde_json::Value; 0]);
    let first = call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    assert!(first.to_string().contains("AUTHORED MAIN"));
    let second = call(&client, "get_hover", hover(&fixture.linked))
        .await
        .unwrap();
    assert!(second.to_string().contains("AUTHORED LINKED"));
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        2
    );
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(client.peer().list_all_tools().await.unwrap()).unwrap()
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Absolute selectors are mandatory for fileless operations and cannot authorize a nested repository.
#[tokio::test]
async fn selectors_and_nested_repository_fail_closed() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    for (name, args) in [
        ("workspace_symbol_search", json!({"query":"x"})),
        ("get_server_logs", json!({})),
        ("get_server_messages", json!({})),
        ("restart_server", json!({"all":true})),
    ] {
        error_code(
            call(&client, name, args).await.unwrap_err(),
            "workspace_required",
        );
    }
    error_code(
        call(
            &client,
            "get_hover",
            json!({"file_path":"file.py","line":1,"character":1}),
        )
        .await
        .unwrap_err(),
        "workspace_invalid",
    );
    error_code(
        call(
            &client,
            "get_tool_support",
            json!({"file_path":fixture.main.join("file.py"),"workspace_path":fixture.linked}),
        )
        .await
        .unwrap_err(),
        "workspace_invalid",
    );
    let nested = fixture.main.join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&nested)
            .arg("init")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(nested.join("file.py"), "x=1\n").unwrap();
    error_code(
        call(&client, "get_hover", hover(&nested))
            .await
            .unwrap_err(),
        "workspace_not_approved",
    );
    assert_eq!(fixture.events(), [] as [serde_json::Value; 0]);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Concurrent first callers share one externally observed server start; canceling one does not cancel the other.
#[tokio::test]
async fn first_start_survives_waiter_cancellation() {
    let fixture = Fixture::new(4);
    std::fs::write(fixture.main.join(".hold-start"), "").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let prototype = fixture.server(dispatcher.clone());
    let (first, first_task) = connect(prototype.for_new_session()).await;
    let (second, second_task) = connect(prototype.for_new_session()).await;
    let peer = first.peer().clone();
    let args = hover(&fixture.main);
    let waiter = tokio::spawn(async move {
        peer.call_tool(
            CallToolRequestParams::new("get_hover")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
    });
    fixture.wait_event("initialize", 1).await;
    let peer = second.peer().clone();
    let args = hover(&fixture.main);
    let survivor = tokio::spawn(async move {
        peer.call_tool(
            CallToolRequestParams::new("get_hover")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
    });
    waiter.abort();
    let _ = waiter.await;
    let status = call(&second, "get_workspace_status", json!({}))
        .await
        .unwrap();
    assert_eq!(status["workspaces"][0]["state"], "starting");
    std::fs::remove_file(fixture.main.join(".hold-start")).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), survivor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_value(result)
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1
    );
    dispatcher.shutdown().await;
    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
    first_task.await.unwrap();
    second_task.await.unwrap();
}

/// An old outline generation cannot page a replacement after a service-profile change.
#[tokio::test]
async fn profile_reload_retires_continuations() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let first = call(
        &client,
        "get_document_outline",
        json!({"file_path":fixture.main.join("file.py"),"limit":1}),
    )
    .await
    .unwrap();
    let generation = first["runtime_generation"].clone();
    let second=call(&client,"get_document_outline",json!({"file_path":fixture.main.join("file.py"),"limit":1,"offset":1,"runtime_generation":generation})).await.unwrap();
    assert!(second.to_string().contains("symbol1"));
    error_code(
        call(
            &client,
            "get_document_outline",
            json!({"file_path":fixture.main.join("file.py"),"offset":1}),
        )
        .await
        .unwrap_err(),
        "stale_continuation",
    );
    let profile = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(
        &fixture.profile,
        format!("{profile}\n# changed service profile\n"),
    )
    .unwrap();
    error_code(call(&client,"get_document_outline",json!({"file_path":fixture.main.join("file.py"),"offset":1,"runtime_generation":generation})).await.unwrap_err(),"stale_continuation");
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1,
        "stale pagination must not start a replacement"
    );
    let fresh = call(
        &client,
        "get_document_outline",
        json!({"file_path":fixture.main.join("file.py")}),
    )
    .await
    .unwrap();
    assert_ne!(fresh["runtime_generation"], generation);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A live diagnostic subscription prevents capacity eviction and releases its pin after unsubscribe.
#[tokio::test]
#[allow(
    deprecated,
    reason = "exercises the supported legacy resource subscription protocol"
)]
async fn diagnostic_subscription_pins_only_its_runtime() {
    let fixture = Fixture::new(1);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let uri = crate::bridge::resources::make_uri(&fixture.main.join("file.py")).unwrap();
    client
        .peer()
        .subscribe(rmcp::model::SubscribeRequestParams::new(uri.clone()))
        .await
        .unwrap();
    error_code(
        call(&client, "get_hover", hover(&fixture.linked))
            .await
            .unwrap_err(),
        "workspace_capacity",
    );
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["subscriptions"], 1);
    client
        .peer()
        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri))
        .await
        .unwrap();
    let value = call(&client, "get_hover", hover(&fixture.linked))
        .await
        .unwrap();
    assert!(value.to_string().contains("AUTHORED LINKED"));
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Profile invalidation cancels a response held by the old child and starts a fresh generation after cleanup.
#[tokio::test]
async fn profile_change_rejects_a_pending_old_result() {
    let fixture = Fixture::new(1);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    std::fs::write(fixture.main.join(".hold-response"), "").unwrap();
    let peer = client.peer().clone();
    let args = hover(&fixture.main);
    let pending = tokio::spawn(async move {
        peer.call_tool(
            CallToolRequestParams::new("get_hover")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
    });
    fixture.wait_event("hover", 2).await;
    let profile = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(&fixture.profile, format!("{profile}\n# reload\n")).unwrap();
    let peer = client.peer().clone();
    let args = hover(&fixture.main);
    let replacement = tokio::spawn(async move {
        peer.call_tool(
            CallToolRequestParams::new("get_hover")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
    });
    let old = tokio::time::timeout(Duration::from_secs(15), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    error_code(old, "workspace_invalidated");
    std::fs::remove_file(fixture.main.join(".hold-response")).unwrap();
    let value = tokio::time::timeout(Duration::from_secs(15), replacement)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_value(value)
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        2
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Two independently initialized HTTP sessions share the same resident language-server process.
#[cfg(feature = "transport-http")]
#[tokio::test]
async fn http_sessions_share_one_worktree_backend() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let server = fixture.server(dispatcher.clone());
    let task = tokio::spawn(crate::transport::run_http(
        server,
        crate::HttpConfig::new(address),
        crate::transport::ShutdownSignal::new(),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let url = format!("http://{address}/mcp");
    let first = ()
        .serve(rmcp::transport::StreamableHttpClientTransport::from_uri(
            url.clone(),
        ))
        .await
        .unwrap();
    let second = ()
        .serve(rmcp::transport::StreamableHttpClientTransport::from_uri(
            url,
        ))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        call(&first, "get_hover", hover(&fixture.main)),
        call(&second, "get_hover", hover(&fixture.main))
    );
    for value in [a.unwrap(), b.unwrap()] {
        assert!(value.to_string().contains("AUTHORED MAIN"));
    }
    let a = call(
        &first,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    let b = call(
        &second,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    let pid = &a["workspaces"][0]["servers"][0]["pid"];
    assert!(pid.as_u64().is_some_and(|pid| pid > 0));
    assert_eq!(pid, &b["workspaces"][0]["servers"][0]["pid"]);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1
    );
    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
    task.abort();
    let _ = task.await;
    dispatcher.shutdown().await;
}

/// Canonical aliases select the same runtime and initialize with its selected roots.
#[tokio::test]
async fn canonical_alias_and_selected_initialize_root() {
    let fixture = Fixture::new(4);
    let alias = fixture.main.with_file_name("alias");
    std::os::unix::fs::symlink(&fixture.main, &alias).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let response = call(&client, "get_hover", hover(&alias)).await.unwrap();
    assert!(response.to_string().contains("AUTHORED MAIN"));
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1
    );
    let initialize = events
        .iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    let expected = url::Url::from_directory_path(&fixture.main)
        .unwrap()
        .to_string();
    assert_eq!(
        initialize["params"]["rootUri"]
            .as_str()
            .unwrap()
            .trim_end_matches('/'),
        expected.trim_end_matches('/')
    );
    assert_eq!(
        initialize["params"]["workspaceFolders"][0]["uri"],
        initialize["params"]["rootUri"]
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A diagnostics-only backend cannot prevent a separately routed hover backend.
#[tokio::test]
async fn unrelated_backend_failure_does_not_block_hover() {
    let fixture = Fixture::new(4);
    let working = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(&fixture.profile, format!("[[lsp_servers]]\nname='diagnostics_backend'\nlanguage_id='python'\ncommand='/missing/diagnostics-backend'\nhandles=['diagnostics']\nfile_patterns=['*.py']\n{working}handles=['hover']\nname='hover_backend'\n")).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    let servers = status["workspaces"][0]["servers"].as_array().unwrap();
    assert_eq!(
        servers
            .iter()
            .find(|server| server["server"] == "diagnostics_backend")
            .unwrap()["state"],
        "not_started"
    );
    assert_eq!(
        servers
            .iter()
            .find(|server| server["server"] == "hover_backend")
            .unwrap()["state"],
        "ready"
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Status observes a confirmed child exit without starting a retry or retaining its PID.
#[tokio::test]
async fn status_reports_reaped_child_failure_once() {
    let mut fixture = Fixture::new(4);
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .idle_timeout_seconds = std::num::NonZeroU64::new(60).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    let pid = fixture
        .events()
        .iter()
        .find(|event| event["event"] == "start")
        .unwrap()["pid"]
        .as_u64()
        .unwrap();
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if status.as_ref().map_or(true, |status| {
                status.rsplit_once(") ").unwrap().1.starts_with('Z')
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let status = call(
            &client,
            "get_workspace_status",
            json!({"workspace_path":fixture.main}),
        )
        .await
        .unwrap();
        let server = &status["workspaces"][0]["servers"][0];
        assert_eq!(server["state"], "failed");
        assert!(server["pid"].is_null());
        assert_eq!(server["start_failures"], 1);
        assert!(server["last_error"].is_string());
        assert!(server["retry_after_ms"].as_u64().unwrap() <= 5000);
    }
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Empty Git markers remain non-Git, while malformed metadata remains a boundary.
#[test]
fn explicit_non_git_empty_marker_and_malformed_boundary() {
    let root = TempDir::new().unwrap();
    let marker = root.path().join(".git");
    std::fs::create_dir(&marker).unwrap();
    assert!(
        super::discovery::inspect(root.path(), true)
            .unwrap()
            .1
            .is_none()
    );
    std::fs::write(marker.join("HEAD"), "malformed").unwrap();
    assert!(super::discovery::inspect(root.path(), true).is_err());
}

/// Every mode/profile keeps its public catalog within the existing byte budget.
#[tokio::test]
async fn dispatcher_catalog_budget_and_static_null_support() {
    for profile in [
        crate::config::ToolProfile::Agent,
        crate::config::ToolProfile::Full,
    ] {
        let mut fixture = Fixture::new(4);
        fixture.config.mcp.tool_profile = profile;
        let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
        let (client, task) = connect(fixture.server(dispatcher.clone())).await;
        assert_eq!(
            call(&client, "get_tool_support", json!({})).await.unwrap(),
            call(&client, "get_tool_support", json!({"file_path":null}))
                .await
                .unwrap()
        );
        let tools = client.peer().list_all_tools().await.unwrap();
        let expected = if profile == crate::config::ToolProfile::Full {
            34
        } else {
            31
        };
        assert_eq!(tools.len(), expected);
        let size = serde_json::to_vec(&tools).unwrap().len();
        assert!(size <= 151_000, "dispatcher catalog uses {size} bytes");
        dispatcher.shutdown().await;
        client.cancel().await.unwrap();
        task.await.unwrap();

        let (client, task) = connect(fixture.static_server()).await;
        let omitted = call(&client, "get_tool_support", json!({})).await.unwrap();
        let explicit_null = call(&client, "get_tool_support", json!({"file_path":null}))
            .await
            .unwrap();
        assert_eq!(omitted, explicit_null);
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}

/// Effective marker filtering and settled fallback apply to lazy demand too.
#[tokio::test]
async fn demand_uses_marker_filtered_and_settled_routes() {
    let fixture = Fixture::new(4);
    let working = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(&fixture.profile, format!("{working}\n{working}name='absent'\n[lsp_servers.heuristics]\nproject_markers=['ABSENT-MARKER']\n")).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let support = call(&client, "get_tool_support", json!({})).await.unwrap();
    assert_eq!(support["basis"], "configured");
    assert_eq!(fixture.events().len(), 0);
    assert!(
        support["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "get_hover" && tool["coverage"] == "unknown")
    );
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();

    std::fs::write(&fixture.profile, format!("{working}\n[[lsp_servers]]\nname='failed_hover'\nlanguage_id='python'\ncommand='/missing/hover-provider'\nhandles=['hover']\nfile_patterns=['*.py']\n")).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(
        &client,
        "get_tool_support",
        json!({"file_path":fixture.main.join("file.py")}),
    )
    .await
    .unwrap_err();
    // The support call settled the explicit failure and started the healthy catch-all.
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Null optional items leave emitted targets usable through public dispatcher calls.
#[tokio::test]
async fn portable_call_targets_treat_null_item_as_absent() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let outline = call(
        &client,
        "get_document_outline",
        json!({"file_path":fixture.main.join("file.py"),"include_targets":true}),
    )
    .await
    .unwrap();
    let target = &outline["symbols"][0]["symbol_target"];
    assert!(target.is_object(), "{outline}");
    for (tool, field) in [("get_incoming_calls", "from"), ("get_outgoing_calls", "to")] {
        let direct = call(&client, tool, json!({"symbol_target":target,"item":null}))
            .await
            .unwrap();
        assert_eq!(direct["status"], "ok", "{tool}: {direct}");
        let item = &direct["calls"][0][field];
        assert_eq!(item["name"], "entry", "{direct}");
        assert_eq!(item["data"]["marker"], "AUTHORED MAIN", "{direct}");
        assert!(item["dispatcher_context"].is_string(), "{direct}");
        let mixed = call(&client, tool, json!({"symbol_target":target,"item":item})).await;
        assert!(mixed.is_err(), "non-null mixed item must reject: {mixed:?}");
        let followed = call(&client, tool, json!({"item":item})).await.unwrap();
        assert_eq!(followed["calls"][0][field]["data"], item["data"]);
    }
    let preparations: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|event| event["method"] == "textDocument/prepareCallHierarchy")
        .collect();
    assert_eq!(preparations.len(), 2, "one direct preparation per consumer");
    for request in preparations {
        assert_eq!(
            request["params"]["position"],
            json!({"line":0,"character":0})
        );
    }
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Only the producing backend's restart invalidates a hierarchy continuation.
#[tokio::test]
async fn hierarchy_context_survives_unrelated_backend_start_and_restart() {
    let fixture = Fixture::new(4);
    let working = std::fs::read_to_string(&fixture.profile).unwrap();
    let go = working
        .replace("language_id='python'", "language_id='go'")
        .replace("*.py", "*.go");
    std::fs::write(&fixture.profile, format!("{working}\n{go}")).unwrap();
    std::fs::write(fixture.main.join("other.go"), "package main\n").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let initial = call(&client, "get_incoming_calls", hover(&fixture.main))
        .await
        .unwrap();
    let item = initial["calls"][0]["from"].clone();
    assert!(item["dispatcher_context"].is_string());
    call(&client, "get_incoming_calls", json!({"item":item}))
        .await
        .unwrap();
    call(
        &client,
        "get_hover",
        json!({"file_path":fixture.main.join("other.go"),"line":1,"character":1}),
    )
    .await
    .unwrap();
    call(&client, "get_incoming_calls", json!({"item":item}))
        .await
        .unwrap();
    call(
        &client,
        "restart_server",
        json!({"workspace_path":fixture.main,"servers":["go"]}),
    )
    .await
    .unwrap();
    call(&client, "get_incoming_calls", json!({"item":item}))
        .await
        .unwrap();
    let mut tampered = item.clone();
    tampered["name"] = json!("forged");
    error_code(
        call(&client, "get_incoming_calls", json!({"item":tampered}))
            .await
            .unwrap_err(),
        "stale_continuation",
    );
    call(
        &client,
        "restart_server",
        json!({"workspace_path":fixture.main,"servers":["python"]}),
    )
    .await
    .unwrap();
    error_code(
        call(&client, "get_incoming_calls", json!({"item":item}))
            .await
            .unwrap_err(),
        "stale_continuation",
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Targeted restart and local cache/log reads never demand an unused missing backend.
#[tokio::test]
async fn targeted_restart_preserves_unrelated_lazy_backend() {
    let fixture = Fixture::new(4);
    let working = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(&fixture.profile,format!("{working}\n[[lsp_servers]]\nlanguage_id='go'\ncommand='/missing/go'\nfile_patterns=['*.go']\n")).unwrap();
    std::fs::write(fixture.main.join("other.go"), "package main\n").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    call(
        &client,
        "restart_server",
        json!({"workspace_path":fixture.main,"servers":["python"]}),
    )
    .await
    .unwrap();
    call(
        &client,
        "get_server_logs",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    call(
        &client,
        "get_server_messages",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    let _ = call(
        &client,
        "get_cached_diagnostics",
        json!({"file_path":fixture.main.join("other.go")}),
    )
    .await;
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    let go = status["workspaces"][0]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["server"] == "go")
        .unwrap();
    assert_eq!(go["state"], "not_started");
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A corrupt nested repository cannot borrow its approved parent's Git authority.
#[tokio::test]
async fn corrupt_nested_git_does_not_fall_back_to_approved_parent() {
    let fixture = Fixture::new(4);
    let nested = fixture.main.join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&nested)
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(nested.join("file.py"), "x=2\n").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    error_code(
        call(&client, "get_hover", hover(&nested))
            .await
            .unwrap_err(),
        "workspace_not_approved",
    );
    std::fs::rename(nested.join(".git/HEAD"), nested.join(".git/HEAD.saved")).unwrap();
    error_code(
        call(&client, "get_hover", hover(&nested))
            .await
            .unwrap_err(),
        "workspace_not_approved",
    );
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Diagnostic resources accept an approved root alias and release its subscription pin.
#[tokio::test]
#[allow(
    deprecated,
    reason = "exercises legacy stateful resource subscriptions"
)]
async fn diagnostic_resources_use_approved_canonical_alias() {
    let fixture = Fixture::new(1);
    let alias = fixture.main.parent().unwrap().join("alias");
    std::os::unix::fs::symlink(&fixture.main, &alias).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&alias)).await.unwrap();
    fixture.wait_event("request", 3).await;
    let uri = crate::bridge::resources::make_uri(&alias.join("file.py")).unwrap();
    let read = client
        .peer()
        .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.clone()))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&read)
            .unwrap()
            .contains("AUTHORED MAIN")
    );
    client
        .peer()
        .subscribe(rmcp::model::SubscribeRequestParams::new(uri.clone()))
        .await
        .unwrap();
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":alias}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["subscriptions"], 1);
    client
        .peer()
        .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri))
        .await
        .unwrap();
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":alias}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["subscriptions"], 0);
    assert!(
        call(&client, "get_hover", hover(&fixture.linked))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED LINKED")
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A failed provider with no healthy fallback is retried only on eligible demand.
#[tokio::test]
async fn lost_route_keeps_demand_only_backoff_and_recovery() {
    let fixture = Fixture::new(4);
    let failure = fixture.main.join(".fail-start");
    std::fs::write(&failure, "").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    error_code(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap_err(),
        "workspace_unavailable",
    );
    error_code(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap_err(),
        "workspace_unavailable",
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        1
    );
    std::fs::remove_file(&failure).unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match call(&client, "get_hover", hover(&fixture.main)).await {
                Ok(value) => break value,
                Err(rmcp::ServiceError::McpError(error)) => {
                    let data = error.data.unwrap();
                    assert_eq!(data["code"], "workspace_unavailable");
                    let retry = data["retry_after_ms"].as_u64().unwrap();
                    tokio::time::sleep(Duration::from_millis(retry.min(1000) + 1)).await;
                }
                Err(other) => panic!("unexpected recovery error: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(recovered.to_string().contains("AUTHORED MAIN"));
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "start")
            .count(),
        2
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Authentic opaque items stay on their producer after an explicit provider recovers.
#[tokio::test]
async fn hierarchy_followups_keep_producer_after_route_recovery() {
    let fixture = Fixture::new(4);
    let working = std::fs::read_to_string(&fixture.profile).unwrap();
    let other_events = fixture.main.parent().unwrap().join("explicit.jsonl");
    let explicit = working.replace(
        &format!("{:?}", fixture.events),
        &format!("{other_events:?}"),
    );
    std::fs::write(
        &fixture.profile,
        format!(
            "{working}\n{explicit}name='explicit'\nhandles=['call_hierarchy','type_hierarchy']\n"
        ),
    )
    .unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    std::fs::write(fixture.main.join(".fail-start"), "").unwrap();
    error_code(
        call(&client, "get_incoming_calls", hover(&fixture.main))
            .await
            .unwrap_err(),
        "workspace_unavailable",
    );
    std::fs::remove_file(fixture.main.join(".fail-start")).unwrap();
    let mut items = Vec::new();
    for (method, field) in [
        ("get_incoming_calls", "from"),
        ("get_outgoing_calls", "to"),
        ("get_supertypes", ""),
        ("get_subtypes", ""),
    ] {
        let result = call(&client, method, hover(&fixture.main)).await.unwrap();
        let item = if field.is_empty() {
            result["items"][0].clone()
        } else {
            result["calls"][0][field].clone()
        };
        assert!(item["dispatcher_context"].is_string(), "{result}");
        items.push((method, item));
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while call(
            &client,
            "get_tool_support",
            json!({"file_path":fixture.main.join("file.py")}),
        )
        .await
        .is_err()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let before = std::fs::read_to_string(&other_events)
        .unwrap_or_default()
        .lines()
        .count();
    for (method, item) in &items {
        call(&client, method, json!({"item":item})).await.unwrap();
    }
    let methods = [
        "callHierarchy/incomingCalls",
        "callHierarchy/outgoingCalls",
        "typeHierarchy/supertypes",
        "typeHierarchy/subtypes",
    ];
    let events: Vec<Value> = std::fs::read_to_string(&other_events)
        .unwrap()
        .lines()
        .skip(before)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !events
            .iter()
            .any(|event| methods.contains(&event["method"].as_str().unwrap_or_default())),
        "opaque item reached recovered provider: {events:?}"
    );
    call(
        &client,
        "restart_server",
        json!({"workspace_path":fixture.main,"servers":["python"]}),
    )
    .await
    .unwrap();
    for (method, item) in items {
        error_code(
            call(&client, method, json!({"item":item}))
                .await
                .unwrap_err(),
            "stale_continuation",
        );
    }
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

async fn wait_activity(client: &Client, active: usize, state: Option<&str>) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = call(client, "get_workspace_status", json!({}))
                .await
                .unwrap();
            let row = &status["workspaces"][0];
            if row["active_requests"] == active
                && state.is_none_or(|state| row["servers"][0]["state"] == state)
            {
                break row.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

/// Cold cancellation cannot add a pin; shared start and overlapping successful callers survive.
#[tokio::test]
#[allow(
    deprecated,
    reason = "covers stateful resources subscription cancellation"
)]
async fn cold_subscribe_cancellation_preserves_survivors_and_committed_pins() {
    for mode in 0..3 {
        let fixture = Fixture::new(4);
        std::fs::write(fixture.main.join(".hold-start"), "").unwrap();
        let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
        let (client, task) = connect(fixture.server(dispatcher.clone())).await;
        let uri = crate::bridge::resources::make_uri(&fixture.main.join("file.py")).unwrap();
        let request = || {
            serde_json::from_value(json!({"method":"resources/subscribe","params":{"uri":uri}}))
                .unwrap()
        };
        let first = client
            .peer()
            .send_cancellable_request(request(), rmcp::service::PeerRequestOptions::no_options())
            .await
            .unwrap();
        fixture.wait_event("initialize", 1).await;
        let second = if mode > 0 {
            Some(
                client
                    .peer()
                    .send_cancellable_request(
                        request(),
                        rmcp::service::PeerRequestOptions::no_options(),
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        if mode > 0 {
            wait_activity(&client, 2, Some("starting")).await;
        }
        first.cancel(None).await.unwrap();
        let second = if mode == 2 {
            second.unwrap().cancel(None).await.unwrap();
            None
        } else {
            second
        };
        let pending = usize::from(mode == 1);
        let held = wait_activity(&client, pending, Some("starting")).await;
        assert_eq!(held["subscriptions"], 0);
        std::fs::remove_file(fixture.main.join(".hold-start")).unwrap();
        if let Some(second) = second {
            second.await_response().await.unwrap();
        } else {
            call(&client, "get_hover", hover(&fixture.main))
                .await
                .unwrap();
        }
        let expected = usize::from(mode == 1);
        let settled = wait_activity(&client, 0, Some("ready")).await;
        assert_eq!(settled["subscriptions"], expected);
        if mode == 1 {
            let duplicate = client
                .peer()
                .send_cancellable_request(
                    request(),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .unwrap();
            duplicate.cancel(None).await.unwrap();
            let status = call(&client, "get_workspace_status", json!({}))
                .await
                .unwrap();
            assert_eq!(status["workspaces"][0]["subscriptions"], 1);
            client
                .peer()
                .unsubscribe(rmcp::model::UnsubscribeRequestParams::new(uri))
                .await
                .unwrap();
            let status = call(&client, "get_workspace_status", json!({}))
                .await
                .unwrap();
            assert_eq!(status["workspaces"][0]["subscriptions"], 0);
        }
        dispatcher.shutdown().await;
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}

fn automatic_fixture(fixture: &mut Fixture) -> PathBuf {
    let zone = fixture.main.parent().unwrap().join("automatic");
    std::fs::create_dir(&zone).unwrap();
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: zone.clone(),
            profile: fixture.profile.clone(),
        });
    zone
}

fn script_directory(path: &Path, marker: &str) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("file.py"), "x = 1\n").unwrap();
    std::fs::write(path.join("marker"), marker).unwrap();
}

/// Automatic status is transient, and markerless files start a shared backend despite heuristics.
#[tokio::test]
async fn automatic_markerless_demand_and_transient_status() {
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    let script = zone.join("script");
    script_directory(&script, "PLAIN SCRIPT");
    let profile = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(
        &fixture.profile,
        format!("{profile}\n[lsp_servers.heuristics]\nproject_markers=['pyproject.toml']\n"),
    )
    .unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let reconciler = dispatcher.spawn_reconciler();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let (second, second_task) = connect(fixture.server(dispatcher.clone())).await;
    for index in 0..5 {
        let directory = zone.join(format!("status-{index}"));
        std::fs::create_dir(&directory).unwrap();
        let status = call(
            &client,
            "get_workspace_status",
            json!({"workspace_path": directory}),
        )
        .await
        .unwrap();
        assert_eq!(status["workspaces"][0]["temporary"], true);
        assert_eq!(status["workspaces"][0]["state"], "discovered");
    }
    let status = call(&client, "get_workspace_status", json!({}))
        .await
        .unwrap();
    assert!(
        status["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["temporary"] == false)
    );
    assert_eq!(fixture.events().len(), 0);
    let (first, other) = tokio::join!(
        call(&client, "get_hover", hover(&script)),
        call(&second, "get_hover", hover(&script))
    );
    assert!(first.unwrap().to_string().contains("PLAIN SCRIPT"));
    assert!(other.unwrap().to_string().contains("PLAIN SCRIPT"));
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "initialize")
            .count(),
        1
    );
    let outline = call(
        &client,
        "get_document_outline",
        json!({"file_path":script.join("file.py"),"limit":1}),
    )
    .await
    .unwrap();
    let generation = outline["runtime_generation"].clone();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = call(&client, "get_workspace_status", json!({}))
                .await
                .unwrap();
            if status["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["temporary"] == false)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        call(&client, "get_hover", hover(&script))
            .await
            .unwrap()
            .to_string()
            .contains("PLAIN SCRIPT")
    );
    error_code(call(&client, "get_document_outline", json!({"file_path":script.join("file.py"),"limit":1,"offset":1,"runtime_generation":generation})).await.unwrap_err(), "stale_continuation");
    dispatcher.shutdown().await;
    reconciler.await.unwrap();
    client.cancel().await.unwrap();
    second.cancel().await.unwrap();
    task.await.unwrap();
    second_task.await.unwrap();
}

/// Only ancestors contribute markers; overlapping parents select their most specific profile.
#[tokio::test]
async fn automatic_ancestor_roots_and_policy_priority() {
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    let root = zone.join("project");
    let nested = root.join("nested");
    script_directory(&root, "PROJECT ROOT");
    script_directory(&nested, "NESTED ROOT");
    std::fs::write(
        root.join("pyproject.toml"),
        "invalid manifest is still a marker",
    )
    .unwrap();
    let profile = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(
        &fixture.profile,
        format!("{profile}\n[lsp_servers.heuristics]\nproject_markers=['pyproject.toml']\n"),
    )
    .unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    assert!(
        call(&client, "get_hover", hover(&nested))
            .await
            .unwrap()
            .to_string()
            .contains("PROJECT ROOT")
    );
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":nested}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["workspace_path"], json!(root));
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();

    // A sibling marker cannot pull the selected root sideways or upward.
    std::fs::remove_file(root.join("pyproject.toml")).unwrap();
    std::fs::create_dir(root.join("sibling")).unwrap();
    std::fs::write(root.join("sibling/pyproject.toml"), "").unwrap();
    let specific_profile = fixture.profile.with_file_name("specific.toml");
    std::fs::write(&specific_profile, std::fs::read(&fixture.profile).unwrap()).unwrap();
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: root.clone(),
            profile: specific_profile.clone(),
        });

    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    assert!(
        call(&client, "get_hover", hover(&nested))
            .await
            .unwrap()
            .to_string()
            .contains("NESTED ROOT")
    );
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":nested}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["profile"], json!(specific_profile));

    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Automatic profiles cannot reduce the selected root or inject explicit Rust projects.
#[tokio::test]
async fn automatic_profile_restrictions_and_duplicate_parents() {
    for extra in [
        "[workspace]\nroots=['sub']\n",
        "[[lsp_servers]]\nlanguage_id='rust'\ncommand='rust-analyzer'\ninitialization_options={linkedProjects=[]}\n",
        "[[lsp_servers]]\nlanguage_id='rust'\ncommand='rust-analyzer'\ninitialization_options={linkedProjects={}}\n",
    ] {
        let mut fixture = Fixture::new(4);
        automatic_fixture(&mut fixture);
        // Only automatic approval is needed for this startup validation.
        fixture
            .config
            .dispatcher
            .as_mut()
            .unwrap()
            .repositories
            .clear();
        std::fs::write(&fixture.profile, extra).unwrap();
        let error = Dispatcher::new(fixture.config.clone()).err().unwrap();
        assert_eq!(error.wire().data.unwrap()["code"], "profile_invalid");
    }
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: zone.join("."),
            profile: fixture.profile.clone(),
        });
    assert_eq!(
        Dispatcher::new(fixture.config.clone())
            .err()
            .unwrap()
            .wire()
            .data
            .unwrap()["code"],
        "profile_invalid"
    );
}

/// Explicit roots and profile errors cannot silently select an overlapping automatic policy.
#[tokio::test]
async fn automatic_preserves_explicit_priority() {
    let mut fixture = Fixture::new(4);
    let automatic_profile = fixture.profile.with_file_name("auto.toml");
    std::fs::copy(&fixture.profile, &automatic_profile).unwrap();
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: fixture.main.parent().unwrap().to_path_buf(),
            profile: automatic_profile,
        });
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap()
            .to_string()
            .contains("AUTHORED MAIN")
    );
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.main}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["temporary"], false);
    assert_eq!(status["workspaces"][0]["profile"], json!(fixture.profile));
    std::fs::write(&fixture.profile, "not valid TOML").unwrap();
    error_code(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .unwrap_err(),
        "profile_invalid",
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| event["event"] == "initialize")
            .count(),
        1
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Canonical targets and the captured automatic parent identity bound source access.
#[cfg(unix)]
#[tokio::test]
async fn automatic_symlink_escape_and_parent_replacement_fail_closed() {
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    let outside = zone.with_file_name("outside");
    script_directory(&outside, "OUTSIDE");
    let alias = zone.join("alias");
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    error_code(
        call(&client, "get_hover", hover(&alias)).await.unwrap_err(),
        "workspace_not_approved",
    );
    let previous = zone.with_file_name("previous");
    std::fs::rename(&zone, &previous).unwrap();
    script_directory(&zone, "REPLACEMENT");
    error_code(
        call(&client, "get_hover", hover(&zone)).await.unwrap_err(),
        "workspace_not_approved",
    );
    assert_eq!(fixture.events().len(), 0);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A registered explicit linked checkout cannot acquire an automatic profile after losing .git.
#[tokio::test]
async fn automatic_does_not_replace_registered_explicit_linked_checkout() {
    let mut fixture = Fixture::new(4);
    let automatic_profile = fixture.profile.with_file_name("automatic.toml");
    let automatic = std::fs::read_to_string(&fixture.profile).unwrap();
    std::fs::write(
        &automatic_profile,
        format!("{automatic}\n[lsp_servers.heuristics]\nproject_markers=['pyproject.toml']\n"),
    )
    .unwrap();
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: fixture.linked.parent().unwrap().to_path_buf(),
            profile: automatic_profile,
        });
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let status = call(
        &client,
        "get_workspace_status",
        json!({"workspace_path":fixture.linked}),
    )
    .await
    .unwrap();
    assert_eq!(status["workspaces"][0]["temporary"], false);
    assert_eq!(status["workspaces"][0]["profile"], json!(fixture.profile));
    std::fs::rename(
        fixture.linked.join(".git"),
        fixture.linked.join(".git-removed"),
    )
    .unwrap();
    for name in ["get_workspace_status", "get_hover"] {
        let args = if name == "get_hover" {
            hover(&fixture.linked)
        } else {
            json!({"workspace_path":fixture.linked})
        };
        error_code(
            call(&client, name, args).await.unwrap_err(),
            "workspace_not_approved",
        );
    }
    assert_eq!(fixture.events().len(), 0);
    // A known explicit source remains closed if its Git registry also disappears,
    // even when automatic markers would select an ancestor of that source root.
    let parent_marker = fixture.linked.parent().unwrap().join("pyproject.toml");
    std::fs::write(&parent_marker, "").unwrap();
    std::fs::rename(fixture.main.join(".git"), fixture.main.join(".git-removed")).unwrap();
    error_code(
        call(&client, "get_hover", hover(&fixture.linked))
            .await
            .unwrap_err(),
        "workspace_not_approved",
    );
    std::fs::rename(fixture.main.join(".git-removed"), fixture.main.join(".git")).unwrap();
    std::fs::remove_file(parent_marker).unwrap();
    assert_eq!(fixture.events().len(), 0);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();

    // The Git registry also protects the linked root before any request observes it.
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let reconciler = dispatcher.spawn_reconciler();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    // Reconciliation discovers the main checkout; it must skip the invalid linked entry.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = call(&client, "get_workspace_status", json!({}))
                .await
                .unwrap();
            let entries = status["workspaces"].as_array().unwrap();
            if !entries.is_empty() {
                assert!(entries.iter().all(|entry| entry["temporary"] == false));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    error_code(
        call(&client, "get_hover", hover(&fixture.linked))
            .await
            .unwrap_err(),
        "workspace_not_approved",
    );
    assert_eq!(fixture.events().len(), 0);
    dispatcher.shutdown().await;
    reconciler.await.unwrap();
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Allowlisting a server never permits executables or helpers from any automatic parent.
#[cfg(unix)]
#[tokio::test]
async fn automatic_untrusted_execution_uses_all_parent_boundaries() {
    use std::os::unix::fs::PermissionsExt as _;
    for other_parent in [false, true] {
        for invocation in ["direct", "wrapper", "argument"] {
            let mut fixture = Fixture::new(4);
            let zone = automatic_fixture(&mut fixture);
            let selected = zone.join("narrow/selected");
            script_directory(&selected, "SELECTED");
            let other = zone.with_file_name("other-zone");
            std::fs::create_dir(&other).unwrap();
            let policies = &mut fixture.config.dispatcher.as_mut().unwrap().auto_workspaces;
            for path in [zone.join("narrow"), other.clone()] {
                policies.push(crate::config::DispatcherEntry {
                    path,
                    profile: fixture.profile.clone(),
                });
            }
            let executable = if other_parent { &other } else { &zone }.join("local-server");
            let effect = fixture.profile.with_file_name("executed");
            std::fs::write(&executable, format!("#!/usr/bin/python3\nfrom pathlib import Path\nPath({effect:?}).write_text('EXECUTED')\nraise SystemExit(99)\n")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            let wrapper = fixture.profile.with_file_name("outside-wrapper");
            let wrapper_effect = fixture.profile.with_file_name("wrapper-executed");
            std::fs::write(&wrapper, format!("#!/usr/bin/python3\nimport os,sys\nfrom pathlib import Path\nPath({wrapper_effect:?}).write_text('WRAPPER')\nos.execv(sys.argv[1], sys.argv[1:])\n")).unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (command, args) = match invocation {
                "direct" => (executable.clone(), Vec::new()),
                "wrapper" => (PathBuf::from("/usr/bin/env"), vec![executable.clone()]),
                _ => (wrapper, vec![executable.clone()]),
            };
            std::fs::write(&fixture.profile, format!("[[lsp_servers]]\nlanguage_id='python'\ncommand={command:?}\nargs={args:?}\nfile_patterns=['*.py']\n")).unwrap();
            fixture.config.workspace_trust =
                crate::config::WorkspaceTrust::untrusted([crate::config::ServerId::from_static(
                    "python",
                )]);
            let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
            let (client, task) = connect(fixture.server(dispatcher.clone())).await;
            let result = call(&client, "get_hover", hover(&selected)).await;
            let text = result.map_or_else(|error| error.to_string(), |value| value.to_string());
            assert!(
                text.contains("inside the workspace"),
                "{invocation}, other={other_parent}: {text}"
            );
            assert!(
                !effect.exists(),
                "{invocation} executed the workspace program"
            );
            assert!(
                !wrapper_effect.exists(),
                "argument backstop did not stop the wrapper"
            );
            assert_eq!(fixture.events().len(), 0);
            dispatcher.shutdown().await;
            client.cancel().await.unwrap();
            task.await.unwrap();
        }
    }
}

/// Untrusted automatic profiles must stay outside every automatic parent, including canonical aliases.
#[cfg(unix)]
#[tokio::test]
async fn automatic_untrusted_profile_provenance_uses_all_parents() {
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    let selected = zone.join("selected");
    script_directory(&selected, "SELECTED");
    let other = zone.with_file_name("other-zone");
    std::fs::create_dir(&other).unwrap();
    fixture
        .config
        .dispatcher
        .as_mut()
        .unwrap()
        .auto_workspaces
        .push(crate::config::DispatcherEntry {
            path: other.clone(),
            profile: fixture.profile.clone(),
        });
    fixture.config.workspace_trust =
        crate::config::WorkspaceTrust::untrusted([crate::config::ServerId::from_static("python")]);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let planted_profile = other.join("planted.toml");
    std::fs::rename(&fixture.profile, &planted_profile).unwrap();
    std::os::unix::fs::symlink(&planted_profile, &fixture.profile).unwrap();
    error_code(
        call(&client, "get_hover", hover(&selected))
            .await
            .unwrap_err(),
        "profile_invalid",
    );
    assert_eq!(fixture.events().len(), 0);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
    // The same canonical provenance rule applies at daemon startup.
    assert_eq!(
        Dispatcher::new(fixture.config.clone())
            .err()
            .unwrap()
            .wire()
            .data
            .unwrap()["code"],
        "profile_invalid"
    );
}

/// Untrusted execution boundaries do not broaden the workspace supplied to an allowed outside LSP.
#[tokio::test]
async fn automatic_untrusted_keeps_selected_analysis_root() {
    let mut fixture = Fixture::new(4);
    let zone = automatic_fixture(&mut fixture);
    let selected = zone.join("selected");
    script_directory(&selected, "SELECTED");
    fixture.config.workspace_trust =
        crate::config::WorkspaceTrust::untrusted([crate::config::ServerId::from_static("python")]);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let support = call(
        &client,
        "get_tool_support",
        json!({"file_path":selected.join("file.py")}),
    )
    .await
    .unwrap();
    assert_eq!(support["basis"], "runtime");
    assert_eq!(support["workspace_path"], json!(selected));
    let events = fixture.events();
    let initialized = events
        .iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    assert_eq!(
        initialized["params"]["rootUri"]
            .as_str()
            .unwrap()
            .trim_end_matches('/'),
        url::Url::from_directory_path(&selected)
            .unwrap()
            .as_str()
            .trim_end_matches('/')
    );
    let cwd = PathBuf::from(initialized["cwd"].as_str().unwrap());
    assert!(!cwd.starts_with(&zone));
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Invalid batch envelopes and last-item authorization failures must not start LSP.
#[tokio::test]
async fn diagnostics_batch_preflight_rejects_before_start() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let file = fixture.main.join("file.py");
    for args in [
        json!({}),
        json!({"file_paths":null}),
        json!({"file_path":null}),
        json!({"file_paths":[]}),
        json!({"file_paths":vec![file.clone();33]}),
        json!({"file_paths":["relative.py"]}),
        json!({"file_paths":[1]}),
        json!({"file_path":file,"file_paths":[file]}),
        json!({"file_paths":[file,fixture.linked.join("file.py")]}),
        json!({"file_paths":[file,fixture.main.with_file_name("denied.py")]}),
        json!({"file_paths":[file],"context":"invalid"}),
    ] {
        assert!(
            call(&client, "get_diagnostics", args.clone())
                .await
                .is_err(),
            "{args}"
        );
    }
    assert_eq!(fixture.events(), [] as [Value; 0]);
    client.cancel().await.unwrap();
    task.await.unwrap();
    dispatcher.shutdown().await;
}

/// Batch errors do not erase ordered, duplicate successful file reports.
#[tokio::test]
async fn diagnostics_batch_preserves_order_duplicates_and_item_errors() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let file = fixture.main.join("file.py");
    let missing = fixture.main.join("missing.py");
    let paths = json!([file, missing, file]);
    let batch = call(&client, "get_diagnostics", json!({"file_paths":paths}))
        .await
        .unwrap();
    assert_eq!(batch["results"].as_array().unwrap().len(), 3);
    for index in 0..3 {
        assert_eq!(batch["results"][index]["input_index"], index);
        assert_eq!(batch["results"][index]["file_path"], paths[index]);
    }
    for index in [0, 2] {
        assert_eq!(batch["results"][index]["status"], "ok", "{batch}");
        assert_eq!(batch["results"][index]["report"]["origin"], "pull");
        assert_eq!(batch["results"][index]["omitted_diagnostics"], 0);
        assert!(
            batch["results"][index]["report"]["diagnostics"]
                .to_string()
                .contains("AUTHORED MAIN")
        );
    }
    assert_eq!(batch["results"][1]["status"], "error");
    assert!(batch["results"][1]["error"]["code"].is_i64());
    assert!(batch["results"][1]["error"]["retryable"].is_boolean());
    assert_eq!(batch["truncated"], false);
    let single = call(&client, "get_diagnostics", json!({"file_path":file}))
        .await
        .unwrap();
    assert!(single.get("results").is_none());
    assert_eq!(single["origin"], "pull");
    client.cancel().await.unwrap();
    task.await.unwrap();
    dispatcher.shutdown().await;
}

/// Public batch output omits complete diagnostics at both item and aggregate limits.
#[tokio::test]
async fn diagnostics_batch_bounds_output_without_rewriting_retained_diagnostics() {
    let fixture = Fixture::new(4);
    let diagnostics:Vec<Value>=(0..120).map(|index| json!({
        "range":{"start":{"line":index*5,"character":0},"end":{"line":index*5,"character":1}},
        "severity":1,"code":format!("E{index}"),"message":format!("problem {index}")
    })).collect();
    std::fs::write(
        fixture.main.join(".diagnostics.json"),
        serde_json::to_vec(&diagnostics).unwrap(),
    )
    .unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let file = fixture.main.join("file.py");
    let batch = call(
        &client,
        "get_diagnostics",
        json!({"file_paths":[file,file,file]}),
    )
    .await
    .unwrap();
    for (index, count) in [100, 100, 56].into_iter().enumerate() {
        let item = &batch["results"][index];
        assert_eq!(item["status"], "ok", "{batch}");
        assert_eq!(
            item["report"]["diagnostics"].as_array().unwrap().len(),
            count
        );
        assert_eq!(item["omitted_diagnostics"], 120 - count);
        assert_eq!(item["truncated"], true);
        assert_eq!(item["report"]["diagnostics"][0]["message"], "problem 0");
        assert_eq!(item["report"]["diagnostics"][0]["code"], "E0");
    }
    assert_eq!(batch["truncated"], true);
    assert!(serde_json::to_vec(&batch).unwrap().len() <= 131_072);
    client.cancel().await.unwrap();
    task.await.unwrap();
    dispatcher.shutdown().await;
}

#[tokio::test]
async fn symbol_context_outgoing_items_continue_within_final_budget() {
    let fixture = Fixture::new(4);
    std::fs::write(fixture.main.join("marker"), "x".repeat(1000)).unwrap();
    std::fs::write(fixture.main.join(".outgoing-count"), "125").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("get_symbol_context").with_arguments(
                json!({"file_path":fixture.main.join("file.py"),"line":1,"character":1,
            "sections":["outgoing_calls"]})
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .unwrap();
    assert!(serde_json::to_vec(&result).unwrap().len() <= 131_072);
    let bundle = result.structured_content.unwrap();
    let calls = bundle["sections"]["outgoing_calls"]["data"]["calls"]
        .as_array()
        .unwrap();
    assert_eq!(bundle["truncated"], true);
    let item = &calls[0]["to"];
    assert!(item["dispatcher_context"].is_string());
    let followed = call(&client, "get_outgoing_calls", json!({"item":item}))
        .await
        .unwrap();
    assert_eq!(followed["calls"][0]["to"]["data"], item["data"]);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.abort();
}

/// Invalid root JSON must fail before launching a backend or advertising coverage.
/// Valid and absent root files still reach the configured executable.
#[tokio::test]
async fn watching_root_json_is_validated_before_backend_launch() {
    for (root_config, launched) in [
        (Some("{ invalid JSON"), false),
        (Some("{}"), true),
        (None, true),
    ] {
        let fixture = Fixture::new(4);
        if let Some(root_config) = root_config {
            std::fs::write(fixture.main.join(".watchmanconfig"), root_config).unwrap();
        }
        let marker = fixture.profile.parent().unwrap().join("backend-started");
        let backend = fixture.profile.parent().unwrap().join("backend");
        std::fs::write(
            &backend,
            format!("#!/bin/sh\nprintf started > {marker:?}\nexit 1\n"),
        )
        .unwrap();
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = std::fs::read_to_string(&fixture.profile).unwrap();
        std::fs::write(
            &fixture.profile,
            format!(
                "{profile}\n[workspace.file_watching]\nbackend='watchman'\ncommand={backend:?}\n"
            ),
        )
        .unwrap();
        let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
        let (client, task) = connect(fixture.server(dispatcher.clone())).await;
        error_code(
            call(
                &client,
                "get_document_outline",
                json!({"file_path":fixture.main.join("file.py")}),
            )
            .await
            .unwrap_err(),
            "workspace_unavailable",
        );
        assert_eq!(
            marker.exists(),
            launched,
            "root configuration: {root_config:?}"
        );
        assert!(
            fixture.events().is_empty(),
            "failed coverage must not start LSP"
        );
        dispatcher.shutdown().await;
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}

/// Caller-held observations round-trip and report exact changed memberships.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one external protocol session checks baseline continuity and cache-only side effects"
)]
async fn diagnostics_delta_round_trip_and_cached_no_sync() {
    let fixture = Fixture::new(4);
    std::fs::write(fixture.main.join(".diagnostics.json"), json!([{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"old problem","severity":1,"source":"fixture","code":"A"}]).to_string()).unwrap();
    let full_items: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.main.join(".diagnostics.json")).unwrap(),
    )
    .unwrap();
    let report_path = fixture.main.join(".diagnostics-report.json");
    std::fs::write(
        &report_path,
        json!({"kind":"full","resultId":"r1","items":full_items}).to_string(),
    )
    .unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    let path = fixture.main.join("file.py");
    for args in [
        json!({"file_path":path,"action":"compare"}),
        json!({"file_path":path,"action":"capture","mode":null}),
        json!({"file_path":path,"action":"capture","extra":1}),
    ] {
        assert!(call(&client, "diagnostics_delta", args).await.is_err());
    }
    assert!(
        fixture.events().is_empty(),
        "rejected requests started an LSP"
    );
    assert!(
        call(
            &client,
            "diagnostics_delta",
            json!({"file_path":path,"action":"capture","mode":"cached"})
        )
        .await
        .is_err()
    );
    assert!(fixture.events().is_empty(), "cached capture started an LSP");
    let first = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"capture"}),
    )
    .await
    .unwrap();
    assert_eq!(first["status"], "baseline_captured");
    let baseline = first["baseline"].clone();
    let same = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(same["remaining"], json!([0]));
    assert_eq!(same["added"], json!([]));
    assert_eq!(same["comparison_complete"], true);
    std::fs::write(
        &report_path,
        json!({"kind":"unchanged","resultId":"r1"}).to_string(),
    )
    .unwrap();
    let unchanged = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(unchanged["remaining"], json!([0]));
    assert_eq!(unchanged["comparison_complete"], true);
    assert_eq!(unchanged["report"]["pull_kind"], "unchanged");
    std::fs::write(
        &report_path,
        json!({"kind":"unchanged","resultId":"unrelated"}).to_string(),
    )
    .unwrap();
    let missing = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(missing["comparison_complete"], false);
    assert_eq!(missing["resolved"], json!([]));
    assert!(
        missing["report"]["uncertainties"]
            .as_array()
            .unwrap()
            .contains(&json!("unchanged_without_report"))
    );
    std::fs::remove_file(&report_path).unwrap();
    let before = fixture.events();
    std::fs::write(&path, "x = 2\n").unwrap();
    let cached = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","mode":"cached","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(cached["remaining"], json!([0]));
    assert_eq!(cached["report"]["source_freshness"], "unknown");
    assert_eq!(
        fixture.events().len(),
        before.len(),
        "cached read synchronized or pulled"
    );
    let changed = json!([{"range":{"start":{"line":10,"character":0},"end":{"line":10,"character":1}},"message":"new problem","severity":2,"source":"fixture","code":"B"}]);
    std::fs::write(fixture.main.join(".diagnostics.json"), changed.to_string()).unwrap();
    let delta = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(delta["resolved"], json!([0]));
    assert_eq!(delta["added"].as_array().unwrap().len(), 1);
    assert_eq!(delta["added"][0]["source"], "fixture");
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// A cancelled unadvertised provider probe cannot qualify report removals.
#[tokio::test]
async fn diagnostics_delta_failed_probe_is_incomplete() {
    let fixture = Fixture::new(4);
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    let path = fixture.main.join("file.py");
    let baseline = loop {
        let captured = call(
            &client,
            "diagnostics_delta",
            json!({"file_path":path,"action":"capture","mode":"cached"}),
        )
        .await
        .unwrap();
        if !captured["baseline"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break captured["baseline"].clone();
        }
        tokio::task::yield_now().await;
    };
    std::fs::write(fixture.main.join(".diagnostics-error.json"), "-32802").unwrap();
    let compared = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(compared["report"]["pull_kind"], "failed");
    assert_eq!(compared["report"]["origin"], "cache_after_failed_pull");
    assert_eq!(compared["comparison_complete"], false);
    assert_eq!(compared["resolved"], json!([]));
    assert!(
        compared["report"]["uncertainties"]
            .as_array()
            .unwrap()
            .contains(&json!("pull_failed"))
    );
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// The first delta refresh recovers a dead backend using ordinary routed recovery.
#[tokio::test]
async fn diagnostics_delta_refresh_recovers_dead_backend() {
    let fixture = Fixture::new(4);
    let mut config: ServerConfig =
        toml::from_str(&std::fs::read_to_string(&fixture.profile).unwrap()).unwrap();
    config.workspace.roots =
        vec![crate::config::ConfiguredRoot::new(fixture.main.clone()).unwrap()];
    let runtime = crate::WorkspaceRuntime::prepare(&config).await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = crate::runtime::spawn_lsp_servers_background(
        runtime
            .applicable_configs
            .into_iter()
            .map(|config| {
                config.with_child_working_dir(crate::lsp::ChildWorkingDir::Fixed(
                    fixture.main.clone(),
                ))
            })
            .collect(),
        runtime.translator.clone(),
        runtime.notification_cache.clone(),
        runtime.subscription_registry.clone(),
        cancel.clone(),
        runtime.workspace_roots.clone(),
        config.workspace.max_concurrent_server_starts,
    );
    let server = McplsServer::new(
        runtime.translator.clone(),
        runtime.notification_cache.clone(),
        runtime.workspace_roots,
        runtime.subscription_registry,
        config.project_config_status,
        config.mcp,
    );
    let (client, task) = connect(server).await;
    let path = fixture.main.join("file.py");
    let first = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match call(
                &client,
                "diagnostics_delta",
                json!({"file_path":path,"action":"capture"}),
            )
            .await
            {
                Ok(observation) => break observation,
                Err(rmcp::ServiceError::McpError(error)) if error.code.0 == -32051 => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected initialization error: {error}"),
            }
        }
    })
    .await
    .expect("static backend initialization did not complete");
    std::fs::write(fixture.main.join(".exit-on-hover"), "").unwrap();
    assert!(
        call(&client, "get_hover", hover(&fixture.main))
            .await
            .is_err()
    );
    let recovered = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"capture"}),
    )
    .await
    .unwrap();
    assert_ne!(
        first["baseline"]["identity"]["server_incarnation"],
        recovered["baseline"]["identity"]["server_incarnation"]
    );
    assert_eq!(
        recovered["baseline"]["report"]["push_notifications_degraded"],
        true
    );
    runtime.translator.shutdown_servers().await;
    cancel.cancel();
    supervisor.await.unwrap();
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// Republishing retained aliases cannot erase evidence of an evicted source.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one public protocol session establishes alias loss, republication and an unrelated complete file"
)]
async fn diagnostics_delta_retains_alias_loss_across_republication() {
    let fixture = Fixture::new(4);
    let path = fixture.main.join("file.py");
    for index in 1..=8 {
        std::os::unix::fs::symlink(&path, fixture.main.join(format!("alias{index}.py"))).unwrap();
    }
    std::fs::write(fixture.main.join(".publish-aliases"), "").unwrap();
    let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
    let (client, task) = connect(fixture.server(dispatcher.clone())).await;
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    let baseline = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let captured = call(
                &client,
                "diagnostics_delta",
                json!({"file_path":path,"action":"capture","mode":"cached"}),
            )
            .await
            .unwrap();
            if captured["baseline"]["diagnostics"]
                .as_array()
                .unwrap()
                .len()
                == 8
            {
                break captured["baseline"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(baseline["report"]["completeness"], "complete");
    let lost = baseline["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .position(|d| d["message"] == "A1")
        .unwrap();
    std::fs::write(&path, "x = 2\n").unwrap();
    call(&client, "get_hover", hover(&fixture.main))
        .await
        .unwrap();
    let current = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let captured = call(
                &client,
                "diagnostics_delta",
                json!({"file_path":path,"action":"capture","mode":"cached"}),
            )
            .await
            .unwrap();
            if captured["baseline"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["message"] == "done")
            {
                break captured;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(current["baseline"]["report"]["completeness"], "incomplete");
    assert_eq!(current["truncated"], true);
    assert!(current["baseline"]["report"]["omitted_diagnostics"].is_null());
    let compared = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":path,"action":"compare","mode":"cached","baseline":baseline}),
    )
    .await
    .unwrap();
    assert_eq!(compared["comparison_complete"], false);
    assert_eq!(compared["resolved"], json!([]));
    assert!(
        compared["uncertain_removed"]
            .as_array()
            .unwrap()
            .contains(&json!(lost))
    );
    let normal = fixture.main.join("normal.py");
    std::fs::write(&normal, "x = 1\n").unwrap();
    call(
        &client,
        "get_hover",
        json!({"file_path":normal,"line":1,"character":1}),
    )
    .await
    .unwrap();
    let clean = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":normal,"action":"capture"}),
    )
    .await
    .unwrap();
    assert_eq!(clean["baseline"]["report"]["completeness"], "complete");
    assert_eq!(clean["truncated"], false);
    let oversized = json!([{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":1,"message":"x".repeat(300_000)}]);
    std::fs::write(
        fixture.main.join(".diagnostics.json"),
        oversized.to_string(),
    )
    .unwrap();
    let clipped = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":normal,"action":"capture"}),
    )
    .await
    .unwrap();
    assert_eq!(clipped["truncated"], true);
    let fresh = json!([{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":1,"message":"fresh complete report"}]);
    std::fs::write(fixture.main.join(".diagnostics.json"), fresh.to_string()).unwrap();
    let replaced = call(
        &client,
        "diagnostics_delta",
        json!({"file_path":normal,"action":"capture"}),
    )
    .await
    .unwrap();
    assert_eq!(replaced["baseline"]["report"]["completeness"], "complete");
    assert_eq!(replaced["truncated"], false);
    dispatcher.shutdown().await;
    client.cancel().await.unwrap();
    task.await.unwrap();
}

/// The identity byte limit permits verbatim reuse at 256 and rejects 257 before capture.
#[tokio::test]
async fn diagnostics_delta_server_identity_byte_boundary() {
    for length in [256, 257] {
        let fixture = Fixture::new(4);
        let mut profile = std::fs::read_to_string(&fixture.profile).unwrap();
        profile.push_str("name='");
        profile.push_str(&"s".repeat(length));
        profile.push_str("'\n");
        std::fs::write(&fixture.profile, profile).unwrap();
        let dispatcher = Dispatcher::new(fixture.config.clone()).unwrap();
        let (client, task) = connect(fixture.server(dispatcher.clone())).await;
        let path = fixture.main.join("file.py");
        let captured = call(
            &client,
            "diagnostics_delta",
            json!({"file_path":path,"action":"capture"}),
        )
        .await;
        if length == 256 {
            let baseline = captured.unwrap()["baseline"].clone();
            assert_eq!(
                baseline["identity"]["server_id"].as_str().unwrap().len(),
                256
            );
            let compared = call(
                &client,
                "diagnostics_delta",
                json!({"file_path":path,"action":"compare","baseline":baseline}),
            )
            .await
            .unwrap();
            assert_eq!(compared["comparison_complete"], true);
        } else {
            error_code(captured.unwrap_err(), "workspace_unavailable");
        }
        call(&client, "get_diagnostics", json!({"file_path":path}))
            .await
            .unwrap();
        dispatcher.shutdown().await;
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}
