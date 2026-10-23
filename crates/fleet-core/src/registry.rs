//! Registry loading: `[engines.*]` and `[hosts.*]` out of `fleet.toml`, plus
//! the untracked local LAN override (`$FLEET_LOCAL` or
//! `~/.config/fleet-control/local.toml`).
//!
//! The semantics are `the original Python fleet-check`'s: only the keys the checks use are
//! kept (`url`, `model`, `kind`, `host`, `health_path`, `exporter_url`,
//! `shared`, `expected_features`); everything else in the file is ignored. The
//! URL in the report is the registry string after the local rewrite, exactly as
//! Python reports it; `base_url()` strips the trailing slash the way Python's
//! `Client` does.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::jsonpy;

/// Default health path for engines that do not name one. Python defaults
/// lfm2d engines to "/v1/models" instead; `EngineConfig::health_path` knows
/// the difference.
pub const DEFAULT_HEALTH_PATH: &str = "/health";

/// The registry file name at the repo root.
pub const REGISTRY_FILE: &str = "fleet.toml";

/// One `[engines.*]` entry, limited to the keys the checks read.
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Health path override; None means the kind's default, while `Some("")`
    /// is Python's present-but-empty `health_path` (a request to the base URL).
    pub health_path: Option<String>,
    /// Prometheus bridge URL, when the engine has one.
    pub exporter_url: Option<String>,
    /// lfm2d read mode as written in the registry. None means absent (Python's
    /// "specs" default); `Some("")` is present-but-empty and takes the span path.
    pub read: Option<String>,
    /// True when other people use the engine, so sanity is skipped while busy.
    /// Python uses truthiness, so `shared = "yes"` is true and `shared = 0` is
    /// false.
    pub shared: bool,
    /// What we believe the engine supports: feature -> the raw registry value.
    pub expected_features: BTreeMap<String, Value>,
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

    /// The lfm2d read mode: Python's `cfg.get("read", "specs")`. Anything
    /// other than "specs" takes the span-scan path, including the empty string.
    pub fn read(&self) -> &str {
        self.read.as_deref().unwrap_or("specs")
    }

    /// The expected result for a feature, if the registry judges it. The value
    /// is raw: Python compares the measured result string against it directly.
    pub fn expected(&self, feature: &str) -> Option<&Value> {
        self.expected_features.get(feature)
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
    /// An invalid port is Python's uncaught ValueError; it comes back as the
    /// error string.
    pub fn apply_local(&mut self, addresses: &BTreeMap<String, String>) -> Result<(), String> {
        for cfg in self.engines.values_mut() {
            cfg.url = apply_local_address(&cfg.url, addresses)?;
            if let Some(exporter) = cfg.exporter_url.as_deref() {
                cfg.exporter_url = Some(apply_local_address(exporter, addresses)?);
            }
        }
        Ok(())
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
    /// A shape Python dies on (an `engines` value that is not a table, an
    /// engine entry that is not a table, or an engine without `url`/`model`).
    Invalid {
        path: PathBuf,
        message: String,
    },
    /// A local override that Python dies on (invalid UTF-8; invalid TOML only
    /// warns).
    LocalOverride {
        path: PathBuf,
        message: String,
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
            RegistryError::Invalid { path, message } => {
                write!(f, "cannot read registry {}: {message}", path.display())
            }
            RegistryError::LocalOverride { path, message } => {
                write!(
                    f,
                    "cannot read local override {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Read { source, .. } => Some(source),
            RegistryError::Parse { source, .. } => Some(source),
            RegistryError::Invalid { .. } | RegistryError::LocalOverride { .. } => None,
        }
    }
}

/// Why registry text could not be parsed. The TOML case carries the decoder
/// error; the invalid case carries the shape Python would die on.
#[derive(Debug)]
pub enum RegistryParseError {
    Toml(toml::de::Error),
    Invalid(String),
}

impl fmt::Display for RegistryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryParseError::Toml(source) => write!(f, "{source}"),
            RegistryParseError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RegistryParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryParseError::Toml(source) => Some(source),
            RegistryParseError::Invalid(_) => None,
        }
    }
}

