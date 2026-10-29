//! Service profiles resolve only documented workspace-relative paths.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::{DispatchError, Result};
use crate::config::{ConfiguredRoot, ServerConfig};

pub(super) fn read(
    path: &Path,
    root: &Path,
    daemon: &ServerConfig,
    automatic: bool,
    automatic_parents: &[PathBuf],
) -> Result<(ServerConfig, String)> {
    let bytes = crate::util::read_regular_file_bounded(
        path,
        std::num::NonZeroU64::MIN.saturating_add(1024 * 1024 - 1),
    )
    .map_err(|_| DispatchError::profile(path))?;
    if bytes.len() > 1024 * 1024 {
        return Err(DispatchError::profile(path));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| DispatchError::profile(path))?;
    let raw: toml::Value = toml::from_str(text).map_err(|_| DispatchError::profile(path))?;
    if raw.get("mcp").is_some() || raw.get("dispatcher").is_some() {
        return Err(DispatchError::profile(path));
    }
    let mut config: ServerConfig =
        toml::from_str(text).map_err(|_| DispatchError::profile(path))?;
    if automatic && !is_standalone(&config) {
        return Err(DispatchError::profile(path));
    }
    if config.workspace.roots.is_empty() {
        config
            .workspace
            .roots
            .push(ConfiguredRoot::new(".").map_err(|_| DispatchError::profile(path))?);
    }
    for entry in &mut config.workspace.roots {
        let relative = entry.as_path();
        if relative.is_absolute() {
            return Err(DispatchError::profile(path));
        }
        let resolved =
            std::fs::canonicalize(root.join(relative)).map_err(|_| DispatchError::profile(path))?;
        if !resolved.is_dir() || !resolved.starts_with(root) {
            return Err(DispatchError::profile(path));
        }
        *entry = ConfiguredRoot::new(resolved).map_err(|_| DispatchError::profile(path))?;
    }
    for server in &mut config.lsp_servers {
        let command = PathBuf::from(server.command.as_str());
        if command.is_relative() && command.components().count() > 1 {
            let resolved = root.join(command).to_string_lossy().into_owned();
            server.command = server
                .command
                .clone()
                .retarget(
                    crate::config::ServerCommand::new(resolved)
                        .map_err(|_| DispatchError::profile(path))?,
                )
                .map_err(|_| DispatchError::profile(path))?;
        }
        if server.language_id.as_str() == "rust"
            && let Some(projects) = server
                .initialization_options
                .as_mut()
                .and_then(|v| v.get_mut("linkedProjects"))
                .and_then(serde_json::Value::as_array_mut)
        {
            for project in projects {
                if let Some(value) = project.as_str() {
                    let relative = Path::new(value);
                    if relative.is_absolute() {
                        return Err(DispatchError::profile(path));
                    }
                    let resolved = std::fs::canonicalize(root.join(relative))
                        .map_err(|_| DispatchError::profile(path))?;
                    if !resolved.starts_with(root) {
                        return Err(DispatchError::profile(path));
                    }
                    *project = serde_json::Value::String(resolved.to_string_lossy().into_owned());
                }
            }
        }
    }
    config.workspace_trust = match &daemon.workspace_trust {
        crate::config::WorkspaceTrust::Trusted => crate::config::WorkspaceTrust::Trusted,
        crate::config::WorkspaceTrust::Untrusted(allowlist) => {
            let canonical =
                std::fs::canonicalize(path).map_err(|_| DispatchError::profile(path))?;
            if canonical.starts_with(root)
                || automatic_parents
                    .iter()
                    .any(|parent| canonical.starts_with(parent))
            {
                return Err(DispatchError::profile(path));
            }
            crate::config::WorkspaceTrust::untrusted(
                allowlist
                    .as_slice()
                    .iter()
                    .filter(|id| config.lsp_servers.iter().any(|server| server.id() == **id))
                    .cloned(),
            )
        }
    };
    config.mcp = daemon.mcp.clone();
    config
        .validate()
        .map_err(|_| DispatchError::profile(path))?;
    Ok((config, format!("{:x}", Sha256::digest(&bytes))))
}

fn is_standalone(config: &ServerConfig) -> bool {
    let roots = &config.workspace.roots;
    (roots.is_empty() || matches!(roots.as_slice(), [root] if root.as_path() == Path::new(".")))
        && !config.lsp_servers.iter().any(|server| {
            server.language_id.as_str() == "rust"
                && server
                    .initialization_options
                    .as_ref()
                    .is_some_and(|options| options.get("linkedProjects").is_some())
        })
}
