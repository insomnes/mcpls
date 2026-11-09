//! Bounded composition of the existing per-file diagnostics read.

use futures::{StreamExt, stream};
use serde_json::{Map, Value, json};

use super::{DiagnosticsParams, DiagnosticsResponse, McpError, McplsServer, PathBuf, ToolRouter};
use crate::dispatcher::DispatchError;
use crate::mcp::parameters::Parameters;

const RESULT_BYTES: usize = 131_072;

fn workspace_invalidated() -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(-32056),
        "workspace invalidated while batch was pending",
        Some(json!({"code":"workspace_invalidated","retryable":true})),
    )
}

fn invalid(message: &str) -> McpError {
    McpError::invalid_params(message.to_owned(), None)
}

/// Validate the complete envelope before workspace selection can start a backend.
pub(super) fn paths(arguments: &Map<String, Value>) -> Result<Option<Vec<String>>, McpError> {
    let single = arguments.contains_key("file_path");
    let batch = arguments.contains_key("file_paths");
    if single == batch {
        return Err(invalid("provide exactly one of file_path or file_paths"));
    }
    if single {
        serde_json::from_value::<DiagnosticsParams>(Value::Object(arguments.clone()))
            .map_err(|_| invalid("invalid diagnostics parameters"))?;
        return Ok(None);
    }
    if arguments
        .keys()
        .any(|key| !matches!(key.as_str(), "file_paths" | "context"))
    {
        return Err(invalid("unknown diagnostics parameter"));
    }
    // Reuse the existing context decoder, including its null/error semantics.
    let mut probe = arguments.clone();
    probe.remove("file_paths");
    probe.insert("file_path".into(), json!("/diagnostics-envelope"));
    serde_json::from_value::<DiagnosticsParams>(Value::Object(probe))
        .map_err(|_| invalid("invalid diagnostics context"))?;
    let array = arguments["file_paths"]
        .as_array()
        .ok_or_else(|| invalid("file_paths must be an array"))?;
    if !(1..=32).contains(&array.len()) {
        return Err(invalid("file_paths must contain 1 through 32 paths"));
    }
    let mut bytes = 0_usize;
    let mut paths = Vec::with_capacity(array.len());
    for value in array {
        let path = value
            .as_str()
            .ok_or_else(|| invalid("file_paths must contain strings"))?;
        if path.is_empty() || path.len() > 4096 || !std::path::Path::new(path).is_absolute() {
            return Err(invalid(
                "batch paths must be absolute, nonempty and at most 4096 UTF-8 bytes",
            ));
        }
        bytes = bytes.saturating_add(value.to_string().len());
        paths.push(path.to_owned());
    }
    if bytes > 16_384 {
        return Err(invalid("encoded batch paths exceed 16384 bytes"));
    }
    Ok(Some(paths))
}

// Missing files are item failures, but their existing ancestors still need approval.
// Never bypass an existing symlink (including a dangling one) or an I/O failure.
async fn selection_path(path: &std::path::Path) -> Result<PathBuf, McpError> {
    let mut current = path.to_path_buf();
    loop {
        match tokio::fs::symlink_metadata(&current).await {
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !current.pop() {
                    return Err(DispatchError::invalid(path).wire());
                }
            }
            Err(_) => return Err(DispatchError::invalid(path).wire()),
        }
    }
}

impl McplsServer {
    pub(super) async fn dispatch_tool(
        &self,
        mut request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        if self.tool_router.has_route(request.name.as_ref())
            && self.unprefixed(&request.name) == "get_diagnostics"
        {
            let arguments = request.arguments.get_or_insert_default();
            if let Some(paths) = paths(arguments)? {
                return self.diagnostics_batch(paths, arguments, &context.ct).await;
            }
        }
        self.dispatch_single_tool(request, context).await
    }

