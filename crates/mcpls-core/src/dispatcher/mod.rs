//! Worktree identities and resident semantic runtimes for the shared service.

mod discovery;
mod profile;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use discovery::{Identity, ObjectId};
use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, OnceCell, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::WorkspaceRuntime;
use crate::config::{DispatcherConfig, ServerConfig, WorkspaceTrust};
use crate::mcp::McplsServer;

type Result<T> = std::result::Result<T, DispatchError>;

#[derive(Debug, Clone)]
pub struct DispatchError {
    code: &'static str,
    message: String,
    retry: Option<u64>,
}
impl DispatchError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry: None,
        }
    }
    pub(crate) fn invalid(path: &Path) -> Self {
        Self::new(
            "workspace_invalid",
            format!(
                "workspace path is unavailable or invalid: {}",
                path.display()
            ),
        )
    }
    fn not_approved(path: &Path) -> Self {
        Self::new(
            "workspace_not_approved",
            format!("workspace is not approved: {}", path.display()),
        )
    }
    fn profile(path: &Path) -> Self {
        Self::new(
            "profile_invalid",
            format!("invalid service profile: {}", path.display()),
        )
    }
    pub(crate) fn stale() -> Self {
        Self::new(
            "stale_continuation",
            "continuation does not belong to a live runtime and server incarnation",
        )
    }
    pub(crate) fn wire(self) -> McpError {
        let (number, retryable) = match self.code {
            "workspace_capacity" => (-32055, true),
            "workspace_invalidated" => (-32056, true),
            "workspace_unavailable" => (-32057, true),
            _ => (-32602, false),
        };
        let mut data = json!({"code":self.code,"retryable":retryable});
        if let Some(retry) = self.retry
            && let Some(data) = data.as_object_mut()
        {
            data.insert("retry_after_ms".into(), json!(retry));
        }
        McpError::new(ErrorCode(number), self.message, Some(data))
    }
}

#[derive(Clone)]
struct Approval {
    root: PathBuf,
    common: Option<PathBuf>,
    object: ObjectId,
    profile: PathBuf,
}
#[derive(Clone)]
pub struct Selection {
    identity: Identity,
    profile: PathBuf,
    config: ServerConfig,
    digest: String,
    temporary: bool,
}
impl Selection {
    pub(crate) fn root(&self) -> &Path {
        &self.identity.root
    }
}
struct Entry {
    selection: Selection,
    resident: Option<Arc<Resident>>,
}

pub struct Dispatcher {
    config: DispatcherConfig,
    daemon: ServerConfig,
    approvals: Vec<Approval>,
    automatic: Vec<Approval>,
    entries: Mutex<BTreeMap<PathBuf, Entry>>,
    starts: Arc<Semaphore>,
    stop: CancellationToken,
}

struct StartState {
    launched: bool,
    recovering: bool,
    failures: usize,
    failed_at: Option<Instant>,
}
struct ServerStart {
    config: crate::lsp::ServerInitConfig,
    pid: Arc<AtomicU32>,
    state: Mutex<StartState>,
}

pub struct Resident {
    selection: Selection,
    pub(crate) generation: String,
    runtime: OnceCell<WorkspaceRuntime>,
    servers: OnceCell<Vec<ServerStart>>,
    pump: OnceCell<crate::runtime::pump::PumpShared>,
    sweep: StdMutex<Option<crate::runtime::disk_sweep::DiskSweepHandle>>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    active: AtomicUsize,
    activity: StdMutex<Option<(Instant, String)>>,
    invalid: CancellationToken,
    cleaned: CancellationToken,
    draining: std::sync::atomic::AtomicBool,
}

