use std::str::FromStr;

use pep440_rs::VersionSpecifiers;
use pep508_rs::{Requirement as Pep508Requirement, VerbatimUrl, VersionOrUrl};
use semver::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Registry {
    Cargo,
    Npm,
    Pypi,
}

impl Registry {
    pub fn package_url(&self, name: &str) -> String {
        match self {
            Registry::Cargo => format!("https://crates.io/crates/{name}"),
            Registry::Npm => format!("https://www.npmjs.com/package/{name}"),
            Registry::Pypi => format!("https://pypi.org/project/{name}/"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestKind {
    Cargo,
    PackageJson,
    Pyproject,
    Requirements,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyVersion {
    Semver(Version),
    Pep440(pep440_rs::Version),
}

impl std::fmt::Display for DependencyVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyVersion::Semver(version) => write!(f, "{version}"),
            DependencyVersion::Pep440(version) => write!(f, "{version}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub section: String,
    pub registry: Registry,
    pub current: String,
    pub value_span: Span,
    pub can_edit: bool,
    pub prefix: String,
    pub current_version: Option<DependencyVersion>,
    pub python_specifiers: Option<VersionSpecifiers>,
    pub is_workspace: bool,
}

impl Dependency {
    pub fn replacement_for(&self, latest: &DependencyVersion) -> Option<String> {
        if !self.can_edit {
            return None;
        }
        match (self.registry, latest) {
            (Registry::Pypi, DependencyVersion::Pep440(version)) => Some(format!("=={version}")),
            (Registry::Cargo | Registry::Npm, DependencyVersion::Semver(version)) => {
                Some(format!("{}{}", self.prefix, version))
            }
            _ => None,
        }
    }
}

const CARGO_SECTIONS: &[&str] = &[
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "workspace.dependencies",
];
const NPM_SECTIONS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

pub fn detect_manifest_kind(uri_path: &str) -> Option<ManifestKind> {
    let normalized = uri_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    if file_name == "Cargo.toml" {
        Some(ManifestKind::Cargo)
    } else if file_name == "package.json" {
        Some(ManifestKind::PackageJson)
    } else if file_name == "pyproject.toml" {
        Some(ManifestKind::Pyproject)
    } else if is_requirements_file(file_name) {
        Some(ManifestKind::Requirements)
    } else {
        None
    }
}

fn is_requirements_file(file_name: &str) -> bool {
    file_name == "requirements.txt"
        || (file_name.starts_with("requirements") && file_name.ends_with(".txt"))
        || file_name.ends_with(".requirements.txt")
}

pub fn parse_manifest(text: &str, kind: ManifestKind) -> Vec<Dependency> {
    match kind {
        ManifestKind::Cargo => parse_cargo_manifest(text),
        ManifestKind::PackageJson => parse_package_json(text),
        ManifestKind::Pyproject => parse_pyproject_manifest(text),
        ManifestKind::Requirements => parse_requirements_manifest(text),
    }
}

pub fn parse_cargo_manifest(text: &str) -> Vec<Dependency> {
    let _ = text.parse::<toml_edit::DocumentMut>();

    let mut dependencies = Vec::new();
    let mut active_section: Option<String> = None;
    let mut line_start = 0;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        let trimmed = line_without_newline.trim();

        if trimmed.starts_with('[') {
            active_section = parse_cargo_section(trimmed);
        } else if let Some(section) = active_section.as_deref() {
            if CARGO_SECTIONS.contains(&section) {
                if let Some(dep) =
                    parse_cargo_dependency_line(line_without_newline, line_start, section)
                {
                    dependencies.push(dep);
                }
            }
        }

        line_start += line.len();
    }

    dependencies
}

fn parse_cargo_section(trimmed: &str) -> Option<String> {
    if trimmed.starts_with("[[") {
        return None;
    }
    let name = trimmed.strip_prefix('[')?.split(']').next()?.trim();
    CARGO_SECTIONS.contains(&name).then(|| name.to_string())
}

fn parse_cargo_dependency_line(line: &str, line_start: usize, section: &str) -> Option<Dependency> {
    let line = strip_unquoted_comment(line);
    let equals = find_unquoted_char(line, '=')?;
    let raw_name = line[..equals].trim();
    let value = line[equals + 1..].trim_start();
    let value_start = equals + 1 + line[equals + 1..].len() - value.len();

    // Handle dotted key syntax: serde.workspace = true
    if let Some(base_name) = raw_name.strip_suffix(".workspace") {
        let base_name = base_name.trim();
        if value.trim() == "true" && !base_name.is_empty() {
            let name = normalize_toml_key(base_name)?;
            let val_trimmed = value.trim();
            let val_start = value_start + value.len() - value.trim_start().len();
            return Some(build_workspace_dependency(
                name,
                section,
                line_start + val_start,
                line_start + val_start + val_trimmed.len(),
            ));
        }
    }

    let name = normalize_toml_key(raw_name)?;

    if let Some((version, rel_span)) = parse_quoted_string(value) {
        return Some(build_dependency(
            name,
            section,
            Registry::Cargo,
            version,
            line_start + value_start + rel_span.start,
            line_start + value_start + rel_span.end,
        ));
    }

    if !value.starts_with('{') {
        return None;
    }

    // Handle workspace = true in inline table
    if let Some(ws) = find_key_in_inline_table(value, "workspace") {
        if ws.value == "true" {
            return Some(build_workspace_dependency(
                name,
                section,
                line_start + value_start + ws.span.start,
                line_start + value_start + ws.span.end,
            ));
        }
    }

    if cargo_inline_table_is_unsupported(value) {
        return None;
    }

    let version_key = find_key_in_inline_table(value, "version")?;
    Some(build_dependency(
        name,
        section,
        Registry::Cargo,
        version_key.value,
        line_start + value_start + version_key.span.start,
        line_start + value_start + version_key.span.end,
    ))
}

fn cargo_inline_table_is_unsupported(value: &str) -> bool {
    ["path", "git", "registry"]
        .iter()
        .any(|key| find_key_in_inline_table(value, key).is_some())
}

pub fn parse_package_json(text: &str) -> Vec<Dependency> {
    let _ = serde_json::from_str::<serde_json::Value>(text);

    let mut dependencies = Vec::new();
    for section in NPM_SECTIONS {
        if let Some(object) = find_top_level_json_object(text, section) {
            dependencies.extend(parse_json_dependency_object(
                text,
                object.start,
                object.end,
                section,
            ));
        }
    }
    dependencies
}

pub fn parse_pyproject_manifest(text: &str) -> Vec<Dependency> {
    let _ = text.parse::<toml_edit::DocumentMut>();

    let mut dependencies = Vec::new();
    for array in find_pyproject_dependency_arrays(text) {
        for token in toml_string_tokens(text, array.start, array.end) {
            if let Some(dep) =
                parse_python_requirement_dependency(&token.value, &array.section, token.inner_start)
            {
                dependencies.push(dep);
            }
        }
    }
    dependencies
}

pub fn parse_requirements_manifest(text: &str) -> Vec<Dependency> {
    let mut dependencies = Vec::new();
    let mut line_start = 0;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if let Some((requirement, requirement_start)) =
            parse_requirements_line(line_without_newline)
        {
            if let Some(dep) = parse_python_requirement_dependency(
                requirement,
                "requirements",
                line_start + requirement_start,
            ) {
                dependencies.push(dep);
            }
        }
        line_start += line.len();
    }

    dependencies
}

pub fn requirements_include_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let Some((directive, value)) = parse_requirements_directive(line) else {
            continue;
        };
        if matches!(directive, "-r" | "--requirement" | "-c" | "--constraint") && !value.is_empty()
        {
            paths.push(value.to_string());
        }
    }
    paths
}

struct TomlDependencyArray {
    section: String,
    start: usize,
    end: usize,
}

fn find_pyproject_dependency_arrays(text: &str) -> Vec<TomlDependencyArray> {
    let mut arrays = Vec::new();
    let mut active_section: Option<String> = None;
    let mut line_start = 0;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        let trimmed = line_without_newline.trim();

        if trimmed.starts_with('[') {
            active_section = parse_toml_section_name(trimmed);
        } else if let Some(section) = active_section.as_deref() {
            let line_without_comment = strip_unquoted_comment(line_without_newline);
            if let Some(equals) = find_unquoted_char(line_without_comment, '=') {
                let raw_key = line_without_comment[..equals].trim();
                if pyproject_dependency_key_is_supported(section, raw_key) {
                    let value_start = line_start + equals + 1;
                    if let Some(array_start) = skip_toml_ws(text, value_start) {
                        if text.as_bytes().get(array_start) == Some(&b'[') {
                            if let Some(array_end) = find_matching_toml_array(text, array_start) {
                                arrays.push(TomlDependencyArray {
                                    section: pyproject_section_label(section, raw_key),
                                    start: array_start,
                                    end: array_end,
                                });
                            }
                        }
                    }
                }
            }
        }

        line_start += line.len();
    }

