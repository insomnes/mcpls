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

pub(super) fn worktrees(common: &Path) -> Result<Vec<PathBuf>> {
    let listed = git(common, &["worktree", "list", "--porcelain", "-z"])?;
    Ok(listed
        .split(|byte| *byte == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect())
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
    // An accessible empty directory is not Git metadata. Every other marker,
    // including unreadable or malformed metadata, remains a fail-closed boundary.
    let boundary = dir.ancestors().find(|ancestor| {
        let marker = ancestor.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                std::fs::read_dir(marker).map_or(true, |mut entries| entries.next().is_some())
            }
            Ok(_) => true,
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        }
    });
    let Some(boundary) = boundary else {
        return Ok((canonical, None));
    };
    let root = git_path(dir, "--show-toplevel")?;
    // Git can ignore corrupt nested metadata and discover an approved parent.
    // The nearest nonempty marker remains authoritative even when Git rejects it.
    if root != boundary {
        return Err(DispatchError::not_approved(path));
    }
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
