use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{
    env,
    fmt::{self, Display},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

const REMOVED_CURSOR_PROVIDER: &str = "cursor";

/// Upstream credential provider used by stored credentials and runtime requests.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// `OpenAI` Codex OAuth backed by the `ChatGPT` Codex backend.
    #[default]
    Codex,
    /// xAI Grok OAuth backed by the xAI API.
    Grok,
    /// Kiro credentials imported from the official local IDE or CLI stores.
    Kiro,
    /// Vercel AI Gateway API key backed by the OpenAI-compatible gateway API.
    Vercel,
}

impl Provider {
    /// Returns the stable CLI/config identifier for this provider.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Kiro => "kiro",
            Self::Vercel => "vercel",
        }
    }

    /// Returns a human-readable provider label.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Kiro => "Kiro",
            Self::Vercel => "Vercel",
        }
    }

    /// Returns whether credentials for this provider are static bearer tokens.
    #[must_use]
    pub const fn uses_static_bearer_token(self) -> bool {
        matches!(self, Self::Vercel)
    }
}

impl Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "openai-codex" | "openai" => Ok(Self::Codex),
            "grok" | "xai" | "xai-oauth" | "grok-oauth" => Ok(Self::Grok),
            "kiro" | "kiro-cli" | "kiro-desktop" => Ok(Self::Kiro),
            "vercel" | "vercel-ai-gateway" | "ai-gateway" => Ok(Self::Vercel),
            other => Err(Error::config(format!("unknown provider: {other}"))),
        }
    }
}

/// Persisted provider credentials used to authenticate API requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    /// Provider that issued or accepts these credentials.
    #[serde(default)]
    pub provider: Provider,
    /// Bearer token used for authenticated API calls.
    pub access_token: String,
    /// Long-lived token used to mint a new access token, or empty for static API keys.
    pub refresh_token: String,
    /// Access-token expiration timestamp, expressed as Unix seconds.
    pub expires_at: i64,
    /// Upstream account identifier associated with the token pair.
    #[serde(default)]
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AuthFile {
    version: u8,
    #[serde(default)]
    active_provider: Provider,
    #[serde(default)]
    providers: BTreeMap<Provider, Credentials>,
}

impl AuthFile {
    fn empty() -> Self {
        Self {
            version: 2,
            active_provider: Provider::default(),
            providers: BTreeMap::new(),
        }
    }

    fn single(credentials: Credentials) -> Self {
        let provider = credentials.provider;
        let mut providers = BTreeMap::new();
        providers.insert(provider, credentials);
        Self {
            version: 2,
            active_provider: provider,
            providers,
        }
    }
}

impl Credentials {
    /// Returns whether the credentials should be considered expired at `now_unix`.
    #[must_use]
    pub const fn is_expired_at(&self, now_unix: i64, skew_secs: i64) -> bool {
        self.expires_at.saturating_sub(skew_secs) <= now_unix
    }

    /// Returns whether the credentials are expired relative to the current system time.
    #[must_use]
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        self.is_expired_at(now_unix(), skew_secs)
    }
}

/// Persisted runtime defaults used by `serve` and `daemon install`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppConfig {
    /// Hostname, IP address, comma-separated hosts, or CIDR selector to bind to.
    #[serde(default)]
    pub bind_host: Option<String>,
    /// TCP port the service should bind to.
    #[serde(default)]
    pub bind_port: Option<u16>,
    /// Override path for the persisted authentication file.
    #[serde(default)]
    pub auth_file: Option<PathBuf>,
    /// Static API key to expose from the local service, when configured.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional fallback model used for known unsupported Anthropic model ids.
    #[serde(default)]
    pub model_fallback: Option<String>,
    /// Default upstream credential provider used by `serve` and `daemon install`.
    #[serde(default)]
    pub provider: Option<Provider>,
}

