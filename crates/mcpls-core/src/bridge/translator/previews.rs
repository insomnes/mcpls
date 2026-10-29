//! Bounded, proposal-only diffs against one immutable source observation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use similar::TextDiff;

use super::Translator;
use super::dto::{
    AnalysisFreshness, DocumentChanges, EditPreview, PositionDegradation, PreviewSource,
    PreviewStatus, Range, SuppliedVersion, TextEdit,
};
use super::encoding_ctx::EncodingCtx;
use crate::bridge::ClientPath;
use crate::bridge::encoding::{ColumnFidelity, lsp_to_mcp_position};
use crate::bridge::state::DocumentText;
use crate::config::ServerId;

const WORK_BYTES: usize = 8 * 1024 * 1024;
const DISPLAY_BYTES: usize = 64 * 1024;

#[derive(Debug, Default, Clone)]
#[cfg(test)]
pub(super) struct PreviewWork {
    pub files: usize,
    pub read_bytes: usize,
    pub tracked_bytes: usize,
    pub constructed_bytes: usize,
    pub output_bytes: usize,
    pub source_starts: usize,
    pub chunk_starts: usize,
    pub diff_jobs: usize,
}

#[derive(Clone)]
struct Observation {
    text: Arc<DocumentText>,
    identity: PreviewSource,
}

/// Shared source cache and budgets for every edit in one response.
pub(super) struct Previews<'a> {
    translator: &'a Translator,
    server: &'a ServerId,
    sources: HashMap<PathBuf, Result<Observation, String>>,
    remaining: usize,
    display_left: usize,
    acquisition_deadline: Instant,
    diff_deadline: Option<Instant>,
    full: bool,
}

fn unavailable(reason: impl Into<String>) -> EditPreview {
    EditPreview {
        status: PreviewStatus::Unavailable,
        diff: None,
        reason: Some(reason.into()),
        redacted: false,
    }
}

impl<'a> Previews<'a> {
    pub(super) fn new(translator: &'a Translator, server: &'a ServerId, full: bool) -> Self {
        let now = translator.clock.now();
        Self {
            translator,
            server,
            sources: HashMap::new(),
            remaining: WORK_BYTES,
            display_left: DISPLAY_BYTES,
            acquisition_deadline: now.checked_add(Duration::from_secs(2)).unwrap_or(now),
            diff_deadline: None,
            full,
        }
    }