    arrays
}

fn parse_toml_section_name(trimmed: &str) -> Option<String> {
    if trimmed.starts_with("[[") {
        return None;
    }
    trimmed
        .strip_prefix('[')?
        .split(']')
        .next()
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(str::to_string)
}

fn pyproject_dependency_key_is_supported(section: &str, key: &str) -> bool {
    matches!(
        (section, normalize_toml_key(key).as_deref()),
        ("project", Some("dependencies")) | ("build-system", Some("requires"))
    ) || (section == "project.optional-dependencies" && normalize_toml_key(key).is_some())
}

fn pyproject_section_label(section: &str, key: &str) -> String {
    match section {
        "project" => "project.dependencies".to_string(),
        "build-system" => "build-system.requires".to_string(),
        "project.optional-dependencies" => normalize_toml_key(key)
            .map(|extra| format!("project.optional-dependencies.{extra}"))
            .unwrap_or_else(|| "project.optional-dependencies".to_string()),
        _ => section.to_string(),
    }
}

fn skip_toml_ws(text: &str, from: usize) -> Option<usize> {
    (from..text.len()).find(|index| !text.as_bytes()[*index].is_ascii_whitespace())
}

fn find_matching_toml_array(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = start;
    while cursor < text.len() {
        match text.as_bytes()[cursor] {
            b'"' => cursor = next_json_string(text, cursor, text.len())?.outer_end,
            b'\'' => cursor = next_toml_literal_string(text, cursor, text.len())?.outer_end,
            b'[' => {
                depth += 1;
                cursor += 1;
            }
            b']' => {
                depth = depth.checked_sub(1)?;
                cursor += 1;
                if depth == 0 {
                    return Some(cursor - 1);
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn toml_string_tokens(text: &str, from: usize, to: usize) -> Vec<StringToken> {
    let mut tokens = Vec::new();
    let mut cursor = from;
    while cursor < to {
        let Some(index) = text[cursor..to]
            .find(|char| char == '"' || char == '\'')
            .map(|offset| cursor + offset)
        else {
            return tokens;
        };
        let token = if text.as_bytes()[index] == b'\'' {
            next_toml_literal_string(text, index, to)
        } else {
            next_json_string(text, index, to)
        };
        let Some(token) = token else { break };
        cursor = token.outer_end;
        tokens.push(token);
    }
    tokens
}

fn next_toml_literal_string(text: &str, from: usize, to: usize) -> Option<StringToken> {
    (text.as_bytes().get(from) == Some(&b'\'')).then_some(())?;
    let end = text[from + 1..to].find('\'')? + from + 1;
    Some(StringToken {
        value: text[from + 1..end].to_string(),
        inner_start: from + 1,
        inner_end: end,
        outer_end: end + 1,
    })
}

fn parse_requirements_line(line: &str) -> Option<(&str, usize)> {
    let line = strip_requirement_comment(line);
    let trimmed = line.trim_start();
    let start = line.len() - trimmed.len();
    let requirement = trimmed.trim_end();
    if requirement.is_empty()
        || requirement.starts_with('-')
        || requirement.starts_with("--")
        || looks_like_requirement_path(requirement)
    {
        return None;
    }
    Some((requirement, start))
}

fn parse_requirements_directive(line: &str) -> Option<(&str, &str)> {
    let line = strip_requirement_comment(line).trim();
    let split = line.find(char::is_whitespace);
    let (directive, rest) = split
        .map(|index| (&line[..index], &line[index..]))
        .unwrap_or((line, ""));
    Some((directive, rest.trim()))
}

fn strip_requirement_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    let mut previous_was_ws = true;
    for (index, char) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if char == '\\' {
            escaped = true;
        } else if char == '"' || char == '\'' {
            quoted = !quoted;
        } else if !quoted && char == '#' && previous_was_ws {
            return line[..index].trim_end();
        }
        previous_was_ws = char.is_whitespace();
    }
    line
}

fn looks_like_requirement_path(requirement: &str) -> bool {
    requirement.starts_with('.')
        || requirement.starts_with('/')
        || requirement.starts_with('\\')
        || requirement.contains("://")
        || requirement.starts_with("git+")
        || requirement.starts_with("file:")
}

fn parse_python_requirement_dependency(
    requirement: &str,
    section: &str,
    absolute_start: usize,
) -> Option<Dependency> {
    let parsed = Pep508Requirement::<VerbatimUrl>::from_str(requirement).ok()?;
    let VersionOrUrl::VersionSpecifier(specifiers) = parsed.version_or_url.as_ref()? else {
        return None;
    };
    if specifiers.is_empty() {
        return None;
    }

    let span = python_specifier_span(requirement)?;
    Some(Dependency {
        name: parsed.name.to_string(),
        section: section.to_string(),
        registry: Registry::Pypi,
        current: requirement[span.start..span.end].trim().to_string(),
        value_span: Span {
            start: absolute_start + span.start,
            end: absolute_start + span.end,
        },
        can_edit: true,
        prefix: "==".to_string(),
        current_version: None,
        python_specifiers: Some(specifiers.clone()),
        is_workspace: false,
    })
}

fn python_specifier_span(requirement: &str) -> Option<Span> {
    let mut bracket_depth = 0usize;
    let mut start = None;
    let mut cursor = 0usize;
    while cursor < requirement.len() {
        let rest = &requirement[cursor..];
        let byte = requirement.as_bytes()[cursor];
        match byte {
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b';' if bracket_depth == 0 => break,
            _ if bracket_depth == 0 => {
                if ["===", "==", "~=", ">=", "<=", "!=", ">", "<"]
                    .iter()
                    .any(|operator| rest.starts_with(*operator))
                {
                    start = Some(cursor);
                    break;
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }

    let start = start?;
    let marker_start = requirement[start..]
        .find(';')
        .map(|offset| start + offset)
        .unwrap_or(requirement.len());
    let end = requirement[..marker_start].trim_end().len();
    (start < end).then_some(Span { start, end })
}

fn parse_json_dependency_object(
    text: &str,
    object_start: usize,
    object_end: usize,
    section: &str,
) -> Vec<Dependency> {
    let mut dependencies = Vec::new();
    let mut cursor = object_start + 1;

    while cursor < object_end {
        let Some(name_token) = next_json_string(text, cursor, object_end) else {
            break;
        };
        cursor = name_token.outer_end;
        let Some(colon) = find_json_colon(text, cursor, object_end) else {
            break;
        };
        let Some(value_token) = next_json_string(text, colon + 1, object_end) else {
            cursor = colon + 1;
            continue;
        };
        cursor = value_token.outer_end;

        if npm_spec_is_unsupported(&value_token.value) {
            continue;
        }

        dependencies.push(build_dependency(
            name_token.value,
            section,
            Registry::Npm,
            value_token.value,
            value_token.inner_start,
            value_token.inner_end,
        ));
    }

    dependencies
}

fn npm_spec_is_unsupported(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    lower.starts_with("file:")
        || lower.starts_with("link:")
        || lower.starts_with("workspace:")
        || lower.starts_with("git:")
        || lower.starts_with("git+")
        || lower.starts_with("github:")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
        || lower.starts_with("npm:")
        || (spec.contains('/') && !spec.starts_with('@'))
}

fn build_dependency(
    name: String,
    section: &str,
    registry: Registry,
    current: String,
    span_start: usize,
    span_end: usize,
) -> Dependency {
    let (prefix, version_text, can_edit) = editable_version_parts(&current);
    let current_version = parse_lenient_version(version_text).map(DependencyVersion::Semver);
    Dependency {
        name,
        section: section.to_string(),
        registry,
        current,
        value_span: Span {
            start: span_start,
            end: span_end,
        },
        can_edit,
        prefix,
        current_version,
        python_specifiers: None,
        is_workspace: false,
    }
}

fn build_workspace_dependency(
    name: String,
    section: &str,
    span_start: usize,
    span_end: usize,
) -> Dependency {
    Dependency {
        name,
        section: section.to_string(),
        registry: Registry::Cargo,
        current: "workspace = true".to_string(),
        value_span: Span {
            start: span_start,
            end: span_end,
        },
        can_edit: false,
        prefix: String::new(),
        current_version: None,
        python_specifiers: None,
        is_workspace: true,
    }
}

fn editable_version_parts(value: &str) -> (String, &str, bool) {
    let (prefix, version) = if let Some(rest) = value.strip_prefix('^') {
        ("^".to_string(), rest)
    } else if let Some(rest) = value.strip_prefix('~') {
        ("~".to_string(), rest)
    } else {
        (String::new(), value)
    };
    let can_edit = is_simple_version(version);
    (prefix, version, can_edit)
}

pub fn strip_semver_metadata(value: &str) -> &str {
    match value.find('+') {
        Some(index) => &value[..index],
        None => value,
    }
}

pub fn parse_lenient_version(value: &str) -> Option<Version> {
    let clean = value.trim_start_matches('=').trim();
    let clean = strip_semver_metadata(clean);
    if !is_simple_version(clean) {
        return None;
    }
    let mut parts = clean.split('.');
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    let patch = parts.next().unwrap_or("0");
    Version::parse(&format!("{major}.{minor}.{patch}")).ok()
}

fn is_simple_version(value: &str) -> bool {
    if value.is_empty() || value.contains('-') || value.contains('+') {
        return false;
    }
    let mut count = 0;
    for part in value.split('.') {
        count += 1;
        if count > 3 || part.is_empty() || !part.chars().all(|char| char.is_ascii_digit()) {
            return false;
        }
    }
    true
}

struct InlineValue {
    value: String,
    span: Span,
}

fn find_key_in_inline_table(value: &str, key: &str) -> Option<InlineValue> {
    let mut cursor = 0;
    while cursor < value.len() {
        let token = next_toml_key_token(value, cursor)?;
        cursor = token.outer_end;
        let after_key = value[cursor..].trim_start();
        if !after_key.starts_with('=') {
            continue;
        }
        let value_start = cursor + value[cursor..].len() - after_key.len() + 1;
        let after_equals = value[value_start..].trim_start();
        let rel_value_start = value_start + value[value_start..].len() - after_equals.len();
        if token.value == key {
            let (string_value, string_span) = parse_quoted_string(after_equals)
                .or_else(|| parse_bare_inline_value(after_equals))?;
            return Some(InlineValue {
                value: string_value,
                span: Span {
                    start: rel_value_start + string_span.start,
                    end: rel_value_start + string_span.end,
                },
            });
        }
    }
    None
}

fn parse_bare_inline_value(text: &str) -> Option<(String, Span)> {
    let trimmed = text.trim_start();
    let start = text.len() - trimmed.len();
    let end = trimmed
        .find(|char: char| char == ',' || char == '}')
        .map(|offset| start + offset)
        .unwrap_or(text.len());
    let value = text[start..end].trim_end();
    (!value.is_empty()).then(|| {
        (
            value.to_string(),
            Span {
                start,
                end: start + value.len(),
            },
        )
    })
}

#[derive(Clone)]
struct StringToken {
    value: String,
    inner_start: usize,
    inner_end: usize,
    outer_end: usize,
}

fn next_toml_key_token(text: &str, from: usize) -> Option<StringToken> {
    let index = text[from..].find(|char: char| {
        char == '"' || char.is_ascii_alphanumeric() || char == '_' || char == '-'
    })? + from;
    if text[index..].starts_with('"') {
        next_json_string(text, index, text.len())
    } else {
        let end = text[index..]
            .find(|char: char| !(char.is_ascii_alphanumeric() || char == '_' || char == '-'))
            .map(|offset| index + offset)
            .unwrap_or(text.len());
        Some(StringToken {
            value: text[index..end].to_string(),
            inner_start: index,
            inner_end: end,
            outer_end: end,
        })
    }
}

fn normalize_toml_key(key: &str) -> Option<String> {
    if key.starts_with('"') {
        parse_quoted_string(key).map(|(value, _)| value)
    } else if !key.is_empty() {
        Some(key.to_string())
    } else {
        None
    }
}

fn parse_quoted_string(text: &str) -> Option<(String, Span)> {
    let token = next_json_string(text, 0, text.len())?;
    (token.inner_start == 1).then_some((
        token.value,
        Span {
            start: token.inner_start,
            end: token.inner_end,
        },
    ))
}

struct JsonObjectSpan {
    start: usize,
    end: usize,
}

fn find_top_level_json_object(text: &str, section: &str) -> Option<JsonObjectSpan> {
    let mut depth = 0usize;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let byte = text.as_bytes()[cursor];
        match byte {
            b'"' => {
                let token = next_json_string(text, cursor, text.len())?;
                cursor = token.outer_end;
                if depth == 1 && token.value == section {
                    let colon = find_json_colon(text, cursor, text.len())?;
                    let object_start = skip_json_ws(text, colon + 1, text.len())?;
                    if text.as_bytes().get(object_start) != Some(&b'{') {
                        continue;
                    }
                    let object_end = find_matching_json_brace(text, object_start)?;
                    return Some(JsonObjectSpan {
                        start: object_start,
                        end: object_end,
                    });
                }
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn next_json_string(text: &str, from: usize, to: usize) -> Option<StringToken> {
    let quote = text[from..to].find('"')? + from;
    let mut cursor = quote + 1;
    let mut escaped = false;
    while cursor < to {
        let byte = text.as_bytes()[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            let raw = &text[quote..=cursor];
            let value = serde_json::from_str::<String>(raw).ok()?;
            return Some(StringToken {
                value,
                inner_start: quote + 1,
                inner_end: cursor,
                outer_end: cursor + 1,
            });
        }
        cursor += 1;
    }
    None
}

fn find_json_colon(text: &str, from: usize, to: usize) -> Option<usize> {
    let cursor = skip_json_ws(text, from, to)?;
    (text.as_bytes().get(cursor) == Some(&b':')).then_some(cursor)
}

fn skip_json_ws(text: &str, from: usize, to: usize) -> Option<usize> {
    (from..to).find(|index| !text.as_bytes()[*index].is_ascii_whitespace())
}

fn find_matching_json_brace(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = start;
    while cursor < text.len() {
        match text.as_bytes()[cursor] {
            b'"' => cursor = next_json_string(text, cursor, text.len())?.outer_end,
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                cursor += 1;
                if depth == 0 {
                    return Some(cursor - 1);
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn strip_unquoted_comment(line: &str) -> &str {
    find_unquoted_char(line, '#')
        .map(|index| &line[..index])
        .unwrap_or(line)
}

fn find_unquoted_char(text: &str, target: char) -> Option<usize> {
    let mut escaped = false;
    let mut quoted = false;
    for (index, char) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if char == '\\' {
            escaped = true;
        } else if char == '"' {
            quoted = !quoted;
        } else if !quoted && char == target {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_string_and_inline_versions() {
        let text = r#"
[dependencies]
serde = "1.0"
tokio = { version = "^1.35", features = ["rt"] }
local = { path = "../local" }
gitdep = { git = "https://example.com/repo", version = "1.0" }

[dev-dependencies]
pretty_assertions = "~1"
"#;

        let deps = parse_cargo_manifest(text);
        assert_eq!(
            deps.iter().map(|dep| dep.name.as_str()).collect::<Vec<_>>(),
            vec!["serde", "tokio", "pretty_assertions"]
        );
        assert_eq!(deps[1].prefix, "^");
        assert_eq!(
            &text[deps[0].value_span.start..deps[0].value_span.end],
            "1.0"
        );
    }

    #[test]
    fn parses_package_json_dependency_sections() {
        let text = r#"{
  "dependencies": {
    "react": "^18.2.0",
    "@types/node": "~20.0.0",
    "local": "file:../local"
  },
  "optionalDependencies": {
    "left-pad": "1"
  }
}"#;

        let deps = parse_package_json(text);
        assert_eq!(
            deps.iter().map(|dep| dep.name.as_str()).collect::<Vec<_>>(),
            vec!["react", "@types/node", "left-pad"]
        );
        assert_eq!(
            &text[deps[0].value_span.start..deps[0].value_span.end],
            "^18.2.0"
        );
        assert_eq!(deps[0].prefix, "^");
    }

    #[test]
    fn parses_pyproject_dependency_sections() {
        let text = r#"
[build-system]
requires = ["hatchling>=1.24"]

[project]
dependencies = [
  "requests>=2,<3; python_version >= '3.10'",
  "httpx[http2]==0.27.0",
  "unversioned",
  "direct @ https://example.com/direct.whl",
]

[project.optional-dependencies]
dev = ["pytest~=8.0"]
"#;

        let deps = parse_pyproject_manifest(text);

        assert_eq!(
            deps.iter().map(|dep| dep.name.as_str()).collect::<Vec<_>>(),
            vec!["hatchling", "requests", "httpx", "pytest"]
        );
        assert_eq!(deps[1].section, "project.dependencies");
        assert_eq!(deps[3].section, "project.optional-dependencies.dev");
        assert_eq!(
            &text[deps[1].value_span.start..deps[1].value_span.end],
            ">=2,<3"
        );
        assert_eq!(
            &text[deps[2].value_span.start..deps[2].value_span.end],
            "==0.27.0"
        );
        assert!(deps.iter().all(|dep| dep.registry == Registry::Pypi));
    }

    #[test]
    fn parses_requirements_safe_subset_and_ignores_pip_options() {
        let text = r#"
--index-url https://example.com/simple
-r dev.txt
-c constraints.txt
requests>=2,<3 # comment
httpx[http2]==0.27.0; python_version >= "3.10"
-e ../local
git+https://example.com/pkg.git
unversioned
"#;

        let deps = parse_requirements_manifest(text);

        assert_eq!(
            deps.iter().map(|dep| dep.name.as_str()).collect::<Vec<_>>(),
            vec!["requests", "httpx"]
        );
        assert_eq!(
            &text[deps[0].value_span.start..deps[0].value_span.end],
            ">=2,<3"
        );
        assert_eq!(
            &text[deps[1].value_span.start..deps[1].value_span.end],
            "==0.27.0"
        );
        assert_eq!(
            requirements_include_paths(text),
            vec!["dev.txt".to_string(), "constraints.txt".to_string()]
        );
    }

    #[test]
    fn marks_complex_ranges_as_non_editable() {
        let dep = build_dependency(
            "example".to_string(),
            "dependencies",
            Registry::Npm,
            ">=1 <2".to_string(),
            0,
            6,
        );
        assert!(!dep.can_edit);
        assert_eq!(dep.current_version, None);
    }

    #[test]
    fn parses_workspace_dependencies_section() {
        let text = r#"
[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.35", features = ["rt"] }

[dependencies]
local = { workspace = true }
"#;
        let deps = parse_cargo_manifest(text);
        assert_eq!(
            deps.iter().map(|dep| dep.name.as_str()).collect::<Vec<_>>(),
            vec!["serde", "tokio", "local"]
        );
        assert!(deps[2].is_workspace);
        assert_eq!(deps[2].current, "workspace = true");
        assert!(!deps[2].can_edit);
    }

    #[test]
    fn parses_dotted_workspace_key() {
        let text = r#"
[dependencies]
serde.workspace = true
"#;
        let deps = parse_cargo_manifest(text);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "serde");
        assert!(deps[0].is_workspace);
        assert_eq!(deps[0].current, "workspace = true");
    }
}