/// Loads and saves persisted OAuth credentials from a single file.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    /// Creates a store for credentials at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default credential file path derived from environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `ROTOM_AUTH_FILE` nor a usable home
    /// directory environment variable is available.
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(path) = env::var("ROTOM_AUTH_FILE") {
            return Ok(PathBuf::from(path));
        }

        let home = env::var("ROTOM_HOME")
            .or_else(|_| env::var("HOME"))
            .map_err(|_| Error::config("HOME is not set; pass --auth-file explicitly"))?;

        Ok(PathBuf::from(home).join(".rotom").join("auth.json"))
    }

    /// Creates a credential store that uses the default path resolution rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the default credential path cannot be resolved.
    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    /// Returns the on-disk path used by this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads credentials from disk, or `None` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load(&self) -> Result<Option<Credentials>> {
        Ok(self
            .load_file()?
            .and_then(|file| file.providers.get(&file.active_provider).cloned()))
    }

    /// Loads credentials for a specific provider from disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load_provider(&self, provider: Provider) -> Result<Option<Credentials>> {
        Ok(self
            .load_file()?
            .and_then(|file| file.providers.get(&provider).cloned()))
    }

    /// Returns all provider credentials currently stored on disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load_all(&self) -> Result<Vec<Credentials>> {
        Ok(self
            .load_file()?
            .map(|file| file.providers.into_values().collect())
            .unwrap_or_default())
    }

    fn load_file(&self) -> Result<Option<AuthFile>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => parse_auth_file(&raw).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Persists credentials to disk using a private temporary file and atomic rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the JSON
    /// cannot be serialized, or the file cannot be written atomically.
    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let mut file = self
            .load_file()?
            .unwrap_or_else(|| AuthFile::single(credentials.clone()));
        let mut credentials = credentials.clone();
        if credentials.account_id.is_empty() && credentials.provider == Provider::Codex {
            credentials.provider = Provider::Codex;
        }
        file.active_provider = credentials.provider;
        file.providers.insert(credentials.provider, credentials);
        self.save_file(&file)
    }

    fn save_file(&self, file: &AuthFile) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("auth file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(file)?;
        // Write to a sibling temp file first so a partial write never replaces the live secrets.
        write_secret_file(&tmp, &bytes)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

fn parse_auth_file(raw: &str) -> Result<AuthFile> {
    let mut value = serde_json::from_str::<serde_json::Value>(raw)?;
    if value.get("providers").is_some() {
        ignore_removed_provider(&mut value, REMOVED_CURSOR_PROVIDER);
        return serde_json::from_value(value).map_err(Into::into);
    }
    if value.get("provider").and_then(serde_json::Value::as_str) == Some(REMOVED_CURSOR_PROVIDER) {
        return Ok(AuthFile::empty());
    }
    let credentials = serde_json::from_value::<Credentials>(value)?;
    Ok(AuthFile::single(credentials))
}

fn ignore_removed_provider(value: &mut serde_json::Value, removed_provider: &str) {
    let Some(file) = value.as_object_mut() else {
        return;
    };
    let active_provider_was_removed = file
        .get("active_provider")
        .and_then(serde_json::Value::as_str)
        == Some(removed_provider);
    let fallback_provider = file
        .get_mut("providers")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|providers| {
            providers.retain(|provider, credentials| {
                provider != removed_provider
                    && credentials
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        != Some(removed_provider)
            });
            providers.keys().next().cloned()
        });
    if active_provider_was_removed {
        file.insert(
            "active_provider".to_owned(),
            serde_json::Value::String(
                fallback_provider.unwrap_or_else(|| Provider::default().as_str().to_owned()),
            ),
        );
    }
}

/// Loads and saves the persisted application configuration file.
#[derive(Debug, Clone)]
pub struct AppConfigStore {
    path: PathBuf,
}

impl AppConfigStore {
    /// Creates a store for application configuration at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default application configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the `rotom` home directory cannot be resolved.
    pub fn default_path() -> Result<PathBuf> {
        Ok(rotom_home()?.join("config.json"))
    }

