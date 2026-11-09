//! Explicit, private feedback independent of repositories and language servers.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FeedbackInput {
    /// The mcpls operation being reported (maximum 128 UTF-8 bytes).
    operation: String,
    /// The concrete observation (maximum 2,048 UTF-8 bytes).
    observation: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "supplied"
    )]
    #[schemars(with = "Category")]
    category: Option<Category>,
    /// An optional workaround (maximum 2,048 UTF-8 bytes).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "supplied"
    )]
    #[schemars(with = "String")]
    workaround: Option<String>,
}

// Absent optional fields use their default. Present fields must deserialize as
// their value type, so JSON null is rejected rather than treated as absence.
fn supplied<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Category {
    Friction,
    Missing,
    Useful,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct FeedbackStored {
    stored: bool,
}

#[derive(Deserialize, Serialize)]
struct Record {
    timestamp: String,
    version: String,
    #[serde(flatten)]
    input: FeedbackInput,
}

pub(super) async fn add(input: FeedbackInput) -> Result<FeedbackStored, McpError> {
    for (field, text, maximum) in [
        ("operation", Some(input.operation.as_str()), 128),
        ("observation", Some(input.observation.as_str()), 2048),
        ("workaround", input.workaround.as_deref(), 2048),
    ] {
        if let Some(text) = text
            && (text.trim().is_empty() || text.len() > maximum)
        {
            return Err(McpError::invalid_params(
                format!("{field} must be nonblank and at most {maximum} UTF-8 bytes"),
                Some(json!({"code":"invalid_feedback","retryable":false})),
            ));
        }
    }
    // Construct the complete line before any filesystem access.
    let mut line = serde_json::to_vec(&Record {
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        version: env!("CARGO_PKG_VERSION").to_string(),
        input,
    })
    .map_err(storage_error)?;
    line.push(b'\n');
    tokio::task::spawn_blocking(move || append(&line))
        .await
        .map_err(storage_error)?
        .map_err(storage_error)?;
    Ok(FeedbackStored { stored: true })
}

fn storage_error(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(
        format!("could not store mcpls feedback: {error}"),
        Some(json!({"code":"feedback_storage_failed","retryable":false})),
    )
}

fn append(line: &[u8]) -> io::Result<()> {
    let directory = directory()?;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory)?;
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "feedback directory must be a real directory",
        ));
    }
    check_private(&metadata)?;
    let path = directory.join("feedback.jsonl");
    // Reject symlinks and special files before opening. O_NOFOLLOW closes the
    // final-component symlink race; O_NONBLOCK prevents a swapped FIFO hanging.
    match fs::symlink_metadata(&path) {
        Ok(metadata) => check_file(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    // Windows locking requires read or write access in addition to append.
    #[cfg(windows)]
    options.read(true);
    let mut file = options.open(path)?;
    check_file(&file.metadata()?)?;
    file.lock()?;
    file.write_all(line)
}

fn check_file(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(io::Error::other(
            "feedback destination must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other(
                "feedback destination must not have hard links",
            ));
        }
    }
    check_private(metadata)
}

