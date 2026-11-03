//! Bounded caller-held diagnostic observations and exact multiset comparison.
use serde_json::{Map, Value, json};

use super::{McpError, McplsServer, RouteSignals, ToolRouter, Translator};

fn invalid() -> McpError {
    McpError::invalid_params("invalid diagnostics_delta parameters", None)
}
fn object<'a>(v: &'a Value, keys: &[&str]) -> Result<&'a Map<String, Value>, McpError> {
    let o = v.as_object().ok_or_else(invalid)?;
    if o.len() != keys.len() || keys.iter().any(|k| !o.contains_key(*k)) {
        return Err(invalid());
    }
    Ok(o)
}
fn text(v: &Value, max: usize, nonempty: bool) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() <= max && (!nonempty || !s.is_empty()))
}
fn choice(v: &Value, choices: &[&str]) -> bool {
    v.as_str().is_some_and(|s| choices.contains(&s))
}
fn positive(v: &Value) -> bool {
    v.as_u64()
        .is_some_and(|n| n > 0 && u32::try_from(n).is_ok())
}
fn nullable_text(v: &Value, max: usize) -> bool {
    v.is_null() || text(v, max, false)
}
fn size(v: &Value) -> usize {
    v.to_string().len()
}
fn diagnostic(v: &Value) -> Result<(), McpError> {
    let d = object(v, &["range", "severity", "code", "message", "source"])?;
    let r = object(&d["range"], &["start", "end"])?;
    for p in [&r["start"], &r["end"]] {
        let p = object(p, &["line", "character"])?;
        if !positive(&p["line"]) || !positive(&p["character"]) {
            return Err(invalid());
        }
    }
    let point = |p: &Value| (p["line"].as_u64(), p["character"].as_u64());
    if point(&r["end"]) < point(&r["start"])
        || !choice(&d["severity"], &["error", "warning", "information", "hint"])
        || !nullable_text(&d["code"], 8192)
        || !nullable_text(&d["source"], 8192)
        || !text(&d["message"], 8192, false)
        || size(v) > 8192
    {
        return Err(invalid());
    }
    Ok(())
}
fn report(v: &Value) -> Result<(), McpError> {
    let r = object(
        v,
        &[
            "availability",
            "origin",
            "pull_kind",
            "result_id",
            "synced_version",
            "report_version",
            "cached_origins",
            "indexing_in_progress",
            "push_notifications_degraded",
            "completeness",
            "uncertainties",
            "omitted_diagnostics",
            "source_freshness",
        ],
    )?;
    if !choice(&r["availability"], &["published", "pending", "evicted"])
        || !choice(
            &r["origin"],
            &["pull", "push_cache", "cache_after_failed_pull", "cache"],
        )
        || !choice(
            &r["pull_kind"],
            &["full", "unchanged", "partial", "failed", "not_requested"],
        )
        || !nullable_text(&r["result_id"], 1024)
        || !choice(&r["completeness"], &["complete", "incomplete", "unknown"])
        || r["source_freshness"] != "unknown"
    {
        return Err(invalid());
    }
    if !r["synced_version"].is_null() && !positive(&r["synced_version"]) {
        return Err(invalid());
    }
    if !r["report_version"].is_null()
        && r["report_version"]
            .as_i64()
            .is_none_or(|v| i32::try_from(v).is_err())
    {
        return Err(invalid());
    }
    for k in ["indexing_in_progress", "push_notifications_degraded"] {
        if !r[k].is_boolean() {
            return Err(invalid());
        }
    }
    if !r["omitted_diagnostics"].is_null() && r["omitted_diagnostics"].as_u64().is_none() {
        return Err(invalid());
    }
    let origins = r["cached_origins"].as_array().ok_or_else(invalid)?;
    if ![
        json!([]),
        json!(["pull"]),
        json!(["push"]),
        json!(["pull", "push"]),
    ]
    .contains(&Value::Array(origins.clone()))
    {
        return Err(invalid());
    }
    let reasons = r["uncertainties"].as_array().ok_or_else(invalid)?;
    let mut prior = None;
    for reason in reasons {
        if !choice(
            reason,
            &[
                "pending",
                "evicted",
                "pull_failed",
                "partial_report",
                "unchanged_without_report",
                "indexing",
                "push_degraded",
                "truncated",
                "cache_completeness_unknown",
                "positions_degraded",
                "metadata_omitted",
                "baseline_incomplete",
            ],
        ) || prior.is_some_and(|p: &Value| p.as_str() >= reason.as_str())
        {
            return Err(invalid());
        }
        prior = Some(reason);
    }
    Ok(())
}
/// Validate before dispatch can acquire or demand a backend.
pub(super) fn validate(v: &Value) -> Result<(), McpError> {
    let o = v.as_object().ok_or_else(invalid)?;
    if o.keys()
        .any(|k| !["file_path", "action", "mode", "baseline"].contains(&k.as_str()))
        || !text(&v["file_path"], 4096, true)
        || !std::path::Path::new(v["file_path"].as_str().ok_or_else(invalid)?).is_absolute()
        || !choice(&v["action"], &["capture", "compare"])
        || o.contains_key("mode") && !choice(&v["mode"], &["refresh", "cached"])
    {
        return Err(invalid());
    }
    if v["action"] == "capture" {
        if o.contains_key("baseline") {
            return Err(invalid());
        }
        return Ok(());
    }
    let b = object(
        &v["baseline"],
        &["schema_version", "identity", "report", "diagnostics"],
    )?;
    if b["schema_version"].as_u64().is_none_or(|n| n == 0) || size(&v["baseline"]) > 65536 {
        return Err(invalid());
    }
    let identity = object(
        &b["identity"],
        &[
            "file_path",
            "workspace_path",
            "runtime_generation",
            "server_id",
            "server_incarnation",
        ],
    )?;
    for k in [
        "file_path",
        "workspace_path",
        "runtime_generation",
        "server_id",
        "server_incarnation",
    ] {
        if !text(
            &identity[k],
            if k.ends_with("path") { 4096 } else { 256 },
            true,
        ) {
            return Err(invalid());
        }
    }
    for k in ["file_path", "workspace_path"] {
        if !std::path::Path::new(identity[k].as_str().ok_or_else(invalid)?).is_absolute() {
            return Err(invalid());
        }
    }
    report(&b["report"])?;
    let ds = b["diagnostics"].as_array().ok_or_else(invalid)?;
    if ds.len() > 256 {
        return Err(invalid());
    }
    for d in ds {
        diagnostic(d)?;
    }
    Ok(())
}
fn incomplete(report: &mut Value, reason: &str) {
    report["completeness"] = json!("incomplete");
    if let Some(reasons) = report["uncertainties"].as_array_mut() {
        let reason = json!(reason);
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
        reasons.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
}
impl McplsServer {
    #[expect(
        clippy::too_many_lines,
        reason = "acquisition, identity checks and final bounded observation form one lifecycle boundary"
    )]
    #[expect(
        clippy::indexing_slicing,
        reason = "validate establishes all input keys; generated output fields are explicit objects"
    )]
    pub(super) async fn diagnostic_delta(&self, input: Value) -> Result<Value, McpError> {
        validate(&input)?;
        if input["action"] == "compare" && input["baseline"]["schema_version"] != 1 {
            return Ok(json!({"status":"incompatible_baseline","reason":"schema_mismatch"}));
        }
        let path = super::parse_client_path(std::path::PathBuf::from(
            input["file_path"].as_str().ok_or_else(invalid)?,
        ))?;
        let translator = &self.context.translator;
        let validated = translator
            .validate_path(&path)
            .await
            .map_err(|e| self.render_error(e))?;
        let route = translator
            .diagnostics_route_for_path(validated.as_path())
            .into_read_result()
            .map_err(|e| self.render_error(e))?
            .ok_or_else(|| McpError::internal_error("diagnostics route unavailable", None))?;
        let before = {
            let cache = self.context.notification_cache.lock().await;
            RouteSignals::sample(&cache, Some(&route))
        };
        let cached = input["mode"] == "cached";
        if !cached {
            translator
                .refresh_delta_route(&validated)
                .await
                .map_err(|e| self.render_error(e))?;
        }
        let incarnation = translator
            .dispatcher_server(&route)
            .filter(|(alive, _)| *alive)
            .map(|(_, id)| id)
            .ok_or_else(|| {
                McpError::new(
                    rmcp::model::ErrorCode(-32002),
                    "diagnostics server is initializing",
                    None,
                )
            })?;
        let workspace = self
            .context
            .workspace_roots
            .canonical_root_of(validated.as_path())
            .ok_or_else(invalid)?;
        let unavailable_identity = || {
            McpError::new(
                rmcp::model::ErrorCode(-32057),
                "diagnostics identity exceeds supported byte bounds",
                Some(json!({"code":"workspace_unavailable","retryable":false})),
            )
        };
        let file_identity = validated
            .as_path()
            .to_str()
            .ok_or_else(unavailable_identity)?;
        let workspace_identity = workspace.to_str().ok_or_else(unavailable_identity)?;
        for (value, limit) in [
            (file_identity, 4096),
            (workspace_identity, 4096),
            (self.diagnostics_generation.as_str(), 256),
            (route.as_str(), 256),
            (incarnation.as_str(), 256),
        ] {
            if value.is_empty() || value.len() > limit {
                return Err(unavailable_identity());
            }
        }
        let identity = json!({"file_path":file_identity,"workspace_path":workspace_identity,"runtime_generation":self.diagnostics_generation,"server_id":route.as_str(),"server_incarnation":incarnation});
        if input["action"] == "compare" {
            for (key, reason) in [
                ("file_path", "file_mismatch"),
                ("workspace_path", "workspace_mismatch"),
                ("runtime_generation", "runtime_mismatch"),
                ("server_id", "server_mismatch"),
                ("server_incarnation", "server_restarted"),
            ] {
                if input["baseline"]["identity"][key] != identity[key] {
                    return Ok(json!({"status":"incompatible_baseline","reason":reason}));
                }
            }
        }
        let (info, availability, origin, pull_kind, continuity) = if cached {
            let (_, snapshot) = self
                .diagnostics_snapshot(&path)
                .await
                .map_err(|e| self.render_error(e))?;
            (
                snapshot.sources.merge(),
                snapshot.availability,
                "cache".to_owned(),
                "not_requested",
                true,
            )
        } else {
            translator
                .delta_observation(&validated, &self.context.notification_cache)
                .await
                .map_err(|e| self.render_error(e))?
        };
        let after = {
            let cache = self.context.notification_cache.lock().await;
            RouteSignals::sample(&cache, Some(&route))
        };
        let signals = before.union(after);
        let normalized = Translator::diagnostics_from_cache_entry(
            info.as_ref(),
            translator.position_encoding_for(&route),
            translator.document_tracker(),
        )
        .await;
        let evidence = info.as_ref().map(|i| &i.evidence);
        let (source_loss, history_unknown) = {
            let uri = crate::bridge::path_to_uri(validated.as_path())
                .map_err(|e| self.render_error(e))?;
            self.context
                .notification_cache
                .lock()
                .await
                .delta_completeness(&uri)
        };
        let cache_unknown = history_unknown || evidence.is_some_and(|e| e.cache_unknown);
        let mut report = json!({"availability":availability,"origin":origin,"pull_kind":pull_kind,"result_id":evidence.and_then(|e|e.result_id.as_ref()),"synced_version":translator.document_tracker().synced_version(validated.as_path(),&route),"report_version":evidence.and_then(|e|e.report_version),"cached_origins":evidence.map(|e|e.origins.iter().copied().filter(|origin| pull_kind != "full" || *origin != "pull").collect::<Vec<_>>()).unwrap_or_default(),"indexing_in_progress":signals.indexing.indexing_in_progress,"push_notifications_degraded":signals.push_notifications_degraded,"completeness":"complete","uncertainties":[],"omitted_diagnostics":evidence.and_then(|e|e.omitted).unwrap_or(0),"source_freshness":"unknown"});
        for (condition, reason) in [
            (
                availability == crate::bridge::DiagnosticsAvailability::Pending,
                "pending",
            ),
            (
                availability == crate::bridge::DiagnosticsAvailability::Evicted,
                "evicted",
            ),
            (pull_kind == "failed", "pull_failed"),
            (cache_unknown, "cache_completeness_unknown"),
            (pull_kind == "partial", "partial_report"),
            (!continuity, "unchanged_without_report"),
            (signals.indexing.indexing_in_progress, "indexing"),
            (signals.push_notifications_degraded, "push_degraded"),
            (
                normalized.positions_degraded.is_some(),
                "positions_degraded",
            ),
            (
                evidence.is_some_and(|e| e.metadata_omitted),
                "metadata_omitted",
            ),
        ] {
            if condition {
                incomplete(&mut report, reason);
            }
        }
        if cache_unknown
            && report["uncertainties"]
                .as_array()
                .is_some_and(|reasons| reasons.len() == 1)
        {
            report["completeness"] = json!("unknown");
        }
        if translator.dispatcher_server(&route).as_ref() != Some(&(true, incarnation)) {
            return Err(McpError::new(
                rmcp::model::ErrorCode(-32056),
                "diagnostics server changed during acquisition",
                None,
            ));
        }
        let mut truncated = source_loss || evidence.is_some_and(|e| e.truncated);
        if truncated {
            incomplete(&mut report, "truncated");
            report["omitted_diagnostics"] = json!(evidence.and_then(|e| e.omitted));
        }
        let mut diagnostics = Vec::new();
        let mut omitted = 0usize;
        for (position, d) in normalized.diagnostics.into_iter().enumerate() {
            let source = info
                .as_ref()
                .and_then(|i| i.diagnostics.get(position))
                .and_then(|d| d.source.clone());
            let mut value = serde_json::to_value(d).map_err(|_| invalid())?;
            value["source"] = json!(source);
            if value.get("code").is_none() {
                value["code"] = Value::Null;
            }
            if diagnostics.len() >= 256 || size(&value) > 8192 {
                omitted = omitted.saturating_add(1);
            } else {
                diagnostics.push(value);
            }
        }
        let baseline_report = &input["baseline"]["report"];
        let baseline_complete = baseline_report["completeness"] == "complete"
            && baseline_report["availability"] == "published"
            && baseline_report["indexing_in_progress"] == false
            && baseline_report["push_notifications_degraded"] == false
            && baseline_report["omitted_diagnostics"] == 0
            && baseline_report["uncertainties"]
                .as_array()
                .is_some_and(Vec::is_empty);
        if input["action"] == "compare" && !baseline_complete {
            incomplete(&mut report, "baseline_incomplete");
        }
        loop {
            if omitted > 0 {
                truncated = true;
                incomplete(&mut report, "truncated");
            }
            let base_omitted = evidence.and_then(|e| e.omitted);
            report["omitted_diagnostics"] =
                if source_loss || cache_unknown || evidence.is_some_and(|e| e.omitted.is_none()) {
                    Value::Null
                } else {
                    json!(base_omitted.unwrap_or(0).saturating_add(omitted))
                };
            let baseline = json!({"schema_version":1,"identity":identity,"report":report,"diagnostics":diagnostics});
            let output = if input["action"] == "capture" {
                json!({"status":"baseline_captured","baseline":baseline,"truncated":truncated})
            } else {
                let old = input["baseline"]["diagnostics"]
                    .as_array()
                    .ok_or_else(invalid)?;
                let mut matched = vec![false; old.len()];
                let mut added = Vec::new();
                for d in &diagnostics {
                    if let Some(index) = old
                        .iter()
                        .enumerate()
                        .position(|(i, b)| !matched.get(i).copied().unwrap_or(true) && b == d)
                    {
                        if let Some(slot) = matched.get_mut(index) {
                            *slot = true;
                        }
                    } else {
                        added.push(d.clone());
                    }
                }
                let remaining: Vec<_> = matched
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| m.then_some(i))
                    .collect();
                let missing: Vec<_> = matched
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| (!m).then_some(i))
                    .collect();
                let complete = report["completeness"] == "complete" && baseline_complete;
                json!({"status":if complete{"compared"}else{"incomplete"},"identity":identity,"report":report,"comparison_complete":complete,"added":added,"remaining":remaining,"resolved":if complete{missing.clone()}else{vec![]},"uncertain_removed":if complete{vec![]}else{missing},"truncated":truncated})
            };
            if (input["action"] != "capture" || size(&baseline) <= 65536)
                && size(&output) <= 131_072
            {
                return Ok(output);
            }
            if diagnostics.pop().is_none() {
                return Err(McpError::internal_error(
                    "diagnostics metadata exceeds output bound",
                    None,
                ));
            }
            omitted = omitted.saturating_add(1);
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "schema construction accepts owned temporary JSON objects"
)]
fn schema_object(properties: Value) -> Value {
    let required: Vec<_> = properties
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys().cloned())
        .collect();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn enumeration(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
#[expect(
    clippy::indexing_slicing,
    reason = "schema roots are explicit JSON objects"
)]
pub(super) fn shape_schema(router: &mut ToolRouter<McplsServer>) {
    let Some(route) = router.map.get_mut("diagnostics_delta") else {
        return;
    };
    let string = json!({"type":"string"});
    let nullable = json!({"type":["string","null"]});
    let integer = json!({"type":"integer","minimum":0});
    let boolean = json!({"type":"boolean"});
    let point = schema_object(
        json!({"line":{"type":"integer","minimum":1,"maximum":4_294_967_295_u64},"character":{"type":"integer","minimum":1,"maximum":4_294_967_295_u64}}),
    );
    let diagnostic = schema_object(
        json!({"range":schema_object(json!({"start":{"$ref":"#/$defs/Point"},"end":{"$ref":"#/$defs/Point"}})),"severity":enumeration(&["error","warning","information","hint"]),"code":nullable,"message":string,"source":nullable}),
    );
    let identity = schema_object(
        json!({"file_path":string,"workspace_path":string,"runtime_generation":string,"server_id":string,"server_incarnation":string}),
    );
    let report = schema_object(
        json!({"availability":enumeration(&["published","pending","evicted"]),"origin":enumeration(&["pull","push_cache","cache_after_failed_pull","cache"]),"pull_kind":enumeration(&["full","unchanged","partial","failed","not_requested"]),"result_id":nullable,"synced_version":{"type":["integer","null"],"minimum":1},"report_version":{"type":["integer","null"]},"cached_origins":{"type":"array","items":enumeration(&["pull","push"]),"uniqueItems":true},"indexing_in_progress":boolean,"push_notifications_degraded":boolean,"completeness":enumeration(&["complete","incomplete","unknown"]),"uncertainties":{"type":"array","items":enumeration(&["pending","evicted","pull_failed","partial_report","unchanged_without_report","indexing","push_degraded","truncated","cache_completeness_unknown","positions_degraded","metadata_omitted","baseline_incomplete"]),"uniqueItems":true},"omitted_diagnostics":{"type":["integer","null"],"minimum":0},"source_freshness":enumeration(&["unknown"])}),
    );
    let diagnostics = json!({"type":"array","items":{"$ref":"#/$defs/Diagnostic"},"maxItems":256});
    let baseline = schema_object(
        json!({"schema_version":{"type":"integer","minimum":1},"identity":{"$ref":"#/$defs/Identity"},"report":{"$ref":"#/$defs/Report"},"diagnostics":diagnostics}),
    );
    let definitions = json!({"Point":point,"Diagnostic":diagnostic,"Identity":identity,"Report":report,"Baseline":baseline});
    let path = json!({"type":"string","minLength":1,"maxLength":4096});
    let mode = enumeration(&["refresh", "cached"]);
    let baseline_ref = json!({"$ref":"#/$defs/Baseline"});
    let mut input = json!({"type":"object","properties":{"file_path":path,"action":enumeration(&["capture","compare"]),"mode":mode,"baseline":baseline_ref},"required":["file_path","action"],"additionalProperties":false,"oneOf":[{"type":"object","properties":{"file_path":path,"action":{"const":"capture"},"mode":mode},"required":["file_path","action"],"additionalProperties":false},{"type":"object","properties":{"file_path":path,"action":{"const":"compare"},"mode":mode,"baseline":baseline_ref},"required":["file_path","action","baseline"],"additionalProperties":false}]});
    input["$defs"] = definitions.clone();
    let indices = json!({"type":"array","items":integer});
    let mut output = json!({"type":"object","oneOf":[schema_object(json!({"status":{"const":"baseline_captured"},"baseline":baseline_ref,"truncated":boolean})),schema_object(json!({"status":enumeration(&["compared","incomplete"]),"identity":{"$ref":"#/$defs/Identity"},"report":{"$ref":"#/$defs/Report"},"comparison_complete":boolean,"added":diagnostics,"resolved":indices,"remaining":indices,"uncertain_removed":indices,"truncated":boolean})),schema_object(json!({"status":{"const":"incompatible_baseline"},"reason":enumeration(&["schema_mismatch","file_mismatch","workspace_mismatch","runtime_mismatch","server_mismatch","server_restarted"])}))]});
    output["$defs"] = definitions;
    if let Some(o) = input.as_object() {
        route.attr.input_schema = std::sync::Arc::new(o.clone());
    }
    if let Some(o) = output.as_object() {
        route.attr.output_schema = Some(std::sync::Arc::new(o.clone()));
    }
}
