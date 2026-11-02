//! Fixed-budget composition of the existing public semantic operations.
#![expect(
    clippy::indexing_slicing,
    reason = "all indexed values are constructed JSON objects; missing read keys yield null"
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use super::{
    AddressableTool, CallHierarchyCallsParams, HoverResult, Json, McpError, McplsServer,
    Parameters, PathBuf, PositionParams, PositionRange, Serialize, client_input_error,
    escape_bounded, parse_target,
};
use crate::bridge::{Indexed, InlayHintsResult, Position2D, Range, ResultContext};
use crate::mcp::tools::SymbolContextParams;
use crate::redaction::ServerText;

const ORDER: [&str; 6] = [
    "source",
    "hover",
    "outgoing_calls",
    "diagnostics",
    "inlay_hints",
    "highlights",
];

pub(super) fn shape_schema(router: &mut super::ToolRouter<McplsServer>) {
    if let Some(route) = router.map.get_mut("get_symbol_context") {
        let schema = json!({
            "type":"object",
            "required":["target","sections","truncated","analysis_freshness","atomic_snapshot"],
            "properties":{
                "target":{"type":"object","required":["file_path","position","range_fidelity"],
                    "properties":{"file_path":{"type":"string"},"position":{"type":"object"},
                        "range_fidelity":{"enum":["symbol","unknown"]}}},
                "sections":{"type":"object","additionalProperties":{"type":"object","required":["status"],
                    "properties":{"status":{"enum":["ok","empty","partial","unsupported","unavailable","error"]},
                        "reason":{"type":"string"},"data":{"type":"object"}}}},
                "truncated":{"type":"boolean"},"analysis_freshness":{"const":"unknown"},
                "atomic_snapshot":{"const":false},"positions_degraded":{"enum":["request","response"]}
            }
        });
        if let Value::Object(schema) = schema {
            route.attr.output_schema = Some(Arc::new(schema));
        }
    }
}

fn outcome(status: &str, reason: &str) -> Value {
    json!({"status":status,"reason":reason})
}

fn value<T: Serialize>(answer: Result<Json<T>, McpError>) -> Value {
    match answer {
        Ok(Json(data)) => serde_json::to_value(data).map_or_else(
            |_| outcome("error", "serialization_failed"),
            |data| json!({"status":"ok","data":data}),
        ),
        Err(error) => {
            let unsupported = error.message.to_ascii_lowercase().contains("capability");
            let message = escape_bounded(&error.message, 512);
            json!({"status":if unsupported {"unsupported"} else {"error"},
                "reason":if unsupported {"capability_not_supported"} else {"provider_error"},
                "data":{"message":message}})
        }
    }
}

fn point(value: &Value) -> Option<(u64, u64)> {
    Some((
        value.get("line")?.as_u64()?,
        value.get("character")?.as_u64()?,
    ))
}

fn inside(position: &Value, range: &Range) -> bool {
    point(position).is_some_and(|point| {
        (
            u64::from(range.start.line),
            u64::from(range.start.character),
        ) <= point
            && point < (u64::from(range.end.line), u64::from(range.end.character))
    })
}

fn intersects(value: &Value, range: &Range) -> bool {
    let Some(start) = value.get("start").and_then(point) else {
        return false;
    };
    let Some(end) = value.get("end").and_then(point) else {
        return false;
    };
    if start == end {
        return value.get("start").is_some_and(|p| inside(p, range));
    }
    start < end
        && start < (u64::from(range.end.line), u64::from(range.end.character))
        && (
            u64::from(range.start.line),
            u64::from(range.start.character),
        ) < end
}

fn qualify(section: &mut Value, name: &str, range: Option<&Range>) {
    let Some(data) = section.get_mut("data") else {
        return;
    };
    let key = match name {
        "outgoing_calls" => "calls",
        "diagnostics" => "diagnostics",
        "inlay_hints" => "hints",
        "highlights" => "highlights",
        _ => "",
    };
    if let Some(range) = range
        && let Some(entries) = data.get_mut(key).and_then(Value::as_array_mut)
    {
        match name {
            "diagnostics" | "highlights" => {
                entries.retain(|entry| entry.get("range").is_some_and(|r| intersects(r, range)));
            }
            "inlay_hints" => {
                entries.retain(|entry| entry.get("position").is_some_and(|p| inside(p, range)));
            }
            _ => (),
        }
    }
    let reason = if name == "outgoing_calls" {
        match section_status(data, "status") {
            Some("selection_required") => Some(("unavailable", "selection_required")),
            Some("preparation_incomplete") => Some(("unavailable", "preparation_incomplete")),
            Some("no_item") => Some(("empty", "")),
            _ => None,
        }
    } else if name == "diagnostics" {
        match section_status(data, "availability") {
            Some("pending" | "evicted") => Some(("unavailable", "diagnostics_unavailable")),
            _ if section_status(data, "origin") == Some("cache_after_failed_pull") => {
                Some(("partial", "pull_failed"))
            }
            _ => None,
        }
    } else {
        None
    };
    let reason = reason.or_else(|| {
        if data.get("truncated").and_then(Value::as_bool) == Some(true) {
            Some(("partial", "provider_truncated"))
        } else if data.get("positions_degraded").is_some() {
            Some(("partial", "positions_degraded"))
        } else if data
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            || (name == "hover" && data.get("contents").is_none_or(Value::is_null))
        {
            Some(("empty", ""))
        } else {
            None
        }
    });
    if let Some((status, reason)) = reason {
        section["status"] = json!(status);
        if !reason.is_empty() {
            section["reason"] = json!(reason);
        }
    }
}