fn check_private(metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(io::Error::other(
                "feedback storage must be private and owned by the current user",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListInput {
    /// Filter open (default), resolved, or all reports.
    #[serde(default, deserialize_with = "supplied")]
    #[schemars(with = "StateFilter")]
    state: Option<StateFilter>,
    /// Maximum returned reports, 1 through 100 (default 20).
    #[serde(default, deserialize_with = "supplied")]
    #[schemars(with = "u32")]
    limit: Option<u32>,
    /// Number of matching reports to skip (default 0).
    #[serde(default, deserialize_with = "supplied")]
    #[schemars(with = "u32")]
    offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StateFilter {
    Open,
    Resolved,
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ResolveInput {
    /// Exact opaque ID copied from `feedback_list`; maximum 256 UTF-8 bytes.
    feedback_id: String,
    /// Resolution text, preserved exactly; maximum 2048 UTF-8 bytes.
    resolution: String,
    /// Literal task reference; no lookup or authority. Maximum 128 UTF-8 bytes.
    #[serde(default, deserialize_with = "supplied")]
    #[schemars(with = "String")]
    task_id: Option<String>,
    /// Literal note reference; no lookup or authority. Maximum 128 UTF-8 bytes.
    #[serde(default, deserialize_with = "supplied")]
    #[schemars(with = "String")]
    note_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReopenInput {
    /// Exact opaque ID copied from `feedback_list`; maximum 256 UTF-8 bytes.
    feedback_id: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("additionalProperties" = true))]
struct Resolution {
    text: String,
    resolved_at: String,
    task_id: Option<String>,
    note_id: Option<String>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("additionalProperties" = true))]
pub(super) struct Transition {
    feedback_id: String,
    state: FeedbackState,
    // Required even when null, so truncated or incomplete state is rejected.
    #[serde(deserialize_with = "nullable")]
    resolution: Option<Resolution>,
}

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FeedbackState {
    Open,
    Resolved,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct Page {
    items: Vec<serde_json::Value>,
    total_count: usize,
    returned_count: usize,
    limit: u32,
    offset: u32,
    truncated: bool,
}

fn validate_text(field: &str, text: &str, maximum: usize) -> Result<(), McpError> {
    if text.trim().is_empty() || text.len() > maximum {
        return Err(McpError::invalid_params(
            format!("{field} must be nonblank and at most {maximum} UTF-8 bytes"),
            Some(json!({"code":"invalid_feedback","retryable":false})),
        ));
    }
    Ok(())
}

pub(super) async fn list(input: ListInput) -> Result<Page, McpError> {
    let limit = input.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(McpError::invalid_params(
            "limit must be from 1 through 100",
            Some(json!({"code":"invalid_feedback","retryable":false})),
        ));
    }
    tokio::task::spawn_blocking(move || {
        let records = load()?.map_or_else(Vec::new, |store| store.items);
        let filtered: Vec<_> = records
            .into_iter()
            .rev()
            .filter(
                |item| match input.state.as_ref().unwrap_or(&StateFilter::Open) {
                    StateFilter::Open => item.resolution.is_none(),
                    StateFilter::Resolved => item.resolution.is_some(),
                    StateFilter::All => true,
                },
            )
            .collect();
        let total_count = filtered.len();
        let offset = input.offset.unwrap_or(0);
        let items = filtered
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|item| {
                let transition = serde_json::to_value(item.transition()).map_err(storage_error)?;
                let mut value = serde_json::to_value(item.record).map_err(storage_error)?;
                value
                    .as_object_mut()
                    .ok_or_else(|| storage_error("invalid report"))?
                    .extend(
                        transition
                            .as_object()
                            .ok_or_else(|| storage_error("invalid state"))?
                            .clone(),
                    );
                Ok(value)
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        let returned_count = items.len();
        Ok(Page {
            items,
            total_count,
            returned_count,
            limit,
            offset,
            truncated: (offset as usize).saturating_add(returned_count) < total_count,
        })
    })
    .await
    .map_err(storage_error)?
}

pub(super) async fn resolve(input: ResolveInput) -> Result<Transition, McpError> {
    validate_text("feedback_id", &input.feedback_id, 256)?;
    validate_text("resolution", &input.resolution, 2048)?;
    for (field, text) in [
        ("task_id", input.task_id.as_deref()),
        ("note_id", input.note_id.as_deref()),
    ] {
        if let Some(text) = text {
            validate_text(field, text, 128)?;
        }
    }
    let resolution = Resolution {
        text: input.resolution,
        resolved_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        task_id: input.task_id,
        note_id: input.note_id,
    };
    transition(input.feedback_id, Some(resolution)).await
}

pub(super) async fn reopen(input: ReopenInput) -> Result<Transition, McpError> {
    validate_text("feedback_id", &input.feedback_id, 256)?;
    transition(input.feedback_id, None).await
}

async fn transition(id: String, resolution: Option<Resolution>) -> Result<Transition, McpError> {
    tokio::task::spawn_blocking(move || {
        let mut store = load()?.ok_or_else(not_found)?;
        let item = store
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(not_found)?;
        if item.resolution.is_some() == resolution.is_some() {
            return Err(McpError::invalid_params(
                "feedback is already in the requested state",
                Some(json!({"code":"feedback_conflict","retryable":false})),
            ));
        }
        item.resolution = resolution;
        let result = item.transition();
        let mut line = serde_json::to_vec(&result).map_err(storage_error)?;
        line.push(b'\n');
        let mut state = open_private(
            &directory()
                .map_err(storage_error)?
                .join("feedback-state.jsonl"),
            true,
        )
        .map_err(storage_error)?;
        state.write_all(&line).map_err(storage_error)?;
        // The original file remains locked until the complete transition is appended.
        drop(store);
        Ok(result)
    })
    .await
    .map_err(storage_error)?
}

fn not_found() -> McpError {
    McpError::invalid_params(
        "feedback ID was not found",
        Some(json!({"code":"feedback_not_found","retryable":false})),
    )
}

struct Item {
    id: String,
    record: Record,
    resolution: Option<Resolution>,
}

impl Item {
    fn transition(&self) -> Transition {
        Transition {
            feedback_id: self.id.clone(),
            state: if self.resolution.is_some() {
                FeedbackState::Resolved
            } else {
                FeedbackState::Open
            },
            resolution: self.resolution.clone(),
        }
    }
}

struct Store {
    // All readers and writers use the original file's lock, including feedback_add.
    _lock: fs::File,
    items: Vec<Item>,
}

fn directory() -> io::Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|base| base.join("mcpls"))
        .ok_or_else(|| io::Error::other("no local state or data directory is available"))
}

fn open_private(path: &Path, append: bool) -> io::Result<fs::File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_file(&metadata)?,
        Err(error) if append && error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    if append {
        options.append(true).create(true);
    }
    #[cfg(windows)]
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    check_file(&file.metadata()?)?;
    Ok(file)
}

fn read_lines<T: serde::de::DeserializeOwned>(file: &mut fs::File) -> io::Result<Vec<T>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(io::Error::other(
            "feedback file has an incomplete final line",
        ));
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    bytes
        .strip_suffix(b"\n")
        .unwrap_or(&bytes)
        .split(|byte| *byte == b'\n')
        .map(|line| serde_json::from_slice(line).map_err(io::Error::other))
        .collect()
}

fn load() -> Result<Option<Store>, McpError> {
    let directory = directory().map_err(storage_error)?;
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error(error)),
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(storage_error("feedback directory is not a real directory"));
            }
            check_private(&metadata).map_err(storage_error)?;
        }
    }
    let mut file = match open_private(&directory.join("feedback.jsonl"), false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(directory.join("feedback-state.jsonl")).is_ok() {
                return Err(storage_error(
                    "feedback state exists without original reports",
                ));
            }
            return Ok(None);
        }
        result => result.map_err(storage_error)?,
    };
    file.lock().map_err(storage_error)?;
    let raw_records: Vec<serde_json::Value> = read_lines(&mut file).map_err(storage_error)?;
    let mut records = Vec::with_capacity(raw_records.len());
    for raw in raw_records {
        let object = raw
            .as_object()
            .ok_or_else(|| storage_error("feedback report must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "timestamp" | "version" | "operation" | "observation" | "category" | "workaround"
            )
        }) {
            return Err(storage_error("feedback report contains an unknown field"));
        }
        let record: Record = serde_json::from_value(raw).map_err(storage_error)?;
        if chrono::DateTime::parse_from_rfc3339(&record.timestamp).is_err()
            || record.version.trim().is_empty()
        {
            return Err(storage_error("invalid feedback timestamp or version"));
        }
        for (field, text, maximum) in [
            ("operation", Some(record.input.operation.as_str()), 128),
            ("observation", Some(record.input.observation.as_str()), 2048),
            ("workaround", record.input.workaround.as_deref(), 2048),
        ] {
            if let Some(text) = text {
                validate_text(field, text, maximum).map_err(storage_error)?;
            }
        }
        records.push(record);
    }
    let mut items: Vec<_> = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| Item {
            id: format!("feedback-{index:x}"),
            record,
            resolution: None,
        })
        .collect();
    let mut state = match open_private(&directory.join("feedback-state.jsonl"), false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(Store { _lock: file, items }));
        }
        result => result.map_err(storage_error)?,
    };
    let transitions: Vec<Transition> = read_lines(&mut state).map_err(storage_error)?;
    for transition in transitions {
        let item = items
            .iter_mut()
            .find(|item| item.id == transition.feedback_id)
            .ok_or_else(|| storage_error("state references an unknown report"))?;
        let resolved = transition.state == FeedbackState::Resolved;
        if resolved != transition.resolution.is_some() || resolved == item.resolution.is_some() {
            return Err(storage_error("invalid feedback state transition"));
        }
        if let Some(resolution) = &transition.resolution {
            validate_text("resolution", &resolution.text, 2048).map_err(storage_error)?;
            for text in [&resolution.task_id, &resolution.note_id]
                .into_iter()
                .flatten()
            {
                validate_text("context", text, 128).map_err(storage_error)?;
            }
            if chrono::DateTime::parse_from_rfc3339(&resolution.resolved_at).is_err() {
                return Err(storage_error("invalid resolution timestamp"));
            }
        }
        item.resolution = transition.resolution;
    }
    Ok(Some(Store { _lock: file, items }))
}
