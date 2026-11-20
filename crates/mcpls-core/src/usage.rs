//! Runtime-local MCP usage accounting and asynchronous SQLite persistence.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use tokio::sync::oneshot;

use crate::bridge::WorkspaceRoots;
use crate::util::lock_std;

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS usage_hourly (
        hour_utc INTEGER NOT NULL,
        workspace TEXT NOT NULL,
        tool TEXT NOT NULL,
        outcome TEXT NOT NULL,
        call_count INTEGER NOT NULL,
        total_duration_us INTEGER NOT NULL,
        max_duration_us INTEGER NOT NULL,
        PRIMARY KEY (hour_utc, workspace, tool, outcome)
    ) WITHOUT ROWID;
    CREATE VIEW IF NOT EXISTS usage_daily AS
        SELECT hour_utc - ((hour_utc % 86400 + 86400) % 86400) AS day_utc,
               workspace, tool, outcome, SUM(call_count) AS call_count,
               SUM(total_duration_us) AS total_duration_us,
               MAX(max_duration_us) AS max_duration_us
        FROM usage_hourly
        GROUP BY day_utc, workspace, tool, outcome;
";

const UPSERT: &str = "
    INSERT INTO usage_hourly VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
    ON CONFLICT (hour_utc, workspace, tool, outcome) DO UPDATE SET
        call_count = call_count + excluded.call_count,
        total_duration_us = total_duration_us + excluded.total_duration_us,
        max_duration_us = MAX(max_duration_us, excluded.max_duration_us);
";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Success,
    ToolError,
    ProtocolError,
    Stopped,
}

impl Outcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ToolError => "tool_error",
            Self::ProtocolError => "protocol_error",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Default)]
enum Workspace {
    #[default]
    Unknown,
    One(PathBuf),
    Multiple,
}

impl Workspace {
    fn observe(&mut self, root: &Path) {
        match self {
            Self::Unknown => *self = Self::One(root.to_path_buf()),
            Self::One(first) if first != root => *self = Self::Multiple,
            _ => {}
        }
    }

