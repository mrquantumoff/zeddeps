use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use pep440_rs::Version as Pep440Version;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use semver::Version;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::manifest::{Dependency, DependencyVersion, Registry, strip_semver_metadata};

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const USER_AGENT_VALUE: &str = concat!("zalezhnosti-lsp/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct LatestInfo {
    pub version: Option<DependencyVersion>,
    pub repository_url: Option<String>,
}

pub type LatestResult = Result<LatestInfo, String>;

#[derive(Clone)]
pub struct RegistryClient {
    http: reqwest::Client,
    cache: Arc<Mutex<HashMap<CacheKey, CacheEntry>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    registry: Registry,
    name: String,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    fetched_at: Instant,
    etag: Option<String>,
    result: LatestResult,
}

impl RegistryClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn latest_for(&self, dep: &Dependency) -> LatestResult {
        let key = CacheKey {
            registry: dep.registry,
            name: dep.name.clone(),
        };

        let cached = self.cache.lock().await.get(&key).cloned();
        if let Some(entry) = cached.as_ref()
            && entry.fetched_at.elapsed() < CACHE_TTL
        {
            return entry.result.clone();
        }

        let result = match dep.registry {
            Registry::Cargo => self.fetch_cargo_latest(&dep.name, cached.as_ref()).await,
            Registry::Npm => self.fetch_npm_latest(&dep.name, cached.as_ref()).await,
            Registry::Pypi => self.fetch_pypi_latest(&dep.name, cached.as_ref()).await,
        };

        let (latest, etag) = match result {
            FetchOutcome::Fresh { result, etag } => (result, etag),
            FetchOutcome::NotModified => {
                let mut cache = self.cache.lock().await;
                if let Some(entry) = cache.get_mut(&key) {
                    entry.fetched_at = Instant::now();
                    return entry.result.clone();
                }
                (
                    Err("Registry cache entry expired before refresh".to_string()),
                    None,
                )
            }
        };

        self.cache.lock().await.insert(
            key,
            CacheEntry {
                fetched_at: Instant::now(),
                etag,
                result: latest.clone(),
            },
        );

        latest
    }

    async fn fetch_cargo_latest(&self, name: &str, cached: Option<&CacheEntry>) -> FetchOutcome {
        let url = format!("https://crates.io/api/v1/crates/{name}");
        let mut request = self.http.get(url).header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(etag) = cached.and_then(|entry| entry.etag.as_deref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return FetchOutcome::Fresh {
                    result: Err(format!("crates.io request failed: {error}")),
                    etag: None,
                };
            }
        };

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return FetchOutcome::NotModified;
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if !response.status().is_success() {
            return FetchOutcome::Fresh {
                result: Err(format!("crates.io returned {}", response.status())),
                etag,
            };
        }

        let body = response.json::<CratesResponse>().await;
        FetchOutcome::Fresh {
            result: body
                .map_err(|error| format!("crates.io response parse failed: {error}"))
                .map(|body| LatestInfo {
                    version: newest_stable_crate_version(body.versions)
                        .map(DependencyVersion::Semver),
                    repository_url: body.crate_info.repository.filter(|s| !s.is_empty()),
                }),
            etag,
        }
    }

    async fn fetch_npm_latest(&self, name: &str, cached: Option<&CacheEntry>) -> FetchOutcome {
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = format!("https://registry.npmjs.org/{encoded}");
        let mut request = self.http.get(url).header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(etag) = cached.and_then(|entry| entry.etag.as_deref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return FetchOutcome::Fresh {
                    result: Err(format!("npm registry request failed: {error}")),
                    etag: None,
                };
            }
        };

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return FetchOutcome::NotModified;
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if !response.status().is_success() {
            return FetchOutcome::Fresh {
                result: Err(format!("npm registry returned {}", response.status())),
                etag,
            };
        }

        let body = response.json::<NpmResponse>().await;
        FetchOutcome::Fresh {
            result: body
                .map_err(|error| format!("npm response parse failed: {error}"))
                .map(|body| LatestInfo {
                    version: newest_stable_npm_version(&body).map(DependencyVersion::Semver),
                    repository_url: body
                        .repository
                        .and_then(|r| r.url)
                        .and_then(|u| clean_npm_repo_url(&u)),
                }),
            etag,
        }
    }

    async fn fetch_pypi_latest(&self, name: &str, cached: Option<&CacheEntry>) -> FetchOutcome {
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = format!("https://pypi.org/pypi/{encoded}/json");
        let mut request = self.http.get(url).header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(etag) = cached.and_then(|entry| entry.etag.as_deref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return FetchOutcome::Fresh {
                    result: Err(format!("PyPI request failed: {error}")),
                    etag: None,
                };
            }
        };

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return FetchOutcome::NotModified;
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if !response.status().is_success() {
            return FetchOutcome::Fresh {
                result: Err(format!("PyPI returned {}", response.status())),
                etag,
            };
        }

        let body = response.json::<PypiResponse>().await;
        FetchOutcome::Fresh {
            result: body
                .map_err(|error| format!("PyPI response parse failed: {error}"))
                .map(|body| LatestInfo {
                    version: newest_stable_pypi_version(&body).map(DependencyVersion::Pep440),
                    repository_url: pypi_repository_url(&body.info),
                }),
            etag,
        }
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

