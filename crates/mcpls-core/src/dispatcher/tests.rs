//! Regression oracles run through real MCP transports, Git, and an external LSP.

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
        .with_dispatcher(dispatcher)
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
    assert_eq!(before.len(), 28);
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
