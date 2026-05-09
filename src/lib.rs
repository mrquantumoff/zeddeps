use zed_extension_api::{self as zed, Result};

const LANGUAGE_SERVER_ID: &str = "zalezhnosti-lsp";
const RELEASE_REPOSITORY: &str = "mrquantumoff/zalezhnosti";
const EXTENSION_VERSION: &str = env!("CARGO_PKG_VERSION");

struct ZalezhnostiExtension;

impl zed::Extension for ZalezhnostiExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let (os, arch) = zed::current_platform();
        let server_binary = server_binary_for_os(os);
        let command = std::env::var("ZALEZHNOSTI_LSP_PATH")
            .ok()
            .or_else(|| worktree.which(server_binary))
            .or_else(|| worktree.which("zalezhnosti-lsp"))
            .or_else(|| self.local_dev_server_path(worktree, server_binary))
            .map(Ok)
            .unwrap_or_else(|| self.download_language_server(os, arch))?;

        Ok(zed::Command {
            command,
            args: Vec::new(),
            env: worktree.shell_env(),
        })
    }
}

impl ZalezhnostiExtension {
    fn local_dev_server_path(
        &self,
        worktree: &zed::Worktree,
        server_binary: &'static str,
    ) -> Option<String> {
        let manifest = worktree.read_text_file("Cargo.toml").ok()?;
        if !(manifest.contains("name = \"zalezhnosti\"")
            && manifest.contains("members = [\"crates/zalezhnosti-lsp\"]"))
        {
            return None;
        }

        let root = worktree.root_path().replace('\\', "/");
        Some(format!("{root}/target/debug/{server_binary}"))
    }

    fn download_language_server(&self, os: zed::Os, arch: zed::Architecture) -> Result<String> {
        let asset_name = platform_asset_name(os, arch)?;
        let download_url = release_asset_url(&asset_name);

        let destination = server_binary_for_os(os).to_string();
        zed::download_file(
            &download_url,
            &destination,
            zed::DownloadedFileType::Uncompressed,
        )
        .map_err(|error| {
            format!(
                "Could not download {LANGUAGE_SERVER_ID} from {download_url}. Set ZALEZHNOSTI_LSP_PATH, put zalezhnosti-lsp on PATH, run `cargo build -p zalezhnosti-lsp` from this repo for local dev, or publish the {asset_name} asset for release v{EXTENSION_VERSION} at {RELEASE_REPOSITORY}. Download error: {error}"
            )
        })?;

        if !matches!(os, zed::Os::Windows) {
            zed::make_file_executable(&destination)?;
        }

        Ok(destination)
    }
}

fn release_asset_url(asset_name: &str) -> String {
    format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/v{EXTENSION_VERSION}/{asset_name}"
    )
}

fn server_binary_for_os(os: zed::Os) -> &'static str {
    match os {
        zed::Os::Windows => "zalezhnosti-lsp.exe",
        _ => "zalezhnosti-lsp",
    }
}

fn platform_asset_name(os: zed::Os, arch: zed::Architecture) -> Result<String> {
    let os = match os {
        zed::Os::Mac => "apple-darwin",
        zed::Os::Linux => "unknown-linux-gnu",
        zed::Os::Windows => "pc-windows-msvc",
    };
    let arch = match arch {
        zed::Architecture::Aarch64 => "aarch64",
        zed::Architecture::X8664 => "x86_64",
        _ => return Err("Unsupported platform architecture for zalezhnosti-lsp".to_string()),
    };
    let suffix = if matches!(os, "pc-windows-msvc") {
        ".exe"
    } else {
        ""
    };
    Ok(format!("zalezhnosti-lsp-{arch}-{os}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_versioned_release_asset_url() {
        assert_eq!(
            release_asset_url("zalezhnosti-lsp-x86_64-pc-windows-msvc.exe"),
            format!(
                "https://github.com/mrquantumoff/zalezhnosti/releases/download/v{EXTENSION_VERSION}/zalezhnosti-lsp-x86_64-pc-windows-msvc.exe"
            )
        );
    }
}

zed::register_extension!(ZalezhnostiExtension);
