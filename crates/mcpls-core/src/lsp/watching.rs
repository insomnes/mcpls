//! Private workspace Watchman coverage and standard LSP registrations.
#![allow(
    clippy::redundant_pub_crate,
    reason = "types remain restricted to this crate across private LSP modules"
)]
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::broadcast;
use wax::Program as _;

use crate::config::{FileWatchingBackend, FileWatchingConfig};
use crate::error::{Error, Result};
use crate::lsp::types::JsonRpcError;
use crate::redaction::RedactedText;

pub(crate) fn unavailable(message: impl std::fmt::Display) -> Error {
    {
        tracing::warn!("File watching backend unavailable: {message}");
        Error::LspProtocolError(RedactedText::fixed(
            "file watching backend unavailable; inspect backend configuration and restart the workspace runtime",
        ))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Change {
    root: PathBuf,
    name: PathBuf,
    kind: u8,
}

#[derive(Debug)]
pub(crate) struct Backend {
    events: broadcast::Sender<Vec<Change>>,
    failure: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Backend {
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(reason) = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return Err(unavailable(reason));
        }
        Ok(())
    }

    pub(crate) async fn start(
        config: &FileWatchingConfig,
        roots: &[PathBuf],
        trusted: bool,
    ) -> Result<Option<Arc<Self>>> {
        if config.backend == FileWatchingBackend::Disabled {
            return Ok(None);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (roots, trusted);
            return Err(unavailable("Watchman requires Linux"));
        }
        #[cfg(target_os = "linux")]
        Self::start_linux(config, roots, trusted).await.map(Some)
    }

    #[cfg(target_os = "linux")]
    #[allow(
        clippy::too_many_lines,
        reason = "one resource-owning setup and subscription pump with bounded startup"
    )]
    async fn start_linux(
        config: &FileWatchingConfig,
        roots: &[PathBuf],
        trusted: bool,
    ) -> Result<Arc<Self>> {
        use std::process::Stdio;

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;
        use tokio::time::{Duration, timeout};
        let directory = tempfile::Builder::new()
            .prefix("mcpls-watch-")
            .tempdir()
            .map_err(unavailable)?;
        let socket = directory.path().join("sock");
        let mut command = tokio::process::Command::new(&config.command);
        command
            .current_dir(
                roots
                    .first()
                    .ok_or_else(|| unavailable("no authorized roots"))?,
            )
            .arg("--foreground")
            .arg("--sockname")
            .arg(&socket)
            .arg("--logfile")
            .arg(directory.path().join("log"))
            .arg("--statefile")
            .arg(directory.path().join("state"))
            .arg("--pidfile")
            .arg(directory.path().join("pid"))
            .arg("--no-save-state")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if !trusted {
            let configured = roots
                .iter()
                .map(|root| crate::config::ConfiguredRoot::new(root.clone()).map_err(unavailable))
                .collect::<Result<Vec<_>>>()?;
            let boundary = crate::bridge::WorkspaceRoots::from_configured(&configured)?;
            let path = super::command_path::path_outside(
                &std::env::var_os("PATH").unwrap_or_default(),
                &boundary,
            );
            command.env(
                "PATH",
                super::command_path::or_system_path(
                    crate::config::HostOs::CURRENT,
                    path,
                    super::process_env,
                ),
            );
        }
        let mut child = super::process::ServerProcess::spawn(command).map_err(unavailable)?;
        let stream = timeout(Duration::from_secs(10), async {
            loop {
                if child.try_wait().map_err(unavailable)?.is_some() {
                    return Err(unavailable("Watchman exited during startup"));
                }
                if let Ok(stream) = UnixStream::connect(&socket).await {
                    break Ok(stream);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(unavailable)??;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        for (index, root) in roots.iter().enumerate() {
            for command in [json!(["watch", root]), json!(["clock", root])] {
                write
                    .write_all(format!("{command}\n").as_bytes())
                    .await
                    .map_err(unavailable)?;
                let line = timeout(Duration::from_secs(10), lines.next_line())
                    .await
                    .map_err(unavailable)?
                    .map_err(unavailable)?
                    .ok_or_else(|| unavailable("Watchman disconnected during setup"))?;
                let response: Value = serde_json::from_str(&line).map_err(unavailable)?;
                if let Some(error) = response.get("error") {
                    return Err(unavailable(error));
                }
                if command.get(0) == Some(&json!("clock")) {
                    let clock = response
                        .get("clock")
                        .ok_or_else(|| unavailable("missing Watchman clock"))?;
                    let subscribe = json!(["subscribe", root, format!("mcpls-{index}"), {"since":clock,"fields":["name","exists","new","type"],"empty_on_fresh_instance":true}]);
                    write
                        .write_all(format!("{subscribe}\n").as_bytes())
                        .await
                        .map_err(unavailable)?;
                    let line = timeout(Duration::from_secs(10), lines.next_line())
                        .await
                        .map_err(unavailable)?
                        .map_err(unavailable)?
                        .ok_or_else(|| unavailable("Watchman disconnected during subscribe"))?;
                    let response: Value = serde_json::from_str(&line).map_err(unavailable)?;
                    if response.get("subscribe").is_none() || response.get("error").is_some() {
                        return Err(unavailable(response));
                    }
                    let baseline = timeout(Duration::from_secs(10), lines.next_line())
                        .await
                        .map_err(unavailable)?
                        .map_err(unavailable)?
                        .ok_or_else(|| unavailable("missing subscription baseline"))?;
                    let baseline: Value = serde_json::from_str(&baseline).map_err(unavailable)?;
                    if baseline.get("subscription").and_then(Value::as_str)
                        != Some(format!("mcpls-{index}").as_str())
                        || baseline.get("error").is_some()
                        || baseline.get("canceled").is_some()
                    {
                        return Err(unavailable("invalid subscription baseline"));
                    }
                }
            }
        }
        let (events, _) = broadcast::channel(256);
        let failure = Arc::new(Mutex::new(None));
        let task_failure = failure.clone();
        let sender = events.clone();
        let roots = roots.to_vec();
        let task = tokio::spawn(async move {
            // These resources belong only to this runtime. Cancellation kills its daemon.
            let (_directory, mut child, _write) = (directory, child, write);
            let outcome: std::result::Result<(), String> = async {
                loop {
                    let line = tokio::select! {
                        line = lines.next_line() => line.map_err(|e| e.to_string())?.ok_or("Watchman disconnected")?,
                        status = child.wait() => return Err(format!("Watchman exited: {status:?}")),
                    };
                    let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                    if response.get("error").is_some() || response.get("canceled").is_some() || response.get("overflow").is_some() || response.get("warning").is_some() { return Err(format!("Watchman lost coverage: {response}")); }
                    let subscription = response.get("subscription").and_then(Value::as_str).ok_or("invalid subscription response")?;
                    let index: usize = subscription.strip_prefix("mcpls-").ok_or("unknown subscription")?.parse().map_err(|_| "invalid subscription")?;
                    let root = roots.get(index).ok_or("unknown root")?;
                    if response.get("is_fresh_instance").and_then(Value::as_bool) == Some(true) {
                        return Err("Watchman fresh instance invalidated coverage".to_owned());
                    }
                    let files = response.get("files").and_then(Value::as_array).ok_or("missing subscription files")?;
                    let changes = files.iter().filter_map(|file| {
                        let name = PathBuf::from(file.get("name")?.as_str()?);
                        if name.is_absolute() || name.components().any(|c| !matches!(c, Component::Normal(_))) || file.get("type").and_then(Value::as_str) == Some("l") { return None; }
                        // Check every existing ancestor, including for deleted paths.
                        if name.ancestors().filter(|p| !p.as_os_str().is_empty()).any(|part| std::fs::symlink_metadata(root.join(part)).is_ok_and(|m| m.file_type().is_symlink())) { return None; }
                        let kind = if file.get("exists").and_then(Value::as_bool) == Some(false) { 3 } else if file.get("new").and_then(Value::as_bool) == Some(true) { 1 } else { 2 };
                        Some(Change { root: root.clone(), name, kind })
                    }).collect::<Vec<_>>();
                    let _ = sender.send(changes);
                }
            }.await;
            if let Err(reason) = outcome {
                *task_failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
            }
        });
        Ok(Arc::new(Self {
            events,
            failure,
            task,
        }))
    }
}

// Normalize only literal spelling differences. Wax owns grammar validation and matching.
fn compile_glob(pattern: &str) -> std::result::Result<wax::Glob<'static>, wax::BuildError> {
    let mut expression = String::with_capacity(pattern.len());
    // None is outside a class; true is its first literal, false is its body.
    let mut class_first: Option<bool> = None;
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(first) = class_first {
            if character == ']' && !first {
                class_first = None;
            } else {
                if matches!(character, '[' | ']' | '\\') {
                    expression.push('\\');
                }
                class_first = Some(false);
            }
        } else if character == '[' {
            expression.push(character);
            if characters.peek() == Some(&'!') {
                expression.push('!');
                characters.next();
            }
            class_first = Some(true);
            continue;
        } else if matches!(character, '$' | '<' | '>' | '(' | ')' | ':' | '\\') {
            expression.push('\\');
        }
        expression.push(character);
    }
    wax::Glob::new(&expression).map(wax::Glob::into_owned)
}

#[derive(Debug)]
struct Watcher {
    matcher: wax::Glob<'static>,
    absolute: bool,
    kinds: u8,
}

#[derive(Debug)]
pub(crate) struct Registrations {
    pub(crate) backend: Arc<Backend>,
    receiver: broadcast::Receiver<Vec<Change>>,
    entries: HashMap<String, Vec<Watcher>>,
}

impl Registrations {
    pub(crate) fn new(backend: Arc<Backend>) -> Self {
        Self {
            receiver: backend.events.subscribe(),
            backend,
            entries: HashMap::new(),
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Vec<Change>> {
        self.receiver.recv().await.map_err(|error| {
            *self
                .backend
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
            unavailable(error)
        })
    }

    pub(crate) fn notification(&self, changes: Vec<Change>) -> Value {
        let mut seen = HashSet::new();
        let changes = changes
            .into_iter()
            .filter_map(|change| {
                let path = change.root.join(&change.name);
                let matches = self.entries.values().flatten().any(|watcher| {
                    watcher.kinds
                        & (match change.kind {
                            1 => 1,
                            2 => 2,
                            _ => 4,
                        })
                        != 0
                        && watcher.matcher.is_match(if watcher.absolute {
                            path.as_path()
                        } else {
                            change.name.as_path()
                        })
                });
                if !matches || !seen.insert((path.clone(), change.kind)) {
                    return None;
                }
                let uri = url::Url::from_file_path(path).ok()?;
                Some(json!({"uri":uri.as_str(),"type":change.kind}))
            })
            .collect::<Vec<_>>();
        json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{"changes":changes}})
    }

    pub(crate) fn request(
        &mut self,
        method: &str,
        params: Option<&Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        let invalid = || {
            JsonRpcError::new(
                lsp_types::ErrorCodes::InvalidParams,
                "Invalid watched-file registration parameters",
            )
        };
        self.backend
            .check()
            .map_err(|e| JsonRpcError::new(lsp_types::ErrorCodes::InternalError, e.to_string()))?;
        let register = method == "client/registerCapability";
        let entries = params
            .and_then(|p| {
                p.get(if register {
                    "registrations"
                } else {
                    "unregisterations"
                })
            })
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        let mut ids = HashSet::new();
        let mut prepared = Vec::new();
        for entry in entries {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(invalid)?;
            if !ids.insert(id) || self.entries.contains_key(id) == register {
                return Err(invalid());
            }
            if entry.get("method").and_then(Value::as_str)
                != Some("workspace/didChangeWatchedFiles")
            {
                return Err(JsonRpcError::new(
                    lsp_types::ErrorCodes::MethodNotFound,
                    "Unsupported capability registration method",
                ));
            }
            let mut watchers = Vec::new();
            if register {
                let options = entry
                    .get("registerOptions")
                    .and_then(|o| o.get("watchers"))
                    .and_then(Value::as_array)
                    .ok_or_else(invalid)?;
                for option in options {
                    let pattern = option
                        .get("globPattern")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(invalid)?;
                    let kinds = match option.get("kind") {
                        None => 7,
                        Some(value) => value
                            .as_u64()
                            .filter(|k| *k > 0 && *k <= 7)
                            .and_then(|k| u8::try_from(k).ok())
                            .ok_or_else(invalid)?,
                    };
                    let matcher = compile_glob(pattern).map_err(|_| invalid())?;
                    watchers.push(Watcher {
                        matcher,
                        kinds,
                        absolute: Path::new(pattern).is_absolute(),
                    });
                }
            }
            prepared.push((id.to_owned(), watchers));
        }
        for (id, watchers) in prepared {
            if register {
                self.entries.insert(id, watchers);
            } else {
                self.entries.remove(&id);
            }
        }
        Ok(Value::Null)
    }
}
