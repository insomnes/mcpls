//! Bounded source context and lexical selection for references only.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::Translator;
use super::dto::{
    ReferenceLocation, ReferenceSource, SourceOrigin, SourceUnavailableReason as Missing,
};
use crate::bridge::{ClientPath, WorkspacePath};
use crate::redaction::Redactions;

const FILE_BYTES: usize = 1024 * 1024;
const SOURCE_BYTES: usize = 4 * FILE_BYTES;
const EXCERPT_BYTES: usize = 2048;
const OUTPUT_BYTES: usize = 64 * 1024;

/// Selection and source options for a references request.
#[derive(Debug, Clone)]
pub struct ReferenceOptions {
    /// Maximum returned matches, between 1 and 10000 inclusive.
    pub limit: usize,
    /// Absolute lexical path selecting a file or directory subtree.
    pub path_filter: Option<PathBuf>,
    /// Attach bounded source outcomes to retained locations.
    pub include_source: bool,
}

impl Default for ReferenceOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            path_filter: None,
            include_source: true,
        }
    }
}

/// Normalize components without filesystem access or symlink resolution.
pub(super) fn lexical_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Some(result)
}

impl ReferenceOptions {
    pub(super) fn matches(&self, uri: &lsp_types::Uri) -> bool {
        self.path_filter.as_ref().is_none_or(|filter| {
            crate::bridge::state::uri_to_path(uri)
                .and_then(|path| lexical_path(&path))
                .is_some_and(|path| path.starts_with(filter))
        })
    }
}

#[derive(Debug, Default, Clone)]
#[cfg(test)]
pub(super) struct ReferenceWork {
    pub normalized: usize,
    pub read_bytes: usize,
    pub tracked_bytes: usize,
    pub source_bytes: usize,
    pub files: usize,
    pub output_bytes: usize,
}

const fn unavailable(reason: Missing) -> ReferenceSource {
    ReferenceSource::Unavailable { reason }
}

impl Translator {
    pub(super) async fn reference_sources(&self, locations: &mut [ReferenceLocation]) {
        let start = self.clock.now();
        let deadline = start.checked_add(Duration::from_secs(2)).unwrap_or(start);
        let mut sources: HashMap<PathBuf, Result<(String, SourceOrigin), Missing>> = HashMap::new();
        let mut source_left = SOURCE_BYTES;
        let mut output_left = OUTPUT_BYTES;
        let redactions = self.server_text_redactions();
        for (index, location) in locations.iter_mut().enumerate() {
            let outcome = if index >= 100 {
                unavailable(Missing::CandidateLimit)
            } else if self.clock.now() >= deadline {
                unavailable(Missing::Deadline)
            } else if let Some(path) = url::Url::parse(&location.uri)
                .ok()
                .and_then(|uri| uri.to_file_path().ok())
            {
                // Share snapshots by the existing canonical access identity.
                // Filter matching above remains entirely lexical.
                let (key, validated) = if sources.contains_key(&path) {
                    (path, None)
                } else if let Ok(client) = ClientPath::try_from(path.clone()) {
                    match self.workspace_roots.validate(&client).await {
                        Ok(validated) => (validated.as_path().to_path_buf(), Some(validated)),
                        Err(_) => (path, None),
                    }
                } else {
                    (path, None)
                };
                if !sources.contains_key(&key) {
                    if sources.len() >= 16 {
                        location.source = Some(unavailable(Missing::FileLimit));
                        continue;
                    }
                    let (source, charge) = if let Some(validated) = validated {
                        self.reference_source_file(validated, source_left, deadline)
                            .await
                    } else if source_left == 0 {
                        (Err(Missing::SourceBudget), 0)
                    } else {
                        (Err(Missing::AccessDenied), 1)
                    };
                    source_left = source_left.saturating_sub(charge);
                    #[cfg(test)]
                    {
                        let mut work = crate::util::lock_std(&self.reference_work);
                        work.source_bytes += charge;
                        work.files += 1;
                    }
                    sources.insert(key.clone(), source);
                }
                if self.clock.now() >= deadline {
                    unavailable(Missing::Deadline)
                } else {
                    match sources.get(&key) {
                        Some(Ok((text, origin))) => excerpt(
                            text,
                            location.range.start.line,
                            *origin,
                            &mut output_left,
                            &redactions,
                        ),
                        Some(Err(reason)) => unavailable(*reason),
                        None => unavailable(Missing::ReadFailed),
                    }
                }
            } else {
                unavailable(Missing::AccessDenied)
            };
            location.source = Some(outcome);
        }
        #[cfg(test)]
        {
            crate::util::lock_std(&self.reference_work).output_bytes = OUTPUT_BYTES - output_left;
        }
    }

