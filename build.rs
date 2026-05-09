use std::fs;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let path = "extension.toml";
    let content = fs::read_to_string(path).expect("failed to read extension.toml");

    let new_content = content
        .lines()
        .map(|line| {
            if line.starts_with("version = ") {
                format!(r#"version = "{}""#, version)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Preserve trailing newline if present
    let new_content = if content.ends_with('\n') {
        new_content + "\n"
    } else {
        new_content
    };

    if new_content != content {
        fs::write(path, new_content).expect("failed to write extension.toml");
    }
}