/// Parse registry text. `[engines.*]` entries become `EngineConfig`s; keys the
/// checks do not read are ignored. A missing `[engines]` table is not an error
/// (Python treats it as empty), but a present `engines` value that is not a
/// table, an engine entry that is not a table, or an engine missing `url` or
/// `model` is: Python raises on each at load or first use.
pub fn parse_registry(text: &str) -> Result<Registry, RegistryParseError> {
    let doc: toml::Value = toml::from_str(text).map_err(RegistryParseError::Toml)?;

    let mut engines = BTreeMap::new();
    if let Some(engines_value) = doc.get("engines") {
        let Some(table) = engines_value.as_table() else {
            return Err(RegistryParseError::Invalid(
                "engines is not a table".to_string(),
            ));
        };
        for (name, entry) in table {
            let Some(entry) = entry.as_table() else {
                return Err(RegistryParseError::Invalid(format!(
                    "engine '{name}' is not a table"
                )));
            };
            for key in ["url", "model"] {
                if !entry.contains_key(key) {
                    return Err(RegistryParseError::Invalid(format!(
                        "engine '{name}' has no {key}"
                    )));
                }
            }
            let string = |key: &str| {
                entry
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };

            let mut expected_features = BTreeMap::new();
            if let Some(features) = entry
                .get("expected_features")
                .and_then(toml::Value::as_table)
            {
                for (feature, value) in features {
                    expected_features.insert(feature.clone(), to_json_value(value));
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
                    health_path: entry
                        .get("health_path")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string),
                    exporter_url: entry
                        .get("exporter_url")
                        .and_then(toml::Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    read: entry
                        .get("read")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string),
                    shared: entry.get("shared").is_some_and(toml_truthy),
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

/// Python truthiness for a TOML value (`shared`): the empty string, zero,
/// false, and empty containers are falsy; non-empty strings are truthy.
fn toml_truthy(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(s) => !s.is_empty(),
        toml::Value::Integer(i) => *i != 0,
        toml::Value::Float(f) => *f != 0.0,
        toml::Value::Boolean(b) => *b,
        toml::Value::Datetime(_) => true,
        toml::Value::Array(a) => !a.is_empty(),
        toml::Value::Table(t) => !t.is_empty(),
    }
}

/// Convert a TOML value to the JSON value Python's `tomllib` would hand the
/// checks, so `expected_features` keeps raw numbers, booleans and lists.
fn to_json_value(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(number) => Value::Number(number),
            None if f.is_nan() => Value::String(jsonpy::NAN_TOK.to_string()),
            None if *f == f64::INFINITY => Value::String(jsonpy::INF_TOK.to_string()),
            None => Value::String(jsonpy::NEGINF_TOK.to_string()),
        },
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(to_json_value).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(key, value)| (key.clone(), to_json_value(value)))
                .collect::<Map<String, Value>>(),
        ),
    }
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
    let mut registry = parse_registry(&text).map_err(|error| match error {
        RegistryParseError::Toml(source) => RegistryError::Parse {
            path: path.to_path_buf(),
            source,
        },
        RegistryParseError::Invalid(message) => RegistryError::Invalid {
            path: path.to_path_buf(),
            message,
        },
    })?;
    let addresses = load_local_addresses(local)?;
    // Python only rewrites when the override file yielded addresses.
    if !addresses.is_empty() {
        registry
            .apply_local(&addresses)
            .map_err(|message| RegistryError::Invalid {
                path: path.to_path_buf(),
                message,
            })?;
    }
    Ok(registry)
}

