//! Canonical, fail-closed Git worktree discovery.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{DispatchError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObjectId {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    created: Option<std::time::SystemTime>,
}

impl ObjectId {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).map_err(|_| DispatchError::invalid(path))?;
        Ok(Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            created: metadata.created().ok(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    pub root: PathBuf,
    pub common: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub objects: Vec<ObjectId>,
}

pub(super) fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    // Git environment overrides must not redirect identity discovery.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command.output().map_err(|_| DispatchError::invalid(dir))?;
    if !output.status.success() {
        return Err(DispatchError::invalid(dir));
    }
    Ok(output.stdout)
}

fn git_path(dir: &Path, argument: &str) -> Result<PathBuf> {
    let bytes = git(dir, &["rev-parse", "--path-format=absolute", argument])?;
    let value = String::from_utf8(bytes).map_err(|_| DispatchError::invalid(dir))?;
    std::fs::canonicalize(value.trim_end_matches('\n')).map_err(|_| DispatchError::invalid(dir))
}

pub(super) fn inspect(path: &Path, directory: bool) -> Result<(PathBuf, Option<Identity>)> {
    if !path.is_absolute() {
        return Err(DispatchError::invalid(path));
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| DispatchError::invalid(path))?;
    let dir = if canonical.is_dir() {
        canonical.as_path()
    } else if directory {
        return Err(DispatchError::invalid(path));
    } else {
        canonical
            .parent()
            .ok_or_else(|| DispatchError::invalid(path))?
    };
    // A .git entry in any ancestor establishes a repository boundary even
    // when Git cannot read it. Such a boundary must never fall back to non-Git.
    let boundary = dir
        .ancestors()
        .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok());
    let Some(_) = boundary else {
        return Ok((canonical, None));
    };
    let root = git_path(dir, "--show-toplevel")?;
    let common = git_path(dir, "--git-common-dir")?;
    let git_dir = git_path(dir, "--absolute-git-dir")?;
    let listed = git(&root, &["worktree", "list", "--porcelain", "-z"])?;
    let registered = listed
        .split(|b| *b == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .any(|entry| {
            String::from_utf8(entry.to_vec())
                .ok()
                .and_then(|s| std::fs::canonicalize(s).ok())
                .as_ref()
                == Some(&root)
        });
    if !registered {
        return Err(DispatchError::not_approved(path));
    }
    let objects = [&root, &common, &git_dir]
        .into_iter()
        .map(|p| ObjectId::read(p))
        .collect::<Result<_>>()?;
    Ok((
        canonical,
        Some(Identity {
            root,
            common: Some(common),
            git_dir: Some(git_dir),
            objects,
        }),
    ))
}
