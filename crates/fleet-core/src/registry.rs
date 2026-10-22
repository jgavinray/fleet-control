//! Registry loading: `[engines.*]` and `[hosts.*]` out of `fleet.toml`, plus
//! the untracked local LAN override (`$FLEET_LOCAL` or
//! `~/.config/fleet-control/local.toml`).
//!
//! The semantics are `check/fleet_check.py`'s: only the keys the checks use are
//! kept (`url`, `model`, `kind`, `host`, `health_path`, `exporter_url`,
//! `shared`, `expected_features`); everything else in the file is ignored. The
//! URL in the report is the registry string after the local rewrite, exactly as
//! Python reports it; `base_url()` strips the trailing slash the way Python's
//! `Client` does.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Default health path for engines that do not name one. Python defaults
/// lfm2d engines to "/v1/models" instead; `EngineConfig::health_path` knows
/// the difference.
pub const DEFAULT_HEALTH_PATH: &str = "/health";

/// The registry file name at the repo root.
pub const REGISTRY_FILE: &str = "fleet.toml";

/// One `[engines.*]` entry, limited to the keys the checks read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineConfig {
    /// The engine key (`[engines.deepseek]` -> "deepseek").
    pub name: String,
    /// Base URL as written in the registry (after the local rewrite).
    pub url: String,
    /// The model id the engine must report.
    pub model: String,
    /// tensorfold | lfm2d | laya | jev-glm.
    pub kind: String,
    /// The `[hosts.*]` key this engine runs on, as reported in `host`.
    pub host: String,
    /// Health path override; None means the kind's default.
    pub health_path: Option<String>,
    /// Prometheus bridge URL, when the engine has one.
    pub exporter_url: Option<String>,
    /// True when other people use the engine, so sanity is skipped while busy.
    pub shared: bool,
    /// What we believe the engine supports: feature -> "yes" | "no".
    pub expected_features: BTreeMap<String, String>,
}

impl EngineConfig {
    /// The health path to use. Python defaults tensorfold/laya to "/health" and
    /// lfm2d to "/v1/models"; jev-glm always uses "/health".
    pub fn health_path(&self) -> &str {
        self.health_path
            .as_deref()
            .unwrap_or(if self.kind == "lfm2d" {
                "/v1/models"
            } else {
                DEFAULT_HEALTH_PATH
            })
    }

    /// The URL to hand `Client::new`: Python strips trailing slashes there.
    pub fn base_url(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    /// The expected result for a feature, if the registry judges it.
    pub fn expected(&self, feature: &str) -> Option<&str> {
        self.expected_features.get(feature).map(String::as_str)
    }
}

/// The parsed registry: engines by name (sorted, like Python's `sorted`), and
/// the host names it knows.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub engines: BTreeMap<String, EngineConfig>,
    pub hosts: BTreeSet<String>,
}

impl Registry {
    pub fn engine(&self, name: &str) -> Option<&EngineConfig> {
        self.engines.get(name)
    }

    /// Engine names in sorted order.
    pub fn names(&self) -> Vec<&str> {
        self.engines.keys().map(String::as_str).collect()
    }

    /// Rewrite every engine's `url` and `exporter_url` with the local override.
    pub fn apply_local(&mut self, addresses: &BTreeMap<String, String>) {
        for cfg in self.engines.values_mut() {
            cfg.url = apply_local_address(&cfg.url, addresses);
            if let Some(exporter) = cfg.exporter_url.as_deref() {
                cfg.exporter_url = Some(apply_local_address(exporter, addresses));
            }
        }
    }
}

/// Why a registry could not be loaded. The display text matches Python's
/// `cannot read registry {path}: {error}`; the caller prefixes "fleet-check: ".
#[derive(Debug)]
pub enum RegistryError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Read { path, source } => {
                write!(f, "cannot read registry {}: {source}", path.display())
            }
            RegistryError::Parse { path, source } => {
                write!(f, "cannot read registry {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Read { source, .. } => Some(source),
            RegistryError::Parse { source, .. } => Some(source),
        }
    }
}

