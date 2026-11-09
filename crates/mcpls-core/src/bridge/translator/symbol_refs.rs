//! Content-bound targets for read-only semantic requests.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::Translator;
use super::dto::{Position, Position2D, SymbolTargetEnvelope, SymbolTargetUnavailable};
use crate::bridge::ClientPath;
use crate::error::{Error, Result, SymbolResolutionData};

const MAX_REFERENCE_BYTES: usize = 8192;
const MAX_SOURCE_BYTES: usize = 1_048_576;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySymbolRef {
    version: u8,
    context: String,
    file: std::path::PathBuf,
    line: u32,
    character: u32,
    sha256: String,
}

enum SymbolRef {
    Legacy(LegacySymbolRef),
    Compact((u8, String, String, u32, u32)),
}

fn lowercase_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn rejected(kind: &str, message: &str) -> Error {
    Error::SymbolResolution(Box::new(SymbolResolutionData::Reference {
        kind: kind.into(),
        message: message.into(),
    }))
}

fn source_error(error: Error) -> Error {
    match error {
        Error::FileIo { .. } | Error::MalformedPath { .. } => {
            rejected("unverifiable_target", "Current source is unavailable")
        }
        other => other,
    }
}

fn valid_point(text: &str, position: Position) -> bool {
    let Some(line) = super::symbol_context::source_lines(text)
        .get(position.line().get().saturating_sub(1) as usize)
        .copied()
    else {
        return false;
    };
    let line = line.trim_end_matches(['\r', '\n']);
    let wanted = position.character().get().saturating_sub(1) as usize;
    let mut column = 0usize;
    for character in line.chars() {
        if column == wanted {
            return true;
        }
        column = column.saturating_add(character.len_utf16());
    }
    column == wanted
}

impl Translator {
    /// Runtime identity for constructing independent legacy-reference test fixtures.
    #[cfg(test)]
    pub(crate) fn symbol_ref_test_context(&self) -> &str {
        &self.symbol_ref_context
    }

    /// Read fresh contained bytes, independently of tracked text and timestamps.
    async fn symbol_ref_source(
        &self,
        path: &ClientPath,
        charge: &mut usize,
    ) -> Result<(String, std::path::PathBuf)> {
        let validated = self.validate_path(path).await.map_err(source_error)?;
        let canonical = validated.as_path().to_path_buf();
        let now = self.clock.now();
        let deadline = now
            .checked_add(std::time::Duration::from_secs(2))
            .unwrap_or(now);
        let (observation, acquired) = self
            .bounded_disk_source(
                validated,
                MAX_SOURCE_BYTES + 1,
                deadline,
                MAX_SOURCE_BYTES,
                None,
            )
            .await;
        *charge = acquired;
        let (text, _) = observation.map_err(|_| {
            rejected(
                "unverifiable_target",
                "Cannot acquire complete current UTF-8 source within the 1 MiB limit",
            )
        })?;
        Ok((text, canonical))
    }

