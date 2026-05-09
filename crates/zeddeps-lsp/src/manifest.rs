use semver::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Registry {
    Cargo,
    Npm,
}

impl Registry {
    pub fn package_url(&self, name: &str) -> String {
        match self {
            Registry::Cargo => format!("https://crates.io/crates/{name}"),
            Registry::Npm => format!("https://www.npmjs.com/package/{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
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
    pub current_version: Option<Version>,
}

impl Dependency {
    pub fn replacement_for(&self, latest: &Version) -> Option<String> {
        self.can_edit.then(|| format!("{}{}", self.prefix, latest))
    }
}

const CARGO_SECTIONS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];
const NPM_SECTIONS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

pub fn detect_manifest_kind(uri_path: &str) -> Option<Registry> {
    if uri_path.ends_with("Cargo.toml") {
        Some(Registry::Cargo)
    } else if uri_path.ends_with("package.json") {
        Some(Registry::Npm)
    } else {
        None
    }
}

pub fn parse_manifest(text: &str, kind: Registry) -> Vec<Dependency> {
    match kind {
        Registry::Cargo => parse_cargo_manifest(text),
        Registry::Npm => parse_package_json(text),
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
    let name = normalize_toml_key(line[..equals].trim())?;
    let value = line[equals + 1..].trim_start();
    let value_start = equals + 1 + line[equals + 1..].len() - value.len();

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

    if !value.starts_with('{') || cargo_inline_table_is_unsupported(value) {
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
        || value.contains("workspace = true")
        || value.contains("workspace=true")
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
    let current_version = parse_lenient_version(version_text);
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
            let (string_value, string_span) = parse_quoted_string(after_equals)?;
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
}
