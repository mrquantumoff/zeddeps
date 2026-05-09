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
    manifest::{
        Dependency, Registry, Span, detect_manifest_kind, parse_cargo_manifest, parse_manifest,
    },
    registry::{LatestInfo, RegistryClient},
};

#[derive(Clone)]
struct Document {
    text: String,
    kind: Registry,
}

struct EditTarget {
    uri: Url,
    text: String,
    span: Span,
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
            .log_message(MessageType::INFO, "Zalezhnosti language server initialized")
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
        let Some((document, mut dep)) = self.dependency_at(&uri, position).await else {
            return Ok(None);
        };
        resolve_workspace_dependency_from_path(&uri, &mut dep);

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
        let Some((document, mut dep)) = self.dependency_at(&uri, position).await else {
            return Ok(None);
        };
        let workspace_edit_target = resolve_workspace_dependency_from_path(&uri, &mut dep);
        if !dep.can_edit {
            return Ok(None);
        }
        let Ok(latest_result) = self.registry.latest_for(&dep).await else {
            return Ok(None);
        };
        let Some(latest) = latest_result.version else {
            return Ok(None);
        };
        if !is_outdated(&dep, &latest) {
            return Ok(None);
        }
        let Some(new_text) = dep.replacement_for(&latest) else {
            return Ok(None);
        };
        let edit_target = workspace_edit_target.unwrap_or_else(|| EditTarget {
            uri,
            text: document.text.clone(),
            span: dep.value_span.clone(),
        });