    async fn reference_source_file(
        &self,
        validated: WorkspacePath,
        remaining: usize,
        deadline: std::time::Instant,
    ) -> (Result<(String, SourceOrigin), Missing>, usize) {
        if remaining == 0 {
            return (Err(Missing::SourceBudget), 0);
        }
        if self.clock.now() >= deadline {
            return (Err(Missing::Deadline), 1);
        }
        let max = FILE_BYTES.min(remaining);
        if let Some(snapshot) = self.document_tracker.bounded_text(validated.as_path(), max) {
            return match snapshot {
                Ok(text) => {
                    let charge = text.len();
                    (Ok((text, SourceOrigin::Tracked)), charge)
                }
                Err(()) => (
                    Err(if max < FILE_BYTES {
                        Missing::SourceBudget
                    } else {
                        Missing::FileTooLarge
                    }),
                    1,
                ),
            };
        }
        let roots = self.workspace_roots.clone();
        let clock = self.clock.clone();
        // Keep the contained handle for the read. Never reopen a validated path.
        let read_cap = remaining.min(FILE_BYTES + 1);
        #[cfg(test)]
        let hook = self.reference_read_hook;
        #[cfg(test)]
        let work = self.reference_work.clone();
        #[cfg(test)]
        let fail_after = self.reference_io_failure_after;
        let result = tokio::task::spawn_blocking(move || {
            if clock.now() >= deadline {
                return Err(Missing::Deadline);
            }
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(validated.as_path(), false);
            }
            let file = roots
                .open_contained(&validated)
                .map_err(|_| Missing::ReadFailed)?;
            let (file, metadata) = file.into_parts();
            if metadata.len() > FILE_BYTES as u64 {
                return Err(Missing::FileTooLarge);
            }
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(validated.as_path(), true);
            }
            if clock.now() >= deadline {
                return Err(Missing::Deadline);
            }
            let mut bytes = Vec::new();
            let mut file = file.take(read_cap as u64);
            let mut chunk = [0; 8192];
            loop {
                #[cfg(test)]
                if fail_after.is_some_and(|limit| bytes.len() >= limit) {
                    return Err(Missing::ReadFailed);
                }
                let read = file.read(&mut chunk).map_err(|_| Missing::ReadFailed)?;
                if read == 0 {
                    break;
                }
                #[cfg(test)]
                {
                    crate::util::lock_std(&work).read_bytes += read;
                }
                bytes.extend_from_slice(chunk.get(..read).unwrap_or_default());
            }
            Ok(bytes)
        })
        .await;
        match result {
            Ok(Ok(bytes)) => {
                let charge = bytes.len();
                if charge > FILE_BYTES {
                    return (Err(Missing::FileTooLarge), charge);
                }
                // Without room for lookahead, EOF cannot be established safely.
                if charge == read_cap && read_cap <= FILE_BYTES {
                    return (Err(Missing::SourceBudget), charge);
                }
                (
                    String::from_utf8(bytes)
                        .map(|text| (text, SourceOrigin::Disk))
                        .map_err(|_| Missing::InvalidUtf8),
                    charge,
                )
            }
            Ok(Err(Missing::Deadline)) => (Err(Missing::Deadline), 1),
            Ok(Err(Missing::FileTooLarge)) => (Err(Missing::FileTooLarge), 1),
            Ok(Err(reason)) => (Err(reason), read_cap),
            Err(_) => (Err(Missing::ReadFailed), read_cap),
        }
    }
}