enum FetchOutcome {
    Fresh {
        result: LatestResult,
        etag: Option<String>,
    },
    NotModified,
}

#[derive(Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
    versions: Vec<CrateVersion>,
}

#[derive(Deserialize)]
struct CrateInfo {
    repository: Option<String>,
}

#[derive(Deserialize)]
struct CrateVersion {
    num: String,
    yanked: bool,
}

fn newest_stable_crate_version(versions: Vec<CrateVersion>) -> Option<Version> {
    versions
        .into_iter()
        .filter(|version| !version.yanked)
        .filter_map(|version| Version::parse(strip_semver_metadata(&version.num)).ok())
        .filter(is_stable)
        .max()
}

#[derive(Deserialize)]
struct NpmResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
    versions: HashMap<String, serde_json::Value>,
    repository: Option<NpmRepository>,
}

#[derive(Deserialize)]
struct NpmRepository {
    url: Option<String>,
}

#[derive(Deserialize)]
struct PypiResponse {
    info: PypiInfo,
    releases: HashMap<String, Vec<PypiReleaseFile>>,
}

#[derive(Deserialize)]
struct PypiInfo {
    home_page: Option<String>,
    project_urls: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct PypiReleaseFile {
    yanked: bool,
}

fn newest_stable_npm_version(body: &NpmResponse) -> Option<Version> {
    if let Some(latest) = body
        .dist_tags
        .get("latest")
        .and_then(|version| Version::parse(strip_semver_metadata(version)).ok())
        .filter(is_stable)
    {
        return Some(latest);
    }

    body.versions
        .keys()
        .filter_map(|version| Version::parse(strip_semver_metadata(version)).ok())
        .filter(is_stable)
        .max()
}

fn clean_npm_repo_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("git+") {
        Some(url.strip_prefix("git+")?.to_string())
    } else if url.starts_with("git://") {
        Some(url.replacen("git://", "https://", 1))
    } else if url.starts_with("github:") {
        Some(format!(
            "https://github.com/{}",
            url.strip_prefix("github:")?
        ))
    } else if url.starts_with("gitlab:") {
        Some(format!(
            "https://gitlab.com/{}",
            url.strip_prefix("gitlab:")?
        ))
    } else {
        Some(url.to_string())
    }
}

