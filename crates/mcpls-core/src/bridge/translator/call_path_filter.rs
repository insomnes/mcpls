//! Request-local lexical source-path membership, without filesystem access.

use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

/// Call peer path filter.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(try_from = "serde_json::Map<String, serde_json::Value>")]
#[schemars(with = "CallPathFilterWire")]
pub struct CallPathFilter {
    include: Vec<PathBuf>,
    exclude: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallPathFilterWire {
    #[serde(default)]
    #[schemars(length(max = 32), extend("items" = {"type":"string","minLength":1,"maxLength":4096}))]
    include: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 32), extend("items" = {"type":"string","minLength":1,"maxLength":4096}))]
    exclude: Vec<String>,
}

fn usable(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|part| part == Component::ParentDir)
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

impl TryFrom<serde_json::Map<String, serde_json::Value>> for CallPathFilter {
    type Error = String;

    fn try_from(fields: serde_json::Map<String, serde_json::Value>) -> Result<Self, Self::Error> {
        let wire: CallPathFilterWire = serde_json::from_value(serde_json::Value::Object(fields))
            .map_err(|error| error.to_string())?;
        Self::try_from(wire)
    }
}

impl TryFrom<CallPathFilterWire> for CallPathFilter {
    type Error = String;

    fn try_from(wire: CallPathFilterWire) -> Result<Self, Self::Error> {
        let paths = |values: Vec<String>| -> Result<Vec<PathBuf>, String> {
            if values.len() > 32 {
                return Err("call_path_filter lists permit at most 32 paths".into());
            }
            values.into_iter().map(|value| {
                if !(1..=4096).contains(&value.len()) || !usable(Path::new(&value)) {
                    return Err("call_path_filter paths must be absolute, 1..4096 bytes, without NUL or parent components".into());
                }
                Ok(PathBuf::from(value))
            }).collect()
        };
        Ok(Self {
            include: paths(wire.include)?,
            exclude: paths(wire.exclude)?,
        })
    }
}

impl CallPathFilter {
    /// Empty lists preserve the unfiltered traversal behavior.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    /// Match provider URI spelling before any authorization canonicalization.
    #[must_use]
    pub fn matches_uri(&self, uri: &str) -> bool {
        // URL parsing removes dot segments. Reject parent components first,
        // including URI-encoded dots and raw URL backslash separators. Query
        // and fragment text do not belong to the native path.
        if uri
            .split(['?', '#'])
            .next()
            .unwrap_or(uri)
            .split(['/', '\\'])
            .any(|part| part.replace("%2e", ".").replace("%2E", ".") == "..")
        {
            return false;
        }
        let Ok(uri) =
            serde_json::from_value::<lsp_types::Uri>(serde_json::Value::String(uri.to_owned()))
        else {
            return false;
        };
        let Some(path) = crate::bridge::uri_to_path(&uri).filter(|path| usable(path)) else {
            return false;
        };
        (self.include.is_empty() || self.include.iter().any(|prefix| path.starts_with(prefix)))
            && !self.exclude.iter().any(|prefix| path.starts_with(prefix))
    }
}
