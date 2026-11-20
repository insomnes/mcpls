//! Fresh workspace pulls, independent of the document diagnostic cache.

use lsp_types::{
    Code, DiagnosticOptions, DiagnosticProvider, Message, Uri, WorkspaceDiagnosticParams,
    WorkspaceDiagnosticReport, WorkspaceDiagnosticRequest, WorkspaceDocumentDiagnosticReport,
};
use schemars::JsonSchema;
use serde::Serialize;

use super::Translator;
use crate::bridge::{ClientPath, WorkspacePath, uri_to_path};
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::redaction::Redactions;
use crate::util::lock_std;

const MAX_DIAGNOSTICS: usize = 256;
const MAX_BYTES: usize = 131_072;

#[derive(Clone, Copy, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Complete,
    Partial,
    Unsupported,
    Failed,
}

#[derive(JsonSchema, Serialize)]
struct WorkspacePosition {
    line: u32,
    character: u32,
}

impl From<lsp_types::Position> for WorkspacePosition {
    fn from(position: lsp_types::Position) -> Self {
        Self {
            line: position.line,
            character: position.character,
        }
    }
}

#[derive(JsonSchema, Serialize)]
struct WorkspaceRange {
    start: WorkspacePosition,
    end: WorkspacePosition,
}

#[derive(JsonSchema, Serialize)]
#[serde(untagged)]
enum DiagnosticCode {
    Number(i32),
    Text(String),
}

#[derive(JsonSchema, Serialize)]
struct WorkspaceDiagnostic {
    file_path: String,
    message: String,
    range: WorkspaceRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<DiagnosticCode>,
}

impl WorkspaceDiagnostic {
    fn from_lsp(path: &str, item: lsp_types::Diagnostic, redactions: &Redactions) -> Option<Self> {
        let lsp_types::Range { start, end } = item.range;
        if (end.line, end.character) < (start.line, start.character) {
            return None;
        }
        let mut message = match item.message {
            Message::String(message) => message,
            Message::MarkupContent(message) => message.value,
        };
        redactions.redact_in_place(&mut message);
        Some(Self {
            file_path: path.to_owned(),
            message,
            range: WorkspaceRange {
                start: start.into(),
                end: end.into(),
            },
            severity: item.severity.map(u32::from),
            code: item.code.map(|code| match code {
                Code::Int(number) => DiagnosticCode::Number(number),
                Code::String(mut text) => {
                    redactions.redact_in_place(&mut text);
                    DiagnosticCode::Text(text)
                }
            }),
        })
    }
}

#[derive(JsonSchema, Serialize)]
pub struct WorkspaceDiagnosticsReport {
    server: ServerId,
    status: Status,
    truncated: bool,
    diagnostics: Vec<WorkspaceDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position_encoding: Option<String>,
}

impl Translator {
    pub(crate) fn has_workspace_diagnostic_server(&self, server: &ServerId) -> bool {
        self.configured_router.server_ids().contains(server)
    }

    pub(crate) async fn workspace_diagnostics(
        &self,
        server: ServerId,
    ) -> Result<Option<WorkspaceDiagnosticsReport>> {
        let mut report = WorkspaceDiagnosticsReport {
            server,
            status: Status::Failed,
            truncated: false,
            diagnostics: Vec::new(),
            position_encoding: None,
        };
        if self.workspace_roots.is_empty() || serde_json::to_vec(&report)?.len() > MAX_BYTES {
            return Ok(None);
        }
        let (client, identifier, encoding) = {
            let mut servers = lock_std(&self.servers);
            if servers.is_expected(&report.server) {
                return Err(Error::ServerInitializing {
                    server_id: report.server,
                });
            }
            let Some(server) = servers.server_mut(&report.server) else {
                return Ok(Some(report));
            };
            if server.is_dead().unwrap_or(true) {
                return Ok(Some(report));
            }
            let Some(options) =
                workspace_options(server.capabilities().diagnostic_provider.as_ref())
            else {
                report.status = Status::Unsupported;
                return Ok((serde_json::to_vec(&report)?.len() <= MAX_BYTES).then_some(report));
            };
            let connection = (
                server.client().clone(),
                options.identifier.clone(),
                server.position_encoding().as_str().to_owned(),
            );
            drop(servers);
            connection
        };
        let redactions = self.server_text_redactions();
        let Ok(response) = client
            .request_typed_classified::<WorkspaceDiagnosticRequest>(
                WorkspaceDiagnosticParams {
                    identifier,
                    ..WorkspaceDiagnosticParams::default()
                },
                client.request_timeout(),
            )
            .await
        else {
            return Ok(Some(report));
        };

        report.status = Status::Complete;
        report.position_encoding = Some(encoding);
        self.collect_workspace_diagnostics(report, response, &redactions)
            .await
    }