    fn unique(self) -> Option<PathBuf> {
        match self {
            Self::One(root) => Some(root),
            Self::Unknown | Self::Multiple => None,
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
struct Bucket {
    hour_utc: i64,
    workspace: Option<PathBuf>,
    tool: &'static str,
    outcome: Outcome,
}

#[derive(Clone, Copy, Default)]
struct Totals {
    count: i64,
    total_us: i64,
    max_us: i64,
}

impl Totals {
    fn merge(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.total_us = self.total_us.saturating_add(other.total_us);
        self.max_us = self.max_us.max(other.max_us);
    }
}

type Batch = HashMap<Bucket, Totals>;

struct ActiveCall {
    tool: &'static str,
    started: Instant,
    workspace: Workspace,
}

#[derive(Default)]
struct State {
    next_id: usize,
    closed: bool,
    active: HashMap<usize, ActiveCall>,
    pending: Batch,
}

impl State {
    fn record(&mut self, call: ActiveCall, outcome: Outcome, hour_utc: i64, finished: Instant) {
        let duration_us =
            i64::try_from(finished.duration_since(call.started).as_micros()).unwrap_or(i64::MAX);
        self.pending
            .entry(Bucket {
                hour_utc,
                workspace: call.workspace.unique(),
                tool: call.tool,
                outcome,
            })
            .or_default()
            .merge(Totals {
                count: 1,
                total_us: duration_us,
                max_us: duration_us,
            });
    }
}

fn completion_time() -> (i64, Instant) {
    let seconds = chrono::Utc::now().timestamp();
    (
        seconds.div_euclid(3600).saturating_mul(3600),
        Instant::now(),
    )
}

/// Shared by all serving sessions. The mutex protects only in-memory data.
#[derive(Clone, Default)]
pub struct Collector(Arc<Mutex<State>>);

impl Collector {
    pub(crate) fn begin(&self, tool: &'static str, standalone: bool) -> Option<Call> {
        let started = Instant::now();
        let mut state = lock_std(&self.0);
        if state.closed {
            return None;
        }
        let id = state.next_id;
        state.next_id = id.wrapping_add(1);
        state.active.insert(
            id,
            ActiveCall {
                tool,
                started,
                workspace: Workspace::Unknown,
            },
        );
        drop(state);
        Some(Call {
            observation: Observation {
                collector: self.clone(),
                id,
                standalone,
            },
            outcome: Outcome::Stopped,
        })
    }

    fn close(&self) {
        let mut state = lock_std(&self.0);
        state.closed = true;
        let active = std::mem::take(&mut state.active);
        if !active.is_empty() {
            let (hour, finished) = completion_time();
            for (_, call) in active {
                state.record(call, Outcome::Stopped, hour, finished);
            }
        }
    }
}

#[derive(Clone)]
struct Observation {
    collector: Collector,
    id: usize,
    standalone: bool,
}

impl Observation {
    fn observe(&self, root: &Path) {
        if let Some(call) = lock_std(&self.collector.0).active.get_mut(&self.id) {
            call.workspace.observe(root);
        }
    }
}

tokio::task_local! {
    static OBSERVATION: Option<Observation>;
}

pub struct Call {
    observation: Observation,
    outcome: Outcome,
}

impl Call {
    pub(crate) async fn track<T>(call: Option<&Self>, handler: impl Future<Output = T>) -> T {
        OBSERVATION
            .scope(call.map(|call| call.observation.clone()), handler)
            .await
    }

    pub(crate) const fn complete(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        let mut state = lock_std(&self.observation.collector.0);
        if let Some(call) = state.active.remove(&self.observation.id) {
            let (hour, finished) = completion_time();
            state.record(call, self.outcome, hour, finished);
        }
    }
}

/// Reuse a selected dispatcher root. The selector has already approved it.
pub fn observe_selection(root: &Path) {
    let _ = OBSERVATION.try_with(|current| {
        if let Some(observation) = current {
            observation.observe(root);
        }
    });
}

/// Reuse a successful standalone path validation, including its canonical form.
pub fn observe_validation(roots: &WorkspaceRoots, path: &Path) {
    let _ = OBSERVATION.try_with(|current| {
        if let Some(observation) = current.as_ref().filter(|call| call.standalone)
            && let Some(root) = roots.most_specific_canonical_root(path)
        {
            observation.observe(root);
        }
    });
}

/// Used by standalone operations that already execute within the whole runtime.
pub fn observe_runtime(roots: &WorkspaceRoots) {
    let _ = OBSERVATION.try_with(|current| {
        if let Some(observation) = current.as_ref().filter(|call| call.standalone)
            && let [root] = roots.canonical()
        {
            observation.observe(root);
        }
    });
}

/// Carry the current tool observation into a blocking routing or validation task.
pub fn carry_observation<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let observation = OBSERVATION.try_with(Clone::clone).ok().flatten();
    move || OBSERVATION.sync_scope(observation, work)
}

pub struct UsageRuntime {
    collector: Collector,
    stop: mpsc::Sender<()>,
    finished: oneshot::Receiver<()>,
}

impl UsageRuntime {
    pub(crate) fn start() -> Option<Self> {
        if std::env::var_os("MCPLS_USAGE").is_some_and(|value| value == "0") {
            return None;
        }
        let directory = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|base| base.join("mcpls"));
        let Some(directory) = directory else {
            tracing::warn!("cannot record MCP tool usage without a local state directory");
            return None;
        };
        let collector = Collector::default();
        let worker_collector = collector.clone();
        let (stop, stopped) = mpsc::channel();
        let (finished_tx, finished) = oneshot::channel();
        // Keep SQLite waits off the runtime's blocking pool, which also
        // handles filesystem requests for tools.
        if let Err(error) = std::thread::Builder::new()
            .name("mcpls-usage".into())
            .spawn(move || {
                persist(&directory, &worker_collector, &stopped);
                let _ = finished_tx.send(());
            })
        {
            tracing::warn!(%error, "could not start MCP tool usage writer");
            return None;
        }
        Some(Self {
            collector,
            stop,
            finished,
        })
    }

    pub(crate) fn collector(&self) -> Collector {
        self.collector.clone()
    }

    pub(crate) async fn finish(self) {
        // rmcp request tasks can outlive their transport. Linearize shutdown
        // with their completions before handing the final batch to the writer.
        self.collector.close();
        drop(self.stop);
        if let Err(error) = self.finished.await {
            tracing::warn!(%error, "MCP tool usage writer stopped");
        }
    }
}

fn persist(directory: &Path, collector: &Collector, stop: &mpsc::Receiver<()>) {
    let mut pending = Batch::new();
    let mut last_warning: Option<Instant> = None;
    loop {
        let final_flush = !matches!(
            stop.recv_timeout(FLUSH_INTERVAL),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let recent = std::mem::take(&mut lock_std(&collector.0).pending);
        for (key, totals) in recent {
            pending.entry(key).or_default().merge(totals);
        }
        if !pending.is_empty() {
            match write(directory, &pending) {
                Ok(()) => pending.clear(),
                Err(error)
                    if last_warning.is_none_or(|last| last.elapsed() >= WARNING_INTERVAL) =>
                {
                    tracing::warn!(%error, "could not persist MCP tool usage");
                    last_warning = Some(Instant::now());
                }
                Err(_) => (),
            }
        }
        if final_flush {
            break;
        }
    }
}

type WriteError = Box<dyn std::error::Error + Send + Sync>;

fn write(directory: &Path, batch: &Batch) -> Result<(), WriteError> {
    fs::create_dir_all(directory)?;
    // The state directory can be a symlink. Refuse symlinks at the database
    // itself so the writer cannot follow one to an unrelated file.
    let path = dunce::canonicalize(directory)?.join("usage.sqlite");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
    }
    let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(Duration::from_secs(1))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA)?;
    {
        let mut statement = transaction.prepare(UPSERT)?;
        for (bucket, totals) in batch {
            let workspace =
                bucket
                    .workspace
                    .as_deref()
                    .map_or(Cow::Borrowed("unattributed"), |root| {
                        root.to_str().map_or_else(
                            || {
                                Cow::Owned(url::Url::from_file_path(root).map_or_else(
                                    |()| root.to_string_lossy().into_owned(),
                                    Into::into,
                                ))
                            },
                            Cow::Borrowed,
                        )
                    });
            statement.execute(params![
                bucket.hour_utc,
                workspace,
                bucket.tool,
                bucket.outcome.label(),
                totals.count,
                totals.total_us,
                totals.max_us,
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}
