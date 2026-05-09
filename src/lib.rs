use zed_extension_api::{self as zed, Result};

const LANGUAGE_SERVER_ID: &str = "zeddeps-lsp";
const RELEASE_REPOSITORY: &str = "mrqua/zeddeps";

struct ZedDepsExtension;

impl zed::Extension for ZedDepsExtension {
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
        let command = std::env::var("ZEDDEPS_LSP_PATH")
            .ok()
            .or_else(|| worktree.which(server_binary))
            .or_else(|| worktree.which("zeddeps-lsp"))
            .map(Ok)
            .unwrap_or_else(|| self.download_language_server(os, arch))?;

        Ok(zed::Command {
            command,
            args: Vec::new(),
            env: worktree.shell_env(),
        })
    }
}

impl ZedDepsExtension {
    fn download_language_server(&self, os: zed::Os, arch: zed::Architecture) -> Result<String> {
        let asset_name = platform_asset_name(os, arch)?;
        let release = zed::latest_github_release(
            RELEASE_REPOSITORY,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "No {asset_name} asset found for {LANGUAGE_SERVER_ID} release {}",
                    release.version
                )
            })?;

        let destination = format!("{LANGUAGE_SERVER_ID}/{}", server_binary_for_os(os));
        zed::download_file(
            &asset.download_url,
            &destination,
            zed::DownloadedFileType::Uncompressed,
        )?;

        if !matches!(os, zed::Os::Windows) {
            zed::make_file_executable(&destination)?;
        }

        Ok(destination)
    }
}

fn server_binary_for_os(os: zed::Os) -> &'static str {
    match os {
        zed::Os::Windows => "zeddeps-lsp.exe",
        _ => "zeddeps-lsp",
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
        _ => return Err("Unsupported platform architecture for zeddeps-lsp".to_string()),
    };
    let suffix = if matches!(os, "pc-windows-msvc") {
        ".exe"
    } else {
        ""
    };
    Ok(format!("zeddeps-lsp-{arch}-{os}{suffix}"))
}

zed::register_extension!(ZedDepsExtension);
