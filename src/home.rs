//! Filesystem layout under `~/.ngiab` (or `$NGIAB_HOME`).
//!
//! `~/.ngiab` is shared with NGIAB_data_preprocess, which keeps `preprocessor`,
//! `hydrofabric/`, `zarr_cache` and more there. ngiab-rs only ever touches the
//! entries named by the functions in this module:
//!
//! ```text
//! ~/.ngiab/
//!   install.json        # InstallInfo, written by ngiab-rs
//!   .lock               # models update/revert lock
//!   container/          # local-backend model bundle (Linux x86_64 only)
//!   container.new/      # extraction target during `models update`
//!   container.tmp/      # intermediate during `models revert`
//!   container.bak/      # previous container/ (one level of undo)
//! ```
//!
//! UI state lives elsewhere (`dirs::config_dir()/ngiab-rs/state.json`, see config.rs).

// Consumers (models update, self update, local backend, D.1) land in later steps.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Overrides the ngiab-rs part of `~/.ngiab` (bundle, install.json, lock).
pub const NGIAB_HOME_ENV: &str = "NGIAB_HOME";

/// `$NGIAB_HOME`, else `~/.ngiab`.
pub fn ngiab_home() -> PathBuf {
    ngiab_home_from(std::env::var_os(NGIAB_HOME_ENV), dirs::home_dir())
}

fn ngiab_home_from(env: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    match env.filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => default_ngiab_dir(home),
    }
}

/// `~/.ngiab` (falls back to a relative `.ngiab` if there is no home directory).
fn default_ngiab_dir(home: Option<PathBuf>) -> PathBuf {
    home.unwrap_or_default().join(".ngiab")
}

/// Local-backend model bundle. The container path `/X` maps to `container_dir()/X`.
pub fn container_dir() -> PathBuf {
    ngiab_home().join("container")
}

/// Previous bundle, kept for `models revert`.
pub fn container_bak_dir() -> PathBuf {
    ngiab_home().join("container.bak")
}

/// Extraction target during `models update`; swapped into place when complete.
pub fn container_new_dir() -> PathBuf {
    ngiab_home().join("container.new")
}

/// Intermediate name used by the three-way swap in `models revert`.
pub fn container_tmp_dir() -> PathBuf {
    ngiab_home().join("container.tmp")
}

pub fn install_json() -> PathBuf {
    ngiab_home().join("install.json")
}

/// Exclusive lock held by `models update|revert`.
pub fn lock_path() -> PathBuf {
    ngiab_home().join(".lock")
}

/// NGIAB_data_preprocess's own settings file. Always `~/.ngiab/preprocessor`:
/// it belongs to the preprocessor, so `NGIAB_HOME` does not move it.
pub fn preprocessor_file() -> PathBuf {
    preprocessor_file_from(dirs::home_dir())
}

fn preprocessor_file_from(home: Option<PathBuf>) -> PathBuf {
    default_ngiab_dir(home).join("preprocessor")
}

/// `install.json`: how and where ngiab-rs was installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallInfo {
    pub schema: u32,
    pub ngiab_rs_version: String,
    pub target: String,
    pub bin_path: Option<PathBuf>,
    /// `installer`, `cargo` or `container`.
    pub install_method: String,
    /// Backend names (`local`, `docker`, `podman`) the user chose to set up.
    pub backends: Vec<String>,
    pub repo: String,
    /// UTC ISO 8601.
    pub installed_at: String,
}

impl Default for InstallInfo {
    fn default() -> Self {
        Self {
            schema: 1,
            ngiab_rs_version: env!("CARGO_PKG_VERSION").into(),
            target: env!("NGIAB_TARGET").into(),
            bin_path: None,
            install_method: String::new(),
            backends: vec![],
            repo: "JoshCu/ngiab-rs".into(),
            installed_at: String::new(),
        }
    }
}

impl InstallInfo {
    /// Load `install_json()`; a missing or unreadable file gives the default.
    pub fn load() -> Self {
        Self::load_from(&install_json())
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&install_json())
    }

    /// Write atomically (temp file + rename), creating the parent directory.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, json + "\n")?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ngiab_home_honours_env() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            ngiab_home_from(Some("/tmp/nh".into()), home.clone()),
            PathBuf::from("/tmp/nh")
        );
        assert_eq!(
            ngiab_home_from(None, home.clone()),
            PathBuf::from("/home/u/.ngiab")
        );
        // An empty value is treated as unset.
        assert_eq!(
            ngiab_home_from(Some("".into()), home),
            PathBuf::from("/home/u/.ngiab")
        );
    }

    #[test]
    fn preprocessor_file_ignores_ngiab_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            preprocessor_file_from(home),
            PathBuf::from("/home/u/.ngiab/preprocessor")
        );
        // The public function never consults $NGIAB_HOME (no env mutation needed
        // to show it: it is built from the home dir alone).
        assert_eq!(
            preprocessor_file(),
            preprocessor_file_from(dirs::home_dir())
        );
    }

    #[test]
    fn layout_is_under_ngiab_home() {
        let h = ngiab_home();
        for (p, name) in [
            (container_dir(), "container"),
            (container_bak_dir(), "container.bak"),
            (container_new_dir(), "container.new"),
            (container_tmp_dir(), "container.tmp"),
            (install_json(), "install.json"),
            (lock_path(), ".lock"),
        ] {
            assert_eq!(p, h.join(name));
        }
    }

    #[test]
    fn install_info_round_trip_and_missing_default() {
        let dir = std::env::temp_dir().join(format!("ngiab-rs-home-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("install.json");

        // Missing file -> default.
        assert_eq!(InstallInfo::load_from(&path), InstallInfo::default());

        let info = InstallInfo {
            bin_path: Some("/home/u/.local/bin/ngiab-rs".into()),
            install_method: "installer".into(),
            backends: vec!["local".into(), "docker".into()],
            installed_at: "2026-11-23T00:00:00Z".into(),
            ..Default::default()
        };
        info.save_to(&path).unwrap();
        assert_eq!(InstallInfo::load_from(&path), info);
        assert!(!path.with_extension("json.part").exists());

        // Unknown fields are tolerated, missing ones defaulted.
        std::fs::write(
            &path,
            r#"{"schema":1,"install_method":"cargo","extra":true}"#,
        )
        .unwrap();
        let loaded = InstallInfo::load_from(&path);
        assert_eq!(loaded.install_method, "cargo");
        assert_eq!(loaded.repo, "JoshCu/ngiab-rs");

        // Unreadable (garbage) file -> default.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(InstallInfo::load_from(&path), InstallInfo::default());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