fn newest_stable_pypi_version(body: &PypiResponse) -> Option<Pep440Version> {
    body.releases
        .iter()
        .filter(|(_, files)| files.is_empty() || files.iter().any(|file| !file.yanked))
        .filter_map(|(version, _)| version.parse::<Pep440Version>().ok())
        .filter(Pep440Version::is_stable)
        .max()
}

fn pypi_repository_url(info: &PypiInfo) -> Option<String> {
    info.project_urls
        .as_ref()
        .and_then(|urls| {
            [
                "Source",
                "Repository",
                "Source Code",
                "Code",
                "Homepage",
                "Home",
            ]
            .iter()
            .find_map(|key| urls.get(*key).filter(|url| !url.is_empty()).cloned())
        })
        .or_else(|| {
            info.home_page
                .as_ref()
                .filter(|url| !url.is_empty())
                .cloned()
        })
}

fn is_stable(version: &Version) -> bool {
    version.pre.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_latest_ignores_yanked_and_prerelease() {
        let versions = vec![
            CrateVersion {
                num: "2.0.0-alpha.1".to_string(),
                yanked: false,
            },
            CrateVersion {
                num: "1.9.0".to_string(),
                yanked: true,
            },
            CrateVersion {
                num: "1.8.0".to_string(),
                yanked: false,
            },
        ];
        assert_eq!(
            newest_stable_crate_version(versions).map(DependencyVersion::Semver),
            Some(DependencyVersion::Semver(Version::parse("1.8.0").unwrap()))
        );
    }

    #[test]
    fn npm_latest_falls_back_when_latest_is_prerelease() {
        let body = NpmResponse {
            dist_tags: HashMap::from([("latest".to_string(), "2.0.0-beta.1".to_string())]),
            versions: HashMap::from([
                ("1.0.0".to_string(), serde_json::json!({})),
                ("1.2.0".to_string(), serde_json::json!({})),
                ("2.0.0-beta.1".to_string(), serde_json::json!({})),
            ]),
            repository: None,
        };
        assert_eq!(
            newest_stable_npm_version(&body).map(DependencyVersion::Semver),
            Some(DependencyVersion::Semver(Version::parse("1.2.0").unwrap()))
        );
    }

    #[test]
    fn cleans_npm_repo_urls() {
        assert_eq!(
            clean_npm_repo_url("git+https://github.com/foo/bar.git"),
            Some("https://github.com/foo/bar.git".to_string())
        );
        assert_eq!(
            clean_npm_repo_url("git://github.com/foo/bar"),
            Some("https://github.com/foo/bar".to_string())
        );
        assert_eq!(
            clean_npm_repo_url("github:foo/bar"),
            Some("https://github.com/foo/bar".to_string())
        );
        assert_eq!(
            clean_npm_repo_url("https://github.com/foo/bar"),
            Some("https://github.com/foo/bar".to_string())
        );
    }

    #[test]
    fn pypi_latest_ignores_yanked_and_prerelease() {
        let body = PypiResponse {
            info: PypiInfo {
                home_page: None,
                project_urls: None,
            },
            releases: HashMap::from([
                (
                    "2.0.0rc1".to_string(),
                    vec![PypiReleaseFile { yanked: false }],
                ),
                ("1.9.0".to_string(), vec![PypiReleaseFile { yanked: true }]),
                ("1.8.0".to_string(), vec![PypiReleaseFile { yanked: false }]),
            ]),
        };

        assert_eq!(
            newest_stable_pypi_version(&body).map(DependencyVersion::Pep440),
            Some(DependencyVersion::Pep440("1.8.0".parse().unwrap()))
        );
    }

    #[test]
    fn picks_pypi_repository_url() {
        let info = PypiInfo {
            home_page: Some("https://example.com".to_string()),
            project_urls: Some(HashMap::from([(
                "Source".to_string(),
                "https://github.com/psf/requests".to_string(),
            )])),
        };

        assert_eq!(
            pypi_repository_url(&info),
            Some("https://github.com/psf/requests".to_string())
        );
    }
}
