// Independent acceptance fragment. Include unchanged inside mcp::server::tests.
// Setup seam only: Translator::symbol_ref_test_context() -> &str, cfg(test).
// All behavioral assertions consume MCP responses or the external LSP boundary.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "independent public-wire compatibility matrix"
)]
async fn compact_targets_independent_known_context_wire() {
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    use crate::test_lsp::{read_framed_message, write_response};

    let mut fixture = support_fixture(
        vec![(
            "rust",
            "rust",
            lsp_types::ServerCapabilities {
                document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
                workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
                hover_provider: Some(lsp_types::HoverProvider::Bool(true)),
                definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
                references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
                call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
                ..Default::default()
            },
        )],
        McpConfig::default(),
    );
    let source = "/*é😀*/fn same() {}\nfn same() {}\n";
    let context = fixture
        .server
        .context
        .translator
        .symbol_ref_test_context()
        .to_owned();
    // Observe the existing context once during fixture setup. Never recover it from v2 output.
    std::fs::write(&fixture.file, source).unwrap();
    let canonical = fixture.file.canonicalize().unwrap();
    let canonical_text = canonical.to_str().unwrap();
    let uri = url::Url::from_file_path(&canonical).unwrap().to_string();
    let source_sha = format!("{:x}", Sha256::digest(source.as_bytes()));
    let binding_input =
        serde_json::to_vec(&json!(["mcpls-symbol-ref-v2", context, canonical_text])).unwrap();
    let binding_sha = format!("{:x}", Sha256::digest(&binding_input));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&requests);
    let fake_uri = uri.clone();
    let mut fake = fixture.fake_servers.pop().unwrap();
    let responder = tokio::spawn(async move {
        let mut wire = tokio::io::BufReader::new(&mut fake.write_stdout);
        loop {
            let request = read_framed_message(&mut wire).await;
            if request.get("id").is_none() {
                continue;
            }
            observed.lock().await.push(request.clone());
            let p = request["params"]["position"].clone();
            let range = json!({"start":p,"end":{"line":p["line"],"character":p["character"].as_u64().unwrap_or(0)+4}});
            let item = json!({"name":"same","kind":12,"uri":fake_uri,"range":range,"selectionRange":range,"data":{"position":p}});
            let result = match request["method"].as_str().unwrap() {
                "textDocument/documentSymbol" => json!([
                    {"name":"same","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":19}},"selectionRange":{"start":{"line":0,"character":10},"end":{"line":0,"character":14}}},
                    {"name":"same","kind":12,"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":12}},"selectionRange":{"start":{"line":1,"character":3},"end":{"line":1,"character":7}}}
                ]),
                "workspace/symbol" => json!([
                    {"name":"same","kind":12,"location":{"uri":fake_uri,"range":{"start":{"line":0,"character":10},"end":{"line":0,"character":14}}}},
                    {"name":"same","kind":12,"location":{"uri":fake_uri,"range":{"start":{"line":1,"character":3},"end":{"line":1,"character":7}}}}
                ]),
                "textDocument/hover" => {
                    json!({"contents":format!("independent:{}:{}",p["line"],p["character"])})
                }
                "textDocument/definition" => json!([{"uri":fake_uri,"range":range}]),
                "textDocument/references" => {
                    let occurrence = json!({"uri":fake_uri,"range":{"start":{"line":1,"character":3},"end":{"line":1,"character":7}}});
                    json!(vec![occurrence; 17])
                }
                "textDocument/prepareCallHierarchy" => json!([item]),
                "callHierarchy/incomingCalls" => {
                    json!([{"from":request["params"]["item"],"fromRanges":[]}])
                }
                "callHierarchy/outgoingCalls" => {
                    json!([{"to":request["params"]["item"],"fromRanges":[]}])
                }
                method => panic!("unexpected external semantic request: {method}"),
            };
            write_response(&mut fake.read_half_stdin, &request["id"], result).await;
        }
    });
    // The flag preserves every old field, source/context option and file summary.
    for mode in ["locations", "files"] {
        for enriched in [false, true] {
            let mut baseline = None;
            for enabled in [None, Some(false), Some(true)] {
                let mut arguments = json!({"file_path":fixture.file,"line":1,"character":11,"mode":mode,"limit":17});
                if enriched {
                    arguments["include_source"] = json!(true);
                    arguments["context"] = json!("enclosing_symbol");
                }
                if let Some(enabled) = enabled {
                    arguments["include_targets"] = json!(enabled);
                }
                let response = tools_call_over_the_wire(fixture.server.for_new_session(), json!({"name":"get_references","arguments":arguments})).await;
                assert!(response.get("error").is_none() && response["result"]["isError"] != true, "{response}");
                let mut result = response["result"]["structuredContent"].clone();
                if enabled == Some(true) && mode == "locations" {
                    let locations = result["locations"].as_array_mut().unwrap();
                    assert_eq!(locations.len(), 17);
                    for (index, location) in locations.iter_mut().enumerate() {
                        if index < 16 {
                            assert!(location.get("symbol_target").is_some(), "{location}");
                            assert!(location.get("symbol_target_unavailable").is_none());
                        } else {
                            assert_eq!(location["symbol_target_unavailable"]["reason"], "budget_exhausted");
                            assert!(location.get("symbol_target").is_none());
                        }
                        location.as_object_mut().unwrap().remove("symbol_target");
                        location.as_object_mut().unwrap().remove("symbol_target_unavailable");
                    }
                }
                if let Some(expected) = &baseline {
                    assert_eq!(&result, expected, "mode={mode}, targets={enabled:?}, enriched={enriched}");
                } else {
                    baseline = Some(result);
                }
            }
        }
    }
    for invalid in [json!(null), json!("true"), json!(1), json!({})] {
        let before = requests.lock().await.len();
        let response = tools_call_over_the_wire(fixture.server.for_new_session(), json!({"name":"get_references","arguments":{"file_path":fixture.file,"line":1,"character":11,"include_targets":invalid}})).await;
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(response.to_string().contains("failed to deserialize parameters"), "{response}");
        assert_eq!(requests.lock().await.len(), before);
    }
    let consumers = [
        ("get_symbol_context", "textDocument/hover"),
        ("get_hover", "textDocument/hover"),
        ("get_definition", "textDocument/definition"),
        ("get_references", "textDocument/references"),
        ("get_incoming_calls", "textDocument/prepareCallHierarchy"),
        ("get_outgoing_calls", "textDocument/prepareCallHierarchy"),
    ];
    let producers = [
        (
            "get_references",
            json!({"file_path":fixture.file,"line":1,"character":11,"include_targets":true}),
            "/locations/0/symbol_target",
        ),
        (
            "workspace_symbol_search",
            json!({"query":"same","include_targets":true}),
            "/symbols/1/symbol_target",
        ),
        (
            "get_document_outline",
            json!({"file_path":fixture.file,"include_targets":true}),
            "/symbols/1/symbol_target",
        ),
        (
            "get_definition",
            json!({"file_path":fixture.file,"line":2,"character":4,"include_targets":true}),
            "/locations/0/symbol_target",
        ),
        (
            "get_symbol_context",
            json!({"file_path":fixture.file,"line":2,"character":4}),
            "/symbol_target",
        ),
    ];
    for (producer, arguments, pointer) in producers {
        let response = tools_call_over_the_wire(
            fixture.server.for_new_session(),
            json!({"name":producer,"arguments":arguments}),
        )
        .await;
        assert!(response.get("error").is_none(), "{producer}: {response}");
        let result = &response["result"]["structuredContent"];
        if producer == "get_symbol_context" {
            assert!(
                result.get("symbol_ref").is_none()
                    && result.get("symbol_ref_unavailable").is_none(),
                "{result}"
            );
        }
        let target = result.pointer(pointer).expect("producer target").clone();
        assert_eq!(target.as_object().unwrap().len(), 2, "closed envelope");
        assert_eq!(target["file_path"], canonical_text);
        if producer == "get_references" {
            for consumer in ["get_definition", "get_symbol_context"] {
                let before = requests.lock().await.len();
                let mut arguments = json!({"symbol_target": target.clone()});
                if consumer == "get_symbol_context" {
                    arguments["sections"] = json!(["hover"]);
                }
                let reused = tools_call_over_the_wire(
                    fixture.server.for_new_session(),
                    json!({"name":consumer,"arguments":arguments}),
                ).await;
                assert!(reused.get("error").is_none() && reused["result"]["isError"] != true, "{reused}");
                let recorded = requests.lock().await;
                assert!(!recorded[before..].is_empty());
                for request in &recorded[before..] {
                    assert_eq!(request["params"]["position"], json!({"line":1,"character":3}));
                }
            }
        }
        let emitted: Value = serde_json::from_str(target["symbol_ref"].as_str().unwrap()).unwrap();
        assert_eq!(
            emitted,
            json!([2, binding_sha, source_sha, 2, 4]),
            "exact independent v2 binding oracle: {producer}"
        );
        let legacy = json!({"version":1,"context":context,"file":canonical_text,"line":2,"character":4,"sha256":source_sha}).to_string();
        for reference in [target["symbol_ref"].as_str().unwrap(), legacy.as_str()] {
            for (consumer, method) in consumers {
                for portable in [true, false] {
                    if !portable && matches!(consumer, "get_incoming_calls" | "get_outgoing_calls")
                    {
                        continue; // No legacy top-level reference API exists for call hierarchy.
                    }
                    let before = requests.lock().await.len();
                    let mut arguments = if portable {
                        json!({"symbol_target":{"file_path":canonical_text,"symbol_ref":reference}})
                    } else {
                        json!({"file_path":canonical_text,"symbol_ref":reference})
                    };
                    if consumer == "get_symbol_context" {
                        arguments["sections"] = json!(["hover"]);
                    }
                    let response = tools_call_over_the_wire(
                        fixture.server.for_new_session(),
                        json!({"name":consumer,"arguments":arguments}),
                    )
                    .await;
                    assert!(
                        response.get("error").is_none() && response["result"]["isError"] != true,
                        "{consumer}: {response}"
                    );
                    let recorded = requests.lock().await;
                    let semantic: Vec<_> = recorded[before..]
                        .iter()
                        .filter(|r| r["method"] == method)
                        .collect();
                    assert!(
                        !semantic.is_empty(),
                        "required external semantic effect: {consumer}"
                    );
                    for request in semantic {
                        assert_eq!(
                            request["params"]["position"],
                            json!({"line":1,"character":3})
                        );
                        assert_eq!(request["params"]["textDocument"]["uri"], uri);
                    }
                    drop(recorded);
                    if matches!(consumer, "get_hover" | "get_symbol_context") {
                        assert!(
                            response.to_string().contains("independent:1:3"),
                            "{response}"
                        );
                    }
                }
            }
        }
    }
    // Preserve v1 case handling: uppercase SHA is syntactically accepted but differs from current bytes.
    for (changed_key, changed_value, expected_kind) in [
        ("sha256", source_sha.to_uppercase(), "stale_target"),
        (
            "context",
            context.to_uppercase(),
            "symbol_ref_context_mismatch",
        ),
    ] {
        let mut legacy = json!({"version":1,"context":context,"file":canonical_text,"line":2,"character":4,"sha256":source_sha});
        legacy[changed_key] = json!(changed_value);
        for (consumer, _) in consumers {
            let before = requests.lock().await.len();
            let response = tools_call_over_the_wire(fixture.server.for_new_session(), json!({"name":consumer,"arguments":{"symbol_target":{"file_path":canonical_text,"symbol_ref":legacy.to_string()}}})).await;
            assert_eq!(
                response["error"]["data"]["kind"], expected_kind,
                "{consumer}: {response}"
            );
            assert_eq!(
                requests.lock().await.len(),
                before,
                "invalid reuse must not dispatch semantics"
            );
        }
    }
    responder.abort();
}