/// The local override file: `$FLEET_LOCAL` when it is set (even to an empty
/// string, which Python turns into a failed `open("")` and no override), else
/// `~/.config/fleet-control/local.toml`.
pub fn local_path() -> PathBuf {
    match std::env::var_os("FLEET_LOCAL") {
        Some(path) => PathBuf::from(path),
        None => home_dir()
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
/// none. A missing or unreadable file is ignored silently, and invalid TOML
/// prints a warning to stderr and is ignored, exactly like Python's
/// `load_local_addresses`. Invalid UTF-8 is Python's uncaught
/// `UnicodeDecodeError` and comes back as an error.
pub fn load_local_addresses(
    path: Option<&Path>,
) -> Result<BTreeMap<String, String>, RegistryError> {
    let path = match path {
        Some(path) if !path.as_os_str().is_empty() => path.to_path_buf(),
        _ => local_path(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(BTreeMap::new()),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            return Err(RegistryError::LocalOverride {
                path,
                message: error.to_string(),
            })
        }
    };
    let doc: toml::Value = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!(
                "fleet-check: ignoring unreadable local override {}: {e}",
                path.display()
            );
            return Ok(BTreeMap::new());
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
    Ok(addresses)
}

/// Swap the host part of `url` for its local address when the host is that
/// host's name or a `*.ts.net` name. The port is kept (port 0 and an empty
/// port are dropped, like Python's falsy `parts.port`), IPv6 addresses are
/// bracketed, and everything else in the URL is untouched. An invalid port is
/// Python's uncaught ValueError and comes back as the error string.
pub fn apply_local_address(
    url: &str,
    addresses: &BTreeMap<String, String>,
) -> Result<String, String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Ok(url.to_string());
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    // Python replaces the whole netloc, so userinfo is dropped with it.
    let host_port = match authority.rsplit_once('@') {
        Some((_, host_port)) => host_port,
        None => authority,
    };
    let (host, raw_port) = split_host_port(host_port);
    let host = host.to_ascii_lowercase();
    let name = host.split('.').next().unwrap_or("");

    let Some(address) = addresses.get(name) else {
        return Ok(url.to_string());
    };
    if !(host == name || host.ends_with(".ts.net")) {
        return Ok(url.to_string());
    }

    // Python reads `parts.port` only after the host matches.
    let port = python_port(raw_port)?;
    let address = if address.contains(':') {
        format!("[{address}]")
    } else {
        address.clone()
    };
    let netloc = match port {
        Some(port) => format!("{address}:{port}"),
        None => address,
    };
    Ok(format!("{scheme}://{netloc}{tail}"))
}

/// Split an authority's host and raw port, handling bracketed IPv6. The port
/// is returned as written, because Python validates it lazily (`parts.port`).
fn split_host_port(host_port: &str) -> (&str, Option<&str>) {
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let port = rest[end + 1..].strip_prefix(':');
            return (&rest[..end], port);
        }
        return (host_port, None);
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (host_port, None),
    }
}

/// Python's `urlsplit(...).port`: `None` for a missing or empty port, `0`
/// dropped by the caller's truthiness, and Python's ValueError text for a
/// non-integer or out-of-range port.
fn python_port(raw: Option<&str>) -> Result<Option<u32>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let value: i64 = raw
        .parse()
        .map_err(|_| format!("Port could not be cast to integer value as '{raw}'"))?;
    if !(0..=65535).contains(&value) {
        return Err("Port out of range 0-65535".to_string());
    }
    Ok(if value == 0 { None } else { Some(value as u32) })
}

/// Resolve the registry path: explicit `--registry`, else `$FLEET_REGISTRY`
/// (even when it is set to an empty string, which Python turns into a failed
/// `open("")`), else the default search.
pub fn registry_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("FLEET_REGISTRY") {
        return PathBuf::from(path);
    }
    default_registry_path()
}

/// Python's `DEFAULT_REGISTRY` is `<repo root>/fleet.toml`, found relative to
/// the script's `os.path.realpath`, so invoking `~/.local/bin/fleet-check` (a
/// symlink into the checkout) still finds the repo. Mirror that: resolve the
/// executable's symlink chain before walking up, then fall back to the
/// industry-standard config home (`~/.config/fleet-control/fleet.toml`, the
/// same directory as the local LAN override), then to the working directory.
/// The config home is a Rust-only step; Python always searches from its
/// script, so a packaged binary there would need `$FLEET_REGISTRY`.
pub fn default_registry_path() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|exe| {
        let resolved = exe.canonicalize().unwrap_or(exe);
        resolved.parent().map(Path::to_path_buf)
    });
    resolve_registry_path(exe_dir, &home_dir(), std::env::current_dir().ok())
}

