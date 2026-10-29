//! Workspace selection wraps the unchanged per-runtime tool implementations.

use serde_json::{Value, json};

#[cfg(feature = "transport-http")]
use super::start_listen_lease;
use super::{
    Arc, CallToolRequestParams, CallToolResponse, ErrorCode, ListenUris, McpError, McplsServer,
    PathBuf, ResolvedResource, RoleServer, SubscriptionContext, Target, ToolCallContext,
    ToolRouter, parse_client_path, parse_uri,
};
use crate::dispatcher::{DispatchError, Lease};

fn selector(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<PathBuf>, McpError> {
    arguments
        .get(key)
        .map(|value| {
            value.as_str().map(PathBuf::from).ok_or_else(|| {
                McpError::invalid_params(
                    format!("{key} must be an absolute path string"),
                    Some(json!({"code":"workspace_invalid","retryable":false})),
                )
            })
        })
        .transpose()
}

fn completed(value: Value) -> CallToolResponse {
    rmcp::model::CallToolResult::structured(value).into()
}

fn hierarchy(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut)
                && properties.contains_key("selectionRange")
                && properties.contains_key("uri")
            {
                properties.insert("dispatcher_context".into(), json!({"type":"string"}));
            }
            for child in map.values_mut() {
                hierarchy(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                hierarchy(child);
            }
        }
        _ => {}
    }
}

pub(super) fn shape_workspace_schemas(router: &mut ToolRouter<McplsServer>) {
    for route in router.map.values_mut() {
        let name = route.attr.name.as_ref();
        let mut schema = (*route.attr.input_schema).clone();
        if matches!(
            name,
            "workspace_symbol_search"
                | "get_server_logs"
                | "get_server_messages"
                | "restart_server"
                | "get_tool_support"
        ) && let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut)
        {
            properties.insert("workspace_path".into(), json!({"type":"string"}));
        }
        if name == "get_document_outline"
            && let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut)
        {
            properties.insert("runtime_generation".into(), json!({"type":"string"}));
        }
        let mut value = Value::Object(schema);
        hierarchy(&mut value);
        if let Value::Object(schema) = value {
            route.attr.input_schema = Arc::new(schema);
        }
        if let Some(output) = &mut route.attr.output_schema {
            let mut value = Value::Object((**output).clone());
            hierarchy(&mut value);
            if let Value::Object(schema) = value {
                *output = Arc::new(schema);
            }
        }
    }
}