    async fn collect_workspace_diagnostics(
        &self,
        mut report: WorkspaceDiagnosticsReport,
        response: WorkspaceDiagnosticReport,
        redactions: &Redactions,
    ) -> Result<Option<WorkspaceDiagnosticsReport>> {
        // The complete envelope is the largest successful envelope.
        // Every retained item adds its JSON bytes and at most one comma.
        let mut bytes = serde_json::to_vec(&report)?.len();
        if bytes > MAX_BYTES {
            return Ok(None);
        }
        for document in response.items {
            let (uri, items) = match document {
                WorkspaceDocumentDiagnosticReport::WorkspaceFullDocumentDiagnosticReport(item) => {
                    (item.uri, Some(item.full_document_diagnostic_report.items))
                }
                WorkspaceDocumentDiagnosticReport::WorkspaceUnchangedDocumentDiagnosticReport(
                    item,
                ) => (item.uri, None),
            };
            if items.as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            let Some(path) = self.workspace_diagnostic_path(&uri).await else {
                continue;
            };
            let (Some(items), Some(path)) = (items, path.as_path().to_str()) else {
                // An unchanged report has no previous result in a fresh pull.
                report.status = Status::Partial;
                continue;
            };
            for item in items {
                if report.diagnostics.len() == MAX_DIAGNOSTICS {
                    report.truncated = true;
                    break;
                }
                let Some(diagnostic) = WorkspaceDiagnostic::from_lsp(path, item, redactions) else {
                    report.status = Status::Partial;
                    continue;
                };
                let next_bytes = bytes
                    .saturating_add(serde_json::to_vec(&diagnostic)?.len())
                    .saturating_add(usize::from(!report.diagnostics.is_empty()));
                if next_bytes <= MAX_BYTES {
                    bytes = next_bytes;
                    report.diagnostics.push(diagnostic);
                } else {
                    report.truncated = true;
                }
            }
            if report.diagnostics.len() == MAX_DIAGNOSTICS && report.truncated {
                break;
            }
        }
        if report.truncated {
            report.status = Status::Partial;
        }
        Ok(Some(report))
    }

    async fn workspace_diagnostic_path(&self, uri: &Uri) -> Option<WorkspacePath> {
        let path = ClientPath::try_from(uri_to_path(uri)?).ok()?;
        let canonical = self.validate_path(&path).await.ok()?;
        tokio::fs::symlink_metadata(canonical.as_path())
            .await
            .ok()?
            .is_file()
            .then_some(canonical)
    }
}

pub(super) const fn workspace_options(
    provider: Option<&DiagnosticProvider>,
) -> Option<&DiagnosticOptions> {
    let options = match provider {
        Some(DiagnosticProvider::DiagnosticOptions(options)) => options,
        Some(DiagnosticProvider::DiagnosticRegistrationOptions(registration)) => {
            &registration.diagnostic_options
        }
        None => return None,
    };
    if options.workspace_diagnostics {
        Some(options)
    } else {
        None
    }
}
