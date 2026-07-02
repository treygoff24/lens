//! Config precedence: env > `~/.config/lens/config.toml` > built-in defaults.
//! Missing API keys are not a load-time error — `doctor` reports them, and
//! `ask` fails with an Auth error only when a key is actually needed.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
    let text = fs::read_to_string(path).map_err(|e| {
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

        Ok(Config {
            cerebras_api_key: env::var("CEREBRAS_API_KEY")
                .ok()
                .or(file_cfg.cerebras_api_key),
            model: env::var("LENS_MODEL")
                .ok()
                .or(file_cfg.model)
                .unwrap_or(defaults.model),
            api_base: env::var("LENS_API_BASE")
                .ok()
                .or(file_cfg.api_base)
                .unwrap_or(defaults.api_base),
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
            .unwrap_or_default()
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
        fs::write(
            &path,
            r#"
            cerebras_api_key = "file-cerebras-key"
            model = "file-model"
            api_base = "http://file-base/v1"
            max_concurrency = 3
            "#,
        )
        .unwrap();

        let cfg = Config::load_from(Some(&path)).unwrap();

        assert_eq!(cfg.cerebras_api_key.as_deref(), Some("file-cerebras-key"));
        assert_eq!(cfg.model, "file-model");
        assert_eq!(cfg.api_base, "http://file-base/v1");
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
        fs::write(&path, r#"model = "file-model""#).unwrap();

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
        fs::write(&path, "not valid toml [[[").unwrap();

        let err = Config::load_from(Some(&path)).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert_eq!(err.code(), "config");

        fs::remove_dir_all(&dir).unwrap();
    }
}