    pub(super) async fn validate_symbol_ref(
        &self,
        path: &ClientPath,
        raw: &str,
    ) -> Result<(Position, usize)> {
        // Authorization derives only from the caller's path and current roots.
        let validated = self.validate_path(path).await.map_err(source_error)?;
        let invalid = || {
            rejected(
                "invalid_symbol_ref",
                "Malformed, oversized, or unsupported symbol_ref; resolve the target afresh",
            )
        };
        if raw.is_empty() || raw.len() > MAX_REFERENCE_BYTES {
            return Err(invalid());
        }
        // Select the JSON root shape before deserialization: structs also accept
        // sequences in Serde, but legacy references must remain objects.
        let reference = match raw.trim_start().as_bytes().first() {
            Some(b'{') => serde_json::from_str(raw).map(SymbolRef::Legacy),
            Some(b'[') => serde_json::from_str(raw).map(SymbolRef::Compact),
            _ => return Err(invalid()),
        }
        .map_err(|_| invalid())?;
        let (line, character, sha256) = match &reference {
            SymbolRef::Legacy(reference)
                if reference.version == 1
                    && !reference.context.is_empty()
                    && reference.file.is_absolute()
                    && !reference.file.as_os_str().as_encoded_bytes().contains(&0)
                    && reference.sha256.len() == 64
                    && reference.sha256.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                (reference.line, reference.character, &reference.sha256)
            }
            SymbolRef::Compact((2, binding, sha256, line, character))
                if lowercase_digest(binding) && lowercase_digest(sha256) =>
            {
                (*line, *character, sha256)
            }
            _ => return Err(invalid()),
        };
        let position = Position::from_client(line, character).map_err(|_| invalid())?;
        let expected_file = validated.as_path().to_path_buf();
        let binding_matches = match &reference {
            SymbolRef::Legacy(reference) => {
                reference.context == self.symbol_ref_context && reference.file == expected_file
            }
            SymbolRef::Compact((_, binding, _, _, _)) => {
                *binding
                    == self
                        .symbol_ref_binding(&expected_file)
                        .map_err(|_| invalid())?
            }
        };
        if !binding_matches {
            return Err(rejected(
                "symbol_ref_context_mismatch",
                "symbol_ref belongs to a different file or runtime context; resolve the target afresh",
            ));
        }
        let mut charge = 0;
        let (text, canonical) = self.symbol_ref_source(path, &mut charge).await?;
        if canonical != expected_file {
            return Err(rejected(
                "unverifiable_target",
                "The file target changed during reference validation",
            ));
        }
        if format!("{:x}", Sha256::digest(text.as_bytes())) != *sha256 {
            return Err(rejected(
                "stale_target",
                "Source bytes changed since observation; resolve the target afresh",
            ));
        }
        if !valid_point(&text, position) {
            return Err(rejected(
                "unverifiable_target",
                "The target is not an exact UTF-16 position",
            ));
        }
        Ok((position, charge))
    }

    /// Enrich only retained discovery entries, with a separate fixed acquisition budget.
    pub(crate) async fn portable_targets(
        &self,
        entries: impl IntoIterator<Item = (String, Position2D)>,
        unavailable: Option<&'static str>,
    ) -> Vec<std::result::Result<SymbolTargetEnvelope, SymbolTargetUnavailable>> {
        let mut remaining = 4_194_304usize;
        let mut sources = std::collections::HashMap::new();
        let now = self.clock.now();
        let deadline = now
            .checked_add(std::time::Duration::from_secs(2))
            .unwrap_or(now);
        let mut results = Vec::new();
        for (index, (uri, point)) in entries.into_iter().enumerate() {
            let result = async {
                if index >= 16 || remaining == 0 {
                    return Err("budget_exhausted");
                }
                if let Some(reason) = unavailable {
                    return Err(reason);
                }
                let uri: lsp_types::Uri = serde_json::from_value(serde_json::Value::String(uri))
                    .map_err(|_| "non_file_uri")?;
                let path = crate::bridge::state::uri_to_path(&uri).ok_or("non_file_uri")?;
                if !sources.contains_key(&path) {
                    let source = async {
                        let client = ClientPath::try_from(path.clone())
                            .map_err(|_| "source_unverifiable")?;
                        let validated = self
                            .validate_path(&client)
                            .await
                            .map_err(|_| "source_unverifiable")?;
                        let canonical = validated.as_path().to_path_buf();
                        let (observation, acquired) = self
                            .bounded_disk_source(
                                validated,
                                remaining,
                                deadline,
                                MAX_SOURCE_BYTES,
                                None,
                            )
                            .await;
                        remaining = remaining.saturating_sub(acquired);
                        let (text, _) = observation.map_err(|reason| match reason {
                            super::dto::SourceUnavailableReason::SourceBudget => "budget_exhausted",
                            _ => "source_unverifiable",
                        })?;
                        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
                        Ok::<_, &'static str>((text, canonical, hash))
                    }
                    .await;
                    sources.insert(path.clone(), source);
                }
                let (text, canonical, hash) = sources
                    .get(&path)
                    .ok_or("source_unverifiable")?
                    .as_ref()
                    .map_err(|reason| *reason)?;
                let position = Position::from_client(point.line, point.character)
                    .map_err(|_| "position_unverifiable")?;
                if !valid_point(text, position) {
                    return Err("position_unverifiable");
                }
                let raw = self.encode_symbol_ref(canonical, position, hash.clone())?;
                Ok(SymbolTargetEnvelope {
                    file_path: canonical.clone(),
                    symbol_ref: raw,
                })
            }
            .await;
            results.push(result.map_err(|reason| SymbolTargetUnavailable {
                reason: reason.into(),
            }));
        }
        results
    }

    fn symbol_ref_binding(&self, file: &std::path::Path) -> serde_json::Result<String> {
        let input = serde_json::to_vec(&("mcpls-symbol-ref-v2", &self.symbol_ref_context, file))?;
        Ok(format!("{:x}", Sha256::digest(input)))
    }

    fn encode_symbol_ref(
        &self,
        file: &std::path::Path,
        position: Position,
        sha256: String,
    ) -> std::result::Result<String, &'static str> {
        let binding = self
            .symbol_ref_binding(file)
            .map_err(|_| "reference_encoding")?;
        let reference = (
            2,
            binding,
            sha256,
            position.line().get(),
            position.character().get(),
        );
        let raw = serde_json::to_string(&reference).map_err(|_| "reference_encoding")?;
        if raw.len() > MAX_REFERENCE_BYTES {
            return Err("reference_too_large");
        }
        Ok(raw)
    }

    pub(crate) async fn issue_symbol_ref(
        &self,
        path: &ClientPath,
        position: Position,
        observed_hash: &str,
    ) -> (std::result::Result<String, &'static str>, usize) {
        let mut charge = 0;
        let result = async {
            let (text, canonical) = self
                .symbol_ref_source(path, &mut charge)
                .await
                .map_err(|_| "source_unverifiable")?;
            let sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
            if sha256 != observed_hash {
                return Err("source_changed");
            }
            if !valid_point(&text, position) {
                return Err("position_unverifiable");
            }
            self.encode_symbol_ref(&canonical, position, sha256)
        }
        .await;
        (result, charge)
    }
}