    /// Creates a configuration store that uses the default path resolution rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the default configuration path cannot be resolved.
    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    /// Returns the on-disk path used by this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads configuration from disk, or `None` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load(&self) -> Result<Option<AppConfig>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(parse_app_config(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Persists configuration to disk using a private temporary file and atomic rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the JSON
    /// cannot be serialized, or the file cannot be written atomically.
    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("config file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(config)?;
        // Use the same temp-file pattern as auth storage so readers never observe truncated JSON.
        write_secret_file(&tmp, &bytes)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    /// Removes the persisted configuration file when it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when removing an existing file fails for reasons other
    /// than it not being present.
    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn parse_app_config(raw: &str) -> Result<AppConfig> {
    let mut value = serde_json::from_str::<serde_json::Value>(raw)?;
    if value.get("provider").and_then(serde_json::Value::as_str) == Some(REMOVED_CURSOR_PROVIDER) {
        if let Some(config) = value.as_object_mut() {
            config.remove("provider");
        }
    }
    serde_json::from_value(value).map_err(Into::into)
}

/// Returns the current Unix timestamp in seconds.
#[must_use]
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

fn rotom_home() -> Result<PathBuf> {
    if let Ok(path) = env::var("ROTOM_HOME") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var("HOME")
        .map_err(|_| Error::config("HOME is not set; pass --auth-file explicitly"))?;
    Ok(PathBuf::from(home).join(".rotom"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    fn sample_credentials() -> Credentials {
        Credentials {
            provider: crate::config::Provider::Codex,
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 123,
            account_id: "acc_1".into(),
        }
    }

    fn sample_app_config() -> AppConfig {
        AppConfig {
            bind_host: Some("127.0.0.1".into()),
            bind_port: Some(14550),
            auth_file: Some(PathBuf::from("/tmp/auth.json")),
            api_key: Some("secret".into()),
            model_fallback: Some("gpt-5.5".into()),
            provider: Some(Provider::Codex),
        }
    }

    #[test]
    fn detects_expiry_with_skew() {
        let credentials = Credentials {
            provider: crate::config::Provider::Codex,
            expires_at: 100,
            ..sample_credentials()
        };

        assert!(credentials.is_expired_at(95, 10));
        assert!(!credentials.is_expired_at(80, 10));
    }

    #[test]
    fn removed_cursor_provider_is_rejected() {
        assert!(Provider::from_str("cursor").is_err());
    }

    #[test]
    fn vercel_provider_aliases_parse() {
        assert_eq!(Provider::from_str("vercel").unwrap(), Provider::Vercel);
        assert_eq!(
            Provider::from_str("vercel-ai-gateway").unwrap(),
            Provider::Vercel
        );
        assert_eq!(Provider::Vercel.as_str(), "vercel");
        assert!(Provider::Vercel.uses_static_bearer_token());
    }

    #[test]
    fn removed_cursor_credentials_are_ignored_without_hiding_supported_providers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{
                "version": 2,
                "active_provider": "cursor",
                "providers": {
                    "codex": {
                        "provider": "codex",
                        "access_token": "codex-access",
                        "refresh_token": "codex-refresh",
                        "expires_at": 123,
                        "account_id": "codex-account"
                    },
                    "cursor": {
                        "provider": "cursor",
                        "access_token": "cursor-access",
                        "refresh_token": "cursor-refresh",
                        "expires_at": 456,
                        "account_id": "cursor-account"
                    }
                }
            }"#,
        )
        .unwrap();
        let store = AuthStore::new(path);
        let credentials = store.load_all().unwrap();

        assert_eq!(store.load().unwrap().unwrap().provider, Provider::Codex);
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].provider, Provider::Codex);
    }

    #[test]
    fn removed_cursor_only_credentials_behave_as_logged_out() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{
                "version": 2,
                "active_provider": "cursor",
                "providers": {
                    "cursor": {
                        "provider": "cursor",
                        "access_token": "cursor-access",
                        "refresh_token": "cursor-refresh",
                        "expires_at": 456,
                        "account_id": "cursor-account"
                    }
                }
            }"#,
        )
        .unwrap();
        let store = AuthStore::new(path);

        assert!(store.load_all().unwrap().is_empty());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn removed_legacy_cursor_credentials_behave_as_logged_out() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            r#"{
                "provider": "cursor",
                "access_token": "cursor-access",
                "refresh_token": "cursor-refresh",
                "expires_at": 456,
                "account_id": "cursor-account"
            }"#,
        )
        .unwrap();
        let store = AuthStore::new(path);

        assert!(store.load_all().unwrap().is_empty());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn removed_cursor_runtime_default_becomes_unselected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{
                "bind_host": "127.0.0.1",
                "bind_port": 14550,
                "provider": "cursor"
            }"#,
        )
        .unwrap();
        let config = AppConfigStore::new(path).load().unwrap().unwrap();

        assert_eq!(config.bind_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(config.bind_port, Some(14550));
        assert_eq!(config.provider, None);
    }

    #[test]
    fn missing_auth_file_loads_as_none() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_credentials() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let credentials = sample_credentials();

        store.save(&credentials).unwrap();

        assert_eq!(store.load().unwrap(), Some(credentials));
    }

    #[test]
    fn missing_app_config_loads_as_none() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_app_config() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();

        assert_eq!(store.load().unwrap(), Some(config));
    }

    #[test]
    fn deletes_app_config() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();
        store.delete().unwrap();

        assert_eq!(store.load().unwrap(), None);
    }
}