pub struct Lease {
    pub(crate) resident: Arc<Resident>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.resident.touch();
        self.resident.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn opaque() -> String {
    use std::fmt::Write as _;
    use std::hash::BuildHasher as _;
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    let seed = (
        SERIAL.fetch_add(1, Ordering::Relaxed),
        std::process::id(),
        std::time::SystemTime::now(),
    );
    let mut token = String::new();
    for _ in 0..4 {
        let _ = write!(
            token,
            "{:016x}",
            std::collections::hash_map::RandomState::new().hash_one(seed)
        );
    }
    token
}

pub fn config_is_inside_workspace(source: &Path, config: &DispatcherConfig) -> Result<bool> {
    let (canonical, identity) = discovery::inspect(source, false)?;
    if let Some(identity) = identity {
        for entry in &config.repositories {
            let (_, approved) = discovery::inspect(&entry.path, true)?;
            if approved.is_some_and(|approved| approved.common == identity.common) {
                return Ok(true);
            }
        }
    } else {
        for entry in &config.workspaces {
            let (root, _) = discovery::inspect(&entry.path, true)?;
            if canonical.starts_with(root) {
                return Ok(true);
            }
        }
    }
    for entry in &config.auto_workspaces {
        let root =
            std::fs::canonicalize(&entry.path).map_err(|_| DispatchError::invalid(&entry.path))?;
        if canonical.starts_with(root) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_limits(config: &DispatcherConfig) -> Result<()> {
    if config.max_concurrent_starts.get() > Semaphore::MAX_PERMITS
        || Instant::now()
            .checked_add(Duration::from_secs(config.reconcile_interval_seconds.get()))
            .is_none()
        || Instant::now()
            .checked_add(Duration::from_secs(config.idle_timeout_seconds.get()))
            .is_none()
    {
        return Err(DispatchError::new(
            "profile_invalid",
            "dispatcher limits overflow runtime capacity",
        ));
    }
    Ok(())
}

fn automatic_approvals(config: &DispatcherConfig) -> Result<Vec<Approval>> {
    let mut automatic: Vec<Approval> = Vec::new();
    for entry in &config.auto_workspaces {
        if !entry.path.is_absolute() || !entry.profile.is_absolute() {
            return Err(DispatchError::invalid(&entry.path));
        }
        let root =
            std::fs::canonicalize(&entry.path).map_err(|_| DispatchError::invalid(&entry.path))?;
        if !root.is_dir() {
            return Err(DispatchError::invalid(&entry.path));
        }
        if automatic.iter().any(|prior| prior.root == root) {
            return Err(DispatchError::new(
                "profile_invalid",
                "duplicate canonical automatic parent",
            ));
        }
        automatic.push(Approval {
            object: ObjectId::read(&root)?,
            root,
            common: None,
            profile: entry.profile.clone(),
        });
    }
    Ok(automatic)
}

impl Dispatcher {
    pub(crate) fn new(daemon: ServerConfig) -> Result<Arc<Self>> {
        let config = daemon
            .dispatcher
            .clone()
            .filter(|d| d.enabled)
            .ok_or_else(|| DispatchError::new("profile_invalid", "dispatcher is not enabled"))?;
        validate_limits(&config)?;
        if !daemon.lsp_servers.is_empty()
            || serde_json::to_value(&daemon.workspace).ok()
                != serde_json::to_value(crate::config::WorkspaceConfig::default()).ok()
        {
            return Err(DispatchError::new(
                "profile_invalid",
                "dispatcher mode forbids top-level workspace and lsp_servers",
            ));
        }
        let automatic = automatic_approvals(&config)?;
        let automatic_parents: Vec<_> = automatic
            .iter()
            .map(|approval| approval.root.clone())
            .collect();
        let mut configured_ids = std::collections::BTreeSet::new();
        let mut approvals: Vec<Approval> = Vec::new();
        for (git, entries) in [(true, &config.repositories), (false, &config.workspaces)] {
            for entry in entries {
                if !entry.path.is_absolute() || !entry.profile.is_absolute() {
                    return Err(DispatchError::invalid(&entry.path));
                }
                let (canonical, identity) = discovery::inspect(&entry.path, true)?;
                let (root, common, object) = match (git, identity) {
                    (true, Some(id)) => {
                        let common = id
                            .common
                            .clone()
                            .ok_or_else(|| DispatchError::invalid(&entry.path))?;
                        (id.root, Some(common.clone()), ObjectId::read(&common)?)
                    }
                    (false, None) => (canonical.clone(), None, ObjectId::read(&canonical)?),
                    _ => return Err(DispatchError::invalid(&entry.path)),
                };
                if approvals
                    .iter()
                    .any(|prior| prior.root == root || common.is_some() && prior.common == common)
                {
                    return Err(DispatchError::new(
                        "profile_invalid",
                        "duplicate canonical dispatcher approval",
                    ));
                }
                let (profile_config, _) =
                    profile::read(&entry.profile, &root, &daemon, false, &automatic_parents)?;
                configured_ids.extend(
                    profile_config
                        .lsp_servers
                        .iter()
                        .map(|server| server.id().to_string()),
                );
                approvals.push(Approval {
                    root,
                    common,
                    object,
                    profile: entry.profile.clone(),
                });
            }
        }
        for approval in &automatic {
            let (profile_config, _) = profile::read(
                &approval.profile,
                &approval.root,
                &daemon,
                true,
                &automatic_parents,
            )?;
            configured_ids.extend(
                profile_config
                    .lsp_servers
                    .iter()
                    .map(|server| server.id().to_string()),
            );
        }
        if let WorkspaceTrust::Untrusted(allowlist) = &daemon.workspace_trust
            && let Some(id) = allowlist
                .as_slice()
                .iter()
                .find(|id| !configured_ids.contains(&id.to_string()))
        {
            return Err(DispatchError::new(
                "profile_invalid",
                format!("allowed server {id} is not configured in an approved profile"),
            ));
        }
        Ok(Arc::new(Self {
            starts: Arc::new(Semaphore::new(config.max_concurrent_starts.get())),
            config,
            daemon,
            approvals,
            automatic,
            entries: Mutex::new(BTreeMap::new()),
            stop: CancellationToken::new(),
        }))
    }

    fn discover(&self, path: &Path, directory: bool) -> Result<Selection> {
        let (canonical, git_identity) = discovery::inspect(path, directory)?;
        // Explicit approvals keep their existing Git/non-Git priority and fail closed.
        let explicit = git_identity.as_ref().map_or_else(
            || {
                self.approvals
                    .iter()
                    .filter(|a| a.common.is_none() && canonical.starts_with(&a.root))
                    .max_by_key(|a| a.root.components().count())
            },
            |id| {
                self.approvals
                    .iter()
                    .find(|a| a.common.is_some() && a.common == id.common)
            },
        );
        if let Some(approval) = explicit {
            let object_path = approval.common.as_ref().unwrap_or(&approval.root);
            if ObjectId::read(object_path)? != approval.object {
                return Err(DispatchError::not_approved(path));
            }
            let identity = git_identity.unwrap_or_else(|| Identity {
                root: approval.root.clone(),
                common: None,
                git_dir: None,
                objects: vec![approval.object.clone()],
            });
            let (config, digest) = self.read_profile(approval, &identity.root, false)?;
            return Ok(Selection {
                identity,
                profile: approval.profile.clone(),
                config,
                digest,
                temporary: false,
            });
        }
        let approval = self
            .automatic
            .iter()
            .filter(|a| canonical.starts_with(&a.root))
            .max_by_key(|a| a.root.components().count())
            .ok_or_else(|| DispatchError::not_approved(path))?;
        if self.inside_explicit_boundary(&canonical) {
            return Err(DispatchError::not_approved(path));
        }
        if ObjectId::read(&approval.root)? != approval.object {
            return Err(DispatchError::not_approved(path));
        }
        let (policy_config, _) = self.read_profile(approval, &approval.root, true)?;
        let start = if canonical.is_dir() {
            canonical.as_path()
        } else {
            canonical
                .parent()
                .ok_or_else(|| DispatchError::invalid(path))?
        };
        let git_identity = git_identity.filter(|id| id.root.starts_with(&approval.root));
        let boundary = git_identity
            .as_ref()
            .map_or(approval.root.as_path(), |id| id.root.as_path());
        let root = start
            .ancestors()
            .take_while(|dir| dir.starts_with(boundary))
            .find(|dir| {
                policy_config
                    .lsp_servers
                    .iter()
                    .filter_map(|server| server.heuristics.as_ref())
                    .flat_map(|heuristics| &heuristics.project_markers)
                    .any(|marker| dir.join(marker.as_str()).exists())
            })
            .unwrap_or_else(|| git_identity.as_ref().map_or(start, |id| id.root.as_path()))
            .to_path_buf();
        let mut objects = vec![approval.object.clone(), ObjectId::read(&root)?];
        let (common, git_dir) = if let Some(id) = git_identity {
            objects.extend(id.objects);
            (id.common, id.git_dir)
        } else {
            (None, None)
        };
        let identity = Identity {
            root,
            common,
            git_dir,
            objects,
        };
        let (config, digest) = self.read_profile(approval, &identity.root, true)?;
        Ok(Selection {
            identity,
            profile: approval.profile.clone(),
            config,
            digest,
            temporary: true,
        })
    }

    fn inside_explicit_boundary(&self, canonical: &Path) -> bool {
        self.approvals.iter().any(|approval| canonical.starts_with(&approval.root))
            // Registered linked checkouts retain explicit authority even if their
            // .git marker disappears. Automatic profiles must not replace it.
            || self.approvals.iter().filter_map(|approval| approval.common.as_ref())
                .filter_map(|common| discovery::worktrees(common).ok())
                .flatten().any(|root| canonical.starts_with(root))
    }

    pub(crate) async fn select(
        self: &Arc<Self>,
        path: PathBuf,
        directory: bool,
    ) -> Result<Selection> {
        let dispatcher = self.clone();
        let selected_path = path.clone();
        let result = crate::on_blocking_pool(move || dispatcher.discover(&path, directory))
            .await
            .map_err(|_| DispatchError::invalid(Path::new("selected workspace")))?;
        let result = match result {
            Ok(selection) if selection.temporary => {
                match tokio::fs::canonicalize(&selected_path).await {
                    Ok(canonical) => {
                        let entries = self.entries.lock().await;
                        let explicit = entries.values().any(|entry| {
                            !entry.selection.temporary
                                && canonical.starts_with(entry.selection.root())
                        });
                        drop(entries);
                        if explicit {
                            Err(DispatchError::not_approved(&selected_path))
                        } else {
                            Ok(selection)
                        }
                    }
                    Err(_) => Err(DispatchError::invalid(&selected_path)),
                }
            }
            other => other,
        };
        if let Ok(selection) = &result
            && !selection.temporary
        {
            self.entries
                .lock()
                .await
                .entry(selection.identity.root.clone())
                .or_insert(Entry {
                    selection: selection.clone(),
                    resident: None,
                });
        }
        if result.is_err() {
            let residents: Vec<_> = self
                .entries
                .lock()
                .await
                .values()
                .filter_map(|entry| entry.resident.clone())
                .filter(|resident| selected_path.starts_with(resident.root()))
                .collect();
            for resident in residents {
                let dispatcher = self.clone();
                let root = resident.root().to_path_buf();
                let current =
                    crate::on_blocking_pool(move || dispatcher.discover(&root, true)).await;
                if !matches!(current,Ok(Ok(ref selection)) if selection.identity==resident.selection.identity && selection.digest==resident.selection.digest)
                {
                    resident.retire();
                }
            }
        }
        result
    }

    pub(crate) async fn acquire(self: &Arc<Self>, selection: Selection) -> Result<Lease> {
        loop {
            let mut entries = self.entries.lock().await;
            let key = selection.identity.root.clone();
            if let Some(entry) = entries.get_mut(&key)
                && let Some(resident) = &entry.resident
            {
                if resident.selection.identity != selection.identity
                    || resident.selection.digest != selection.digest
                {
                    resident.retire();
                }
                if resident.draining.load(Ordering::Acquire) {
                    let resident = resident.clone();
                    drop(entries);
                    resident.cleaned.cancelled().await;
                    self.remove_cleaned(&resident).await;
                    continue;
                }
                resident.active.fetch_add(1, Ordering::AcqRel);
                resident.touch();
                let resident = resident.clone();
                drop(entries);
                return Ok(Lease { resident });
            }
            let residents: Vec<_> = entries
                .values()
                .filter_map(|e| e.resident.clone())
                .collect();
            if residents.len() >= self.config.max_resident_workspaces.get() {
                let mut eligible = None;
                for resident in residents {
                    if resident.active.load(Ordering::Acquire) == 0
                        && !resident.has_subscriptions().await
                        && !resident.draining.load(Ordering::Acquire)
                    {
                        eligible = Some(resident);
                        break;
                    }
                }
                let Some(victim) = eligible else {
                    return Err(DispatchError::new(
                        "workspace_capacity",
                        "all resident workspaces have active requests, subscriptions, or cleanup",
                    ));
                };
                victim.retire();
                drop(entries);
                victim.cleaned.cancelled().await;
                self.remove_cleaned(&victim).await;
                continue;
            }
            let resident = Arc::new(Resident {
                selection: selection.clone(),
                generation: opaque(),
                runtime: OnceCell::new(),
                servers: OnceCell::new(),
                pump: OnceCell::new(),
                sweep: StdMutex::new(None),
                tasks: StdMutex::new(Vec::new()),
                active: AtomicUsize::new(1),
                activity: StdMutex::new(None),
                invalid: CancellationToken::new(),
                cleaned: CancellationToken::new(),
                draining: std::sync::atomic::AtomicBool::new(false),
            });
            resident.touch();
            entries.insert(
                key,
                Entry {
                    selection: selection.clone(),
                    resident: Some(resident.clone()),
                },
            );
            drop(entries);
            return Ok(Lease { resident });
        }
    }

    pub(crate) async fn validate(self: &Arc<Self>, resident: &Arc<Resident>) -> Result<()> {
        match self
            .select(resident.selection.identity.root.clone(), true)
            .await
        {
            Ok(selection)
                if selection.identity == resident.selection.identity
                    && selection.digest == resident.selection.digest
                    && !resident.invalid.is_cancelled() =>
            {
                Ok(())
            }
            _ => {
                resident.retire();
                Err(DispatchError::new(
                    "workspace_invalidated",
                    "workspace identity or service profile changed before publication",
                ))
            }
        }
    }

    pub(crate) fn spawn_reconciler(self: &Arc<Self>) -> JoinHandle<()> {
        let dispatcher = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(
                dispatcher.config.reconcile_interval_seconds.get(),
            ));
            loop {
                tokio::select! {()=dispatcher.stop.cancelled()=>break,_=interval.tick()=>{}}
                dispatcher.reconcile().await;
            }
        })
    }
    async fn remove_cleaned(&self, resident: &Arc<Resident>) {
        if !resident.cleaned.is_cancelled() {
            return;
        }
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(resident.root())
            && entry
                .resident
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, resident))
        {
            if entry.selection.temporary {
                entries.remove(resident.root());
            } else {
                entry.resident = None;
            }
        }
        drop(entries);
    }
    async fn reconcile(self: &Arc<Self>) {
        // Reconciliation discovers registered additions without choosing a default workspace.
        for approval in &self.approvals {
            if let Some(common) = &approval.common {
                let common = common.clone();
                let listed = crate::on_blocking_pool(move || discovery::worktrees(&common)).await;
                if let Ok(Ok(listed)) = listed {
                    for path in listed {
                        if let Ok(selection) = self.select(path, true).await
                            && !selection.temporary
                        {
                            self.entries
                                .lock()
                                .await
                                .entry(selection.identity.root.clone())
                                .or_insert(Entry {
                                    selection,
                                    resident: None,
                                });
                        }
                    }
                } else {
                    tracing::warn!("dispatcher Git worktree reconciliation failed");
                }
            }
        }
        let residents: Vec<_> = self
            .entries
            .lock()
            .await
            .values()
            .filter_map(|entry| entry.resident.clone())
            .collect();
        for resident in residents {
            if self.validate(&resident).await.is_ok() {
                // Admission and idle eviction make their pin decision under the same short lock.
                let entries = self.entries.lock().await;
                let current = entries
                    .get(resident.root())
                    .and_then(|entry| entry.resident.as_ref())
                    .is_some_and(|current| Arc::ptr_eq(current, &resident));
                let idle = crate::util::lock_std(&resident.activity)
                    .as_ref()
                    .is_some_and(|(time, _)| {
                        time.elapsed()
                            >= Duration::from_secs(self.config.idle_timeout_seconds.get())
                    });
                if current
                    && idle
                    && resident.active.load(Ordering::Acquire) == 0
                    && !resident.has_subscriptions().await
                {
                    resident.retire();
                }
                drop(entries);
            }
            self.remove_cleaned(&resident).await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.stop.cancel();
        let residents: Vec<_> = self
            .entries
            .lock()
            .await
            .values()
            .filter_map(|e| e.resident.clone())
            .collect();
        for resident in &residents {
            resident.retire();
        }
        for resident in residents {
            resident.cleaned.cancelled().await;
        }
    }

    pub(crate) async fn resource_paths(self: &Arc<Self>) -> (Vec<PathBuf>, String) {
        let residents: Vec<_> = self
            .entries
            .lock()
            .await
            .values()
            .filter_map(|e| e.resident.clone())
            .collect();
        let mut paths = Vec::new();
        let mut generations = Vec::new();
        for resident in residents {
            if self.validate(&resident).await.is_ok()
                && let Some(runtime) = resident.runtime.get()
            {
                paths.extend(runtime.translator.open_document_paths());
                generations.push(resident.generation.clone());
            }
        }
        paths.sort();
        paths.dedup();
        generations.sort();
        (
            paths,
            format!("{:x}", Sha256::digest(generations.join(":"))),
        )
    }
    pub(crate) async fn existing(&self, generation: &str) -> Result<Lease> {
        let entries = self.entries.lock().await;
        let resident = entries
            .values()
            .filter_map(|e| e.resident.as_ref())
            .find(|r| r.generation == generation && !r.invalid.is_cancelled())
            .cloned()
            .ok_or_else(DispatchError::stale)?;
        resident.active.fetch_add(1, Ordering::AcqRel);
        drop(entries);
        resident.touch();
        Ok(Lease { resident })
    }
    fn read_profile(
        &self,
        approval: &Approval,
        root: &Path,
        automatic: bool,
    ) -> Result<(ServerConfig, String)> {
        profile::read(
            &approval.profile,
            root,
            &self.daemon,
            automatic,
            &self.automatic_parent_roots(),
        )
    }

    fn automatic_parent_roots(&self) -> Vec<PathBuf> {
        self.automatic
            .iter()
            .map(|approval| approval.root.clone())
            .collect()
    }

    pub(crate) fn configured_profiles(&self) -> Vec<ServerConfig> {
        self.approvals
            .iter()
            .map(|approval| (approval, false))
            .chain(self.automatic.iter().map(|approval| (approval, true)))
            .filter_map(|(approval, automatic)| {
                self.read_profile(approval, &approval.root, automatic)
                    .ok()
                    .map(|(config, _)| config)
            })
            .collect()
    }

    pub(crate) async fn status(self: &Arc<Self>, path: Option<PathBuf>) -> Result<Value> {
        let selected = if let Some(path) = path {
            let selection = self.select(path, true).await?;
            let key = selection.identity.root.clone();
            let mut entries = self.entries.lock().await;
            if selection.temporary && !entries.contains_key(&key) {
                drop(entries);
                let workspace = workspace_status(selection, None).await;
                return Ok(self.status_response(&[workspace]));
            }
            let entry = entries.entry(key.clone()).or_insert(Entry {
                selection: selection.clone(),
                resident: None,
            });
            if let Some(resident) = &entry.resident
                && (resident.selection.identity != selection.identity
                    || resident.selection.digest != selection.digest)
            {
                resident.retire();
            }
            entry.selection = selection;
            drop(entries);
            Some(key)
        } else {
            None
        };
        let snapshot: Vec<_> = {
            let entries = self.entries.lock().await;
            let mut snapshot = Vec::new();
            for (_, entry) in entries
                .iter()
                .filter(|(path, _)| selected.as_ref().is_none_or(|key| key == *path))
            {
                if let Some(resident) = &entry.resident {
                    snapshot.push((resident.selection.clone(), Some(resident.clone())));
                    if resident.selection.identity != entry.selection.identity
                        || resident.selection.digest != entry.selection.digest
                    {
                        snapshot.push((entry.selection.clone(), None));
                    }
                } else {
                    snapshot.push((entry.selection.clone(), None));
                }
            }
            drop(entries);
            snapshot.sort_by(|(a, ar), (b, br)| {
                a.identity.root.cmp(&b.identity.root).then_with(|| {
                    ar.as_ref()
                        .map(|r| &r.generation)
                        .cmp(&br.as_ref().map(|r| &r.generation))
                })
            });
            snapshot
        };
        let mut workspaces = Vec::new();
        for (selection, resident) in snapshot {
            workspaces.push(workspace_status(selection, resident).await);
        }
        Ok(self.status_response(&workspaces))
    }

    fn status_response(&self, workspaces: &[Value]) -> Value {
        json!({"mode":"dispatcher","limits":{"max_resident_workspaces":self.config.max_resident_workspaces,"max_concurrent_starts":self.config.max_concurrent_starts,"idle_timeout_seconds":self.config.idle_timeout_seconds,"reconcile_interval_seconds":self.config.reconcile_interval_seconds},"workspaces":workspaces})
    }
}

