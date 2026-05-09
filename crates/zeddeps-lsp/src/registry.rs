use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use semver::Version;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::manifest::{Dependency, Registry, strip_semver_metadata};

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const USER_AGENT_VALUE: &str = concat!("zeddeps-lsp/", env!("CARGO_PKG_VERSION"));

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

pub type LatestResult = Result<Option<Version>, String>;

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
        if let Some(entry) = cached.as_ref() {
            if entry.fetched_at.elapsed() < CACHE_TTL {
                return entry.result.clone();
            }
        }

        let result = match dep.registry {
            Registry::Cargo => self.fetch_cargo_latest(&dep.name, cached.as_ref()).await,
            Registry::Npm => self.fetch_npm_latest(&dep.name, cached.as_ref()).await,
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
                .map(|body| newest_stable_crate_version(body.versions)),
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
                .map(newest_stable_npm_version),
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
    versions: Vec<CrateVersion>,
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
}

fn newest_stable_npm_version(body: NpmResponse) -> Option<Version> {
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
            newest_stable_crate_version(versions),
            Some(Version::parse("1.8.0").unwrap())
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
        };
        assert_eq!(
            newest_stable_npm_version(body),
            Some(Version::parse("1.2.0").unwrap())
        );
    }
}
