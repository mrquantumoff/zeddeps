use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::Mutex;
use tower_lsp::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
        HoverContents, HoverParams, InitializeParams, InitializeResult, InitializedParams,
        MarkupContent, MarkupKind, MessageType, Position, Range, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
    },
};

use crate::{
    manifest::{
        Dependency, DependencyVersion, ManifestKind, Registry, Span, detect_manifest_kind,
        parse_cargo_manifest, parse_manifest, requirements_include_paths,
    },
    registry::{LatestInfo, RegistryClient},
};

#[derive(Clone)]
struct Document {
    text: String,
    kind: ManifestKind,
}

struct EditTarget {
    uri: Url,
    text: String,
    span: Span,
}

struct DependencyTextEdit {
    uri: Url,
    text_edit: TextEdit,
    latest: DependencyVersion,
}

struct DependencyWorkspaceEdit {
    workspace_edit: WorkspaceEdit,
    latest: DependencyVersion,
}

pub struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Url, Document>>>,
    known_requirement_files: Arc<Mutex<HashSet<Url>>>,
    registry: RegistryClient,
}

pub async fn run() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(Mutex::new(HashMap::new())),
        known_requirement_files: Arc::new(Mutex::new(HashSet::new())),
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
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::SOURCE_FIX_ALL,
                        ]),
                        ..CodeActionOptions::default()
                    },
                )),
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
        if let Some(kind) = self.kind_for_uri(&params.text_document.uri).await {
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
        let kind = self.kind_for_uri(&uri).await;
        let mut should_publish = false;
        {
            let mut documents = self.documents.lock().await;
            if let Some(document) = documents.get_mut(&uri) {
                document.text = change.text;
                should_publish = true;
            } else if let Some(kind) = kind {
                documents.insert(
                    uri.clone(),
                    Document {
                        text: change.text,
                        kind,
                    },
                );
                should_publish = true;
            }
        }
        if should_publish {
            self.publish_diagnostics(uri).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let was_tracked = self.documents.lock().await.remove(&uri).is_some();
        let was_known_requirement = self.known_requirement_files.lock().await.remove(&uri);
        if was_tracked || was_known_requirement {
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some((document, mut dep)) = self.dependency_at(&uri, position).await else {
            return Ok(None);
        };
        resolve_workspace_dependency_from_path(&uri, &mut dep);

        let latest = self.registry.latest_for(&dep).await;
        if let Err(error) = &latest {
            self.log_dependency_error(&uri, &dep, "hover", error).await;
        }
        let markdown = hover_markdown(&dep, latest.as_ref().ok());
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(span_to_range(&document.text, &dep.value_span)),
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let Some(document) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(None);
        };

        let mut actions = Vec::new();
        if let Some((update_count, edit)) = self.update_all_workspace_edit(&uri, &document).await {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!(
                    "Update all {} outdated {} in file",
                    update_count,
                    if update_count == 1 {
                        "dependency"
                    } else {
                        "dependencies"
                    }
                ),
                kind: Some(CodeActionKind::SOURCE_FIX_ALL),
                edit: Some(edit),
                ..CodeAction::default()
            }));
        }

        if let Some(action) = self
            .update_dependency_code_action(&uri, &document, params.range.start)
            .await
        {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }
}

impl Backend {
    async fn kind_for_uri(&self, uri: &Url) -> Option<ManifestKind> {
        if let Some(kind) = detect_manifest_kind(uri.path()) {
            return Some(kind);
        }
        self.known_requirement_files
            .lock()
            .await
            .contains(uri)
            .then_some(ManifestKind::Requirements)
    }

    async fn dependency_at(&self, uri: &Url, position: Position) -> Option<(Document, Dependency)> {
        let document = self.documents.lock().await.get(uri).cloned()?;
        let offset = position_to_offset(&document.text, position)?;
        let dep = parse_manifest(&document.text, document.kind)
            .into_iter()
            .find(|dep| dep.value_span.start <= offset && offset <= dep.value_span.end)?;
        Some((document, dep))
    }

    async fn update_dependency_code_action(
        &self,
        uri: &Url,
        document: &Document,
        position: Position,
    ) -> Option<CodeAction> {
        let offset = position_to_offset(&document.text, position)?;
        let mut dep = parse_manifest(&document.text, document.kind)
            .into_iter()
            .find(|dep| dep.value_span.start <= offset && offset <= dep.value_span.end)?;
        let edit = self
            .update_dependency_workspace_edit(uri, document, &mut dep, "code action")
            .await?;

        Some(CodeAction {
            title: format!("Update {} to {}", dep.name, edit.latest),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(edit.workspace_edit),
            is_preferred: Some(true),
            ..CodeAction::default()
        })
    }