impl McplsServer {
    fn require_dispatcher(&self) -> Result<&Arc<crate::dispatcher::Dispatcher>, McpError> {
        self.dispatcher
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("dispatcher mode is disabled", None))
    }

    fn configured_support(
        &self,
        dispatcher: &crate::dispatcher::Dispatcher,
    ) -> Result<CallToolResponse, McpError> {
        let mut languages = std::collections::BTreeSet::new();
        let mut grouped = std::collections::BTreeMap::<String, Vec<Value>>::new();
        for config in dispatcher.configured_profiles() {
            // Without a selected workspace these are alternatives, not one live router.
            for alternative in &config.lsp_servers {
                let router = crate::config::ToolRouter::from_configs([alternative])
                    .map_err(|e| self.render_error(e))?;
                let translator = crate::bridge::Translator::new()
                    .with_router(router)
                    .with_extensions(config.build_effective_language_map());
                translator.set_expected_servers(std::iter::once(alternative.id()).collect());
                let snapshot = translator.tool_support_snapshot();
                let languages_in_profile = snapshot.languages();
                languages.extend(languages_in_profile.iter().map(ToString::to_string));
                let report = super::ToolSupportReport::build(
                    &snapshot,
                    &languages_in_profile,
                    self.context.mcp.tool_prefix.as_ref(),
                    self.context.mcp.tool_profile,
                );
                let report = serde_json::to_value(report)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                for tool in report
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = tool.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let entries = grouped.entry(name.to_string()).or_default();
                    for route in tool
                        .get("routes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if !entries.contains(route) {
                            entries.push(route.clone());
                        }
                    }
                }
            }
        }
        // An alternative with no claim must not negate another possible route.
        for routes in grouped.values_mut() {
            let mut possible = std::collections::BTreeSet::new();
            let mut workspace_possible = false;
            for route in routes
                .iter()
                .filter(|route| route["status"] == "initializing")
            {
                if let Some(languages) = route["languages"].as_array() {
                    possible.extend(
                        languages
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                } else {
                    workspace_possible = true;
                }
            }
            routes.retain_mut(|route| {
                if route["status"] != "no_server" {
                    return true;
                }
                route["languages"]
                    .as_array_mut()
                    .map_or(!workspace_possible, |languages| {
                        languages.retain(|language| {
                            !language
                                .as_str()
                                .is_some_and(|language| possible.contains(language))
                        });
                        !languages.is_empty()
                    })
            });
        }
        let tools=self.tool_router.list_all().iter().map(|tool| {
            let plain=self.unprefixed(&tool.name);
            let local=plain=="get_workspace_status" || super::McpTool::ALL.iter().find(|kind|kind.name()==plain).is_some_and(|kind|matches!(kind.spec().backend,super::super::tool_support::ToolBackend::Local));
            let routes=grouped.remove(tool.name.as_ref()).unwrap_or_default();
            if local {json!({"name":tool.name,"coverage":"always"})} else {let unknown=routes.iter().any(|route|route.get("status").and_then(Value::as_str)==Some("initializing"));json!({"name":tool.name,"coverage":if unknown {"unknown"} else {"none"},"routes":routes})}
        }).collect::<Vec<_>>();
        Ok(completed(
            json!({"basis":"configured","languages":languages,"tools":tools}),
        ))
    }

    pub(super) async fn dispatch_tool(
        &self,
        mut request: CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if !self.tool_router.has_route(request.name.as_ref()) {
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        let name = self.unprefixed(&request.name).to_string();
        let scoped = matches!(
            name.as_str(),
            "workspace_symbol_search"
                | "get_server_logs"
                | "get_server_messages"
                | "restart_server"
                | "get_tool_support"
        );
        let arguments = request.arguments.get_or_insert_default();
        let workspace = if scoped {
            selector(arguments, "workspace_path")?
        } else {
            None
        };
        let Some(dispatcher) = &self.dispatcher else {
            if scoped {
                arguments.remove("workspace_path");
            }
            if name == "get_document_outline" {
                arguments.remove("runtime_generation");
            }
            if let Some(item) = arguments.get_mut("item").and_then(Value::as_object_mut) {
                item.remove("dispatcher_context");
            }
            if let Some(workspace) = workspace {
                if !workspace.is_dir() {
                    return Err(McpError::invalid_params(
                        "workspace_path must identify an existing directory",
                        Some(json!({"code":"workspace_invalid","retryable":false})),
                    ));
                }
                let client = parse_client_path(workspace)?;
                self.context
                    .workspace_roots
                    .validate(&client)
                    .await
                    .map_err(|e| self.render_error(e))?;
                arguments.remove("workspace_path");
            }
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        };
        let file = if name == "get_tool_support"
            && arguments.get("file_path").is_some_and(Value::is_null)
        {
            None
        } else {
            selector(arguments, "file_path")?
        };
        if name == "get_workspace_status" {
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        if name == "get_tool_support" && file.is_none() && workspace.is_none() {
            return self.configured_support(dispatcher);
        }

        let (lease, origin) = self
            .select_tool_runtime(dispatcher, arguments, file.as_ref(), workspace.as_ref())
            .await?;
        // Identity selection uses canonical paths; the scoped translator must
        // receive that same spelling after approval, including symlink aliases.
        let file = if let Some(file) = file {
            let canonical = tokio::fs::canonicalize(&file)
                .await
                .map_err(|_| DispatchError::invalid(&file).wire())?;
            arguments.insert("file_path".into(), json!(canonical));
            Some(canonical)
        } else {
            None
        };
        let resident = &lease.resident;
        if name == "get_document_outline" {
            let generation = arguments.remove("runtime_generation");
            if generation
                .as_ref()
                .is_some_and(|g| g.as_str() != Some(resident.generation.as_str()))
                || arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) > 0
                    && generation.is_none()
            {
                return Err(DispatchError::stale().wire());
            }
        }
        if scoped {
            arguments.remove("workspace_path");
        }
        self.invoke_tool(lease, origin, file, name, request, context)
            .await
    }

    async fn invoke_tool(
        &self,
        lease: Lease,
        origin: Option<(crate::config::ServerId, String)>,
        file: Option<PathBuf>,
        name: String,
        request: CallToolRequestParams,
        mut context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let dispatcher = self.require_dispatcher()?;
        let resident = &lease.resident;
        dispatcher
            .validate(resident)
            .await
            .map_err(DispatchError::wire)?;
        let local_only = request
            .arguments
            .as_ref()
            .is_some_and(|arguments| arguments.contains_key("item"))
            || matches!(
                name.as_str(),
                "get_server_logs"
                    | "get_server_messages"
                    | "get_cached_diagnostics"
                    | "restart_server"
            );
        let tool = super::McpTool::ALL
            .iter()
            .find(|tool| tool.name() == name)
            .and_then(|tool| match tool.spec().backend {
                super::super::tool_support::ToolBackend::Document(kind)
                | super::super::tool_support::ToolBackend::Workspace(kind) => Some(kind),
                super::super::tool_support::ToolBackend::Local => None,
            });
        resident
            .demand(dispatcher, file.as_deref(), local_only, tool)
            .await
            .map_err(DispatchError::wire)?;
        let server = self.runtime_session(&lease).await?;
        // Keep the token-checked connection through every dispatch await.
        let continuation = origin.is_some();
        let (origin, expected_incarnation) = origin.unzip();
        let origin = if let Some(origin) = origin {
            Some(origin)
        } else if let (Some(file), Some(tool)) = (&file, tool) {
            let path = crate::bridge::ClientPath::try_from(file.clone())
                .map_err(|_| DispatchError::invalid(file).wire())?;
            let language = server
                .context
                .translator
                .language_for_path(&path)
                .await
                .map_err(|error| server.render_error(error))?;
            server
                .context
                .translator
                .router_snapshot()
                .resolve(language.as_ref(), tool)
                .cloned()
        } else {
            None
        };
        let incarnation = expected_incarnation.or_else(|| {
            origin
                .as_ref()
                .and_then(|origin| resident.incarnation(origin))
        });
        if continuation {
            let authenticated = origin
                .as_ref()
                .zip(incarnation.as_ref())
                .and_then(|(origin, incarnation)| {
                    server
                        .context
                        .translator
                        .hierarchy_origin(origin, incarnation)
                })
                .ok_or_else(|| DispatchError::stale().wire())?;
            context.extensions.insert(authenticated);
        }
        let mut result = tokio::select! {
            ()=resident.cancel().cancelled_owned()=>return Err(McpError::new(ErrorCode(-32056),"workspace invalidated while request was pending",Some(json!({"code":"workspace_invalidated","retryable":true})))),
            result=server.tool_router.call(ToolCallContext::new(server.as_ref(),request,context))=>result?,
        };
        dispatcher
            .validate(resident)
            .await
            .map_err(DispatchError::wire)?;
        if let CallToolResponse::Complete(output) = &mut result
            && let Some(value) = &mut output.structured_content
        {
            if name == "get_tool_support" {
                value["basis"] = json!("runtime");
                value["workspace_path"] = json!(resident.root());
                value["runtime_generation"] = json!(resident.generation);
            }
            if name == "get_document_outline" {
                value["runtime_generation"] = json!(resident.generation);
            }
            if let (Some(origin), Some(incarnation)) = (&origin, &incarnation) {
                if resident.incarnation(origin).as_ref() != Some(incarnation) {
                    return Err(DispatchError::stale().wire());
                }
                self.bind_items(value, &lease, origin, incarnation).await;
            }
            output.content = vec![rmcp::model::ContentBlock::text(value.to_string())];
        }
        Ok(result)
    }

    async fn select_tool_runtime(
        &self,
        dispatcher: &Arc<crate::dispatcher::Dispatcher>,
        arguments: &mut serde_json::Map<String, Value>,
        file: Option<&PathBuf>,
        workspace: Option<&PathBuf>,
    ) -> Result<(Lease, Option<(crate::config::ServerId, String)>), McpError> {
        let continuation = if let Some(item) = arguments.get_mut("item") {
            let token = item
                .get("dispatcher_context")
                .and_then(Value::as_str)
                .ok_or_else(|| DispatchError::stale().wire())?
                .to_string();
            item.as_object_mut()
                .ok_or_else(|| DispatchError::stale().wire())?
                .remove("dispatcher_context");
            let contexts = self.hierarchy_contexts.lock().await;
            let (generation, original, origin, incarnation) = contexts
                .get(&token)
                .cloned()
                .ok_or_else(|| DispatchError::stale().wire())?;
            drop(contexts);
            if *item != original {
                return Err(DispatchError::stale().wire());
            }
            Some((generation, origin, incarnation))
        } else {
            None
        };
        let (lease, origin) = if let Some((generation, origin, incarnation)) = continuation {
            let lease = dispatcher
                .existing(&generation)
                .await
                .map_err(DispatchError::wire)?;
            dispatcher
                .validate(&lease.resident)
                .await
                .map_err(|_| DispatchError::stale().wire())?;
            if lease.resident.incarnation(&origin).as_ref() != Some(&incarnation) {
                return Err(DispatchError::stale().wire());
            }
            (lease, Some((origin, incarnation)))
        } else {
            let selection = if let Some(file) = file {
                let selected = dispatcher
                    .select(file.clone(), false)
                    .await
                    .map_err(DispatchError::wire)?;
                if let Some(workspace) = workspace {
                    let other = dispatcher
                        .select(workspace.clone(), true)
                        .await
                        .map_err(DispatchError::wire)?;
                    if selected.root() != other.root() {
                        return Err(McpError::invalid_params(
                            "file_path and workspace_path select different workspaces",
                            Some(json!({"code":"workspace_invalid","retryable":false})),
                        ));
                    }
                }
                selected
            } else if let Some(workspace) = workspace {
                dispatcher
                    .select(workspace.clone(), true)
                    .await
                    .map_err(DispatchError::wire)?
            } else {
                return Err(McpError::invalid_params(
                    "workspace_path is required for this operation in dispatcher mode",
                    Some(json!({"code":"workspace_required","retryable":false})),
                ));
            };
            (
                dispatcher
                    .acquire(selection)
                    .await
                    .map_err(DispatchError::wire)?,
                None,
            )
        };
        Ok((lease, origin))
    }

    pub(super) async fn dispatch_listen(
        &self,
        context: SubscriptionContext,
    ) -> Result<(), McpError> {
        let requested = context
            .requested()
            .resource_subscriptions
            .as_deref()
            .unwrap_or_default();
        let accepted = context
            .accepted()
            .resource_subscriptions
            .as_deref()
            .unwrap_or_default();
        if ListenUris::exceeds_budget(requested) {
            return Err(
                self.render_error(crate::bridge::resources::SubscriptionError::LimitReached.into())
            );
        }
        let _global = self
            .context
            .session
            .registry()
            .try_reserve_listen()
            .map_err(|e| self.render_error(e.into()))?;
        let mut runtimes = std::collections::BTreeMap::new();
        for raw in accepted {
            let (lease, server, resolved) = self.resource_session(raw).await?;
            let (_, _, uris) = runtimes
                .entry(lease.resident.generation.clone())
                .or_insert((lease, server, Vec::new()));
            uris.push((raw.clone(), resolved));
        }
        #[cfg(feature = "transport-http")]
        start_listen_lease(&context);
        let mut registrations = Vec::new();
        for (lease, server, resolved) in runtimes.values_mut() {
            let uris = Arc::new(ListenUris::from_resolved(std::mem::take(resolved)));
            let sink = context.sink().clone();
            let registration = server
                .context
                .session
                .registry()
                .try_reserve_listen()
                .map_err(|error| server.render_error(error.into()))?
                .register(uris.clone(), |uris| Target::Sink { sink, uris });
            let registration = server
                .settle_listen_startup_failures(registration, &uris)
                .await?;
            self.require_dispatcher()?
                .validate(&lease.resident)
                .await
                .map_err(DispatchError::wire)?;
            let cached = {
                let cache = server.context.notification_cache.lock().await;
                uris.canonical()
                    .filter(|(_, uri)| cache.is_listen_replayable(uri))
                    .map(|(uri, _)| uri.clone())
                    .collect::<Vec<_>>()
            };
            for uri in cached {
                registration.publish(&uri).await;
            }
            registrations.push(registration);
        }
        if runtimes.is_empty() {
            return Ok(());
        }
        let invalidations = runtimes
            .values()
            .map(|(lease, _, _)| Box::pin(lease.resident.cancel().cancelled_owned()))
            .collect::<Vec<_>>();
        let result = tokio::select! {
            ()=context.cancelled()=>Ok(()),
            _=futures::future::select_all(invalidations)=>Err(McpError::new(ErrorCode(-32056),"workspace invalidated during diagnostic listen",Some(json!({"code":"workspace_invalidated","retryable":true})))),
        };
        drop(registrations);
        drop(runtimes);
        result
    }

    pub(super) async fn resource_session(
        &self,
        uri: &str,
    ) -> Result<(Lease, Arc<Self>, ResolvedResource), McpError> {
        let path = parse_uri(uri).map_err(|e| self.render_error(e.into()))?;
        let dispatcher = self.require_dispatcher()?;
        let selection = dispatcher
            .select(path.as_path().to_path_buf(), false)
            .await
            .map_err(DispatchError::wire)?;
        let lease = dispatcher
            .acquire(selection)
            .await
            .map_err(DispatchError::wire)?;
        let canonical = tokio::fs::canonicalize(path.as_path())
            .await
            .map_err(|_| DispatchError::invalid(path.as_path()).wire())?;
        lease
            .resident
            .demand(
                dispatcher,
                Some(&canonical),
                false,
                Some(crate::config::ToolKind::Diagnostics),
            )
            .await
            .map_err(DispatchError::wire)?;
        let server = self.runtime_session(&lease).await?;
        let canonical_uri = crate::bridge::resources::make_uri(&canonical)
            .map_err(|error| server.render_error(error.into()))?;
        let resolved = server
            .resolve_resource(&canonical_uri)
            .await
            .map_err(|error| server.render_error(error))?;
        Ok((lease, server, resolved))
    }

    async fn runtime_session(&self, lease: &Lease) -> Result<Arc<Self>, McpError> {
        let mut sessions = self.workspace_sessions.lock().await;
        if let Some(server) = sessions.get(&lease.resident.generation) {
            return Ok(server.clone());
        }
        let dispatcher = self.require_dispatcher()?;
        let server = Arc::new(
            lease
                .resident
                .server(dispatcher)
                .await
                .map_err(DispatchError::wire)?
                .with_stop(self.stop.clone()),
        );
        sessions.insert(lease.resident.generation.clone(), server.clone());
        drop(sessions);
        Ok(server)
    }

    async fn bind_items(
        &self,
        value: &mut Value,
        lease: &Lease,
        origin: &crate::config::ServerId,
        incarnation: &str,
    ) {
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            match value {
                Value::Object(object) => {
                    if object.contains_key("uri")
                        && object.contains_key("selectionRange")
                        && object.contains_key("range")
                        && object.contains_key("name")
                    {
                        let token = crate::dispatcher::opaque();
                        self.hierarchy_contexts.lock().await.insert(
                            token.clone(),
                            (
                                lease.resident.generation.clone(),
                                Value::Object(object.clone()),
                                origin.clone(),
                                incarnation.to_owned(),
                            ),
                        );
                        object.insert("dispatcher_context".into(), Value::String(token));
                    } else {
                        pending.extend(object.values_mut());
                    }
                }
                Value::Array(items) => pending.extend(items.iter_mut()),
                _ => {}
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod connection_tests {
    use std::process::Command;

    use rmcp::ServiceExt as _;

    use super::*;

    fn init_repo(root: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir(root)?;
        assert!(
            Command::new("git")
                .arg("init")
                .arg(root)
                .output()?
                .status
                .success()
        );
        Ok(())
    }

    /// A completed MCP restart rejects old items without forwarding their opaque data.
    #[tokio::test]
    async fn hierarchy_continuation_rejects_replacement_connection() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("repo");
        init_repo(&root).unwrap();
        std::fs::write(root.join("file.py"), "x=1\n").unwrap();
        std::fs::write(root.join("marker"), "AUTHORED CONNECTION").unwrap();
        let events = temp.path().join("events.jsonl");
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dispatcher_lsp.py");
        let profile = temp.path().join("profile.toml");
        std::fs::write(&profile, format!("[[lsp_servers]]\nlanguage_id='python'\ncommand='/usr/bin/python3'\nargs=[{script:?},{events:?}]\nfile_patterns=['*.py']\n")).unwrap();
        let config: crate::config::ServerConfig = toml::from_str(&format!("[dispatcher]\nenabled=true\n[[dispatcher.repositories]]\npath={root:?}\nprofile={profile:?}\n")).unwrap();
        let dispatcher = crate::dispatcher::Dispatcher::new(config.clone()).unwrap();
        let prototype = McplsServer::new(
            Arc::new(crate::bridge::Translator::new()),
            Arc::new(tokio::sync::Mutex::new(
                crate::bridge::NotificationCache::new(),
            )),
            crate::bridge::WorkspaceRoots::default(),
            crate::mcp::SubscriptionRegistry::new(),
            config.project_config_status,
            config.mcp,
        )
        .with_dispatcher(dispatcher.clone());
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = prototype.for_new_session();
        let task = tokio::spawn(async move {
            let service = server.serve(tokio::io::split(server_io)).await.unwrap();
            let _ = service.waiting().await;
        });
        let client = ().serve(tokio::io::split(client_io)).await.unwrap();
        let prepared = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("get_incoming_calls").with_arguments(
                    json!({"file_path":root.join("file.py"),"line":1,"character":1})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap()
            .structured_content
            .unwrap();
        let item = prepared["calls"][0]["from"].clone();
        assert!(item["dispatcher_context"].is_string());
        let followed = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("get_incoming_calls")
                    .with_arguments(json!({"item":item}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            followed.structured_content.unwrap()["calls"][0]["from"]["data"],
            item["data"]
        );
        // Each operation crosses the complete MCP tools/call boundary.
        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("restart_server").with_arguments(
                    json!({"workspace_path":root,"servers":["python"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let before = std::fs::read_to_string(&events).unwrap().lines().count();
        let error = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("get_incoming_calls")
                    .with_arguments(json!({"item":item}).as_object().unwrap().clone()),
            )
            .await
            .unwrap_err();
        match error {
            rmcp::ServiceError::McpError(error) => {
                assert_eq!(error.data.unwrap()["code"], "stale_continuation");
            }
            other => panic!("expected stale_continuation, got {other:?}"),
        }
        let later: Vec<Value> = std::fs::read_to_string(&events)
            .unwrap()
            .lines()
            .skip(before)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            !later
                .iter()
                .any(|event| event["method"] == "callHierarchy/incomingCalls"),
            "old opaque item reached the replacement: {later:?}"
        );
        dispatcher.shutdown().await;
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}