/// Slice lines without changing LF, CRLF or standalone CR bytes.
fn excerpt(
    text: &str,
    line: u32,
    origin: SourceOrigin,
    remaining: &mut usize,
    redactions: &Redactions,
) -> ReferenceSource {
    let target = line.saturating_sub(1) as usize;
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    let mut index = 0;
    while let Some(byte) = bytes.get(index) {
        index = index.saturating_add(1);
        if *byte == b'\r' {
            if bytes.get(index) == Some(&b'\n') {
                index = index.saturating_add(1);
            }
            starts.push(index);
        } else if *byte == b'\n' {
            starts.push(index);
        }
    }
    if target >= starts.len() {
        return unavailable(Missing::LineUnavailable);
    }
    let start = target.saturating_sub(2);
    let end = starts
        .get(target.saturating_add(3))
        .copied()
        .unwrap_or(text.len());
    let Some(window_start) = starts.get(start) else {
        return unavailable(Missing::LineUnavailable);
    };
    let Some(window) = text.get(*window_start..end) else {
        return unavailable(Missing::LineUnavailable);
    };
    let allowance = EXCERPT_BYTES.min(*remaining);
    let reference_line = text
        .get(
            starts.get(target).copied().unwrap_or(text.len())
                ..starts
                    .get(target.saturating_add(1))
                    .copied()
                    .unwrap_or(text.len()),
        )
        .unwrap_or_default();
    let mut shortened = window.len() > allowance;
    // Bound the raw copy before redaction; then account the returned text.
    let selected = if shortened { reference_line } else { window };
    if selected.len() > allowance {
        return unavailable(Missing::TextBudget);
    }
    let mut selected = selected.to_owned();
    redactions.redact_in_place(&mut selected);
    if selected.len() > allowance && !shortened {
        if reference_line.len() > allowance {
            return unavailable(Missing::TextBudget);
        }
        reference_line.clone_into(&mut selected);
        redactions.redact_in_place(&mut selected);
        shortened = true;
    }
    if selected.len() > allowance {
        return unavailable(Missing::TextBudget);
    }
    *remaining = remaining.saturating_sub(selected.len());
    ReferenceSource::Text {
        text: selected,
        start_line: if shortened {
            line
        } else {
            u32::try_from(start.saturating_add(1)).unwrap_or(u32::MAX)
        },
        origin,
        shortened,
    }
}