fn section_status<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

fn records(section: &Value, name: &str) -> usize {
    let Some(data) = section.get("data") else {
        return 0;
    };
    match name {
        "source" => usize::from(data.get("text").is_some()),
        "hover" => usize::from(data.get("contents").is_some()),
        "outgoing_calls" => data
            .get("calls")
            .and_then(Value::as_array)
            .map_or(0, |calls| {
                calls
                    .iter()
                    .map(|call| {
                        1usize.saturating_add(
                            call.get("from_ranges")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len),
                        )
                    })
                    .sum::<usize>()
            })
            .saturating_add(
                data.get("items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
            ),
        _ => data
            .get(match name {
                "diagnostics" => "diagnostics",
                "inlay_hints" => "hints",
                _ => "highlights",
            })
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    }
}

fn files(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "uri" | "file_path")
                    && let Some(path) = value.as_str()
                {
                    if url::Url::parse(path).is_ok_and(|uri| uri.scheme() != "file") {
                        found.insert(path.to_owned());
                        continue;
                    }
                    let normalized = url::Url::parse(path)
                        .ok()
                        .and_then(|uri| uri.to_file_path().ok())
                        .unwrap_or_else(|| PathBuf::from(path));
                    let normalized =
                        normalized
                            .components()
                            .fold(PathBuf::new(), |mut path, component| {
                                match component {
                                    std::path::Component::CurDir => (),
                                    std::path::Component::ParentDir => {
                                        path.pop();
                                    }
                                    part => path.push(part.as_os_str()),
                                }
                                path
                            });
                    found.insert(normalized.to_string_lossy().into_owned());
                } else {
                    files(value, found);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                files(value, found);
            }
        }
        _ => (),
    }
}

fn shrink(section: &mut Value, reason: &str) -> bool {
    let Some(data) = section.get_mut("data") else {
        return false;
    };
    let mut removed = false;
    for key in ["calls", "items", "diagnostics", "hints", "highlights"] {
        if let Some(entries) = data.get_mut(key).and_then(Value::as_array_mut)
            && !entries.is_empty()
        {
            if key == "calls"
                && let Some(ranges) = entries
                    .last_mut()
                    .and_then(|call| call.get_mut("from_ranges"))
                    .and_then(Value::as_array_mut)
                && !ranges.is_empty()
            {
                ranges.pop();
                removed = true;
                break;
            }
            entries.pop();
            removed = true;
            break;
        }
    }
    if !removed {
        section.as_object_mut().map(|map| map.remove("data"));
    }
    section["status"] = json!(if removed { "partial" } else { "unavailable" });
    section["reason"] = json!(reason);
    true
}

fn result_bytes(bundle: &Value) -> usize {
    serde_json::to_vec(&CallToolResult::structured(bundle.clone()))
        .map_or(usize::MAX, |bytes| bytes.len())
}

/// Dispatcher context tokens are output facts and must fit the same final response cap.
pub(super) fn enforce_response_budget(bundle: &mut Value) {
    for name in ORDER.into_iter().rev() {
        while result_bytes(bundle) > 131_072 {
            let Some(section) = bundle
                .get_mut("sections")
                .and_then(|sections| sections.get_mut(name))
            else {
                break;
            };
            if !shrink(section, "response_budget") {
                break;
            }
            bundle["truncated"] = json!(true);
        }
    }
}

