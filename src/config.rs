//! Config precedence: env > `~/.config/lens/config.toml` > built-in defaults.
//! Missing API keys are not a load-time error — `doctor` reports them, and
//! `index` and `find` fail with an Auth error only when a key is actually needed.

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use serde::Deserialize;

use crate::error::LensError;

pub const DEFAULT_MODEL: &str = "gemma-4-31b";
pub const DEFAULT_API_BASE: &str = "https://api.cerebras.ai/v1";
pub const DEFAULT_MAX_CONCURRENCY: u32 = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub cerebras_api_key: Option<String>,
    pub model: String,
    pub api_base: String,
    pub max_concurrency: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cerebras_api_key: None,
            model: DEFAULT_MODEL.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    cerebras_api_key: Option<String>,
    model: Option<String>,
    api_base: Option<String>,
    max_concurrency: Option<u32>,
}

fn default_config_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/lens/config.toml"))
}

fn read_file_config(path: &Path) -> Result<FileConfig, LensError> {
    let mut file = open_config_file(path)?;
    validate_config_file(path, &file)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|e| {
        LensError::config(format!(
            "failed to read config file {}: {e}",
            path.display()
        ))
    })?;
    toml::from_str(&text).map_err(|e| {
        LensError::config(format!(
            "failed to parse config file {}: {e}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn open_config_file(path: &Path) -> Result<File, LensError> {
    use rustix::fs::{Mode, OFlags, open};
    open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|err| {
        LensError::config(format!(
            "failed to open config file {} without following symlinks: {err}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn open_config_file(path: &Path) -> Result<File, LensError> {
    File::open(path).map_err(|err| {
        LensError::config(format!(
            "failed to open config file {}: {err}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn validate_config_file(path: &Path, file: &File) -> Result<(), LensError> {
    let meta = file.metadata().map_err(|err| {
        LensError::config(format!(
            "failed to inspect config file {}: {err}",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(LensError::config(format!(
            "config path is not a regular file: {}",
            path.display()
        )));
    }
    if meta.uid() != rustix::process::getuid().as_raw() || meta.mode() & 0o077 != 0 {
        return Err(LensError::config(format!(
            "config file must be owned by the current user and mode 600: {}",
            path.display()
        ))
        .with_suggested_fix(format!("chmod 600 {}", path.display())));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_config_file(_path: &Path, _file: &File) -> Result<(), LensError> {
    Ok(())
}

fn validate_api_base(value: &str) -> Result<String, LensError> {
    let uri: ureq::http::Uri = value.parse().map_err(|err| {
        LensError::config(format!("invalid Cerebras API base URL {value:?}: {err}"))
    })?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| LensError::config("Cerebras API base URL must include an https scheme"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| LensError::config("Cerebras API base URL must include a host"))?;
    if authority.as_str().contains('@') {
        return Err(LensError::config(
            "Cerebras API base URL must not contain embedded credentials",
        ));
    }
    if uri
        .path_and_query()
        .and_then(|value| value.query())
        .is_some()
    {
        return Err(LensError::config(
            "Cerebras API base URL must not contain a query string",
        ));
    }
    let host = uri.host().expect("an authority always has a host");
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if scheme != "https" && !(scheme == "http" && loopback) {
        return Err(LensError::config(
            "Cerebras API base URL must use HTTPS; HTTP is allowed only for loopback development",
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

impl Config {
    /// Loads config from `~/.config/lens/config.toml` (if present) merged
    /// under environment variables, falling back to built-in defaults.
    pub fn load() -> Result<Self, LensError> {
        Self::load_from(default_config_path().as_deref())
    }

    /// Same as `load`, but with an explicit (possibly absent) config file
    /// path — the seam tests use to avoid touching the real home directory.
    fn load_from(path: Option<&Path>) -> Result<Self, LensError> {
        let file_cfg = match path {
            Some(p) if p.exists() => read_file_config(p)?,
            _ => FileConfig::default(),
        };

        let defaults = Config::default();

        let api_base = env::var("LENS_API_BASE")
            .ok()
            .or(file_cfg.api_base)
            .unwrap_or(defaults.api_base);

        Ok(Config {
            cerebras_api_key: env::var("CEREBRAS_API_KEY")
                .ok()
                .or(file_cfg.cerebras_api_key),
            model: env::var("LENS_MODEL")
                .ok()
                .or(file_cfg.model)
                .unwrap_or(defaults.model),
            api_base: validate_api_base(&api_base)?,
            max_concurrency: env::var("LENS_MAX_CONCURRENCY")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .or(file_cfg.max_concurrency)
                .unwrap_or(defaults.max_concurrency),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};

    /// `Config::load` reads process-wide env vars; serialize the tests that
    /// touch them so parallel `cargo test` runs don't race each other.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    const ENV_KEYS: &[&str] = &[
        "CEREBRAS_API_KEY",
        "LENS_MODEL",
        "LENS_API_BASE",
        "LENS_MAX_CONCURRENCY",
    ];

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("lens-test-{name}-{}-{nanos}", std::process::id()))
    }

    fn clear_env() {
        for key in ENV_KEYS {
            // SAFETY: serialized by `env_lock`; no other thread reads/writes
            // these process-wide env vars concurrently.
            unsafe { env::remove_var(key) };
        }
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: serialized by `env_lock`; no other thread reads/writes
        // these process-wide env vars concurrently.
        unsafe { env::set_var(key, value) };
    }

    fn write_config(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn defaults_used_when_nothing_set() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        let cfg = Config::load_from(None).unwrap();

        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.api_base, DEFAULT_API_BASE);
        assert_eq!(cfg.max_concurrency, DEFAULT_MAX_CONCURRENCY);
    }

    #[test]
    fn env_override_wins_over_defaults() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        set_env("CEREBRAS_API_KEY", "env-cerebras-key");
        set_env("LENS_MODEL", "some-other-model");
        set_env("LENS_API_BASE", "http://localhost:8000/v1");
        set_env("LENS_MAX_CONCURRENCY", "7");

        let cfg = Config::load_from(None).unwrap();

        assert_eq!(cfg.cerebras_api_key.as_deref(), Some("env-cerebras-key"));
        assert_eq!(cfg.model, "some-other-model");
        assert_eq!(cfg.api_base, "http://localhost:8000/v1");
        assert_eq!(cfg.max_concurrency, 7);

        clear_env();
    }

    #[test]
    fn file_config_used_when_no_env_set() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        let dir = temp_dir("file-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        write_config(
            &path,
            r#"
            cerebras_api_key = "file-cerebras-key"
            model = "file-model"
            api_base = "https://file-base/v1"
            max_concurrency = 3
            "#,
        );

        let cfg = Config::load_from(Some(&path)).unwrap();

        assert_eq!(cfg.cerebras_api_key.as_deref(), Some("file-cerebras-key"));
        assert_eq!(cfg.model, "file-model");
        assert_eq!(cfg.api_base, "https://file-base/v1");
        assert_eq!(cfg.max_concurrency, 3);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_wins_over_file() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        let dir = temp_dir("env-wins");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        write_config(&path, r#"model = "file-model""#);

        set_env("LENS_MODEL", "env-model");

        let cfg = Config::load_from(Some(&path)).unwrap();
        assert_eq!(cfg.model, "env-model");

        clear_env();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_api_keys_are_not_a_load_error() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        let result = Config::load_from(None);
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert_eq!(cfg.cerebras_api_key, None);
    }

    #[test]
    fn malformed_config_file_is_a_config_error() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        write_config(&path, "not valid toml [[[");

        let err = Config::load_from(Some(&path)).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert_eq!(err.code(), "config");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn api_base_rejects_insecure_or_credentialed_urls() {
        let _guard = env_lock().lock().unwrap();
        clear_env();

        for value in [
            "http://example.com/v1",
            "https://user:password@example.com/v1",
            "https://example.com/v1?redirect=evil",
            "not a url",
        ] {
            set_env("LENS_API_BASE", value);
            assert!(Config::load_from(None).is_err(), "accepted {value}");
        }

        set_env("LENS_API_BASE", "http://127.0.0.1:8000/v1/");
        assert_eq!(
            Config::load_from(None).unwrap().api_base,
            "http://127.0.0.1:8000/v1"
        );
        clear_env();
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_config_is_rejected_with_chmod_fix() {
        let _guard = env_lock().lock().unwrap();
        clear_env();
        let dir = temp_dir("insecure-mode");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, r#"cerebras_api_key = "secret""#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let err = Config::load_from(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("mode 600"));
        assert!(err.suggested_fix().unwrap().starts_with("chmod 600"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_rejected() {
        use std::os::unix::fs::symlink;

        let _guard = env_lock().lock().unwrap();
        clear_env();
        let dir = temp_dir("symlink");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.toml");
        let path = dir.join("config.toml");
        write_config(&target, r#"cerebras_api_key = "secret""#);
        symlink(&target, &path).unwrap();

        assert!(Config::load_from(Some(&path)).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }
}
