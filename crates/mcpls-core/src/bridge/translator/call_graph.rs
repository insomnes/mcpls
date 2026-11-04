//! Request-local bounded call traversal. Inbound frame parsing and analyzer work
//! remain subject to the existing transport limits, not these graph budgets.
#![allow(
    clippy::indexing_slicing,
    reason = "graph JSON objects and bounded node indices are constructed here"
)]
#![expect(
    clippy::unwrap_used,
    reason = "all unwrapped arrays and objects are constructed graph-state invariants; provider fields use fallible parsing"
)]
#![expect(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "graph counters and indices are bounded by 4096, depths by 8, and serialized lengths by fixed caps"
)]
#![expect(
    clippy::similar_names,
    reason = "caller and callee are the established directed-edge terms"
)]
#![expect(
    clippy::too_many_lines,
    reason = "preparation, sequential expansion, and whole-relationship admission each keep their budget checks in one flow"
)]

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::dto::{CheckedHierarchyItem, HierarchyItem, Position2D, PositionDegradation, Range};
use super::encoding_ctx::EncodingCtx;
use super::hierarchy::{clamp_to_input_limit, hierarchy_item_to_lsp};
use super::routing::{Capability, DisclosedCapability};
use super::{AddressableTool, HierarchyOrigin, SymbolTarget, Translator};
use crate::bridge::{ClientPath, path_to_uri, uri_to_path};
use crate::error::{Error, Result};
use crate::lsp::CALL_GRAPH_REQUESTS;

/// Direction followed by a bounded call graph.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallGraphDirection {
    /// Follow caller relationships.
    Incoming,
    /// Follow callee relationships.
    Outgoing,
}

/// Validated root selector, using the existing target and item contracts.
pub enum CallGraphStart {
    /// Resolve a file target and prepare its hierarchy.
    Target(ClientPath, SymbolTarget),
    /// Reuse an authenticated normalized hierarchy item.
    Item(Box<CheckedHierarchyItem>),
}

/// Limits selected for this request.
pub struct CallGraphOptions {
    /// Traversal direction.
    pub direction: CallGraphDirection,
    /// Requested maximum hop count.
    pub depth: usize,
    /// Retained-node bound, including root.
    pub max_nodes: usize,
    /// The dispatcher will attach fixed-size authentication tokens.
    pub authenticated: bool,
}

/// Maximum compact structured result, including its own byte counter.
pub const MAX_OUTPUT_BYTES: usize = 262_144;
const MAX_DATA_BYTES: usize = 1_048_576;
const MAX_RECORDS: usize = 4096;
const MAX_RANGES: usize = 2048;
const MAX_EDGES: usize = 512;

