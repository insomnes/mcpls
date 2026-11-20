//! Retention and MCP projection for selected completions.

use std::collections::VecDeque;
use std::sync::Arc;

use rmcp::model::JsonObject;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::{McpError, McplsServer};
use crate::bridge::{Completion, CompletionOrigin, CompletionsResult};
use crate::redaction::{Redactions, ServerText};

/// At most 4 MiB of provider item JSON per MCP session, across workspaces.
const MAX_RETAINED_COMPLETIONS: usize = 128;

const MAX_COMPLETION_REF_BYTES: usize = 256;

#[derive(Default)]
pub(super) struct CompletionRefs {
    entries: VecDeque<(String, Arc<SelectedCompletion>)>,
}

pub(super) struct SelectedCompletion {
    pub(super) origin: Arc<CompletionOrigin>,
    pub(super) item: Box<[u8]>,
}

impl CompletionRefs {
    pub(super) fn retain(&mut self, reference: String, selection: SelectedCompletion) {
        if self.entries.len() == MAX_RETAINED_COMPLETIONS {
            self.entries.pop_front();
        }
        self.entries.push_back((reference, Arc::new(selection)));
    }

    pub(super) fn get(&self, reference: &str) -> Option<Arc<SelectedCompletion>> {
        self.entries
            .iter()
            .find_map(|(key, selection)| (key == reference).then(|| Arc::clone(selection)))
    }
}

#[derive(Serialize, JsonSchema)]
pub(super) struct ListedCompletion {
    #[serde(flatten)]
    pub(super) display: Completion,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("type" = "string"))]
    pub(super) completion_ref: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct ResolveCompletionResult {
    status: ResolveStatus,
    #[schemars(required, extend("type" = ["object", "null"]))]
    item: Option<Completion>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
enum ResolveStatus {
    Resolved,
    Unsupported,
}

impl ResolveCompletionResult {
    pub(super) const fn new(item: Option<Completion>) -> Self {
        let status = if item.is_some() {
            ResolveStatus::Resolved
        } else {
            ResolveStatus::Unsupported
        };
        Self { status, item }
    }
}

pub(super) fn validate_resolve_arguments(arguments: &JsonObject) -> Result<(), McpError> {
    if arguments.len() != 2
        || arguments.get("file_path").and_then(Value::as_str).is_none()
        || !arguments
            .get("completion_ref")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.trim().is_empty() && value.len() <= MAX_COMPLETION_REF_BYTES
            })
    {
        return Err(McpError::invalid_params(
            "resolve_completion requires file_path and a valid completion_ref",
            None,
        ));
    }
    Ok(())
}

pub(super) fn stale_reference() -> McpError {
    McpError::invalid_params(
        "completion_ref is stale; call get_completions again with include_completion_refs",
        Some(serde_json::json!({"code": "stale_completion_ref"})),
    )
}

pub(super) fn bound_display(item: &mut Completion, redactions: &Redactions) {
    item.redact_server_text(redactions);
    for (field, value, maximum) in [
        ("label", Some(&mut item.label), 512),
        ("detail", item.detail.as_mut(), 2048),
        ("documentation", item.documentation.as_mut(), 4096),
    ] {
        if let Some(value) = value.filter(|value| value.len() > maximum) {
            let end = value.floor_char_boundary(maximum);
            value.truncate(end);
            item.truncated_fields.push(field.to_owned());
        }
    }
}

impl McplsServer {
    pub(super) fn bound_completion_list(
        &self,
        result: &mut CompletionsResult<ListedCompletion>,
    ) -> Result<(), McpError> {
        let redactions = self.context.translator.server_text_redactions();
        for item in &mut result.items {
            bound_display(&mut item.display, &redactions);
        }
        loop {
            result.truncated = result.omitted_count > 0
                || result
                    .items
                    .iter()
                    .any(|item| !item.display.truncated_fields.is_empty());
            let bytes = serde_json::to_vec(result)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            if bytes.len() <= 65536 {
                return Ok(());
            }
            // Each bounded item is larger than any omission-count digit increase.
            // Removing the tail therefore finds the longest fitting provider prefix.
            let _ = result.items.pop();
            result.omitted_count = result.omitted_count.saturating_add(1);
        }
    }
}
