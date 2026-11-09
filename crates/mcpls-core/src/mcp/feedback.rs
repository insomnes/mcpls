//! Explicit, private feedback independent of repositories and language servers.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

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

#[derive(Serialize)]
struct Record {
    timestamp: String,
    version: &'static str,
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
        version: env!("CARGO_PKG_VERSION"),
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
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| io::Error::other("no local state or data directory is available"))?;
    let directory: PathBuf = base.join("mcpls");
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