impl McplsServer {
    #[expect(
        clippy::too_many_lines,
        reason = "six explicit independent sections keep budget admission and failures visible together"
    )]
    pub(super) async fn symbol_context(
        &self,
        params: SymbolContextParams,
    ) -> Result<CallToolResult, McpError> {
        if !params.target.file_path.is_absolute() {
            return Err(McpError::invalid_params("file_path must be absolute", None));
        }
        let (path, target) = parse_target(params.target.file_path, params.target.target)?;
        let translator = &self.context.translator;
        let resolved = translator
            .resolve_symbol_target(&path, target, AddressableTool::SymbolContext)
            .await
            .map_err(|error| self.render_error(error))?;
        let position = resolved.position;
        let needs_range = params.sections.iter().any(|name| {
            matches!(
                name.as_str(),
                "source" | "diagnostics" | "inlay_hints" | "highlights"
            )
        });
        let (mut range, mut degraded) = if needs_range {
            translator
                .context_symbol_range(&path, position)
                .await
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        if resolved.positions_degraded.is_some() {
            degraded = resolved.positions_degraded;
            range = None;
        }
        let mut target = json!({"file_path":path.as_path(),"position":Position2D{line:position.line().get(),character:position.character().get()},"range_fidelity":if range.is_some(){"symbol"}else{"unknown"}});
        if let Some(range) = &range {
            target["range"] = json!(range);
        }
        if let Some(mut symbol) = resolved.resolved {
            symbol.redact_server_text(&translator.server_text_redactions());
            target["resolved_symbol"] = json!(symbol);
        }
        let mut bundle = json!({"target":target,"sections":{},"truncated":false,"analysis_freshness":"unknown","atomic_snapshot":false});
        if let Some(degraded) = degraded {
            bundle["positions_degraded"] = json!(degraded);
        }
        for name in &params.sections {
            bundle["sections"][name] = outcome("unavailable", "request_budget");
        }
        for name in ORDER {
            if !params.sections.iter().any(|section| section == name) {
                continue;
            }
            let filepath = path.as_path().to_path_buf();
            let pos = PositionParams {
                file_path: filepath.clone(),
                line: position.line().get(),
                character: position.character().get(),
            };
            let mut section = match name {
                "source" => {
                    translator
                        .context_source(&path, position, range.as_ref())
                        .await
                }
                "hover" => match translator.context_hover(&path, position).await {
                    Ok(Some(hover)) => value(self.structured_result(Ok(hover))),
                    Ok(None) => json!({"status":"empty"}),
                    Err(error) => value::<HoverResult>(Err(self.render_error(error))),
                },
                "outgoing_calls" => value(
                    self.get_outgoing_calls_with_origin(
                        Parameters(CallHierarchyCallsParams::Target(pos.clone().into())),
                        None,
                    )
                    .await,
                ),
                "diagnostics" if range.is_some() => value(
                    self.diagnostics_for_context(filepath, ResultContext::None, true)
                        .await,
                ),
                "inlay_hints" if range.is_some() => {
                    let range = range
                        .as_ref()
                        .ok_or_else(|| McpError::internal_error("range disappeared", None))?;
                    match PositionRange::from_client(
                        (range.start.line, range.start.character),
                        (range.end.line, range.end.character),
                    ) {
                        Ok(range) => value(self.structured_result(
                            translator.handle_inlay_hints(path.clone(), range).await,
                        )),
                        Err(error) => {
                            value::<Indexed<InlayHintsResult>>(Err(client_input_error(error)))
                        }
                    }
                }
                "highlights" if range.is_some() => {
                    value(self.get_document_highlights(Parameters(pos)).await)
                }
                _ => outcome("unavailable", "range_unavailable"),
            };
            if section["status"] == "ok" {
                qualify(&mut section, name, range.as_ref());
            }
            if bundle["positions_degraded"] == "request" && section["status"] == "ok" {
                section["status"] = json!("partial");
                section["reason"] = json!("positions_degraded");
            }
            if matches!(
                section["reason"].as_str(),
                Some("text_budget" | "source_budget" | "file_too_large" | "provider_truncated")
            ) || section
                .get("data")
                .and_then(|data| data.get("truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                bundle["truncated"] = json!(true);
            }
            bundle["sections"][name] = section;
            loop {
                let count = ORDER
                    .iter()
                    .map(|name| records(&bundle["sections"][*name], name))
                    .sum::<usize>();
                let mut participants = BTreeSet::new();
                files(&bundle, &mut participants);
                let reason = if count > 256 {
                    Some("item_budget")
                } else if participants.len() > 16 {
                    Some("file_budget")
                } else if result_bytes(&bundle) > 131_072 {
                    Some("response_budget")
                } else {
                    None
                };
                let Some(reason) = reason else {
                    break;
                };
                bundle["truncated"] = json!(true);
                if !shrink(&mut bundle["sections"][name], reason) {
                    return Err(McpError::internal_error(
                        "target exceeds response budget",
                        None,
                    ));
                }
            }
        }
        Ok(CallToolResult::structured(bundle))
    }
}