    async fn update_all_workspace_edit(
        &self,
        uri: &Url,
        document: &Document,
    ) -> Option<(usize, WorkspaceEdit)> {
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut update_count = 0;

        for mut dep in parse_manifest(&document.text, document.kind) {
            let Some(edit) = self
                .update_dependency_text_edit(uri, document, &mut dep, "update all")
                .await
            else {
                continue;
            };
            changes.entry(edit.uri).or_default().push(edit.text_edit);
            update_count += 1;
        }

        (update_count > 0).then(|| {
            (
                update_count,
                WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                },
            )
        })
    }

    async fn update_dependency_workspace_edit(
        &self,
        uri: &Url,
        document: &Document,
        dep: &mut Dependency,
        context: &str,
    ) -> Option<DependencyWorkspaceEdit> {
        let edit = self
            .update_dependency_text_edit(uri, document, dep, context)
            .await?;
        Some(DependencyWorkspaceEdit {
            latest: edit.latest,
            workspace_edit: WorkspaceEdit {
                changes: Some(HashMap::from([(edit.uri, vec![edit.text_edit])])),
                ..WorkspaceEdit::default()
            },
        })
    }

    async fn update_dependency_text_edit(
        &self,
        uri: &Url,
        document: &Document,
        dep: &mut Dependency,
        context: &str,
    ) -> Option<DependencyTextEdit> {
        let workspace_edit_target = resolve_workspace_dependency_from_path(uri, dep);
        if !dep.can_edit {
            return None;
        }
        let latest_result = match self.registry.latest_for(dep).await {
            Ok(latest_result) => latest_result,
            Err(error) => {
                self.log_dependency_error(uri, dep, context, &error).await;
                return None;
            }
        };
        let latest = latest_result.version?;
        if !is_outdated(dep, &latest) {
            return None;
        }
        let new_text = dep.replacement_for(&latest)?;
        let edit_target = workspace_edit_target.unwrap_or_else(|| EditTarget {
            uri: uri.clone(),
            text: document.text.clone(),
            span: dep.value_span.clone(),
        });

        Some(DependencyTextEdit {
            uri: edit_target.uri,
            text_edit: TextEdit {
                range: span_to_range(&edit_target.text, &edit_target.span),
                new_text,
            },
            latest,
        })
    }

    async fn publish_diagnostics(&self, uri: Url) {
        let Some(document) = self.documents.lock().await.get(&uri).cloned() else {
            return;
        };

        let diagnostics = self
            .diagnostics_for_text(&uri, &document.text, document.kind)
            .await;
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;

        if document.kind == ManifestKind::Requirements {
            self.publish_included_requirements_diagnostics(&uri, &document.text)
                .await;
        }
    }

    async fn diagnostics_for_text(
        &self,
        uri: &Url,
        text: &str,
        kind: ManifestKind,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for mut dep in parse_manifest(text, kind) {
            resolve_workspace_dependency_from_path(uri, &mut dep);
            let latest = self.registry.latest_for(&dep).await;
            match latest {
                Ok(latest)
                    if latest
                        .version
                        .as_ref()
                        .is_some_and(|v| is_outdated(&dep, v)) =>
                {
                    diagnostics.push(Diagnostic {
                        range: span_to_range(text, &dep.value_span),
                        severity: Some(DiagnosticSeverity::INFORMATION),
                        source: Some("zalezhnosti".to_string()),
                        message: format!("{} {} is available", dep.name, latest.version.unwrap()),
                        ..Diagnostic::default()
                    });
                }
                Err(error) => {
                    self.log_dependency_error(uri, &dep, "diagnostics", &error)
                        .await;
                }
                _ => {}
            }
        }

        diagnostics
    }

    async fn log_dependency_error(&self, uri: &Url, dep: &Dependency, context: &str, error: &str) {
        self.client
            .log_message(
                MessageType::ERROR,
                format!(
                    "Zalezhnosti {context} failed for {} in {}: {error}",
                    dep.name, uri
                ),
            )
            .await;
    }

    async fn publish_included_requirements_diagnostics(&self, root_uri: &Url, root_text: &str) {
        let Ok(root_path) = root_uri.to_file_path() else {
            return;
        };
        let Some(root_dir) = root_path.parent().map(Path::to_path_buf) else {
            return;
        };

        let mut visited = HashSet::from([normalize_requirement_path(root_path)]);
        let mut stack = vec![(root_dir, root_text.to_string())];

        while let Some((base_dir, text)) = stack.pop() {
            for include in requirements_include_paths(&text) {
                let include_path = normalize_requirement_path(resolve_requirement_include_path(
                    &base_dir, &include,
                ));
                if !visited.insert(include_path.clone()) {
                    continue;
                }

                let Ok(include_uri) = Url::from_file_path(&include_path) else {
                    continue;
                };
                self.known_requirement_files
                    .lock()
                    .await
                    .insert(include_uri.clone());

                let include_text = self
                    .documents
                    .lock()
                    .await
                    .get(&include_uri)
                    .map(|document| document.text.clone())
                    .or_else(|| std::fs::read_to_string(&include_path).ok());
                let Some(include_text) = include_text else {
                    continue;
                };

                let diagnostics = self
                    .diagnostics_for_text(&include_uri, &include_text, ManifestKind::Requirements)
                    .await;
                self.client
                    .publish_diagnostics(include_uri, diagnostics, None)
                    .await;

                if let Some(include_dir) = include_path.parent().map(Path::to_path_buf) {
                    stack.push((include_dir, include_text));
                }
            }
        }
    }
}