async fn workspace_status(selection: Selection, resident: Option<Arc<Resident>>) -> Value {
    let mut servers = Vec::new();
    for configured in &selection.config.lsp_servers {
        let id = configured.id();
        let mut state = "not_started";
        let mut pid = None;
        let mut failures = 0;
        let mut error = None;
        let mut retry = None;
        if let Some(resident) = &resident {
            if let Some(runtime) = resident.runtime.get() {
                if runtime
                    .translator
                    .dispatcher_server(&id)
                    .is_some_and(|(alive, _)| alive)
                {
                    state = "ready";
                }
                if let Some(start) = resident
                    .servers
                    .get()
                    .and_then(|s| s.iter().find(|s| s.config.server_config().id() == id))
                {
                    let mut start_state = start.state.lock().await;
                    let dead = runtime
                        .translator
                        .dispatcher_server(&id)
                        .is_some_and(|(alive, _)| !alive);
                    if dead && !start_state.recovering && start_state.failed_at.is_none() {
                        start_state.failed_at = Some(Instant::now());
                        start_state.failures = start_state.failures.saturating_add(1);
                    }
                    failures = start_state.failures;
                    if state != "ready" && start_state.launched {
                        state = "starting";
                    }
                    if let Some(at) = start_state.failed_at {
                        state = "failed";
                        error = Some("language server failed to start".to_string());
                        retry = Some(5000u64.saturating_sub(
                            u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        ));
                    }
                    drop(start_state);
                    let raw = start.pid.load(Ordering::Acquire);
                    if raw > 0 && state != "failed" {
                        pid = Some(raw);
                    }
                }
                if let Some(reason) = runtime.translator.watching_error(&id) {
                    state = "failed";
                    error = Some(reason);
                    pid = None;
                }
                if runtime.translator.startup_failure(&id).is_some() {
                    state = "failed";
                    failures = failures.max(1);
                    error = Some("language server failed to start".to_string());
                    pid = None;
                }
            }
            if resident.draining.load(Ordering::Acquire) {
                state = "draining";
            }
        }
        servers.push(json!({"server":id,"state":state,"pid":pid,"start_failures":failures,"last_error":error,"retry_after_ms":retry}));
    }
    let state = resident.as_ref().map_or("discovered", |r| {
        if r.draining.load(Ordering::Acquire) {
            "draining"
        } else if servers.iter().any(|s| s["state"] == "starting") {
            "starting"
        } else if servers.iter().any(|s| s["state"] == "ready") {
            "ready"
        } else if servers.iter().any(|s| s["state"] == "failed") {
            "failed"
        } else {
            "ready"
        }
    });
    let last_activity = resident.as_ref().and_then(|r| {
        crate::util::lock_std(&r.activity)
            .as_ref()
            .map(|(_, date)| date.clone())
    });
    let subscriptions = if let Some(resident) = &resident {
        resident.subscription_count().await
    } else {
        0
    };
    json!({"temporary":selection.temporary,"workspace_path":selection.identity.root,"identity":format!("{:x}",Sha256::digest(format!("{:?}",selection.identity))),"repository_common_dir":selection.identity.common,"git_dir":selection.identity.git_dir,"profile":selection.profile,"generation":resident.as_ref().map(|r|&r.generation),"state":state,"last_activity":last_activity,"active_requests":resident.as_ref().map_or(0,|r|r.active.load(Ordering::Acquire)),"subscriptions":subscriptions,"servers":servers})
}

