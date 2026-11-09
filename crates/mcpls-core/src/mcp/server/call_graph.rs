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
    let input = Arc::make_mut(&mut route.attr.input_schema);
    if let Some(direction) = input
        .get_mut("$defs")
        .and_then(|v| v.get_mut("CallGraphDirection"))
    {
        *direction = json!({"type":"string","enum":["incoming","outgoing"]});
    }
    if let Some(target) = input
        .get_mut("$defs")
        .and_then(|v| v.get_mut("SymbolTargetEnvelope"))
    {
        target["description"] = json!("Unchanged source-bound target.");
    }
    if let Some(direction) = input
        .get_mut("properties")
        .and_then(|v| v.get_mut("direction"))
        .and_then(Value::as_object_mut)
    {
        direction.remove("description");
    }
    let hierarchy = json!({"type":"object","required":["name","kind","uri","range","selectionRange"],"properties":{
        "name":{"type":"string"},"kind":{"type":"integer","minimum":0,"maximum":4_294_967_295_u64,"description":"Numeric LSP symbol kind."},
        "detail":{"type":["string","null"]},"uri":{"type":"string"},
        "range":{"$ref":"#/$defs/Range"},"selectionRange":{"$ref":"#/$defs/Range"},
        "data":{"description":"Opaque provider data; preserve unchanged."},"out_of_workspace":{"type":"boolean","description":"Advisory membership outside configured workspace roots."},
        "dispatcher_context":{"type":"string"}}});
    let point = json!({"type":"object","required":["line","character"],"description":"Coordinates follow enclosing positions_base and positions_encoding.","properties":{
        "line":{"type":"integer","minimum":0,"maximum":4_294_967_295_u64,"description":"Line in the declared base."},"character":{"type":"integer","minimum":0,"maximum":4_294_967_295_u64,"description":"Column in the declared encoding and base."}}});
    let range = json!({"type":"object","required":["start","end"],"description":"Range in the enclosing coordinate convention.","properties":{"start":{"$ref":"#/$defs/Position2D"},"end":{"$ref":"#/$defs/Position2D"}}});
    let mut schema = json!({"type":"object","required":["status","direction","root","nodes","edges","limits","usage","warnings","stop_reasons","truncated","indexing_in_progress","analysis_freshness","atomic_snapshot"],"properties":{
        "status":{"enum":["ok","no_item","selection_required","preparation_incomplete"]},
        "direction":{"enum":["incoming","outgoing"]},
        "root":{"type":["string","null"]},
        "nodes":{"type":"array","description":"Nodes in first-seen breadth-first order.","items":{"type":"object","required":["id","item","depth","expansion_status","positions_encoding","positions_base","source_identity"],"properties":{
            "id":{"type":"string"},"item":{"$ref":"#/$defs/HierarchyItem"},
            "depth":{"type":"integer","minimum":0},
            "expansion_status":{"enum":["expanded","depth_boundary","excluded_scope","truncated","failed","unexpanded_budget"],"description":"Expansion outcome; completeness remains unknown."},
            "positions_encoding":{"enum":["utf-8","utf-16","utf-32"]},"positions_base":{"enum":[0,1],"description":"Raw base 0 or normalized UTF-16 base 1."},
            "source_identity":{"oneOf":[{"type":"object","required":["status"],"properties":{"status":{"const":"unavailable"}}},{"type":"object","required":["status","origin","sha256"],"properties":{"status":{"const":"observed"},"origin":{"enum":["tracked","disk"]},"sha256":{"type":"string"}}}],"description":"Hash of already observed complete source, or unavailable."}
        }}},
        "edges":{"type":"array","description":"First-seen caller-to-callee relationships; ranges belong to caller URI.","items":{"type":"object","required":["caller","callee","from_ranges","positions_encoding","positions_base"],"properties":{
            "caller":{"type":"string"},"callee":{"type":"string"},"from_ranges":{"type":"array","items":{"$ref":"#/$defs/Range"},"description":"Distinct caller call sites in first-seen order."},"positions_encoding":{"enum":["utf-8","utf-16","utf-32"]},"positions_base":{"enum":[0,1],"description":"Raw zero-based or normalized one-based coordinates."}}}},
        "items":{"type":"array","items":{"$ref":"#/$defs/HierarchyItem"},"description":"Bounded preparation candidates; present only when preparation stops."},
        "limits":{"type":"object"},"usage":{"type":"object"},
        "warnings":{"type":"array","items":{"type":"string"}},"stop_reasons":{"type":"array","items":{"type":"string"}},
        "truncated":{"type":"boolean","description":"An aggregate budget, bounded admission or replacement omitted known work."},"indexing_in_progress":{"type":"boolean"},"positions_degraded":{"enum":["request","response"],"description":"Worst coordinate conversion degradation; request takes precedence."},
        "positions_encoding":{"enum":["utf-8","utf-16","utf-32"],"description":"Encoding of raw preparation candidates."},
        "positions_base":{"const":0,"description":"Raw preparation candidates use base 0."},
        "resolved_symbol":{"type":"object"},"analysis_freshness":{"const":"unknown"},"atomic_snapshot":{"const":false}
    }});
    schema["$defs"] = json!({"HierarchyItem":hierarchy,"Range":range,"Position2D":point});
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
            let mut count = json!({"type":"integer","minimum":0});
            if field == "usage" {
                let description = usage_description(name);
                if let Some(description) = description {
                    count["description"] = json!(description);
                }
            }
            schema["properties"][field]["properties"][name] = count;
        }
    }
    if let Value::Object(schema) = schema {
        route.attr.output_schema = Some(Arc::new(schema));
    }
}

/// Work counters whose semantics need more than their field names.
fn usage_description(name: &str) -> Option<&'static str> {
    match name {
        "output_bytes" => Some("Final compact UTF-8 bytes, including this count."),
        "omitted_relationships" => Some("Examined relationships omitted; excludes unseen work."),
        "retained_data_bytes" => Some("Opaque JSON bytes per retained identity or candidate."),
        "ranges_examined" => Some("Call-site range visits, including duplicates."),
        "records_examined" => Some("Item and relationship visits, including duplicates."),
        "analysis_requests" => Some("Analysis send attempts, including retries."),
        _ => None,
    }
}