/// Recheck final emitted source against the current redaction union and budgets.
pub(super) fn redact_sources(locations: &mut [ReferenceLocation], redactions: &Redactions) {
    let mut remaining = OUTPUT_BYTES;
    for item in locations {
        let Some(ReferenceSource::Text { .. }) = &item.source else {
            continue;
        };
        if let Some(ReferenceSource::Text {
            text,
            start_line,
            origin,
            shortened,
        }) = item.source.take()
        {
            let mut final_text = text.clone();
            redactions.redact_in_place(&mut final_text);
            if final_text.len() <= EXCERPT_BYTES.min(remaining) {
                remaining = remaining.saturating_sub(final_text.len());
                item.source = Some(ReferenceSource::Text {
                    text: final_text,
                    start_line,
                    origin,
                    shortened,
                });
                continue;
            }
            let relative_line = item
                .range
                .start
                .line
                .saturating_sub(start_line)
                .saturating_add(1);
            let mut source = excerpt(&text, relative_line, origin, &mut remaining, redactions);
            if let ReferenceSource::Text {
                start_line: first,
                shortened: short,
                ..
            } = &mut source
            {
                *first = first.saturating_add(start_line.saturating_sub(1));
                *short |= shortened;
            }
            item.source = Some(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::translator::dto::ReferencesResult;
    use crate::bridge::translator::enclosing::ResultContext;
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;
    use crate::test_lsp::client_path;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::io::BufReader;

    fn location(path: &Path, line: u32) -> Value {
        json!({"uri":url::Url::from_file_path(path).unwrap().to_string(),"range":{"start":{"line":line,"character":0},"end":{"line":line,"character":1}}})
    }

    async fn request(
        dir: &TempDir,
        response: Vec<Value>,
        options: ReferenceOptions,
        clock: Option<Arc<dyn super::super::clock::Clock>>,
    ) -> (ReferencesResult, ReferenceWork) {
        request_with_setup(dir, response, options, clock, None).await
    }

    async fn request_with_setup(
        dir: &TempDir,
        response: Vec<Value>,
        options: ReferenceOptions,
        clock: Option<Arc<dyn super::super::clock::Clock>>,
        setup: Option<fn(&mut Translator, &TempDir)>,
    ) -> (ReferencesResult, ReferenceWork) {
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (mut translator, mut server) =
            translator_with_capabilities(dir, &ServerId::from_static("rust"), caps);
        if let Some(clock) = clock {
            translator = translator.with_clock(clock);
        }
        let path = canonical_dir(dir).join("query.rs");
        std::fs::write(&path, "fn query() {}\n").unwrap();
        if let Some(setup) = setup {
            setup(&mut translator, dir);
        }
        let translator = Arc::new(translator);
        let handle = {
            let translator = translator.clone();
            tokio::spawn(async move {
                translator
                    .handle_references_with_options(
                        client_path(path.to_string_lossy().to_string()),
                        pos(1, 1),
                        false,
                        ResultContext::None,
                        options,
                    )
                    .await
                    .unwrap()
            })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let requested = read_framed_message(&mut wire).await;
        assert_eq!(requested["method"], "textDocument/references");
        write_response(
            &mut server.read_half_stdin,
            &requested["id"],
            json!(response),
        )
        .await;
        let result = handle.await.unwrap();
        let mut work = crate::util::lock_std(&translator.reference_work).clone();
        work.tracked_bytes = translator
            .document_tracker
            .snapshot_copied_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        (result, work)
    }

    // A late filter match must survive the old normalization cap; discarded
    // references cause no normalization or excerpt work.
    #[tokio::test]
    async fn reference_selection_precedes_normalization_and_opt_out_reads_nothing() {
        let dir = TempDir::new().unwrap();
        let wanted = canonical_dir(&dir).join("wanted.rs");
        let mut response = vec![location(&canonical_dir(&dir).join("discard.rs"), 0); 10_001];
        response.extend(vec![location(&wanted, 2); 3]);
        let (result, work) = request(
            &dir,
            response,
            ReferenceOptions {
                limit: 2,
                path_filter: Some(wanted),
                include_source: false,
            },
            None,
        )
        .await;
        assert_eq!(result.matched_count, 3);
        assert!(result.result.truncated);
        assert_eq!(result.result.locations.len(), 2);
        assert_eq!(work.normalized, 2);
        assert_eq!(work.source_bytes, 0);
        assert_eq!(work.files, 0);
        assert!(
            result
                .result
                .locations
                .iter()
                .all(|loc| loc.source.is_none())
        );
        assert!(result.index_coverage.is_none());
    }

    // Duplicate locations share one bounded file snapshot; line endings and
    // Unicode survive source context serialization.
    #[tokio::test]
    async fn reference_excerpts_share_snapshots_and_preserve_lines() {
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("disk.rs");
        let text = "α\r\nβ\rgamma\nδ\r\nlast";
        std::fs::write(&path, text).unwrap();
        let (result, work) = request(
            &dir,
            vec![location(&path, 2); 101],
            ReferenceOptions {
                limit: 101,
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(work.files, 1);
        assert_eq!(work.source_bytes, text.len());
        assert_eq!(work.read_bytes, text.len());
        assert_eq!(work.tracked_bytes, 0);
        assert_eq!(work.output_bytes, 100 * text.len());
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["text"], text);
        assert_eq!(value["locations"][0]["source"]["start_line"], 1);
        assert_eq!(value["locations"][0]["source"]["origin"], "disk");
        assert_eq!(
            value["locations"][100]["source"]["reason"],
            "candidate_limit"
        );
    }

    #[tokio::test]
    async fn reference_tracked_snapshot_and_shortened_line() {
        let dir = TempDir::new().unwrap();
        let query = canonical_dir(&dir).join("query.rs");
        let long = canonical_dir(&dir).join("long.rs");
        std::fs::write(
            &long,
            format!("{}\nref\n{}", "x".repeat(2048), "y".repeat(2048)),
        )
        .unwrap();
        let (result, work) = request(
            &dir,
            vec![location(&query, 0), location(&long, 1), location(&long, 0)],
            ReferenceOptions::default(),
            None,
        )
        .await;
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["origin"], "tracked");
        assert_eq!(value["locations"][1]["source"]["text"], "ref\n");
        assert_eq!(value["locations"][1]["source"]["start_line"], 2);
        assert_eq!(value["locations"][1]["source"]["shortened"], true);
        assert_eq!(value["locations"][2]["source"]["reason"], "text_budget");
        assert_eq!(work.files, 2);
    }

    // Public results show all 17 locations although only 16 files are read.
    #[tokio::test]
    async fn reference_file_and_output_budgets_preserve_locations() {
        let dir = TempDir::new().unwrap();
        let mut response = Vec::new();
        for index in 0..17 {
            let path = canonical_dir(&dir).join(format!("{index}.rs"));
            std::fs::write(&path, "x").unwrap();
            response.push(location(&path, 0));
        }
        let (result, work) = request(&dir, response, ReferenceOptions::default(), None).await;
        assert_eq!(work.files, 16);
        assert_eq!(work.source_bytes, 16);
        assert_eq!(result.result.locations.len(), 17);
        assert_eq!(
            serde_json::to_value(result).unwrap()["locations"][16]["source"]["reason"],
            "file_limit"
        );
        let path = canonical_dir(&dir).join("output.rs");
        std::fs::write(&path, "x".repeat(2048)).unwrap();
        let (result, work) = request(
            &dir,
            vec![location(&path, 0); 100],
            ReferenceOptions::default(),
            None,
        )
        .await;
        assert_eq!(work.source_bytes, 2048);
        assert_eq!(work.output_bytes, 65536);
        assert_eq!(
            serde_json::to_value(result).unwrap()["locations"][32]["source"]["reason"],
            "text_budget"
        );
    }

    #[derive(Debug)]
    struct AdvancingClock(std::sync::Mutex<std::time::Instant>);
    impl super::super::clock::Clock for AdvancingClock {
        fn now(&self) -> std::time::Instant {
            let mut now = crate::util::lock_std(&self.0);
            let result = *now;
            *now += Duration::from_secs(3);
            result
        }
    }

    // A controlled clock expires before enrichment; no timing threshold is used.
    #[tokio::test]
    async fn reference_deadline_prevents_source_work() {
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("query.rs");
        let clock = Arc::new(AdvancingClock(std::sync::Mutex::new(
            std::time::Instant::now(),
        )));
        let (result, work) = request(
            &dir,
            vec![location(&path, 0)],
            ReferenceOptions::default(),
            Some(clock),
        )
        .await;
        assert_eq!(work.files, 0);
        assert_eq!(work.source_bytes, 0);
        assert_eq!(
            serde_json::to_value(result).unwrap()["locations"][0]["source"]["reason"],
            "deadline"
        );
    }

    // Repeated oversized or invalid sources cannot bypass the per-request
    // accounting; source failures preserve every selected reference.
    #[tokio::test]
    async fn reference_source_allowance_charges_failures_and_bounds_reads() {
        let dir = TempDir::new().unwrap();
        let mut response = Vec::new();
        for index in 0..5 {
            let path = canonical_dir(&dir).join(format!("large{index}.rs"));
            std::fs::write(&path, vec![b'x'; FILE_BYTES]).unwrap();
            response.push(location(&path, 0));
        }
        let (result, work) = request(&dir, response, ReferenceOptions::default(), None).await;
        assert_eq!(work.source_bytes, SOURCE_BYTES);
        assert_eq!(result.result.locations.len(), 5);
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][4]["source"]["reason"], "source_budget");
        let invalid = canonical_dir(&dir).join("invalid.rs");
        std::fs::write(&invalid, [255, 254]).unwrap();
        let missing = canonical_dir(&dir).join("missing.rs");
        let oversized = canonical_dir(&dir).join("oversized.rs");
        std::fs::write(&oversized, vec![b'x'; FILE_BYTES + 1]).unwrap();
        let (result, work) = request(
            &dir,
            vec![
                location(&invalid, 0),
                location(&missing, 0),
                location(&oversized, 0),
            ],
            ReferenceOptions::default(),
            None,
        )
        .await;
        assert!(work.source_bytes >= 4);
        assert!(work.source_bytes <= SOURCE_BYTES);
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["reason"], "invalid_utf8");
        assert_eq!(value["locations"][2]["source"]["reason"], "file_too_large");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reference_symlink_escape_discloses_no_source() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "private data").unwrap();
        let link = canonical_dir(&dir).join("link.rs");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let (result, _) = request(
            &dir,
            vec![location(&link, 0), location(&secret, 0)],
            ReferenceOptions::default(),
            None,
        )
        .await;
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["reason"], "access_denied");
        assert_eq!(value["locations"][1]["source"]["reason"], "access_denied");
        assert!(!value.to_string().contains("private data"));
    }
    // A file that grows after metadata still consumes at most 1 MiB plus
    // one charged lookahead byte, through the public references handler.
    #[tokio::test]
    async fn reference_growth_lookahead_is_charged() {
        fn grow(path: &Path, after_stat: bool) {
            if after_stat {
                std::fs::write(path, vec![b'x'; FILE_BYTES + 100]).unwrap();
            }
        }
        fn setup(translator: &mut Translator, _: &TempDir) {
            translator.reference_read_hook = Some(grow);
        }
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("growth.rs");
        std::fs::write(&path, "small").unwrap();
        let (result, work) = request_with_setup(
            &dir,
            vec![location(&path, 0)],
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert_eq!(work.source_bytes, FILE_BYTES + 1);
        assert_eq!(work.read_bytes, FILE_BYTES + 1);
        assert_eq!(
            serde_json::to_value(result).unwrap()["locations"][0]["source"]["reason"],
            "file_too_large"
        );
    }

    #[tokio::test]
    async fn reference_oversized_tracked_snapshot_is_not_copied_or_read_from_disk() {
        fn setup(translator: &mut Translator, dir: &TempDir) {
            translator
                .document_tracker
                .open(
                    canonical_dir(dir).join("tracked.rs"),
                    "x".repeat(FILE_BYTES + 1),
                )
                .unwrap();
        }
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("tracked.rs");
        std::fs::write(&path, "must not fall back").unwrap();
        let (result, work) = request_with_setup(
            &dir,
            vec![location(&path, 0)],
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert_eq!(work.source_bytes, 1);
        assert_eq!(work.tracked_bytes, 0);
        assert_eq!(work.read_bytes, 0);
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["reason"], "file_too_large");
        assert!(!value.to_string().contains("must not fall back"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reference_contained_open_rejects_replaced_path() {
        fn replace(path: &Path, after_stat: bool) {
            if !after_stat {
                let target = std::fs::read_to_string(path).unwrap();
                std::fs::remove_file(path).unwrap();
                std::os::unix::fs::symlink(target, path).unwrap();
            }
        }
        fn setup(translator: &mut Translator, _: &TempDir) {
            translator.reference_read_hook = Some(replace);
        }
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("replaced.rs");
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "secret source").unwrap();
        std::fs::write(&path, secret.to_str().unwrap()).unwrap();
        let (result, _) = request_with_setup(
            &dir,
            vec![location(&path, 0)],
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["reason"], "read_failed");
        assert!(!value.to_string().contains("secret source"));
    }
    #[tokio::test]
    async fn reference_partial_read_failures_observe_real_bytes_and_charge_attempts() {
        fn setup(translator: &mut Translator, _: &TempDir) {
            translator.reference_io_failure_after = Some(8192);
        }
        let dir = TempDir::new().unwrap();
        let mut response = Vec::new();
        for index in 0..5 {
            let path = canonical_dir(&dir).join(format!("partial{index}.rs"));
            std::fs::write(&path, vec![b'x'; FILE_BYTES]).unwrap();
            response.push(location(&path, 0));
        }
        let (result, work) = request_with_setup(
            &dir,
            response,
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert_eq!(work.read_bytes, 4 * 8192);
        assert_eq!(work.source_bytes, SOURCE_BYTES);
        assert_eq!(work.tracked_bytes, 0);
        let value = serde_json::to_value(result).unwrap();
        for index in 0..4 {
            assert_eq!(value["locations"][index]["source"]["reason"], "read_failed");
        }
        assert_eq!(value["locations"][4]["source"]["reason"], "source_budget");
    }

    #[tokio::test]
    async fn reference_tracked_budget_measures_copies_and_prefers_tracked_text() {
        fn setup(translator: &mut Translator, dir: &TempDir) {
            for index in 0..5 {
                translator
                    .document_tracker
                    .open(
                        canonical_dir(dir).join(format!("tracked{index}.rs")),
                        format!("tracked\n{}", "x".repeat(FILE_BYTES - 8)),
                    )
                    .unwrap();
            }
        }
        let dir = TempDir::new().unwrap();
        let mut response = Vec::new();
        for index in 0..5 {
            let path = canonical_dir(&dir).join(format!("tracked{index}.rs"));
            std::fs::write(&path, "disk text must not appear").unwrap();
            response.extend(vec![location(&path, 0); 2]);
        }
        let (result, work) = request_with_setup(
            &dir,
            response,
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert_eq!(work.tracked_bytes, SOURCE_BYTES);
        assert_eq!(work.source_bytes, SOURCE_BYTES);
        assert_eq!(work.read_bytes, 0);
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["text"], "tracked\n");
        assert_eq!(value["locations"][0]["source"]["origin"], "tracked");
        assert_eq!(value["locations"][8]["source"]["reason"], "source_budget");
        assert!(!value.to_string().contains("disk text must not appear"));
    }

    #[derive(Debug)]
    struct AfterSourceClock {
        work: Arc<std::sync::Mutex<ReferenceWork>>,
        start: std::time::Instant,
    }
    impl super::super::clock::Clock for AfterSourceClock {
        fn now(&self) -> std::time::Instant {
            if crate::util::lock_std(&self.work).files == 0 {
                self.start
            } else {
                self.start + Duration::from_secs(3)
            }
        }
    }

    #[tokio::test]
    async fn reference_deadline_after_disk_failure_success_or_snapshot_stops_new_work() {
        fn clock(translator: &mut Translator, _: &TempDir) {
            translator.clock = Arc::new(AfterSourceClock {
                work: translator.reference_work.clone(),
                start: std::time::Instant::now(),
            });
        }
        for first in ["disk.rs", "missing.rs", "query.rs"] {
            let dir = TempDir::new().unwrap();
            let path = canonical_dir(&dir).join(first);
            if first == "disk.rs" {
                std::fs::write(&path, "disk").unwrap();
            }
            let second = canonical_dir(&dir).join("second.rs");
            std::fs::write(&second, "must not read").unwrap();
            let (result, work) = request_with_setup(
                &dir,
                vec![location(&path, 0), location(&second, 0)],
                ReferenceOptions::default(),
                None,
                Some(clock),
            )
            .await;
            assert_eq!(work.files, 1, "{first}");
            assert_eq!(work.read_bytes, if first == "disk.rs" { 4 } else { 0 });
            assert_eq!(work.tracked_bytes, if first == "query.rs" { 14 } else { 0 });
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["locations"][1]["source"]["reason"], "deadline");
        }
    }

    #[tokio::test]
    async fn reference_redaction_expansion_obeys_excerpt_and_total_budgets() {
        fn setup(translator: &mut Translator, _: &TempDir) {
            translator.startup_redactions =
                Arc::new(Redactions::new([("API_TOKEN".into(), "abcdefgh".into())]));
        }
        let dir = TempDir::new().unwrap();
        let oversized = canonical_dir(&dir).join("expanded.rs");
        std::fs::write(&oversized, "abcdefgh".repeat(256)).unwrap();
        let (result, work) = request_with_setup(
            &dir,
            vec![location(&oversized, 0); 32],
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert_eq!(work.output_bytes, 0);
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["locations"][0]["source"]["reason"], "text_budget");
        assert!(!value.to_string().contains("abcdefgh"));
        let short = canonical_dir(&dir).join("aggregate.rs");
        std::fs::write(&short, "abcdefgh".repeat(100)).unwrap();
        let (result, work) = request_with_setup(
            &dir,
            vec![location(&short, 0); 100],
            ReferenceOptions::default(),
            None,
            Some(setup),
        )
        .await;
        assert!(work.output_bytes <= OUTPUT_BYTES);
        let value = serde_json::to_value(result).unwrap();
        let texts: Vec<&str> = value["locations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["source"]["text"].as_str())
            .collect();
        assert_eq!(texts.len(), 32);
        assert!(texts.iter().all(|text| text.len() <= EXCERPT_BYTES));
        assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= OUTPUT_BYTES);
        assert!(!value.to_string().contains("abcdefgh"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn reference_symlink_aliases_share_one_canonical_snapshot() {
        let dir = TempDir::new().unwrap();
        let path = canonical_dir(&dir).join("canonical.rs");
        std::fs::write(&path, format!("reference\n{}", "x".repeat(FILE_BYTES - 10))).unwrap();
        let mut response = Vec::new();
        for index in 0..5 {
            let alias = canonical_dir(&dir).join(format!("alias{index}.rs"));
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            response.push(location(&alias, 0));
        }
        let (result, work) = request(&dir, response, ReferenceOptions::default(), None).await;
        assert_eq!(work.read_bytes, FILE_BYTES);
        assert_eq!(work.source_bytes, FILE_BYTES);
        assert_eq!(work.files, 1);
        let value = serde_json::to_value(result).unwrap();
        for item in value["locations"].as_array().unwrap() {
            assert_eq!(item["source"]["text"], "reference\n");
            assert_eq!(item["source"]["origin"], "disk");
        }
    }
}
