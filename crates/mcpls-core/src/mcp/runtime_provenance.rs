//! Local installation receipt matched against the running executable.

use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline, extend("additionalProperties" = true))]
pub(super) struct RuntimeProvenance {
    status: Status,
    revision: String,
    executable_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
enum Status {
    Verified,
    Unknown,
}

pub(super) fn current() -> &'static RuntimeProvenance {
    static PROVENANCE: OnceLock<RuntimeProvenance> = OnceLock::new();
    PROVENANCE.get_or_init(|| {
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_mut, reason = "only Linux measures executable provenance")
        )]
        let mut provenance = RuntimeProvenance {
            status: Status::Unknown,
            revision: "unknown".into(),
            executable_sha256: "unknown".into(),
        };
        // Other platforms conservatively report unknown until measurement of
        // the executed image, rather than a potentially replaced path, is supported.
        #[cfg(target_os = "linux")]
        provenance.measure();
        provenance
    })
}

impl RuntimeProvenance {
    /// Check stored syntax without reauthenticating a historical observation.
    pub(super) fn is_well_formed(&self) -> bool {
        let revision_is_hex = self.revision.len() == 40
            && self
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let digest_is_hex = self.executable_sha256.len() == 64
            && self
                .executable_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        match self.status {
            Status::Verified => revision_is_hex && digest_is_hex,
            Status::Unknown => {
                self.revision == "unknown" && (digest_is_hex || self.executable_sha256 == "unknown")
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn measure(&mut self) {
        use std::fs::{File, read, read_link};
        use std::io::Read;

        use sha2::{Digest, Sha256};

        let Ok(mut executable) = File::open("/proc/self/exe") else {
            return;
        };
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match executable.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let Some(bytes) = buffer.get(..count) else {
                        return;
                    };
                    hash.update(bytes);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
        self.executable_sha256 = format!("{:x}", hash.finalize());
        let Some(receipt) = read_link("/proc/self/exe")
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join("build.json")))
            .and_then(|path| read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            return;
        };
        let Some(revision) = receipt.get("revision").and_then(serde_json::Value::as_str) else {
            return;
        };
        if revision.len() == 40
            && revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && receipt.get("sha256").and_then(serde_json::Value::as_str)
                == Some(self.executable_sha256.as_str())
        {
            self.revision = revision.into();
            self.status = Status::Verified;
        }
    }
}