/// A bounded serializer sink. It does not allocate the serialized payload.
struct ByteCounter {
    count: usize,
    limit: usize,
}
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.count = self.count.saturating_add(bytes.len());
        if self.count > self.limit {
            return Err(io::Error::other("graph byte limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Compact JSON length, stopping serialization when the supplied bound is exceeded.
pub fn bounded_size(value: &impl Serialize, limit: usize) -> usize {
    let mut writer = ByteCounter { count: 0, limit };
    if serde_json::to_writer(&mut writer, value).is_err() {
        limit.saturating_add(1)
    } else {
        writer.count
    }
}

/// Opaque null values consume no retained-data allowance.
fn data_size(item: &Value) -> usize {
    item.get("data")
        .filter(|v| !v.is_null())
        .map_or(0, |v| bounded_size(v, MAX_DATA_BYTES))
}

/// Add one stable warning or reason, preserving its first occurrence.
fn unique(value: &mut Value, field: &str, reason: &str) {
    if let Some(values) = value[field].as_array_mut()
        && !values.iter().any(|v| v == reason)
    {
        values.push(json!(reason));
    }
}

/// Update the self-inclusive byte counter after all final metadata is present.
pub fn finish_bytes(value: &mut Value) -> usize {
    loop {
        let bytes = bounded_size(value, usize::MAX);
        if value["usage"]["output_bytes"].as_u64() == Some(bytes as u64) {
            return bytes;
        }
        value["usage"]["output_bytes"] = json!(bytes);
    }
}

/// Authoritative final check after payload policy and dispatcher authentication.
/// Normal admission measures exact token sizes; this fail-closed fallback also
/// protects the public bound if metadata changes in a future dispatcher update.
pub fn enforce_final_bytes(value: &mut Value) {
    if finish_bytes(value) <= MAX_OUTPUT_BYTES {
        return;
    }
    unique(value, "stop_reasons", "max_output_bytes");
    value["truncated"] = json!(true);
    value["status"] = json!("preparation_incomplete");
    value["root"] = Value::Null;
    value["nodes"] = json!([]);
    value["edges"] = json!([]);
    value["items"] = json!([]);
    value["usage"]["nodes"] = json!(0);
    value["usage"]["edges"] = json!(0);
    value["usage"]["retained_data_bytes"] = json!(0);
    if finish_bytes(value) > MAX_OUTPUT_BYTES {
        value.as_object_mut().unwrap().remove("resolved_symbol");
        finish_bytes(value);
    }
}

/// Account exactly for dispatcher metadata without minting a token prematurely.
fn projected_bytes(value: &mut Value, authenticated: bool) -> usize {
    // opaque() emits 64 hexadecimal characters. Each item adds this exact
    // comma/key/string length; payload identifiers are preserved by redaction.
    let token_bytes = ",\"dispatcher_context\":\"\"".len() + 64;
    let reusable_nodes = value["nodes"].as_array().map_or(0, |nodes| {
        nodes
            .iter()
            .filter(|n| n["positions_base"] == 1 && n["expansion_status"] != "excluded_scope")
            .count()
    });
    let candidates = if value.get("positions_degraded").is_none() {
        value["items"].as_array().map_or(0, Vec::len)
    } else {
        0
    };
    let extra = if authenticated {
        (reusable_nodes + candidates) * token_bytes
    } else {
        0
    };
    loop {
        let bytes = bounded_size(value, MAX_OUTPUT_BYTES).saturating_add(extra);
        if value["usage"]["output_bytes"].as_u64() == Some(bytes as u64) {
            return bytes;
        }
        value["usage"]["output_bytes"] = json!(bytes);
    }
}

/// One retained identity. Provider data lives only in the public node item.
struct Pending {
    raw: lsp_types::CallHierarchyItem,
    eligible: bool,
}

/// Enough information to undo one whole admitted provider relationship if
/// final warnings or counter digits exceed the final response allowance.
struct Admission {
    parent: usize,
    nodes: usize,
    edges: usize,
    previous_ranges: Option<(usize, usize)>,
    data_bytes: usize,
}

/// Graph state is bounded by the public aggregate limits.
struct Graph {
    value: Value,
    pending: Vec<Pending>,
    options: CallGraphOptions,
    requests: Arc<AtomicUsize>,
    scope: PathBuf,
    origin: HierarchyOrigin,
    admissions: Vec<Admission>,
}
impl Graph {
    fn usage(&self, field: &str) -> usize {
        self.value["usage"][field].as_u64().unwrap_or(0) as usize
    }
    fn add(&mut self, field: &str, amount: usize) {
        let n = self.usage(field) + amount;
        self.value["usage"][field] = json!(n);
    }
    fn reason(&mut self, reason: &str, truncated: bool) {
        unique(&mut self.value, "stop_reasons", reason);
        if truncated {
            self.value["truncated"] = json!(true);
        }
    }
    fn warning(&mut self, warning: &str) {
        unique(&mut self.value, "warnings", warning);
    }
    fn degraded(&mut self, degradation: PositionDegradation) {
        let request = degradation == PositionDegradation::Request;
        if request || self.value.get("positions_degraded").is_none() {
            self.value["positions_degraded"] = json!(if request { "request" } else { "response" });
        }
        if self.value.get("items").is_some() {
            self.value["positions_encoding"] = json!(self.origin.encoding.encoding.to_lsp());
            self.value["positions_base"] = json!(0);
        }
        self.warning(if request {
            "positions_degraded_request"
        } else {
            "positions_degraded_response"
        });
        self.reason("positions_degraded", false);
    }
    fn sync_usage(&mut self) {
        self.value["usage"]["nodes"] = json!(self.value["nodes"].as_array().map_or(0, Vec::len));
        self.value["usage"]["edges"] = json!(self.value["edges"].as_array().map_or(0, Vec::len));
        self.value["usage"]["analysis_requests"] = json!(self.requests.load(Ordering::Relaxed));
    }
    fn capacity(&self) -> Option<&'static str> {
        if self.pending.len() >= self.options.max_nodes {
            Some("max_nodes")
        } else if self.usage("edges") >= MAX_EDGES {
            Some("max_edges")
        } else if self.usage("ranges_examined") >= MAX_RANGES {
            Some("max_ranges")
        } else if self.usage("records_examined") >= MAX_RECORDS {
            Some("max_records")
        } else if self.usage("retained_data_bytes") >= MAX_DATA_BYTES {
            Some("max_data_bytes")
        } else if self.usage("output_bytes") >= MAX_OUTPUT_BYTES {
            Some("max_output_bytes")
        } else {
            None
        }
    }
    fn interrupted(&mut self, index: usize, reason: &str, omitted: bool) {
        self.value["nodes"][index]["expansion_status"] = json!("truncated");
        self.reason(reason, true);
        self.reason("child_truncated", true);
        self.warning("child_truncated");
        if omitted {
            self.add("omitted_relationships", 1);
        }
    }
    fn server_changed(&mut self) {
        self.reason("server_changed", true);
        self.warning("server_changed");
    }
    fn fits(&mut self) -> bool {
        self.sync_usage();
        projected_bytes(&mut self.value, self.options.authenticated) <= MAX_OUTPUT_BYTES
    }
}

/// The graph's initial bounded empty result.
fn empty(options: &CallGraphOptions) -> Value {
    json!({"status":"preparation_incomplete","direction":options.direction,"root":null,
        "nodes":[],"edges":[],"items":[],"limits":{"depth":options.depth,"max_nodes":options.max_nodes,
        "max_edges":MAX_EDGES,"max_ranges":MAX_RANGES,"max_records":MAX_RECORDS,"max_requests":64,
        "max_data_bytes":MAX_DATA_BYTES,"max_output_bytes":MAX_OUTPUT_BYTES},
        "usage":{"nodes":0,"edges":0,"ranges_examined":0,"records_examined":0,"analysis_requests":0,
        "retained_data_bytes":0,"output_bytes":0,"omitted_relationships":0},
        "warnings":[],"stop_reasons":[],"truncated":false,"indexing_in_progress":false,
        "analysis_freshness":"unknown","atomic_snapshot":false})
}

/// Normalize a canonical, authorized item. Keep original coordinates on failure.
async fn normalized_item(
    translator: &Translator,
    raw: lsp_types::CallHierarchyItem,
    allowed: bool,
    origin: &HierarchyOrigin,
) -> (HierarchyItem, bool, Option<PositionDegradation>) {
    let mut item = HierarchyItem {
        name: raw.name,
        kind: u32::from(raw.kind),
        detail: raw.detail,
        uri: raw.uri.to_string(),
        range: raw_range(raw.range),
        selection_range: raw_range(raw.selection_range),
        data: raw.data,
        out_of_workspace: origin.encoding.is_out_of_workspace(&raw.uri),
    };
    if !allowed {
        return (item, false, None);
    }
    let ctx = EncodingCtx::new(
        origin.encoding.encoding,
        translator.document_tracker.clone(),
        translator.workspace_roots.clone(),
    );
    let range = ctx.normalize_range(&raw.uri, raw.range).await;
    let selection_range = ctx.normalize_range(&raw.uri, raw.selection_range).await;
    let degraded = ctx.positions_degraded();
    if degraded.is_none() {
        item.range = clamp_to_input_limit(range);
        item.selection_range = clamp_to_input_limit(selection_range);
    }
    (item, degraded.is_none(), degraded)
}
const fn raw_range(range: lsp_types::Range) -> Range {
    Range {
        start: Position2D {
            line: range.start.line,
            character: range.start.character,
        },
        end: Position2D {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

impl Translator {
    /// Traverse one request without creating persistent graph state.
    pub(crate) async fn handle_call_graph(
        &self,
        start: CallGraphStart,
        options: CallGraphOptions,
        supplied: Option<HierarchyOrigin>,
    ) -> Result<Value> {
        let requests = Arc::new(AtomicUsize::new(0));
        CALL_GRAPH_REQUESTS
            .scope(
                requests.clone(),
                self.call_graph_inner(start, options, supplied, requests),
            )
            .await
    }

    async fn call_graph_inner(
        &self,
        start: CallGraphStart,
        options: CallGraphOptions,
        supplied: Option<HierarchyOrigin>,
        requests: Arc<AtomicUsize>,
    ) -> Result<Value> {
        let file = match &start {
            CallGraphStart::Target(file, _) => file.clone(),
            CallGraphStart::Item(item) => ClientPath::try_from(
                self.parse_file_uri(&lsp_types::Uri::from(item.uri()))
                    .await?
                    .into_path_buf(),
            )?,
        };
        let validated = self.workspace_roots.validate(&file).await?;
        let scope = self
            .workspace_roots
            .canonical_root_of(validated.as_path())
            .ok_or_else(|| Error::PathOutsideWorkspace(validated.as_path().to_owned()))?
            .to_owned();
        // Snapshot the hierarchy connection before target resolution. The name
        // resolver may use a different document-symbol route, but cannot rebind
        // hierarchy data to a replacement connection.
        let doc = self
            .prepare_disclosed_document(&file, DisclosedCapability::CallHierarchy, &[])
            .await?;
        let origin = if let Some(origin) = supplied {
            self.require_hierarchy_origin(&origin)?;
            origin
        } else {
            let (_, incarnation) = self
                .dispatcher_server(doc.server_id())
                .ok_or(Error::ServerTerminated)?;
            self.hierarchy_origin(doc.server_id(), &incarnation)
                .ok_or(Error::ServerTerminated)?
        };
        let mut graph = Graph {
            value: empty(&options),
            pending: Vec::new(),
            options,
            requests,
            scope,
            origin,
            admissions: Vec::new(),
        };
        let response = match start {
            CallGraphStart::Target(file, target) => {
                let resolved = self
                    .resolve_symbol_target(&file, target, AddressableTool::PrepareCallHierarchy)
                    .await?;
                if let Some(symbol) = resolved.resolved {
                    graph.value["resolved_symbol"] = serde_json::to_value(symbol)?;
                }
                let prepared = self
                    .prepare_disclosed_document(
                        &file,
                        DisclosedCapability::CallHierarchy,
                        &[resolved.position],
                    )
                    .await?;
                if self.require_hierarchy_origin(&graph.origin).is_err() {
                    graph.server_changed();
                    graph.sync_usage();
                    return Ok(graph.value);
                }
                let ctx = graph.origin.encoding.clone();
                let position = ctx.to_lsp(prepared.uri(), resolved.position).await;
                if let Some(d) = resolved.positions_degraded.max(ctx.positions_degraded()) {
                    graph.degraded(d);
                }
                let before = self.sample_indexing(&graph.origin.server).await;
                let response: Value = graph
                    .origin
                    .client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        json!({"textDocument":{"uri":prepared.uri()},"position":position}),
                        graph.origin.client.request_timeout(),
                    )
                    .await?;
                let after = self.sample_indexing(&graph.origin.server).await;
                if before.union(after).indexing_in_progress {
                    graph.value["indexing_in_progress"] = json!(true);
                    graph.warning("indexing_in_progress");
                }
                response
            }
            CallGraphStart::Item(item) => {
                self.require_hierarchy_origin(&graph.origin)?;
                self.require_capability(&graph.origin.server, Capability::CallHierarchy)?;
                let bytes = item
                    .data
                    .as_ref()
                    .filter(|v| !v.is_null())
                    .map_or(0, |v| bounded_size(v, MAX_DATA_BYTES));
                let reason = if bytes > MAX_DATA_BYTES {
                    Some("max_data_bytes")
                } else if bounded_size(&(&item.name, &item.detail, &item.data), MAX_OUTPUT_BYTES)
                    > MAX_OUTPUT_BYTES
                {
                    Some("max_output_bytes")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    graph.add("records_examined", 1);
                    graph.reason(reason, true);
                    graph.sync_usage();
                    projected_bytes(&mut graph.value, graph.options.authenticated);
                    return Ok(graph.value);
                }
                let raw: lsp_types::CallHierarchyItem =
                    hierarchy_item_to_lsp(*item, doc.uri().clone(), &graph.origin.encoding).await;
                if let Some(d) = graph.origin.encoding.positions_degraded() {
                    graph.degraded(d);
                }
                json!([raw])
            }
        };
        if self.require_hierarchy_origin(&graph.origin).is_err() {
            graph.server_changed();
            graph.sync_usage();
            return Ok(graph.value);
        }
        self.graph_prepare(&mut graph, response).await?;
        if graph.value["status"] == "ok" {
            self.graph_expand(&mut graph).await;
        }
        graph.sync_usage();
        Self::graph_final_fit(&mut graph);
        projected_bytes(&mut graph.value, graph.options.authenticated);
        Ok(graph.value)
    }

    fn graph_final_fit(graph: &mut Graph) {
        while !graph.fits() {
            graph.reason("max_output_bytes", true);
            if let Some(admitted) = graph.admissions.pop() {
                graph.value["edges"]
                    .as_array_mut()
                    .unwrap()
                    .truncate(admitted.edges);
                if let Some((index, len)) = admitted.previous_ranges {
                    graph.value["edges"][index]["from_ranges"]
                        .as_array_mut()
                        .unwrap()
                        .truncate(len);
                }
                graph.value["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .truncate(admitted.nodes);
                graph.pending.truncate(admitted.nodes);
                graph.value["usage"]["retained_data_bytes"] = json!(admitted.data_bytes);
                if admitted.parent < graph.pending.len() {
                    graph.interrupted(admitted.parent, "max_output_bytes", true);
                }
            } else if graph.value["items"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            {
                let item = graph.value["items"].as_array_mut().unwrap().pop().unwrap();
                graph.value["usage"]["retained_data_bytes"] =
                    json!(graph.usage("retained_data_bytes") - data_size(&item));
            } else {
                graph.pending.clear();
                graph.value["nodes"] = json!([]);
                graph.value["edges"] = json!([]);
                graph.value["root"] = Value::Null;
                graph.value["items"] = json!([]);
                graph.value["status"] = json!("preparation_incomplete");
                graph.value["usage"]["retained_data_bytes"] = json!(0);
                if !graph.fits() {
                    graph
                        .value
                        .as_object_mut()
                        .unwrap()
                        .remove("resolved_symbol");
                }
            }
        }
    }

    /// Canonicalize without reading content. Failed authorization retains the URI.
    #[expect(
        clippy::single_match_else,
        reason = "existing-file authorization and missing-source prefix validation are separate paths"
    )]
    async fn graph_scope(
        &self,
        raw: &mut lsp_types::CallHierarchyItem,
        scope: &std::path::Path,
    ) -> bool {
        let Some(path) = uri_to_path(&raw.uri) else {
            return false;
        };
        let Ok(client) = ClientPath::try_from(path) else {
            return false;
        };
        let path = match self.workspace_roots.validate(&client).await {
            Ok(path) => path.into_path_buf(),
            Err(_) => {
                // Missing source remains an in-scope observation when its
                // existing canonical ancestor is authorized. This does not
                // read content or permit an escape through a symlink.
                let Ok(path) = crate::bridge::workspace_roots::canonicalize_existing_prefix(
                    client.as_path(),
                    &|path| dunce::canonicalize(path),
                ) else {
                    return false;
                };
                if self.workspace_roots.canonical_root_of(&path).is_none() {
                    return false;
                }
                path
            }
        };
        if !path.starts_with(scope) {
            return false;
        }
        let Ok(uri) = path_to_uri(&path) else {
            return false;
        };
        raw.uri = uri;
        true
    }

    fn graph_source_identity(&self, raw: &lsp_types::CallHierarchyItem, allowed: bool) -> Value {
        if allowed
            && let Some(path) = uri_to_path(&raw.uri)
            && let Some(text) = self.document_tracker.text(&path)
        {
            return json!({"status":"observed","origin":"tracked","sha256":format!("{:x}",Sha256::digest(text.as_bytes()))});
        }
        json!({"status":"unavailable"})
    }

    async fn graph_prepare(&self, graph: &mut Graph, mut response: Value) -> Result<()> {
        let values = if response.is_null() {
            &mut Vec::new()
        } else {
            response.as_array_mut().ok_or_else(|| {
                Error::InvalidToolParams("invalid hierarchy preparation response".into())
            })?
        };
        let total = values.len();
        for raw in values {
            if let Some(reason) = graph.capacity() {
                graph.reason(reason, true);
                break;
            }
            graph.add("records_examined", 1);
            let bytes = data_size(raw);
            if bytes > MAX_DATA_BYTES.saturating_sub(graph.usage("retained_data_bytes")) {
                graph.reason("max_data_bytes", true);
                break;
            }
            if bounded_size(
                &(raw.get("name"), raw.get("detail"), raw.get("data")),
                MAX_OUTPUT_BYTES,
            ) > MAX_OUTPUT_BYTES
            {
                graph.reason("max_output_bytes", true);
                break;
            }
            let mut raw: lsp_types::CallHierarchyItem = serde_json::from_value(raw.take())?;
            let allowed = self.graph_scope(&mut raw, &graph.scope).await;
            if !allowed {
                // A preparation candidate retained in raw coordinates is an
                // incomplete preparation, never a normalized continuation.
                graph.degraded(PositionDegradation::Response);
                graph.reason("excluded_scope", false);
            }
            let data = raw.data.take();
            let (mut item, normalized, degraded) =
                normalized_item(self, raw.clone(), allowed, &graph.origin).await;
            item.data = data;
            if let Some(d) = degraded {
                graph.degraded(d);
            }
            let item = serde_json::to_value(item)?;
            graph.value["items"].as_array_mut().unwrap().push(item);
            graph.add("retained_data_bytes", bytes);
            if !graph.fits() {
                graph.value["items"].as_array_mut().unwrap().pop();
                graph.value["usage"]["retained_data_bytes"] =
                    json!(graph.usage("retained_data_bytes") - bytes);
                graph.reason("max_output_bytes", true);
                break;
            }
            graph.pending.push(Pending {
                raw,
                eligible: allowed && normalized,
            });
        }
        let retained = graph.pending.len();
        let degraded = graph.value.get("positions_degraded").is_some();
        let capped = graph.value["truncated"] == true;
        let status = if degraded {
            "preparation_incomplete"
        } else if capped || total > 1 {
            "selection_required"
        } else if total == 0 {
            "no_item"
        } else {
            "ok"
        };
        graph.value["status"] = json!(status);
        if status != "ok" {
            if degraded {
                // Use one explicit raw convention for the whole stopped list,
                // including candidates normalized before degradation was seen.
                for (item, pending) in graph.value["items"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .zip(&graph.pending)
                {
                    item["range"] = serde_json::to_value(raw_range(pending.raw.range))?;
                    item["selectionRange"] =
                        serde_json::to_value(raw_range(pending.raw.selection_range))?;
                }
            }
            graph.pending.clear();
            graph.value["usage"]["nodes"] = json!(0);
            if capped && retained == 0 && total == 1 {
                graph.value["status"] = json!("preparation_incomplete");
            }
            return Ok(());
        }
        let pending = &graph.pending[0];
        let source = self.graph_source_identity(&pending.raw, pending.eligible);
        let item = graph.value["items"][0].take();
        let normalized = pending.eligible;
        let node = json!({"id":"n0","item":item,"depth":0,"expansion_status":if normalized {"unexpanded_budget"} else {"excluded_scope"},
            "positions_encoding":if normalized {"utf-16"} else {graph.origin.encoding.encoding.to_lsp()},"positions_base":u8::from(normalized),"source_identity":source});
        graph.value["nodes"].as_array_mut().unwrap().push(node);
        graph.value.as_object_mut().unwrap().remove("items");
        graph.value["root"] = json!("n0");
        if !graph.fits() {
            graph.pending.clear();
            graph.value["nodes"] = json!([]);
            graph.value["root"] = Value::Null;
            graph.value["items"] = json!([]);
            graph.value["status"] = json!("preparation_incomplete");
            graph.value["usage"]["retained_data_bytes"] = json!(0);
            graph.reason("max_output_bytes", true);
        }
        Ok(())
    }

    async fn graph_expand(&self, graph: &mut Graph) {
        let mut index = 0;
        while index < graph.pending.len() {
            if !graph.pending[index].eligible {
                index += 1;
                continue;
            }
            let depth = graph.value["nodes"][index]["depth"].as_u64().unwrap_or(0) as usize;
            if depth >= graph.options.depth {
                graph.value["nodes"][index]["expansion_status"] = json!("depth_boundary");
                graph.reason("depth", false);
                index += 1;
                continue;
            }
            if let Some(reason) = graph.capacity() {
                graph.reason(reason, true);
                break;
            }
            if graph.requests.load(Ordering::Relaxed) >= 64 {
                graph.reason("max_requests", true);
                break;
            }
            if self.require_hierarchy_origin(&graph.origin).is_err() {
                graph.server_changed();
                break;
            }
            let mut checked_caller = graph.pending[index].raw.clone();
            if !self.graph_scope(&mut checked_caller, &graph.scope).await {
                graph.pending[index].eligible = false;
                graph.value["nodes"][index]["expansion_status"] = json!("excluded_scope");
                graph.reason("excluded_scope", false);
                index += 1;
                continue;
            }
            let mut raw = serde_json::to_value(&graph.pending[index].raw).unwrap_or(Value::Null);
            raw["data"] = graph.value["nodes"][index]["item"]
                .get("data")
                .cloned()
                .unwrap_or(Value::Null);
            let method = if graph.options.direction == CallGraphDirection::Incoming {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            };
            let before = self.sample_indexing(&graph.origin.server).await;
            let response: Result<Value> = graph
                .origin
                .client
                .request(
                    method,
                    json!({"item":raw}),
                    graph.origin.client.request_timeout(),
                )
                .await;
            let after = self.sample_indexing(&graph.origin.server).await;
            if before.union(after).indexing_in_progress {
                graph.value["indexing_in_progress"] = json!(true);
                graph.warning("indexing_in_progress");
            }
            if self.require_hierarchy_origin(&graph.origin).is_err() {
                graph.server_changed();
                break;
            }
            match response {
                Ok(mut response) => {
                    if response.is_null() {
                        response = json!([]);
                    }
                    if let Some(records) = response.as_array_mut() {
                        let total = records.len();
                        let mut processed = 0;
                        for record in records {
                            if let Some(reason) = graph.capacity() {
                                graph.interrupted(index, reason, false);
                                break;
                            }
                            graph.add("records_examined", 1);
                            match self.graph_relationship(graph, index, depth, record).await {
                                Ok(()) => processed += 1,
                                Err(reason) => {
                                    graph.interrupted(index, reason, true);
                                    break;
                                }
                            }
                        }
                        if processed == total {
                            graph.value["nodes"][index]["expansion_status"] = json!("expanded");
                        }
                    } else {
                        graph.value["nodes"][index]["expansion_status"] = json!("failed");
                        graph.reason("branch_failed", false);
                        graph.warning("branch_failed");
                    }
                }
                Err(Error::CallGraphRequestLimit) => {
                    graph.value["nodes"][index]["expansion_status"] = json!("failed");
                    graph.reason("max_requests", true);
                    break;
                }
                Err(_) => {
                    graph.value["nodes"][index]["expansion_status"] = json!("failed");
                    graph.reason("branch_failed", false);
                    graph.warning("branch_failed");
                }
            }
            graph.sync_usage();
            index += 1;
        }
        // Frontier status describes known depth boundaries even if an earlier
        // aggregate stop prevented visiting them in the expansion loop.
        for i in index..graph.pending.len() {
            if graph.pending[i].eligible
                && graph.value["nodes"][i]["depth"].as_u64().unwrap_or(0) as usize
                    >= graph.options.depth
            {
                graph.value["nodes"][i]["expansion_status"] = json!("depth_boundary");
                graph.reason("depth", false);
            }
        }
    }

    async fn graph_relationship(
        &self,
        graph: &mut Graph,
        index: usize,
        depth: usize,
        record: &mut Value,
    ) -> std::result::Result<(), &'static str> {
        let nodes_before = graph.pending.len();
        let edges_before = graph.value["edges"].as_array().unwrap().len();
        let data_before = graph.usage("retained_data_bytes");
        let ranges = record
            .get("fromRanges")
            .and_then(Value::as_array)
            .ok_or("branch_failed")?;
        if ranges.len() > MAX_RANGES.saturating_sub(graph.usage("ranges_examined")) {
            return Err("max_ranges");
        }
        let peer_key = if graph.options.direction == CallGraphDirection::Incoming {
            "from"
        } else {
            "to"
        };
        let peer = record.get(peer_key).ok_or("branch_failed")?;
        let bytes = data_size(peer);
        // Preflight a possible new identity before materializing provider data.
        if bytes > MAX_DATA_BYTES {
            return Err("max_data_bytes");
        }
        if bounded_size(
            &(peer.get("name"), peer.get("detail"), peer.get("data")),
            MAX_OUTPUT_BYTES,
        ) > MAX_OUTPUT_BYTES
        {
            return Err("max_output_bytes");
        }
        let mut raw_value = record[peer_key].take();
        let opaque = raw_value.as_object_mut().and_then(|v| v.remove("data"));
        let mut raw: lsp_types::CallHierarchyItem =
            serde_json::from_value(raw_value).map_err(|_| "branch_failed")?;
        let allowed = self.graph_scope(&mut raw, &graph.scope).await;
        let existing = graph
            .pending
            .iter()
            .enumerate()
            .find(|(i, p)| {
                p.raw.uri == raw.uri
                    && p.raw.name == raw.name
                    && p.raw.kind == raw.kind
                    && p.raw.range == raw.range
                    && p.raw.selection_range == raw.selection_range
                    && graph.value["nodes"][*i]["item"]
                        .get("data")
                        .filter(|v| !v.is_null())
                        == opaque.as_ref().filter(|v| !v.is_null())
            })
            .map(|(i, _)| i);
        let new = existing.is_none();
        if new && bytes > MAX_DATA_BYTES.saturating_sub(graph.usage("retained_data_bytes")) {
            return Err("max_data_bytes");
        }
        let peer_index = existing.unwrap_or(graph.pending.len());
        let (caller, callee) = if graph.options.direction == CallGraphDirection::Incoming {
            (peer_index, index)
        } else {
            (index, peer_index)
        };
        let caller_id = format!("n{caller}");
        let callee_id = format!("n{callee}");
        let edge_index = graph.value["edges"]
            .as_array()
            .unwrap()
            .iter()
            .position(|e| e["caller"] == caller_id && e["callee"] == callee_id);
        if edge_index.is_none() && graph.usage("edges") >= MAX_EDGES {
            return Err("max_edges");
        }
        if new {
            let source = self.graph_source_identity(&raw, allowed);
            raw.data = None;
            let (mut item, normalized, degraded) =
                normalized_item(self, raw.clone(), allowed, &graph.origin).await;
            item.data = opaque;
            if let Some(d) = degraded {
                graph.degraded(d);
            }
            let status = if !allowed {
                "excluded_scope"
            } else if !normalized {
                "failed"
            } else if depth + 1 >= graph.options.depth {
                "depth_boundary"
            } else {
                "unexpanded_budget"
            };
            if !allowed {
                graph.reason("excluded_scope", false);
            } else if !normalized {
                graph.reason("positions_degraded", false);
            } else if depth + 1 >= graph.options.depth {
                graph.reason("depth", false);
            }
            graph.value["nodes"].as_array_mut().unwrap().push(json!({"id":format!("n{peer_index}"),"item":item,"depth":depth+1,"expansion_status":status,
                "positions_encoding":if normalized {"utf-16"} else {graph.origin.encoding.encoding.to_lsp()},"positions_base":u8::from(normalized),"source_identity":source}));
            graph.pending.push(Pending {
                raw,
                eligible: allowed && normalized,
            });
            graph.add("retained_data_bytes", bytes);
        }
        let mut checked_caller = graph.pending[caller].raw.clone();
        let caller_allowed = graph.value["nodes"][caller]["expansion_status"] != "excluded_scope"
            && self.graph_scope(&mut checked_caller, &graph.scope).await;
        let caller_uri = checked_caller.uri;
        let ctx = EncodingCtx::new(
            graph.origin.encoding.encoding,
            self.document_tracker.clone(),
            self.workspace_roots.clone(),
        );
        let ranges = record["fromRanges"].as_array_mut().ok_or("branch_failed")?;
        let mut raw_ranges = Vec::with_capacity(ranges.len());
        let mut converted = Vec::with_capacity(ranges.len());
        for range in ranges {
            graph.add("ranges_examined", 1);
            let range: lsp_types::Range =
                serde_json::from_value(range.take()).map_err(|_| "branch_failed")?;
            raw_ranges.push(raw_range(range));
            if caller_allowed {
                converted.push(ctx.normalize_range(&caller_uri, range).await);
            }
        }
        let normalized = caller_allowed && ctx.positions_degraded().is_none();
        if let Some(d) = ctx.positions_degraded() {
            graph.degraded(d);
        }
        let ranges = if normalized { converted } else { raw_ranges };
        let old_edge = edge_index.map(|i| graph.value["edges"][i].clone());
        let previous_ranges = edge_index.map(|i| {
            (
                i,
                graph.value["edges"][i]["from_ranges"]
                    .as_array()
                    .unwrap()
                    .len(),
            )
        });
        if let Some(edge_index) = edge_index {
            let edge = &mut graph.value["edges"][edge_index];
            let kept = edge["from_ranges"].as_array_mut().unwrap();
            for range in ranges {
                let range = serde_json::to_value(range).map_err(|_| "branch_failed")?;
                if !kept.contains(&range) {
                    kept.push(range);
                }
            }
        } else {
            let mut distinct = Vec::new();
            for range in ranges {
                if !distinct.contains(&range) {
                    distinct.push(range);
                }
            }
            graph.value["edges"].as_array_mut().unwrap().push(json!({"caller":caller_id,"callee":callee_id,"from_ranges":distinct,
                "positions_encoding":if normalized {"utf-16"} else {graph.origin.encoding.encoding.to_lsp()},"positions_base":u8::from(normalized)}));
        }
        if !graph.fits() {
            if let Some(edge_index) = edge_index {
                graph.value["edges"][edge_index] = old_edge.unwrap();
            } else {
                graph.value["edges"].as_array_mut().unwrap().pop();
            }
            if new {
                graph.pending.pop();
                graph.value["nodes"].as_array_mut().unwrap().pop();
                graph.value["usage"]["retained_data_bytes"] =
                    json!(graph.usage("retained_data_bytes") - bytes);
            }
            graph.sync_usage();
            return Err("max_output_bytes");
        }
        graph.admissions.push(Admission {
            parent: index,
            nodes: nodes_before,
            edges: edges_before,
            previous_ranges,
            data_bytes: data_before,
        });
        Ok(())
    }
}