        let edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                edit_target.uri,
                vec![TextEdit {
                    range: span_to_range(&edit_target.text, &edit_target.span),
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
        for mut dep in parse_manifest(&document.text, document.kind) {
            resolve_workspace_dependency_from_path(&uri, &mut dep);
            let latest = self.registry.latest_for(&dep).await;
            match latest {
                Ok(latest)
                    if latest
                        .version
                        .as_ref()
                        .is_some_and(|v| is_outdated(&dep, v)) =>
                {
                    diagnostics.push(Diagnostic {
                        range: span_to_range(&document.text, &dep.value_span),
                        severity: Some(DiagnosticSeverity::INFORMATION),
                        source: Some("zalezhnosti".to_string()),
                        message: format!("{} {} is available", dep.name, latest.version.unwrap()),
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

fn hover_markdown(dep: &Dependency, latest: std::result::Result<&LatestInfo, &String>) -> String {
    match latest {
        Ok(latest_info)
            if latest_info
                .version
                .as_ref()
                .is_some_and(|latest| is_outdated(dep, latest)) =>
        {
            let latest = latest_info.version.as_ref().expect("checked by guard");
            let replacement = dep
                .replacement_for(latest)
                .unwrap_or_else(|| latest.to_string());
            format!(
                "**{}**\n\n{}\n\nCurrent: `{}`\n\nLatest stable: `{}`\n\nReplacement: `{}`",
                dep.name,
                dependency_links_markdown(dep, Some(latest_info)),
                dep.current,
                latest,
                replacement
            )
        }
        Ok(
            latest_info @ LatestInfo {
                version: Some(latest),
                ..
            },
        ) => format!(
            "**{}**\n\n{}\n\nCurrent: `{}`\n\nLatest stable: `{}`\n\nAlready up to date.",
            dep.name,
            dependency_links_markdown(dep, Some(latest_info)),
            dep.current,
            latest
        ),
        Ok(latest_info @ LatestInfo { version: None, .. }) => format!(
            "**{}**\n\n{}\n\nCurrent: `{}`\n\nNo stable registry version was found.",
            dep.name,
            dependency_links_markdown(dep, Some(latest_info)),
            dep.current
        ),
        Err(error) => format!(
            "**{}**\n\n{}\n\nCurrent: `{}`\n\nCould not check latest version: `{}`",
            dep.name,
            dependency_links_markdown(dep, None),
            dep.current,
            error
        ),
    }
}

fn dependency_links_markdown(dep: &Dependency, latest: Option<&LatestInfo>) -> String {
    let registry = match dep.registry {
        Registry::Cargo => "crates.io",
        Registry::Npm => "npm",
    };
    let package_url = dep.registry.package_url(&dep.name);
    let mut links = format!("[Open in {registry}]({package_url})");
    if let Some(repository_url) = latest.and_then(|latest| latest.repository_url.as_deref()) {
        if repository_url != package_url {
            links.push_str(&format!(" | [Open repository]({repository_url})"));
        }
    }
    links
}

fn resolve_workspace_dependency_from_path(uri: &Url, dep: &mut Dependency) -> Option<EditTarget> {
    if !dep.is_workspace {
        return None;
    }
    let Ok(path) = uri.to_file_path() else {
        return None;
    };
    let mut dir = path.parent();
    while let Some(d) = dir {
        let workspace_toml = d.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&workspace_toml) {
            let workspace_deps = parse_cargo_manifest(&text);
            if let Some(ws_dep) = workspace_deps
                .into_iter()
                .find(|d| d.name == dep.name && d.section == "workspace.dependencies")
            {
                dep.current = ws_dep.current.clone();
                dep.current_version = ws_dep.current_version.clone();
                dep.prefix = ws_dep.prefix.clone();
                dep.can_edit = ws_dep.can_edit;

                if !ws_dep.can_edit {
                    return None;
                }
                let Ok(uri) = Url::from_file_path(workspace_toml) else {
                    return None;
                };
                return Some(EditTarget {
                    uri,
                    text,
                    span: ws_dep.value_span,
                });
            }
        }
        dir = d.parent();
    }
    None
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
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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
        let latest = LatestInfo {
            version: Some(Version::parse("1.1.0").unwrap()),
            repository_url: None,
        };
        let markdown = hover_markdown(&dep, Ok(&latest));
        assert!(markdown.contains("[Open in crates.io](https://crates.io/crates/serde)"));
        assert!(markdown.contains("Latest stable: `1.1.0`"));
        assert!(markdown.contains("Replacement: `1.1.0`"));
    }

    #[test]
    fn hover_includes_registry_and_repository_links() {
        let text = r#"{
  "dependencies": {
    "react": "18.2.0"
  }
}"#;
        let dep = parse_manifest(text, Registry::Npm).remove(0);
        let latest = LatestInfo {
            version: Some(Version::parse("18.2.0").unwrap()),
            repository_url: Some("https://github.com/facebook/react".to_string()),
        };

        let markdown = hover_markdown(&dep, Ok(&latest));

        assert!(markdown.contains("[Open in npm](https://www.npmjs.com/package/react)"));
        assert!(markdown.contains("[Open repository](https://github.com/facebook/react)"));
    }

    #[test]
    fn resolves_workspace_dependency_to_workspace_manifest_edit_target() {
        let temp_root = std::env::temp_dir().join(format!(
            "zalezhnosti-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let member_dir = temp_root.join("crates").join("member");
        fs::create_dir_all(&member_dir).unwrap();

        let workspace_manifest = temp_root.join("Cargo.toml");
        let workspace_text = r#"
[workspace]
members = ["crates/member"]

[workspace.dependencies]
serde = "1.0"
"#;
        fs::write(&workspace_manifest, workspace_text).unwrap();

        let member_manifest = member_dir.join("Cargo.toml");
        let member_text = r#"
[package]
name = "member"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
"#;
        fs::write(&member_manifest, member_text).unwrap();

        let mut dep = parse_manifest(member_text, Registry::Cargo).remove(0);
        let uri = Url::from_file_path(&member_manifest).unwrap();
        let edit_target = resolve_workspace_dependency_from_path(&uri, &mut dep).unwrap();

        assert_eq!(dep.current, "1.0");
        assert!(dep.can_edit);
        assert_eq!(
            edit_target.uri,
            Url::from_file_path(&workspace_manifest).unwrap()
        );
        assert_eq!(
            &edit_target.text[edit_target.span.start..edit_target.span.end],
            "1.0"
        );

        fs::remove_dir_all(temp_root).unwrap();
    }
}