    async fn select_diagnostics_batch(
        &self,
        paths: &[String],
    ) -> Result<(Option<crate::dispatcher::Selection>, Vec<PathBuf>), McpError> {
        let mut selected = None;
        let mut read_paths = Vec::with_capacity(paths.len());
        // Selection is deliberately separate from demand: even the last path
        // must be approved before the first logical diagnostic read.
        for path in paths {
            // Match single-file dispatch: demand and reads use the approved
            // canonical spelling, while the response preserves the input.
            let read_path = tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| PathBuf::from(path));
            let existing = selection_path(&read_path).await?;
            read_paths.push(read_path);
            if let Some(dispatcher) = &self.dispatcher {
                let selection = dispatcher
                    .select(existing, false)
                    .await
                    .map_err(DispatchError::wire)?;
                if selected
                    .as_ref()
                    .is_some_and(|first: &crate::dispatcher::Selection| {
                        first.root() != selection.root()
                    })
                {
                    return Err(invalid("batch paths select different workspaces"));
                }
                selected = Some(selection);
            } else {
                let client = super::parse_client_path(existing)?;
                self.context
                    .workspace_roots
                    .validate(&client)
                    .await
                    .map_err(|error| self.render_error(error))?;
            }
        }
        Ok((selected, read_paths))
    }

    pub(super) async fn diagnostics_batch(
        &self,
        paths: Vec<String>,
        arguments: &Map<String, Value>,
        cancelled: &tokio_util::sync::CancellationToken,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let (selected, read_paths) = self.select_diagnostics_batch(&paths).await?;
        let lease = if let (Some(dispatcher), Some(selection)) = (&self.dispatcher, selected) {
            Some(
                dispatcher
                    .acquire(selection)
                    .await
                    .map_err(DispatchError::wire)?,
            )
        } else {
            None
        };
        let runtime_server = if let Some(lease) = &lease {
            Some(self.runtime_session(lease).await?)
        } else {
            None
        };
        let server = runtime_server.as_deref().unwrap_or(self);
        let resident = lease.as_ref().map(|lease| &lease.resident);
        let invalidated = resident
            .map(|resident| resident.cancel())
            .unwrap_or_default();
        let context = arguments.get("context").cloned();
        let input_count = paths.len();
        let reads = stream::iter(paths.into_iter().zip(read_paths).enumerate().map(
            |(index, (path, path_buf))| {
                let context = context.clone();
                async move {
                    if cancelled.is_cancelled() || invalidated_for(resident) {
                        return Err(workspace_invalidated());
                    }
                    let result = async {
                        if let (Some(dispatcher), Some(resident)) = (&self.dispatcher, resident) {
                            dispatcher
                                .validate(resident)
                                .await
                                .map_err(DispatchError::wire)?;
                            resident
                                .demand(
                                    dispatcher,
                                    Some(&path_buf),
                                    false,
                                    Some(crate::config::ToolKind::Diagnostics),
                                )
                                .await
                                .map_err(DispatchError::wire)?;
                        }
                        let mut params = Map::from_iter([("file_path".into(), json!(path_buf))]);
                        if let Some(context) = context {
                            params.insert("context".into(), context);
                        }
                        let params = serde_json::from_value(Value::Object(params))
                            .map_err(|_| invalid("invalid diagnostics parameters"))?;
                        let report = server.get_diagnostics(Parameters(params)).await?;
                        Ok(report.0)
                    }
                    .await;
                    Ok((index, path, result))
                }
            },
        ))
        .buffered(4);
        tokio::pin!(reads);
        let mut outcomes = Vec::with_capacity(input_count);
        loop {
            let next = tokio::select! {
                biased;
                () = invalidated.cancelled() => return Err(workspace_invalidated()),
                () = cancelled.cancelled() => return Err(invalid("diagnostics batch cancelled")),
                next = reads.next() => next,
            };
            let Some(outcome) = next else { break };
            outcomes.push(outcome?);
        }
        if let (Some(dispatcher), Some(resident)) = (&self.dispatcher, resident) {
            dispatcher
                .validate(resident)
                .await
                .map_err(DispatchError::wire)?;
        }
        let bounded = serde_json::to_value(bound_results(outcomes)?)
            .map_err(|error| serialization_error(&error))?;
        Ok(rmcp::model::CallToolResult::structured(bounded).into())
    }
}

fn invalidated_for(resident: Option<&std::sync::Arc<crate::dispatcher::Resident>>) -> bool {
    resident.is_some_and(|resident| resident.cancel().is_cancelled())
}

type Outcome = (usize, String, Result<DiagnosticsResponse, McpError>);

#[derive(serde::Serialize)]
struct BatchResponse {
    results: Vec<BatchItem>,
    truncated: bool,
}