    async fn source(&mut self, uri: &lsp_types::Uri) -> Result<Observation, String> {
        if self.translator.clock.now() >= self.acquisition_deadline {
            return Err("deadline".into());
        }
        let path = crate::bridge::state::uri_to_path(uri).ok_or("access_denied")?;
        let client = ClientPath::try_from(path.clone()).map_err(|_| "access_denied")?;
        let validated = self.translator.workspace_roots.validate(&client).await;
        let key = validated
            .as_ref()
            .map_or(path, |path| path.as_path().to_path_buf());
        if let Some(source) = self.sources.get(&key) {
            return source.clone();
        }
        if self.sources.len() >= 32 {
            return Err("file_limit".into());
        }
        if self.remaining == 0 {
            return Err("source_budget".into());
        }
        if self.translator.clock.now() >= self.acquisition_deadline {
            return Err("deadline".into());
        }
        let Ok(validated) = validated else {
            self.remaining = self.remaining.saturating_sub(1);
            let result = Err("read_failed".into());
            self.sources.insert(key, result.clone());
            #[cfg(test)]
            {
                crate::util::lock_std(&self.translator.preview_work).files += 1;
            }
            return result;
        };
        let (result, charge) = self
            .translator
            .bounded_source_file(
                validated,
                self.remaining,
                self.acquisition_deadline,
                WORK_BYTES,
                Some(self.server),
            )
            .await;
        self.remaining = self.remaining.saturating_sub(charge);
        #[cfg(test)]
        {
            crate::util::lock_std(&self.translator.preview_work).files += 1;
        }
        let result = match result {
            Ok((text, origin)) => {
                let identity = PreviewSource {
                    origin,
                    sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
                };
                DocumentText::build(text)
                    .await
                    .map(|text| Observation {
                        identity,
                        text: Arc::new(text),
                    })
                    .map_err(|_| "source_index_failed".into())
            }
            Err(reason) => Err(serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "read_failed".into())),
        };
        self.sources.insert(key, result.clone());
        result
    }

    /// Convert ranges using exactly the buffer later used for hashing and diffing.
    pub(super) async fn document(
        &mut self,
        uri: lsp_types::Uri,
        edits: Vec<lsp_types::TextEdit>,
        version: Option<SuppliedVersion>,
        ctx: &EncodingCtx,
    ) -> DocumentChanges {
        let source = self.source(&uri).await;
        let converted = edits
            .into_iter()
            .map(|edit| TextEdit {
                range: normalize(edit.range, source.as_ref().ok().map(|s| &*s.text), ctx),
                new_text: edit.new_text,
            })
            .collect::<Vec<_>>();
        let preview = if ctx.positions_degraded().is_some() {
            unavailable("positions_degraded")
        } else if let Ok(source) = &source {
            self.render(source, &converted).await
        } else {
            unavailable(
                source
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| "read_failed".into()),
            )
        };
        DocumentChanges {
            uri: uri.to_string(),
            edit_count: converted.len(),
            preview,
            preview_source: source.ok().map(|s| s.identity),
            supplied_version: version,
            analysis_freshness: AnalysisFreshness::Unknown,
            edits: self.full.then_some(converted),
        }
    }

    async fn render(&mut self, source: &Observation, edits: &[TextEdit]) -> EditPreview {
        let mut ranges = Vec::with_capacity(edits.len());
        let mut size = source.text.as_str().len();
        for edit in edits {
            if self.translator.clock.now() >= self.acquisition_deadline {
                return unavailable("deadline");
            }
            let Some(start) = offset(
                &source.text,
                edit.range.start.line,
                edit.range.start.character,
            ) else {
                return unavailable("invalid_range");
            };
            let Some(end) = offset(&source.text, edit.range.end.line, edit.range.end.character)
            else {
                return unavailable("invalid_range");
            };
            if start > end {
                return unavailable("invalid_range");
            }
            size = size
                .saturating_sub(end.saturating_sub(start))
                .saturating_add(edit.new_text.len());
            ranges.push((start, end, &edit.new_text));
        }
        // Stable ordering preserves the LSP ordering of equal-position insertions.
        ranges.sort_by_key(|(start, end, _)| (*start, *end));
        let mut previous_end = 0;
        for (start, end, _) in &ranges {
            if *start < previous_end {
                return unavailable("overlapping_edits");
            }
            previous_end = *end;
        }
        if size > self.remaining {
            return unavailable("construction_budget");
        }
        let mut constructed = String::with_capacity(size);
        let mut cursor = 0;
        for (start, end, replacement) in ranges {
            constructed.push_str(source.text.as_str().get(cursor..start).unwrap_or_default());
            constructed.push_str(replacement);
            cursor = end;
        }
        constructed.push_str(source.text.as_str().get(cursor..).unwrap_or_default());
        self.remaining = self.remaining.saturating_sub(constructed.len());
        #[cfg(test)]
        {
            crate::util::lock_std(&self.translator.preview_work).constructed_bytes +=
                constructed.len();
        }
        let now = Instant::now();
        let deadline = *self
            .diff_deadline
            .get_or_insert_with(|| now.checked_add(Duration::from_millis(100)).unwrap_or(now));
        if now >= deadline {
            return unavailable("diff_deadline");
        }
        let old = source.text.clone();
        #[cfg(test)]
        {
            crate::util::lock_std(&self.translator.preview_work).diff_jobs += 1;
        }
        let result = tokio::task::spawn_blocking(move || {
            TextDiff::configure()
                .deadline(deadline)
                .diff_lines(old.as_str(), constructed.as_str())
                .unified_diff()
                .context_radius(3)
                .header("observed", "proposed")
                .to_string()
        })
        .await;
        let Ok(diff) = result else {
            return unavailable("diff_failed");
        };
        let redacted = self
            .translator
            .server_text_redactions()
            .apply(&diff)
            .into_owned();
        let changed = redacted != diff;
        let mut preview = EditPreview {
            status: PreviewStatus::Complete,
            diff: Some(redacted),
            reason: None,
            redacted: changed,
        };
        limit_one(&mut preview, &mut self.display_left);
        #[cfg(test)]
        {
            crate::util::lock_std(&self.translator.preview_work).output_bytes =
                DISPLAY_BYTES.saturating_sub(self.display_left);
        }
        preview
    }
}

fn normalize(range: lsp_types::Range, source: Option<&DocumentText>, ctx: &EncodingCtx) -> Range {
    let convert = |position: lsp_types::Position| {
        let text = source.and_then(|text| text.line(position.line));
        let converted = lsp_to_mcp_position(position, text, ctx.encoding);
        if converted.fidelity == ColumnFidelity::PassedThrough {
            ctx.record_degradation(PositionDegradation::Response);
        }
        converted.value
    };
    Range {
        start: convert(range.start),
        end: convert(range.end),
    }
}

fn offset(text: &DocumentText, row: u32, column: u32) -> Option<usize> {
    let row = row.checked_sub(1)?;
    let start = text.line_start(row)?;
    let content = text.line(row)?;
    let target = column.checked_sub(1)?;
    let mut units = 0;
    for (byte, character) in content.char_indices() {
        if units == target {
            return start.checked_add(byte);
        }
        units = units.checked_add(u32::try_from(character.len_utf16()).ok()?)?;
        if units > target {
            return None;
        }
    }
    (units == target)
        .then(|| start.checked_add(content.len()))
        .flatten()
}

fn limit_one(preview: &mut EditPreview, left: &mut usize) {
    if let Some(diff) = &mut preview.diff {
        if diff.len() > *left {
            let mut end = *left;
            while !diff.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            diff.truncate(end);
            preview.status = PreviewStatus::Truncated;
            preview.reason = Some("display_budget".into());
        }
        *left = left.saturating_sub(diff.len());
    }
}

