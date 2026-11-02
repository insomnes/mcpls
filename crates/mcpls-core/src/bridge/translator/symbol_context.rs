//! Primary-file range identity and bounded source observation for context bundles.
#![expect(
    clippy::indexing_slicing,
    reason = "all indexed values are JSON objects constructed in this module"
)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Translator;
use super::dto::{Position, PositionDegradation, Range, Symbol};
use super::symbols::{DocumentSymbolTree, symbol_tree};
use crate::bridge::ClientPath;

const fn key(position: &super::dto::Position2D) -> (u32, u32) {
    (position.line, position.character)
}

fn source_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes.get(cursor).copied().unwrap_or_default();
        cursor = cursor.saturating_add(1);
        if byte == b'\r' {
            if bytes.get(cursor) == Some(&b'\n') {
                cursor = cursor.saturating_add(1);
            }
        } else if byte != b'\n' {
            continue;
        }
        lines.push(text.get(start..cursor).unwrap_or_default());
        start = cursor;
    }
    if start < text.len() || lines.is_empty() {
        lines.push(text.get(start..).unwrap_or_default());
    }
    lines
}

impl Translator {
    pub(crate) async fn context_hover(
        &self,
        path: &ClientPath,
        position: Position,
    ) -> crate::error::Result<Option<super::dto::HoverResult>> {
        let fetched = self
            .position_request::<lsp_types::HoverRequest>(
                path,
                position,
                super::routing::IndexingGate::Required(super::routing::Capability::Hover),
                (),
            )
            .await?;
        let Some(hover) = fetched.result else {
            return Ok(None);
        };
        let range = match hover.range {
            Some(range) => Some(fetched.ctx.normalize_range(fetched.doc.uri(), range).await),
            None => None,
        };
        Ok(Some(super::dto::HoverResult {
            contents: super::navigation::extract_hover_contents(hover.contents),
            range,
            positions_degraded: fetched.ctx.positions_degraded(),
        }))
    }

    pub(crate) async fn context_symbol_range(
        &self,
        path: &ClientPath,
        position: Position,
    ) -> crate::error::Result<(Option<Range>, Option<PositionDegradation>)> {
        let fetched = self.request_document_symbols(path).await?;
        let tree = symbol_tree(fetched.response, &fetched.ctx, fetched.doc.uri()).await;
        let degraded = fetched.ctx.positions_degraded();
        if degraded.is_some() {
            return Ok((None, degraded));
        }
        let DocumentSymbolTree::Hierarchical(symbols) = tree else {
            return Ok((None, None));
        };
        let point = (position.line().get(), position.character().get());
        let mut stack: Vec<&Symbol> = symbols.iter().collect();
        let mut matches = Vec::new();
        while let Some(symbol) = stack.pop() {
            if let Some(children) = &symbol.children {
                stack.extend(children);
            }
            let selection = &symbol.selection_range;
            let range = &symbol.range;
            if key(&range.start) < key(&range.end)
                && key(&selection.start) < key(&selection.end)
                && key(&range.start) <= key(&selection.start)
                && key(&selection.end) <= key(&range.end)
                && key(&selection.start) <= point
                && point < key(&selection.end)
                && range.start.line > 0
                && range.start.character > 0
                && range.end.line > 0
                && range.end.character > 0
            {
                matches.push(symbol);
            }
        }
        let smallest: Vec<_> = matches
            .iter()
            .filter(|candidate| {
                matches.iter().all(|other| {
                    std::ptr::eq(**candidate, *other)
                        || (key(&other.selection_range.start)
                            <= key(&candidate.selection_range.start)
                            && key(&candidate.selection_range.end)
                                <= key(&other.selection_range.end)
                            && candidate.selection_range != other.selection_range)
                })
            })
            .collect();
        Ok((
            if let [symbol] = smallest.as_slice() {
                let converter = crate::bridge::encoding::EncodingConverter::new(
                    crate::bridge::encoding::PositionEncoding::Utf16,
                );
                let valid = [
                    &symbol.range.start,
                    &symbol.range.end,
                    &symbol.selection_range.start,
                    &symbol.selection_range.end,
                ]
                .iter()
                .all(|position| {
                    self.document_tracker
                        .line_text(fetched.doc.path(), position.line.saturating_sub(1))
                        .is_some_and(|text| {
                            converter
                                .character_to_byte_offset(
                                    &text,
                                    position.character.saturating_sub(1),
                                )
                                .is_ok()
                        })
                });
                valid.then(|| symbol.range.clone())
            } else {
                None
            },
            None,
        ))
    }

    pub(crate) async fn context_source(
        &self,
        path: &ClientPath,
        position: Position,
        range: Option<&Range>,
    ) -> Value {
        let unavailable = |reason: &str| json!({"status":"unavailable","reason":reason});
        let Ok(validated) = self.validate_path(path).await else {
            return unavailable("access_denied");
        };
        let now = self.clock.now();
        let deadline = now
            .checked_add(std::time::Duration::from_secs(2))
            .unwrap_or(now);
        // One primary observation uses the existing contained handle and bounded
        // tracked-copy/disk-read accounting, including failed-read reservations.
        let (observation, charge) = self
            .bounded_source_file(validated, 4_194_304, deadline, 1_048_576, None)
            .await;
        let (text, origin) = match observation {
            Ok(value) => value,
            Err(reason) => {
                return json!({"status":"unavailable","reason":reason,"source_acquisition_bytes":charge});
            }
        };
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let lines = source_lines(&text);
        let target = position.line().get().saturating_sub(1) as usize;
        let (first, end) = range.map_or_else(
            || {
                let first = target.saturating_sub(2).min(lines.len().saturating_sub(5));
                (first, first.saturating_add(5).min(lines.len()))
            },
            |range| {
                (
                    range.start.line.saturating_sub(1) as usize,
                    range
                        .end
                        .line
                        .saturating_sub(u32::from(range.end.character == 1))
                        as usize,
                )
            },
        );
        if first > target || target >= end || end > lines.len() {
            return unavailable("range_unavailable");
        }
        let redactions = self.server_text_redactions();
        let selected = lines
            .iter()
            .take(end)
            .skip(first)
            .copied()
            .collect::<String>();
        let displayed = redactions.apply(&selected);
        let redacted = displayed.as_ref() != selected;
        let displayed_lines = source_lines(&displayed);
        let mut result = String::new();
        let mut retained = first;
        for line in &displayed_lines {
            if result.len().saturating_add(line.len()) > 16_384 {
                break;
            }
            result.push_str(line);
            retained = retained.saturating_add(1);
        }
        let output_limited = result.len() < displayed.len();
        if output_limited
            && (retained <= target || displayed_lines.len() != end.saturating_sub(first))
        {
            return unavailable("text_budget");
        }
        let limited = output_limited;
        let mut section = json!({"status":if range.is_some() && !limited {"ok"} else {"partial"},
            "data":{"file_path":path.as_path(),"text":result,"start_line":first.saturating_add(1),"origin":origin,
            "sha256":hash,"completeness":if range.is_some() && !limited {"complete_symbol"} else {"excerpt"},"redacted":redacted},
            "source_acquisition_bytes":charge});
        if limited {
            section["reason"] = json!("text_budget");
        } else if range.is_none() {
            section["reason"] = json!("range_unavailable");
        }
        section
    }
}
