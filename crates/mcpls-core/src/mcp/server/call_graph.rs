//! Public bounded graph schema; runtime traversal lives in the translator.
#![expect(
    clippy::indexing_slicing,
    reason = "all schema objects are constructed locally"
)]
use std::sync::Arc;

use serde_json::{Value, json};

use super::{McplsServer, ToolRouter};

/// Install the complete object schema instead of Value's unconstrained schema.
pub(super) fn shape_schema(router: &mut ToolRouter<McplsServer>) {
    let Some(route) = router.map.get_mut("get_call_graph") else {
        return;
    };
    let mut hierarchy = serde_json::to_value(schemars::schema_for!(crate::bridge::HierarchyItem))
        .unwrap_or_else(|_| json!({"type":"object"}));
    let definitions = hierarchy.as_object_mut().and_then(|m| m.remove("$defs"));
    hierarchy.as_object_mut().map(|m| m.remove("$schema"));
    if let Some(properties) = hierarchy
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "dispatcher_context".into(),
            json!({"type":"string","description":"Authenticated normalized-item continuation."}),
        );
    }
    let range = json!({"type":"object","required":["start","end"],"properties":{"start":{"type":"object","required":["line","character"],"properties":{"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0}}},"end":{"type":"object","required":["line","character"],"properties":{"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0}}}}});
    let mut schema = json!({"type":"object","required":["status","direction","root","nodes","edges","limits","usage","warnings","stop_reasons","truncated","indexing_in_progress","analysis_freshness","atomic_snapshot"],"properties":{
        "status":{"enum":["ok","no_item","selection_required","preparation_incomplete"],"description":"Root preparation outcome."},
        "direction":{"enum":["incoming","outgoing"],"description":"Traversal direction."},
        "root":{"type":["string","null"],"description":"Root node ID; null for preparation stops."},
        "nodes":{"type":"array","description":"Nodes in first-seen breadth-first order.","items":{"type":"object","required":["id","item","depth","expansion_status","positions_encoding","positions_base","source_identity"],"properties":{
            "id":{"type":"string","description":"Request-local n0, n1, ... identifier."},"item":hierarchy,
            "depth":{"type":"integer","minimum":0,"description":"Minimum observed hops from root."},
            "expansion_status":{"enum":["expanded","depth_boundary","excluded_scope","truncated","failed","unexpanded_budget"],"description":"Observed expansion outcome; expanded does not imply completeness."},
            "positions_encoding":{"enum":["utf-8","utf-16","utf-32"],"description":"Encoding of item coordinates."},"positions_base":{"enum":[0,1],"description":"Raw LSP coordinates use zero; normalized UTF-16 uses one."},
            "source_identity":{"oneOf":[{"type":"object","required":["status"],"properties":{"status":{"const":"unavailable"}}},{"type":"object","required":["status","origin","sha256"],"properties":{"status":{"const":"observed"},"origin":{"enum":["tracked","disk"]},"sha256":{"type":"string"}}}],"description":"Hash of already observed complete source bytes, or unavailable."}
        }}},
        "edges":{"type":"array","description":"First-seen caller-to-callee relationships; ranges belong to caller URI.","items":{"type":"object","required":["caller","callee","from_ranges","positions_encoding","positions_base"],"properties":{
            "caller":{"type":"string","description":"Caller node ID."},"callee":{"type":"string","description":"Callee node ID."},"from_ranges":{"type":"array","items":range,"description":"Distinct caller-document call sites in first-seen order."},"positions_encoding":{"enum":["utf-8","utf-16","utf-32"],"description":"Call-site coordinate encoding."},"positions_base":{"enum":[0,1],"description":"Raw zero-based or normalized one-based coordinates."}}}},
        "items":{"type":"array","items":hierarchy,"description":"Bounded preparation candidates; present only when preparation stops."},
        "limits":{"type":"object","description":"Effective traversal limits."},"usage":{"type":"object","description":"Actual work and retained-result counters."},
        "warnings":{"type":"array","items":{"type":"string"},"description":"Deduplicated route and traversal warnings."},"stop_reasons":{"type":"array","items":{"type":"string"},"description":"Stable observed boundaries and budget reasons."},
        "truncated":{"type":"boolean","description":"An aggregate budget, bounded admission or replacement omitted known work."},"indexing_in_progress":{"type":"boolean","description":"A sampled route reported indexing."},"positions_degraded":{"enum":["request","response"],"description":"Worst coordinate conversion degradation; request takes precedence."},
        "positions_encoding":{"enum":["utf-8","utf-16","utf-32"],"description":"Coordinate encoding for every item in a raw stopped preparation list."},
        "positions_base":{"const":0,"description":"Stopped preparation candidates retain raw zero-based coordinates when this field is present."},
        "resolved_symbol":{"type":"object","description":"Existing name-resolution metadata when applicable."},"analysis_freshness":{"const":"unknown","description":"Analyzer freshness is not established."},"atomic_snapshot":{"const":false,"description":"Observations are not an atomic source snapshot."}
    }});
    if let Some(definitions) = definitions {
        schema["$defs"] = definitions;
    }
    for (field, names) in [
        (
            "limits",
            vec![
                "depth",
                "max_nodes",
                "max_edges",
                "max_ranges",
                "max_records",
                "max_requests",
                "max_data_bytes",
                "max_output_bytes",
            ],
        ),
        (
            "usage",
            vec![
                "nodes",
                "edges",
                "ranges_examined",
                "records_examined",
                "analysis_requests",
                "retained_data_bytes",
                "output_bytes",
                "omitted_relationships",
            ],
        ),
    ] {
        schema["properties"][field]["required"] = json!(names);
        for name in names {
            schema["properties"][field]["properties"][name] = json!({"type":"integer","minimum":0,"description":match name {"output_bytes"=>"Exact compact final structured UTF-8 JSON bytes, including this counter.","omitted_relationships"=>"Examined provider relationships actually omitted; excludes unseen work.","retained_data_bytes"=>"Compact opaque data bytes, counted once per retained identity or candidate.","ranges_examined"=>"Call-site fromRanges examined, including duplicates.","records_examined"=>"Preparation items and relationship records examined, including duplicates.","analysis_requests"=>"Actual analysis send attempts, including resolution, preparation and retries.",_=>"Effective bound or retained count named by this field."}});
        }
    }
    if let Value::Object(schema) = schema {
        route.attr.output_schema = Some(Arc::new(schema));
    }
}
