use std::{collections::HashMap, sync::Arc};

use semver::Version;
use tokio::sync::Mutex;
use tower_lsp::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
        HoverContents, HoverParams, InitializeParams, InitializeResult, InitializedParams,
        MarkupContent, MarkupKind, MessageType, Position, Range, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
    },
};

use crate::{
    manifest::{Dependency, Registry, Span, detect_manifest_kind, parse_manifest},
    registry::RegistryClient,
};

#[derive(Clone)]
struct Document {
    text: String,
    kind: Registry,
}

pub struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Url, Document>>>,
    registry: RegistryClient,
}

pub async fn run() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(Mutex::new(HashMap::new())),
        registry: RegistryClient::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: None,
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "ZedDeps language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        if let Some(kind) = detect_manifest_kind(params.text_document.uri.path()) {
            let uri = params.text_document.uri;
            self.documents.lock().await.insert(
                uri.clone(),
                Document {
                    text: params.text_document.text,
                    kind,
                },
            );
            self.publish_diagnostics(uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        {
            let mut documents = self.documents.lock().await;
            if let Some(document) = documents.get_mut(&uri) {
                document.text = change.text;
            } else if let Some(kind) = detect_manifest_kind(uri.path()) {
                documents.insert(
                    uri.clone(),
                    Document {
                        text: change.text,
                        kind,
                    },
                );
            }
        }
        self.publish_diagnostics(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().await.remove(&uri);
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some((document, dep)) = self.dependency_at(&uri, position).await else {
            return Ok(None);
        };

        let latest = self.registry.latest_for(&dep).await;
        let markdown = hover_markdown(&dep, latest.as_ref());
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(span_to_range(&document.text, &dep.value_span)),
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let position = params.range.start;
        let Some((document, dep)) = self.dependency_at(&uri, position).await else {
            return Ok(None);
        };
        if !dep.can_edit {
            return Ok(None);
        }
        let Ok(Some(latest)) = self.registry.latest_for(&dep).await else {
            return Ok(None);
        };
        if !is_outdated(&dep, &latest) {
            return Ok(None);
        }
        let Some(new_text) = dep.replacement_for(&latest) else {
            return Ok(None);
        };

        let edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri,
                vec![TextEdit {
                    range: span_to_range(&document.text, &dep.value_span),
                    new_text,
                }],
            )])),
            ..WorkspaceEdit::default()
        };
        let action = CodeAction {
            title: format!("Update {} to {}", dep.name, latest),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(edit),
            is_preferred: Some(true),
            ..CodeAction::default()
        };

        Ok(Some(vec![CodeActionOrCommand::CodeAction(action)]))
    }
}

impl Backend {
    async fn dependency_at(&self, uri: &Url, position: Position) -> Option<(Document, Dependency)> {
        let document = self.documents.lock().await.get(uri).cloned()?;
        let offset = position_to_offset(&document.text, position)?;
        let dep = parse_manifest(&document.text, document.kind)
            .into_iter()
            .find(|dep| dep.value_span.start <= offset && offset <= dep.value_span.end)?;
        Some((document, dep))
    }

    async fn publish_diagnostics(&self, uri: Url) {
        let Some(document) = self.documents.lock().await.get(&uri).cloned() else {
            return;
        };

        let mut diagnostics = Vec::new();
        for dep in parse_manifest(&document.text, document.kind) {
            let latest = self.registry.latest_for(&dep).await;
            match latest {
                Ok(Some(latest)) if is_outdated(&dep, &latest) => {
                    diagnostics.push(Diagnostic {
                        range: span_to_range(&document.text, &dep.value_span),
                        severity: Some(DiagnosticSeverity::INFORMATION),
                        source: Some("zeddeps".to_string()),
                        message: format!("{} {} is available", dep.name, latest),
                        ..Diagnostic::default()
                    });
                }
                _ => {}
            }
        }

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

fn hover_markdown(
    dep: &Dependency,
    latest: std::result::Result<&Option<Version>, &String>,
) -> String {
    let registry = match dep.registry {
        Registry::Cargo => "crates.io",
        Registry::Npm => "npm",
    };
    let package_url = dep.registry.package_url(&dep.name);
    match latest {
        Ok(Some(latest)) if is_outdated(dep, latest) => {
            let replacement = dep
                .replacement_for(latest)
                .unwrap_or_else(|| latest.to_string());
            format!(
                "**{}**\n\n[Open in {}]({})\n\nCurrent: `{}`\n\nLatest stable: `{}`\n\nReplacement: `{}`",
                dep.name, registry, package_url, dep.current, latest, replacement
            )
        }
        Ok(Some(latest)) => format!(
            "**{}**\n\n[Open in {}]({})\n\nCurrent: `{}`\n\nLatest stable: `{}`\n\nAlready up to date.",
            dep.name, registry, package_url, dep.current, latest
        ),
        Ok(None) => format!(
            "**{}**\n\n[Open in {}]({})\n\nCurrent: `{}`\n\nNo stable registry version was found.",
            dep.name, registry, package_url, dep.current
        ),
        Err(error) => format!(
            "**{}**\n\n[Open in {}]({})\n\nCurrent: `{}`\n\nCould not check latest version: `{}`",
            dep.name, registry, package_url, dep.current, error
        ),
    }
}

fn is_outdated(dep: &Dependency, latest: &Version) -> bool {
    dep.current_version
        .as_ref()
        .is_some_and(|current| latest > current)
}

fn span_to_range(text: &str, span: &Span) -> Range {
    Range {
        start: offset_to_position(text, span.start),
        end: offset_to_position(text, span.end),
    }
}

fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line = 0u32;
    let mut character = 0u32;
    for (offset, char) in text.char_indices() {
        if line == position.line && character == position.character {
            return Some(offset);
        }
        if char == '\n' {
            line += 1;
            character = 0;
        } else {
            character += char.len_utf16() as u32;
        }
    }
    (line == position.line && character == position.character).then_some(text.len())
}

fn offset_to_position(text: &str, target: usize) -> Position {
    let mut line = 0u32;
    let mut character = 0u32;
    for (offset, char) in text.char_indices() {
        if offset >= target {
            return Position { line, character };
        }
        if char == '\n' {
            line += 1;
            character = 0;
        } else {
            character += char.len_utf16() as u32;
        }
    }
    Position { line, character }
}

type HoverProviderCapability = tower_lsp::lsp_types::HoverProviderCapability;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Registry, parse_manifest};

    #[test]
    fn converts_offsets_and_positions() {
        let text = "a\nserde = \"1.0\"\n";
        let offset = text.find("1.0").unwrap();
        let position = offset_to_position(text, offset);
        assert_eq!(position.line, 1);
        assert_eq!(position_to_offset(text, position), Some(offset));
    }

    #[test]
    fn builds_hover_for_outdated_dependency() {
        let text = "[dependencies]\nserde = \"1.0\"\n";
        let dep = parse_manifest(text, Registry::Cargo).remove(0);
        let latest = Some(Version::parse("1.1.0").unwrap());
        let markdown = hover_markdown(&dep, Ok(&latest));
        assert!(markdown.contains("[Open in crates.io](https://crates.io/crates/serde)"));
        assert!(markdown.contains("Latest stable: `1.1.0`"));
        assert!(markdown.contains("Replacement: `1.1.0`"));
    }
}
