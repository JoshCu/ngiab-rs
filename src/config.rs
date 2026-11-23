//! Realization parsing (just the bits ngiab-rs cares about) and persisted user state.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Default `output_root` used by bmi-driver when the realization doesn't set one.
pub const BMI_DEFAULT_OUTPUT_ROOT: &str = "outputs/bmi-driver";
/// Where rs-route writes its NetCDF output (its own default).
pub const TROUTE_OUTPUT_DIR: &str = "outputs/troute";

#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub library_file: Option<String>,
    pub init_config: Option<String>,
}

/// Lightweight view of `config/realization.json`.
#[derive(Debug, Clone)]
pub struct Realization {
    pub start_time: String,
    pub end_time: String,
    pub output_interval: i64,
    /// Relative to the data folder, normalised (no leading `./`).
    pub output_root: String,
    pub output_format: String,
    pub forcing_path: Option<String>,
    pub modules: Vec<Module>,
}

impl Realization {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        let time = &v["time"];
        let start_time = time["start_time"]
            .as_str()
            .context("time.start_time missing")?
            .to_string();
        let end_time = time["end_time"]
            .as_str()
            .context("time.end_time missing")?
            .to_string();
        let output_interval = time["output_interval"].as_i64().unwrap_or(3600);

        let output_root = v["output_root"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(BMI_DEFAULT_OUTPUT_ROOT);
        let output_root = normalise_rel(output_root);
        let output_format = v["output_format"].as_str().unwrap_or("csv").to_lowercase();
        let forcing_path = v["global"]["forcing"]["path"].as_str().map(str::to_string);

        let mut modules = Vec::new();
        if let Some(forms) = v["global"]["formulations"].as_array() {
            for f in forms {
                let params = &f["params"];
                let mods = params["modules"].as_array().cloned().unwrap_or_default();
                if mods.is_empty() {
                    modules.push(module_from(params));
                } else {
                    for m in &mods {
                        modules.push(module_from(&m["params"]));
                    }
                }
            }
        }

        Ok(Self {
            start_time,
            end_time,
            output_interval,
            output_root,
            output_format,
            forcing_path,
            modules,
        })
    }

    pub fn is_csv(&self) -> bool {
        self.output_format == "csv"
    }
}

fn module_from(p: &Value) -> Module {
    let s = |k: &str| p[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Module {
        name: s("model_type_name")
            .or_else(|| s("name"))
            .unwrap_or_else(|| "?".into()),
        library_file: s("library_file"),
        init_config: s("init_config"),
    }
}

/// Strip a leading `./` and trailing `/` so the path can be joined onto the data dir
/// or onto `/data` inside a container.
pub fn normalise_rel(p: &str) -> String {
    let mut s = p.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// Persisted state (~/.config/ngiab-rs/state.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub last_path: Option<PathBuf>,
}

fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ngiab-rs").join("state.json"))
}

impl State {
    pub fn load() -> Self {
        state_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = state_path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise() {
        assert_eq!(normalise_rel("./outputs/ngen/"), "outputs/ngen");
        assert_eq!(normalise_rel("outputs/ngen"), "outputs/ngen");
        assert_eq!(normalise_rel("././a"), "a");
    }

    #[test]
    fn realization_defaults() {
        let dir = std::env::temp_dir().join(format!("ngiab-rs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("realization.json");
        std::fs::write(
            &p,
            r#"{"global":{"forcing":{"path":"./forcings/forcings.nc"},
                "formulations":[{"name":"bmi_multi","params":{"modules":[
                  {"name":"bmi_c","params":{"model_type_name":"lstm","library_file":"/x.so","init_config":"./config/{{id}}.yml"}}]}}]},
                "time":{"start_time":"2010-01-01 00:00:00","end_time":"2010-01-02 00:00:00"}}"#,
        )
        .unwrap();
        let r = Realization::load(&p).unwrap();
        assert_eq!(r.output_root, BMI_DEFAULT_OUTPUT_ROOT);
        assert!(r.is_csv());
        assert_eq!(r.output_interval, 3600);
        assert_eq!(r.modules.len(), 1);
        assert_eq!(r.modules[0].name, "lstm");
        assert_eq!(r.forcing_path.as_deref(), Some("./forcings/forcings.nc"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