struct RuntimeValidity {
    dispatcher: std::sync::Weak<Dispatcher>,
    resident: std::sync::Weak<Resident>,
}
impl std::fmt::Debug for RuntimeValidity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuntimeValidity")
    }
}
impl crate::mcp::ResourceValidity for RuntimeValidity {
    fn check(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            match (self.dispatcher.upgrade(), self.resident.upgrade()) {
                (Some(dispatcher), Some(resident)) => dispatcher.validate(&resident).await.is_ok(),
                _ => false,
            }
        })
    }
}

impl Resident {
    fn touch(&self) {
        *crate::util::lock_std(&self.activity) = Some((
            Instant::now(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ));
    }
    async fn subscription_count(&self) -> usize {
        if let Some(runtime) = self.runtime.get() {
            runtime.subscription_registry.subscription_count().await
        } else {
            0
        }
    }
    async fn has_subscriptions(&self) -> bool {
        self.subscription_count().await > 0
    }
    pub(crate) fn cancel(&self) -> CancellationToken {
        self.invalid.clone()
    }
    pub(crate) fn root(&self) -> &Path {
        &self.selection.identity.root
    }
    pub(crate) fn incarnation(&self, server: &crate::config::ServerId) -> Option<String> {
        self.runtime
            .get()?
            .translator
            .dispatcher_server(server)
            .filter(|(alive, _)| *alive)
            .map(|(_, incarnation)| incarnation)
    }
    pub(crate) async fn prepare(
        self: &Arc<Self>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<&WorkspaceRuntime> {
        self.runtime
            .get_or_try_init(|| async {
                let mut config = self.selection.config.clone();
                if self.selection.temporary {
                    for server in &mut config.lsp_servers {
                        server.heuristics = None;
                    }
                }
                let execution_boundary = if self.selection.temporary {
                    dispatcher.automatic_parent_roots()
                } else {
                    vec![self.selection.identity.root.clone()]
                };
                let mut runtime =
                    WorkspaceRuntime::prepare_scoped(&config, Some(execution_boundary))
                        .await
                        .map_err(|error| match error {
                            crate::error::Error::FileWatchingUnavailable => {
                                DispatchError::new("workspace_unavailable", error.to_string())
                            }
                            _ => DispatchError::profile(&self.selection.profile),
                        })?;
                runtime
                    .subscription_registry
                    .set_validity(Arc::new(RuntimeValidity {
                        dispatcher: Arc::downgrade(dispatcher),
                        resident: Arc::downgrade(self),
                    }));
                let local = Arc::new(Semaphore::new(
                    self.selection
                        .config
                        .workspace
                        .max_concurrent_server_starts
                        .get(),
                ));
                let starts = runtime
                    .applicable_configs
                    .drain(..)
                    .map(|config| {
                        let pid = Arc::new(AtomicU32::new(0));
                        let config = if matches!(
                            self.selection.config.workspace_trust,
                            WorkspaceTrust::Trusted
                        ) {
                            config.with_child_working_dir(crate::lsp::ChildWorkingDir::Fixed(
                                self.selection.identity.root.clone(),
                            ))
                        } else {
                            config
                        };
                        ServerStart {
                            config: config.with_dispatcher_limits(
                                vec![dispatcher.starts.clone(), local.clone()],
                                pid.clone(),
                            ),
                            pid,
                            state: Mutex::new(StartState {
                                launched: false,
                                recovering: false,
                                failures: 0,
                                failed_at: None,
                            }),
                        }
                    })
                    .collect();
                let shared = crate::runtime::pump::PumpShared {
                    notification_cache: runtime.notification_cache.clone(),
                    subs: runtime.subscription_registry.clone(),
                    workspace_roots: runtime.workspace_roots.clone(),
                    roles: crate::runtime::pump::DiagnosticsRoles::default(),
                };
                let _ = self.pump.set(shared);
                self.servers.set(starts).map_err(|_| {
                    DispatchError::new("workspace_unavailable", "runtime initialization raced")
                })?;
                let sweep = crate::runtime::disk_sweep::spawn_disk_sweep(
                    &runtime.translator,
                    &runtime.notification_cache,
                    &runtime.subscription_registry,
                    &self.invalid,
                );
                *crate::util::lock_std(&self.sweep) = Some(sweep);
                Ok(runtime)
            })
            .await
    }
    pub(crate) async fn server(
        self: &Arc<Self>,
        dispatcher: &Arc<Dispatcher>,
    ) -> Result<McplsServer> {
        let runtime = self.prepare(dispatcher).await?;
        Ok(McplsServer::new(
            runtime.translator.clone(),
            runtime.notification_cache.clone(),
            runtime.workspace_roots.clone(),
            runtime.subscription_registry.clone(),
            self.selection.config.project_config_status,
            self.selection.config.mcp.clone(),
        ))
    }
    pub(crate) async fn demand(
        self: &Arc<Self>,
        dispatcher: &Arc<Dispatcher>,
        file: Option<&Path>,
        local_only: bool,
        tool: Option<crate::config::ToolKind>,
    ) -> Result<()> {
        let runtime = self.prepare(dispatcher).await?;
        if local_only {
            return Ok(());
        }
        let language = Self::demand_language(runtime, file).await?;
        let selected = Self::demand_route(runtime, language.as_ref(), tool);
        for start in self.servers.get().into_iter().flatten() {
            if tool.is_some() && selected.as_ref() != Some(&start.config.server_config().id()) {
                continue;
            }
            if language
                .as_ref()
                .is_some_and(|language| &start.config.server_config().language_id != language)
            {
                continue;
            }
            let id = start.config.server_config().id();
            let mut state = start.state.lock().await;
            if runtime
                .translator
                .dispatcher_server(&id)
                .is_some_and(|(alive, _)| alive)
            {
                continue;
            }
            let crashed = runtime.translator.dispatcher_server(&id).is_some();
            if crashed && !state.recovering && state.failed_at.is_none() {
                state.failed_at = Some(Instant::now());
                state.failures = state.failures.saturating_add(1);
            }
            if let Some(at) = state.failed_at {
                let retry = 5000u64
                    .saturating_sub(u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX));
                if retry > 0 {
                    let mut error = DispatchError::new(
                        "workspace_unavailable",
                        format!("server {id} is in startup backoff"),
                    );
                    error.retry = Some(retry);
                    return Err(error);
                }
                state.launched = false;
                state.failed_at = None;
            }
            if crashed && !state.recovering {
                state.launched = true;
                state.recovering = true;
                let resident = self.clone();
                let translator = runtime.translator.clone();
                let server_id = id.clone();
                crate::util::lock_std(&self.tasks).push(tokio::spawn(async move {
                    let failed = translator.respawn_if_dead(&server_id).await.is_err();
                    if let Some(start) = resident.servers.get().and_then(|starts| {
                        starts
                            .iter()
                            .find(|s| s.config.server_config().id() == server_id)
                    }) {
                        let mut state = start.state.lock().await;
                        state.recovering = false;
                        if failed {
                            state.failed_at = Some(Instant::now());
                            state.failures = state.failures.saturating_add(1);
                        }
                    }
                }));
            } else if !state.launched {
                state.launched = true;
                runtime.translator.dispatcher_expect(id.clone());
                let configured = self
                    .servers
                    .get()
                    .into_iter()
                    .flatten()
                    .map(|start| {
                        (
                            start.config.server_config().id(),
                            start.config.server_config().language_id.clone(),
                        )
                    })
                    .collect();
                let handle = crate::runtime::startup::spawn_dispatcher_server(
                    start.config.clone(),
                    runtime.translator.clone(),
                    self.pump.get().cloned().ok_or_else(|| {
                        DispatchError::new("workspace_unavailable", "runtime pump is not prepared")
                    })?,
                    self.invalid.clone(),
                    configured,
                );
                let mut tasks = crate::util::lock_std(&self.tasks);
                tasks.retain(|task| !task.is_finished());
                tasks.push(handle);
            }
            drop(state);
            self.wait_started(start, runtime, &id).await?;
        }
        Ok(())
    }
    fn demand_route(
        runtime: &WorkspaceRuntime,
        language: Option<&crate::config::LanguageId>,
        tool: Option<crate::config::ToolKind>,
    ) -> Option<crate::config::ServerId> {
        tool.and_then(|tool| runtime.translator.dispatcher_demand_route(language, tool))
    }
    async fn demand_language(
        runtime: &WorkspaceRuntime,
        file: Option<&Path>,
    ) -> Result<Option<crate::config::LanguageId>> {
        let language = if let Some(file) = file {
            let path = crate::bridge::ClientPath::try_from(file.to_path_buf())
                .map_err(|_| DispatchError::invalid(file))?;
            Some(
                runtime
                    .translator
                    .language_for_path(&path)
                    .await
                    .map_err(|_| DispatchError::invalid(file))?,
            )
        } else {
            None
        };
        Ok(language)
    }
    async fn wait_started(
        &self,
        start: &ServerStart,
        runtime: &WorkspaceRuntime,
        id: &crate::config::ServerId,
    ) -> Result<()> {
        loop {
            if self.invalid.is_cancelled() {
                return Err(DispatchError::new(
                    "workspace_invalidated",
                    "workspace runtime was invalidated",
                ));
            }
            if runtime
                .translator
                .dispatcher_server(id)
                .is_some_and(|(alive, _)| alive)
            {
                return Ok(());
            }
            if start.state.lock().await.failed_at.is_some()
                || runtime.translator.startup_failure(id).is_some()
            {
                let mut state = start.state.lock().await;
                if state.failed_at.is_none() {
                    state.failures = state.failures.saturating_add(1);
                    state.failed_at = Some(Instant::now());
                    start.pid.store(0, Ordering::Release);
                }
                drop(state);
                let mut error = DispatchError::new(
                    "workspace_unavailable",
                    format!("server {id} failed to start"),
                );
                error.retry = Some(5000);
                return Err(error);
            }
            tokio::select! {()=self.invalid.cancelled()=>return Err(DispatchError::new("workspace_invalidated","workspace runtime was invalidated")), ()=tokio::time::sleep(Duration::from_millis(10))=>{}}
        }
    }
    fn retire(self: &Arc<Self>) {
        if self.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        self.invalid.cancel();
        let resident = self.clone();
        tokio::spawn(async move {
            // Stored start configurations can outlive retirement. Stop their
            // shared backend explicitly rather than waiting for the final Arc.
            if let Some(starts) = resident.servers.get() {
                for start in starts {
                    if let Some(backend) = &start.config.watching {
                        backend.stop().await;
                    }
                }
            }
            if let Some(runtime) = resident.runtime.get() {
                runtime.translator.begin_shutdown();
                runtime.subscription_registry.invalidate().await;
                let sweep = crate::util::lock_std(&resident.sweep).take();
                if let Some(sweep) = sweep {
                    crate::runtime::disk_sweep::stop_disk_sweep(sweep).await;
                }
                runtime.translator.shutdown_servers().await;
                *runtime.notification_cache.lock().await = crate::bridge::NotificationCache::new();
            }
            let tasks = std::mem::take(&mut *crate::util::lock_std(&resident.tasks));
            for task in tasks {
                task.abort();
                let _ = task.await;
            }
            resident.cleaned.cancel();
        });
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