#[derive(serde::Serialize)]
struct BatchItem {
    input_index: usize,
    file_path: String,
    truncated: bool,
    #[serde(flatten)]
    outcome: BatchOutcome,
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BatchOutcome {
    Ok {
        report: DiagnosticsResponse,
        omitted_diagnostics: usize,
    },
    Error {
        error: BatchError,
    },
}

#[derive(serde::Serialize)]
struct BatchError {
    code: i32,
    message: String,
    retryable: bool,
}

fn serialization_error(error: &serde_json::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn bound_results(outcomes: Vec<Outcome>) -> Result<BatchResponse, McpError> {
    let mut retained = Vec::new();
    let mut results = Vec::new();
    for (input_index, file_path, outcome) in outcomes {
        let (outcome, truncated, diagnostics) = match outcome {
            Ok(mut report) => {
                let diagnostics = std::mem::take(&mut report.result.diagnostics);
                (
                    BatchOutcome::Ok {
                        omitted_diagnostics: diagnostics.len(),
                        report,
                    },
                    false,
                    diagnostics,
                )
            }
            Err(error) => {
                let retryable = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("retryable"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut wire = BatchError {
                    code: error.code.0,
                    message: error.message.into_owned(),
                    retryable,
                };
                // Bound clipping work before measuring escaped JSON.
                let mut truncated = if let Some((offset, _)) = wire.message.char_indices().nth(1024)
                {
                    wire.message.truncate(offset);
                    true
                } else {
                    false
                };
                while serde_json::to_vec(&wire)
                    .map_err(|error| serialization_error(&error))?
                    .len()
                    > 1024
                {
                    wire.message.pop();
                    truncated = true;
                }
                (BatchOutcome::Error { error: wire }, truncated, Vec::new())
            }
        };
        results.push(BatchItem {
            input_index,
            file_path,
            truncated,
            outcome,
        });
        retained.push(diagnostics);
    }
    let mut batch = BatchResponse {
        results,
        truncated: false,
    };
    // false occupies more bytes than true, and initial omission counts can only
    // shrink. Reserve all association/report/error metadata before diagnostics.
    let mut bytes = serde_json::to_vec(&batch)
        .map_err(|error| serialization_error(&error))?
        .len();
    let mut total = 0_usize;
    for (item, diagnostics) in batch.results.iter_mut().zip(retained) {
        if let BatchOutcome::Ok {
            report,
            omitted_diagnostics,
        } = &mut item.outcome
        {
            for diagnostic in diagnostics {
                let count = report.result.diagnostics.len();
                if count == 100 || total == 256 {
                    continue;
                }
                let size = serde_json::to_vec(&diagnostic)
                    .map_err(|error| serialization_error(&error))?
                    .len();
                let added = size.saturating_add(usize::from(count > 0));
                if size > 8192 || bytes.saturating_add(added) > RESULT_BYTES {
                    continue;
                }
                report.result.diagnostics.push(diagnostic);
                *omitted_diagnostics = omitted_diagnostics.saturating_sub(1);
                total = total.saturating_add(1);
                bytes = bytes.saturating_add(added);
            }
            item.truncated = *omitted_diagnostics > 0;
        }
    }
    batch.truncated = batch.results.iter().any(|item| item.truncated);
    Ok(batch)
}

pub(super) fn shape_schema(router: &mut ToolRouter<McplsServer>) {
    let Some(route) = router.map.get_mut("get_diagnostics") else {
        return;
    };
    let mut input = (*route.attr.input_schema).clone();
    input.remove("required");
    input.insert(
        "oneOf".into(),
        json!([{"required":["file_path"]},{"required":["file_paths"]}]),
    );
    let Some(properties) = input.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    properties.insert("file_paths".into(), json!({"type":"array","minItems":1,"maxItems":32,"items":{"type":"string","minLength":1,"maxLength":4096,"pattern":"^/"}}));
    route.attr.input_schema = std::sync::Arc::new(input);
    if let Some(output) = &mut route.attr.output_schema {
        let mut single = (**output).clone();
        let mut definitions = single
            .remove("$defs")
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let batch = json!({"type":"object","required":["results","truncated"],"properties":{"truncated":{"type":"boolean"},"results":{"type":"array","items":{"type":"object","required":["input_index","file_path","status","truncated"],"properties":{"input_index":{"type":"integer","minimum":0},"file_path":{"type":"string"},"status":{"enum":["ok","error"]},"truncated":{"type":"boolean"},"omitted_diagnostics":{"type":"integer","minimum":0},"report":{"$ref":"#/$defs/DiagnosticsReport"},"error":{"type":"object","required":["code","message","retryable"],"properties":{"code":{"type":"integer"},"message":{"type":"string"},"retryable":{"type":"boolean"}}}},"oneOf":[{"required":["report","omitted_diagnostics"],"not":{"required":["error"]},"properties":{"status":{"const":"ok"}}},{"required":["error"],"not":{"required":["report"]},"properties":{"status":{"const":"error"}}}]}}}});
        definitions.insert("DiagnosticsReport".into(), Value::Object(single));
        if let Value::Object(shaped) =
            json!({"$defs":definitions,"anyOf":[{"$ref":"#/$defs/DiagnosticsReport"},batch]})
        {
            *output = std::sync::Arc::new(shaped);
        }
    }
}