/// Parse registry text. `[engines.*]` entries become `EngineConfig`s; keys the
/// checks do not read are ignored. A missing `[engines]` table is not an error
/// (Python treats it as empty).
pub fn parse_registry(text: &str) -> Result<Registry, toml::de::Error> {
    let doc: toml::Value = toml::from_str(text)?;

    let mut engines = BTreeMap::new();
    if let Some(table) = doc.get("engines").and_then(toml::Value::as_table) {
        for (name, entry) in table {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            let string = |key: &str| {
                entry
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let non_empty = |key: &str| {
                entry
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };

            let mut expected_features = BTreeMap::new();
            if let Some(features) = entry
                .get("expected_features")
                .and_then(toml::Value::as_table)
            {
                for (feature, value) in features {
                    if let Some(value) = value.as_str() {
                        expected_features.insert(feature.clone(), value.to_string());
                    }
                }
            }

            engines.insert(
                name.clone(),
                EngineConfig {
                    name: name.clone(),
                    url: string("url"),
                    model: string("model"),
                    kind: string("kind"),
                    host: string("host"),
                    health_path: non_empty("health_path"),
                    exporter_url: non_empty("exporter_url"),
                    shared: entry
                        .get("shared")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                    expected_features,
                },
            );
        }
    }

    let mut hosts = BTreeSet::new();
    if let Some(table) = doc.get("hosts").and_then(toml::Value::as_table) {
        hosts.extend(table.keys().cloned());
    }

    Ok(Registry { engines, hosts })
}

/// Load and parse a registry file, applying the local LAN override.
pub fn load_registry(path: &Path) -> Result<Registry, RegistryError> {
    load_registry_with(path, None)
}

/// Load a registry with an explicit local override path (None = `$FLEET_LOCAL`
/// or the default). Tests use this to avoid touching the owner's home.
pub fn load_registry_with(path: &Path, local: Option<&Path>) -> Result<Registry, RegistryError> {
    let text = std::fs::read_to_string(path).map_err(|source| RegistryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut registry = parse_registry(&text).map_err(|source| RegistryError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let addresses = load_local_addresses(local);
    registry.apply_local(&addresses);
    Ok(registry)
}

/// The local override file: `$FLEET_LOCAL` when set and non-empty, else
/// `~/.config/fleet-control/local.toml`.
pub fn local_path() -> PathBuf {
    match std::env::var_os("FLEET_LOCAL") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => home_dir()
            .join(".config")
            .join("fleet-control")
            .join("local.toml"),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `{host name: address}` from the local override file; empty when there is
/// none. A missing or unreadable file is ignored silently; invalid TOML prints
/// a warning to stderr and is ignored, exactly like Python's
/// `load_local_addresses`.
pub fn load_local_addresses(path: Option<&Path>) -> BTreeMap<String, String> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => local_path(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return BTreeMap::new(),
    };
    let doc: toml::Value = match std::str::from_utf8(&bytes)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str::<toml::Value>(text).map_err(|e| e.to_string()))
    {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!(
                "fleet-check: ignoring unreadable local override {}: {e}",
                path.display()
            );
            return BTreeMap::new();
        }
    };

    let mut addresses = BTreeMap::new();
    if let Some(hosts) = doc.get("hosts").and_then(toml::Value::as_table) {
        for (name, host) in hosts {
            if let Some(address) = host
                .get("address")
                .and_then(toml::Value::as_str)
                .filter(|address| !address.is_empty())
            {
                addresses.insert(name.clone(), address.to_string());
            }
        }
    }
    addresses
}

/// Swap the host part of `url` for its local address when the host is that
/// host's name or a `*.ts.net` name. The port is kept, IPv6 addresses are
/// bracketed, and everything else in the URL is untouched.
pub fn apply_local_address(url: &str, addresses: &BTreeMap<String, String>) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    // Python replaces the whole netloc, so userinfo is dropped with it.
    let host_port = match authority.rsplit_once('@') {
        Some((_, host_port)) => host_port,
        None => authority,
    };
    let (host, port) = split_host_port(host_port);
    let host = host.to_ascii_lowercase();
    let name = host.split('.').next().unwrap_or("");

    let Some(address) = addresses.get(name) else {
        return url.to_string();
    };
    if !(host == name || host.ends_with(".ts.net")) {
        return url.to_string();
    }

    let address = if address.contains(':') {
        format!("[{address}]")
    } else {
        address.clone()
    };
    let netloc = match port {
        Some(port) => format!("{address}:{port}"),
        None => address,
    };
    format!("{scheme}://{netloc}{tail}")
}

/// Split an authority's host and optional port, handling bracketed IPv6.
fn split_host_port(host_port: &str) -> (&str, Option<&str>) {
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let port = rest[end + 1..].strip_prefix(':');
            return (&rest[..end], port);
        }
        return (host_port, None);
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (host_port, None),
    }
}

/// Resolve the registry path: explicit `--registry`, else `$FLEET_REGISTRY`,
/// else the default search.
pub fn registry_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("FLEET_REGISTRY").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    default_registry_path()
}

/// Python's `DEFAULT_REGISTRY` is `<repo root>/fleet.toml`, found relative to
/// the script. The Rust binary lives under `target/`, so walk up from the
/// executable (then from the working directory) to the first `fleet.toml`.
pub fn default_registry_path() -> PathBuf {
    let starts = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .into_iter()
        .chain(std::env::current_dir().ok());
    for start in starts {
        for dir in start.ancestors() {
            let candidate = dir.join(REGISTRY_FILE);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(REGISTRY_FILE)
}
