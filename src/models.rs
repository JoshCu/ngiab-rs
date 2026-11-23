//! Model bundle manifest (tasks.md A.6 / B.4 / C.1) and the `ngiab-rs models
//! update | revert | status` subcommands. The subcommands are not implemented yet:
//! they are wired up so the CLI shape is final, and a later step fills them in.

use crate::cli::ModelsUpdateArgs;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Highest manifest `schema` this binary understands.
pub const MANIFEST_SCHEMA: u32 = 1;

/// Container prefixes mapped into the bundle when a manifest doesn't list any.
pub const DEFAULT_PREFIXES: [&str; 2] = ["/dmod", "/ngen/ngen/extern"];

/// `manifest.json` at the root of the model bundle (and the sibling `.json` release
/// asset). Unknown fields are ignored and missing ones defaulted, so older and newer
/// bundles both parse; only a newer `schema` is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub schema: u32,
    /// `models-YYYY.MM.DD[.N]`.
    pub release: String,
    pub source_image: String,
    pub source_digest: String,
    /// UTC ISO 8601.
    pub created: String,
    /// Rust target triple the libraries are built for.
    pub target: String,
    /// Highest `GLIBC_x.y` symbol version any bundled library needs.
    pub min_glibc: String,
    /// Container path prefixes the bundle provides (`/X` maps to `<bundle>/X`).
    pub prefixes: Vec<String>,
    pub libs: Vec<String>,
    /// Added by `models update` (the copy inside the tarball can't hold its own hash).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tar_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_from: Option<String>,
}

impl Manifest {
    /// `<bundle>/manifest.json`.
    pub fn path(bundle: &Path) -> PathBuf {
        bundle.join("manifest.json")
    }

    /// Read `<bundle>/manifest.json`. Fails on a missing or unparsable file, and on a
    /// `schema` newer than [`MANIFEST_SCHEMA`] (with a hint to update ngiab-rs).
    pub fn load(bundle: &Path) -> Result<Self> {
        let path = Self::path(bundle);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = serde_json::from_str(text).context("parsing the model manifest")?;
        if m.schema > MANIFEST_SCHEMA {
            anyhow::bail!(
                "model manifest schema {} is newer than this ngiab-rs understands ({}): \
                 run `ngiab-rs self update`",
                m.schema,
                MANIFEST_SCHEMA
            );
        }
        Ok(m)
    }

    /// The mapped container prefixes: the manifest's, else [`DEFAULT_PREFIXES`].
    pub fn prefixes_or_default(m: Option<&Self>) -> Vec<String> {
        match m {
            Some(m) if !m.prefixes.is_empty() => m.prefixes.clone(),
            _ => DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A short label for messages: the release, or "model files" when unnamed.
    pub fn label(&self) -> &str {
        if self.release.is_empty() {
            "model files"
        } else {
            &self.release
        }
    }
}

/// Compare dotted numeric versions (`2.34` vs `2.39.1`). Non-numeric parts count as 0.
pub fn version_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim()
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    };
    let (mut a, mut b) = (parse(a), parse(b));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a < b
}

fn not_implemented(what: &str) -> anyhow::Result<ExitCode> {
    anyhow::bail!("`ngiab-rs models {what}` is not implemented yet")
}

/// `models update [--version R] [--file TAR] [--check] [--force] [-y]`.
pub fn update(_args: &ModelsUpdateArgs) -> anyhow::Result<ExitCode> {
    not_implemented("update")
}

/// `models revert [-y]`.
pub fn revert(_yes: bool) -> anyhow::Result<ExitCode> {
    not_implemented("revert")
}

/// `models status`.
pub fn status() -> anyhow::Result<ExitCode> {
    not_implemented("status")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = r#"{"schema":1,"release":"models-2026.11.23",
        "source_image":"awiciroh/ngiab:v2.1.0","source_digest":"sha256:abc",
        "created":"2026-11-23T00:00:00Z","target":"x86_64-unknown-linux-gnu",
        "min_glibc":"2.34","prefixes":["/dmod","/ngen/ngen/extern"],
        "libs":["libcfebmi.so"],"tar_sha256":"ff"}"#;

    #[test]
    fn parses_the_contract_schema() {
        let m = Manifest::parse(CONTRACT).unwrap();
        assert_eq!(m.release, "models-2026.11.23");
        assert_eq!(m.min_glibc, "2.34");
        assert_eq!(m.prefixes, vec!["/dmod", "/ngen/ngen/extern"]);
        assert_eq!(m.tar_sha256.as_deref(), Some("ff"));
        assert_eq!(m.installed_at, None);
        // Round trip.
        let back = Manifest::parse(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn tolerates_unknown_and_missing_fields() {
        let m = Manifest::parse(r#"{"schema":1,"release":"r","future":{"x":1}}"#).unwrap();
        assert_eq!(m.release, "r");
        assert!(m.prefixes.is_empty());
        assert_eq!(
            Manifest::prefixes_or_default(Some(&m)),
            vec!["/dmod", "/ngen/ngen/extern"]
        );
        assert_eq!(Manifest::prefixes_or_default(None).len(), 2);
    }

    #[test]
    fn newer_schema_asks_for_self_update() {
        let e = Manifest::parse(r#"{"schema":2}"#).unwrap_err();
        assert!(format!("{e:#}").contains("ngiab-rs self update"), "{e:#}");
        assert!(Manifest::parse("not json").is_err());
    }

    #[test]
    fn load_reads_bundle_manifest() {
        let d = std::env::temp_dir().join(format!("ngiab-rs-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(Manifest::load(&d).is_err());
        std::fs::write(Manifest::path(&d), CONTRACT).unwrap();
        assert_eq!(Manifest::load(&d).unwrap().label(), "models-2026.11.23");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dotted_versions() {
        assert!(version_lt("2.31", "2.34"));
        assert!(!version_lt("2.34", "2.34"));
        assert!(!version_lt("2.39", "2.34"));
        assert!(version_lt("2.34", "2.34.1"));
        assert!(!version_lt("2.34.0", "2.34"));
        assert!(version_lt("2.9", "2.10"));
    }
}