fn resolve_requirement_include_path(base_dir: &Path, include: &str) -> PathBuf {
    let include = include.trim_matches(['"', '\'']);
    let path = PathBuf::from(include);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn normalize_requirement_path(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn hover_markdown(dep: &Dependency, latest: Option<&LatestInfo>) -> String {
    match latest {
        Some(latest_info)
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
        Some(
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
        Some(latest_info @ LatestInfo { version: None, .. }) => format!(
            "**{}**\n\n{}\n\nCurrent: `{}`\n\nNo stable registry version was found.",
            dep.name,
            dependency_links_markdown(dep, Some(latest_info)),
            dep.current
        ),
        None => format!(
            "**{}**\n\n{}\n\nCurrent: `{}`\n\nLatest stable: unavailable.",
            dep.name,
            dependency_links_markdown(dep, None),
            dep.current
        ),
    }
}

fn dependency_links_markdown(dep: &Dependency, latest: Option<&LatestInfo>) -> String {
    let registry = match dep.registry {
        Registry::Cargo => "crates.io",
        Registry::Npm => "npm",
        Registry::Pypi => "PyPI",
    };
    let package_url = dep.registry.package_url(&dep.name);
    let mut links = format!("[Open in {registry}]({package_url})");
    if let Some(repository_url) = latest.and_then(|latest| latest.repository_url.as_deref())
        && repository_url != package_url
    {
        links.push_str(&format!(" | [Open repository]({repository_url})"));
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

fn is_outdated(dep: &Dependency, latest: &DependencyVersion) -> bool {
    match latest {
        DependencyVersion::Semver(latest) => dep.current_version.as_ref().is_some_and(
            |current| matches!(current, DependencyVersion::Semver(current) if latest > current),
        ),
        DependencyVersion::Pep440(latest) => dep
            .python_specifiers
            .as_ref()
            .is_some_and(|specifiers| !specifiers.contains(latest)),
    }
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
    use crate::manifest::{DependencyVersion, ManifestKind, parse_manifest};
    use semver::Version;
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
        let dep = parse_manifest(text, ManifestKind::Cargo).remove(0);
        let latest = LatestInfo {
            version: Some(DependencyVersion::Semver(Version::parse("1.1.0").unwrap())),
            repository_url: None,
        };
        let markdown = hover_markdown(&dep, Some(&latest));
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
        let dep = parse_manifest(text, ManifestKind::PackageJson).remove(0);
        let latest = LatestInfo {
            version: Some(DependencyVersion::Semver(Version::parse("18.2.0").unwrap())),
            repository_url: Some("https://github.com/facebook/react".to_string()),
        };

        let markdown = hover_markdown(&dep, Some(&latest));

        assert!(markdown.contains("[Open in npm](https://www.npmjs.com/package/react)"));
        assert!(markdown.contains("[Open repository](https://github.com/facebook/react)"));
    }

    #[test]
    fn builds_hover_for_outdated_pypi_dependency() {
        let text = "requests>=2,<3\n";
        let dep = parse_manifest(text, ManifestKind::Requirements).remove(0);
        let latest = LatestInfo {
            version: Some(DependencyVersion::Pep440("3.0.0".parse().unwrap())),
            repository_url: Some("https://github.com/psf/requests".to_string()),
        };

        let markdown = hover_markdown(&dep, Some(&latest));

        assert!(markdown.contains("[Open in PyPI](https://pypi.org/project/requests/)"));
        assert!(markdown.contains("[Open repository](https://github.com/psf/requests)"));
        assert!(markdown.contains("Latest stable: `3.0.0`"));
        assert!(markdown.contains("Replacement: `==3.0.0`"));
    }

    #[test]
    fn python_specifier_containment_drives_outdated_status() {
        let text = "requests>=2,<3\n";
        let dep = parse_manifest(text, ManifestKind::Requirements).remove(0);

        assert!(!is_outdated(
            &dep,
            &DependencyVersion::Pep440("2.31.0".parse().unwrap())
        ));
        assert!(is_outdated(
            &dep,
            &DependencyVersion::Pep440("3.0.0".parse().unwrap())
        ));
    }

    #[test]
    fn resolves_relative_requirement_include_paths() {
        let base = PathBuf::from("project").join("requirements");

        assert_eq!(
            resolve_requirement_include_path(&base, "dev.txt"),
            base.join("dev.txt")
        );
        assert_eq!(
            resolve_requirement_include_path(&base, "\"constraints.txt\""),
            base.join("constraints.txt")
        );
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

        let mut dep = parse_manifest(member_text, ManifestKind::Cargo).remove(0);
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