/// The registry's config home: `~/.config/fleet-control/fleet.toml`. Plain
/// `~/.config` (no `$XDG_CONFIG_HOME` indirection), matching `local_path()`.
pub fn config_registry_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("fleet-control")
        .join(REGISTRY_FILE)
}

fn resolve_registry_path(exe_dir: Option<PathBuf>, home: &Path, cwd: Option<PathBuf>) -> PathBuf {
    if let Some(found) = search_registry(exe_dir.into_iter()) {
        return found;
    }
    let config = config_registry_path(home);
    if config.is_file() {
        return config;
    }
    if let Some(found) = search_registry(cwd.into_iter()) {
        return found;
    }
    PathBuf::from(REGISTRY_FILE)
}

fn search_registry(starts: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    for start in starts {
        for dir in start.ancestors() {
            let candidate = dir.join(REGISTRY_FILE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use serde_json::json;

    /// Serializes the tests that mutate process-wide environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("fleet-core-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    #[test]
    fn registry_search_finds_the_repo_through_a_symlinked_launcher() {
        let tmp = std::env::temp_dir().join(format!("fleet-core-symlink-{}", std::process::id()));
        let real = tmp.join("checkout/bin");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(tmp.join("checkout/fleet.toml"), "[hosts]\n").unwrap();
        let link = tmp.join("launcher");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Without canonicalizing (the old behavior) no fleet.toml sits above
        // the link, so the search finds nothing at all.
        assert_eq!(search_registry([link.clone()].into_iter()), None);
        // Canonicalized first — what default_registry_path does — it is found.
        let found = search_registry([link.canonicalize().unwrap()].into_iter());
        assert_eq!(
            found,
            Some(tmp.canonicalize().unwrap().join("checkout/fleet.toml"))
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn config_home_is_the_default_when_nothing_sits_above_the_binary() {
        let tmp = std::env::temp_dir().join(format!("fleet-core-config-{}", std::process::id()));
        let bin = tmp.join("opt/tools/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let config = tmp.join("home/.config/fleet-control");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("fleet.toml"), "[hosts]\n").unwrap();
        let home = tmp.join("home");
        // No registry above the binary dir and none in the cwd: the config
        // home answers.
        let cwd = tmp.join("somewhere-else");
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(
            resolve_registry_path(Some(bin.clone()), &home, Some(cwd)),
            config.join("fleet.toml")
        );
        // A checkout above the binary still wins over the config home.
        std::fs::write(tmp.join("opt/fleet.toml"), "[hosts]\n").unwrap();
        assert_eq!(
            resolve_registry_path(Some(bin), &home, None),
            tmp.join("opt/fleet.toml")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn without_a_config_home_the_cwd_walk_and_bare_fallback_remain() {
        let tmp = std::env::temp_dir().join(format!("fleet-core-cwd-{}", std::process::id()));
        let cwd = tmp.join("work/sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(tmp.join("work/fleet.toml"), "[hosts]\n").unwrap();
        let empty_home = tmp.join("home");
        std::fs::create_dir_all(&empty_home).unwrap();
        assert_eq!(
            resolve_registry_path(None, &empty_home, Some(cwd)),
            tmp.join("work/fleet.toml")
        );
        assert_eq!(
            resolve_registry_path(None, &empty_home, None),
            PathBuf::from(REGISTRY_FILE)
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn read_mode_defaults_and_parses() {
        let default = EngineConfig::default();
        assert_eq!(default.read(), "specs", "no read key means specs");

        let registry = parse_registry(
            "[engines.specs]\nkind = \"lfm2d\"\nmodel = \"m\"\nurl = \"http://x\"\n\n\
             [engines.spans]\nkind = \"lfm2d\"\nmodel = \"m\"\nurl = \"http://x\"\nread = \"spans\"\n",
        )
        .expect("parse registry");
        assert_eq!(registry.engine("specs").expect("specs").read(), "specs");
        assert_eq!(registry.engine("spans").expect("spans").read(), "spans");
    }

    #[test]
    fn shared_uses_python_truthiness() {
        let registry = parse_registry(
            "[engines.yes]\nurl = \"u\"\nmodel = \"m\"\nshared = \"yes\"\n\n\
             [engines.no]\nurl = \"u\"\nmodel = \"m\"\nshared = \"no\"\n\n\
             [engines.zero]\nurl = \"u\"\nmodel = \"m\"\nshared = 0\n\n\
             [engines.empty]\nurl = \"u\"\nmodel = \"m\"\nshared = \"\"\n\n\
             [engines.off]\nurl = \"u\"\nmodel = \"m\"\nshared = false\n\n\
             [engines.on]\nurl = \"u\"\nmodel = \"m\"\nshared = true\n",
        )
        .expect("parse registry");
        assert!(registry.engine("yes").expect("yes").shared);
        assert!(
            registry.engine("no").expect("no").shared,
            "\"no\" is truthy"
        );
        assert!(!registry.engine("zero").expect("zero").shared);
        assert!(!registry.engine("empty").expect("empty").shared);
        assert!(!registry.engine("off").expect("off").shared);
        assert!(registry.engine("on").expect("on").shared);
        assert!(!EngineConfig::default().shared);
    }

    #[test]
    fn health_path_and_read_honor_present_empty_values() {
        let registry = parse_registry(
            "[engines.empty]\nkind = \"lfm2d\"\nurl = \"u\"\nmodel = \"m\"\n\
             health_path = \"\"\nread = \"\"\n\n\
             [engines.absent]\nkind = \"lfm2d\"\nurl = \"u\"\nmodel = \"m\"\n",
        )
        .expect("parse registry");

        let empty = registry.engine("empty").expect("empty");
        assert_eq!(empty.health_path(), "", "present empty health path is used");
        assert_eq!(empty.read(), "", "present empty read takes the span path");

        let absent = registry.engine("absent").expect("absent");
        assert_eq!(absent.health_path(), "/v1/models");
        assert_eq!(absent.read(), "specs");
    }

    #[test]
    fn expected_features_keep_raw_values() {
        let registry = parse_registry(
            "[engines.e]\nurl = \"u\"\nmodel = \"m\"\n\
             [engines.e.expected_features]\nlogprobs = 1\nguided_choice = \"no\"\n",
        )
        .expect("parse registry");
        let engine = registry.engine("e").expect("engine");
        assert_eq!(engine.expected("logprobs"), Some(&json!(1)));
        assert_eq!(engine.expected("guided_choice"), Some(&json!("no")));
        assert_eq!(engine.expected("absent"), None);
    }

    #[test]
    fn nonfinite_expected_features_are_raw_and_drift() {
        use crate::result::Feature;

        let registry = parse_registry(
            "[engines.e]\nurl = \"u\"\nmodel = \"m\"\n\
             [engines.e.expected_features]\nlogprobs = nan\nprompt_logprobs = inf\n",
        )
        .expect("tomllib accepts nan/inf floats");
        let engine = registry.engine("e").expect("engine");
        let nan = engine.expected("logprobs").expect("logprobs expectation");
        let inf = engine
            .expected("prompt_logprobs")
            .expect("prompt_logprobs expectation");
        assert_eq!(nan, &json!(jsonpy::NAN_TOK));
        assert_eq!(inf, &json!(jsonpy::INF_TOK));
        assert!(jsonpy::is_nonfinite(nan));

        let feature = Feature::record("yes", "returned", Some(nan));
        assert!(feature.drift, "a float nan never equals the result string");
        // Python's json.dumps would emit a bare NaN here (invalid JSON), which
        // the parity harness could not load. The Rust report uses the token as
        // a string, so this fixture is deliberately not byte-comparable.
        assert_eq!(feature.to_json()["expected"], json!("NaN"));

        let inf_feature = Feature::record("yes", "returned", Some(inf));
        assert!(inf_feature.drift);
        assert_eq!(inf_feature.to_json()["expected"], json!("Infinity"));
    }

    #[test]
    fn broken_registry_shapes_are_errors() {
        for (text, message) in [
            ("engines = 5\n", "engines is not a table"),
            ("[engines]\nfoo = \"x\"\n", "engine 'foo' is not a table"),
            ("[engines.foo]\nmodel = \"m\"\n", "engine 'foo' has no url"),
            (
                "[engines.foo]\nurl = \"http://x\"\n",
                "engine 'foo' has no model",
            ),
        ] {
            match parse_registry(text) {
                Err(RegistryParseError::Invalid(got)) => assert_eq!(got, message, "{text:?}"),
                other => panic!("{text:?} should be invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn apply_local_address_port_edges() {
        let addrs = BTreeMap::from([("spark".to_string(), "192.0.2.10".to_string())]);
        assert_eq!(
            apply_local_address("http://spark:0/health", &addrs),
            Ok("http://192.0.2.10/health".to_string()),
            "port 0 is dropped"
        );
        assert_eq!(
            apply_local_address("http://spark:/health", &addrs),
            Ok("http://192.0.2.10/health".to_string()),
            "an empty port is dropped"
        );
        assert_eq!(
            apply_local_address("http://spark:8888/health", &addrs),
            Ok("http://192.0.2.10:8888/health".to_string())
        );
        assert_eq!(
            apply_local_address("http://spark:abc/health", &addrs),
            Err("Port could not be cast to integer value as 'abc'".to_string())
        );
        assert_eq!(
            apply_local_address("http://spark:70000/health", &addrs),
            Err("Port out of range 0-65535".to_string())
        );
        assert_eq!(
            apply_local_address("http://other:abc/health", &addrs),
            Ok("http://other:abc/health".to_string()),
            "an invalid port on a host we do not rewrite is never read"
        );
    }

    #[test]
    fn empty_fleet_local_means_no_override_and_no_warning() {
        let _guard = env_guard();
        std::env::set_var("FLEET_LOCAL", "");
        assert_eq!(local_path(), PathBuf::from(""));
        assert!(load_local_addresses(None)
            .expect("an empty path is an unreadable file, not a decode error")
            .is_empty());

        let path = temp_file(
            "empty-local.toml",
            b"[engines.e]\nurl = \"http://spark.puma-wyvern.ts.net:8888\"\nmodel = \"m\"\n",
        );
        let registry = load_registry(&path).expect("registry loads");
        assert_eq!(
            registry.engine("e").expect("engine").url,
            "http://spark.puma-wyvern.ts.net:8888",
            "a set-but-empty FLEET_LOCAL disables the override"
        );
        std::fs::remove_file(&path).ok();
        std::env::remove_var("FLEET_LOCAL");
    }

    #[test]
    fn dev_null_local_override_is_empty() {
        let _guard = env_guard();
        std::env::set_var("FLEET_LOCAL", "/dev/null");
        assert_eq!(local_path(), PathBuf::from("/dev/null"));
        assert!(load_local_addresses(None)
            .expect("/dev/null is valid empty TOML")
            .is_empty());
        std::env::remove_var("FLEET_LOCAL");
    }

    #[test]
    fn empty_fleet_registry_names_the_empty_path() {
        let _guard = env_guard();
        std::env::set_var("FLEET_LOCAL", "/dev/null");
        std::env::set_var("FLEET_REGISTRY", "");
        let path = registry_path(None);
        assert_eq!(path, PathBuf::from(""));
        let message = load_registry(&path).expect_err("the empty path cannot be read");
        assert!(
            message.to_string().starts_with("cannot read registry : "),
            "{message}"
        );
        std::env::remove_var("FLEET_REGISTRY");
        std::env::remove_var("FLEET_LOCAL");
    }

    #[test]
    fn invalid_utf8_local_override_is_an_error() {
        let path = temp_file("local-utf8.toml", b"[hosts.spark]\naddress = \"\xff\"\n");
        let error = load_local_addresses(Some(&path)).expect_err("invalid UTF-8 is fatal");
        assert!(
            error.to_string().starts_with("cannot read local override "),
            "{error}"
        );
        std::fs::remove_file(&path).ok();
    }
}
