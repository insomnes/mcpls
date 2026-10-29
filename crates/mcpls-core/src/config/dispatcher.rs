//! Explicit service-controlled workspace approvals and lifecycle limits.

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A repository checkout or explicit non-Git root and its service profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatcherEntry {
    /// Absolute path to an existing checkout or directory.
    pub path: PathBuf,
    /// Absolute path to a service-controlled profile.
    pub profile: PathBuf,
}

/// Dispatcher mode is disabled unless `enabled` is true.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatcherConfig {
    /// Opt in to isolated worktree runtimes.
    pub enabled: bool,
    /// Maximum resident runtimes, including starts and cleanup.
    pub max_resident_workspaces: NonZeroUsize,
    /// Global concurrent LSP initialize limit.
    pub max_concurrent_starts: NonZeroUsize,
    /// Inactive, unpinned runtime lifetime.
    pub idle_timeout_seconds: NonZeroU64,
    /// Interval between identity and profile checks.
    pub reconcile_interval_seconds: NonZeroU64,
    /// Approved Git common-directory identities.
    pub repositories: Vec<DispatcherEntry>,
    /// Approved explicit non-Git roots.
    pub workspaces: Vec<DispatcherEntry>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_resident_workspaces: NonZeroUsize::MIN.saturating_add(3),
            max_concurrent_starts: NonZeroUsize::MIN.saturating_add(1),
            idle_timeout_seconds: NonZeroU64::MIN.saturating_add(599),
            reconcile_interval_seconds: NonZeroU64::MIN.saturating_add(29),
            repositories: Vec::new(),
            workspaces: Vec::new(),
        }
    }
}