/// Recheck after the final configured-secret redaction, which can expand text.
pub(super) fn limit_display<'a>(documents: impl Iterator<Item = &'a mut DocumentChanges>) {
    let mut left = DISPLAY_BYTES;
    for document in documents {
        limit_one(&mut document.preview, &mut left);
    }
}

/// A later conversion failure invalidates earlier diffs in the same response.
pub(super) fn suppress_degraded<'a>(
    documents: impl Iterator<Item = &'a mut DocumentChanges>,
    warning: Option<PositionDegradation>,
) {
    if warning.is_some() {
        for document in documents {
            document.preview = unavailable("positions_degraded");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::BufReader;

    use super::super::testing::*;
    use crate::bridge::{ClientPath, TabSize};
    use crate::config::ServerId;
    use crate::redaction::Redactions;

    async fn formatting(source: &str, edits: Value, full: bool, redact: bool) -> Value {
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("preview.rs");
        std::fs::write(&path, source).unwrap();
        let (translator, mut server) = translator_with_capabilities(
            &dir,
            &ServerId::from_static("rust"),
            lsp_types::ServerCapabilities {
                document_formatting_provider: Some(lsp_types::DocumentFormattingProvider::Bool(
                    true,
                )),
                ..Default::default()
            },
        );
        let translator = if redact {
            translator.with_startup_redactions(Arc::new(Redactions::new([(
                "API_TOKEN".into(),
                "abcdefgh".into(),
            )])))
        } else {
            translator
        };
        let handle = tokio::spawn(async move {
            translator
                .handle_format_document_with_options(
                    ClientPath::try_from(path).unwrap(),
                    TabSize::default(),
                    true,
                    full,
                )
                .await
                .unwrap()
        });
        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/formatting");
        write_response(&mut server.read_half_stdin, &request["id"], edits).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(canonical_dir(&dir).join("preview.rs")).unwrap(),
            source
        );
        serde_json::to_value(result).unwrap()
    }

    fn edit(start: u32, end: u32, replacement: &str) -> Value {
        json!({"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"newText":replacement})
    }

    // A default proposal shows the change while keeping raw edits opt-in.
    #[tokio::test]
    async fn previews_default_diff_and_exact_raw_opt_in() {
        for full in [false, true] {
            let value = formatting("old\n", json!([edit(0, 3, "new")]), full, false).await;
            let doc = &value["changes"][0];
            assert_eq!(doc["edit_count"], 1);
            assert_eq!(doc["analysis_freshness"], "unknown");
            assert_eq!(doc["preview"]["status"], "complete");
            let diff = doc["preview"]["diff"].as_str().unwrap();
            assert!(diff.contains("-old\n+new\n"));
            assert_eq!(doc.get("edits").is_some(), full);
            if full {
                assert_eq!(doc["edits"][0]["new_text"], "new");
            }
            assert_eq!(doc["preview_source"]["origin"], "tracked");
            assert!(doc.get("supplied_version").is_none());
        }
    }

    // Equal-position insertions preserve server order, including at Unicode EOF.
    #[tokio::test]
    async fn previews_preserve_insertion_order_unicode_and_newline_bytes() {
        for newline in ["\n", "\r\n", "\r", ""] {
            let value = formatting(
                &format!("😀old{newline}"),
                json!([edit(2, 2, "A"), edit(2, 2, "B"), edit(2, 5, "new")]),
                false,
                false,
            )
            .await;
            let diff = value["changes"][0]["preview"]["diff"].as_str().unwrap();
            assert!(diff.contains("-😀old"));
            assert!(diff.contains("+😀ABnew"));
            if newline.is_empty() {
                assert!(diff.contains("\\ No newline at end of file"));
            }
        }
    }

    // Invalid edits retain exact requested raw edits but cannot produce a reliable diff.
    #[tokio::test]
    async fn previews_reject_overlap_and_invalid_unicode_offsets() {
        for edits in [
            json!([edit(0, 3, "one"), edit(1, 3, "two")]),
            json!([edit(1, 2, "split")]),
        ] {
            let value = formatting("😀old", edits, true, false).await;
            let doc = &value["changes"][0];
            assert_eq!(doc["preview"]["status"], "unavailable");
            assert!(doc["preview"].get("diff").is_none());
            assert_ne!(doc["edits"].as_array().unwrap().len(), 0);
            assert!(doc.get("preview_source").is_some());
        }
    }

    // Secret redaction changes display only; full output retains the exact replacement.
    #[tokio::test]
    async fn previews_redact_display_and_preserve_full_edits() {
        let value = formatting("old", json!([edit(0, 3, "abcdefgh")]), true, true).await;
        let doc = &value["changes"][0];
        assert_eq!(doc["preview"]["redacted"], true);
        assert!(
            !doc["preview"]["diff"]
                .as_str()
                .unwrap()
                .contains("abcdefgh")
        );
        assert_eq!(doc["edits"][0]["new_text"], "abcdefgh");
    }
}
