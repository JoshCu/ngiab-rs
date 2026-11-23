//! Pre-run validation of the data folder, realization and backend.

use crate::backend::{BackendKind, CONTAINER_DATA, Launcher};
use crate::config::{Realization, TROUTE_OUTPUT_DIR, normalise_rel};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Level {
    pub fn symbol(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Info => "ℹ",
            Level::Warn => "⚠",
            Level::Fail => "✗",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Preflight {
    pub checks: Vec<Check>,
    pub realization: Option<Realization>,
    pub gpkg: Option<PathBuf>,
    pub divides: Option<usize>,
    /// Host path of bmi-driver's output_root (where the cat-*.csv files go).
    pub output_dir: Option<PathBuf>,
    /// Output dirs that already contain files, with their file counts.
    pub existing_outputs: Vec<(PathBuf, usize)>,
}

impl Preflight {
    pub fn worst(&self) -> Level {
        self.checks
            .iter()
            .map(|c| c.level)
            .max()
            .unwrap_or(Level::Ok)
    }

    pub fn can_run(&self) -> bool {
        self.worst() < Level::Fail
    }

    pub fn existing_output_files(&self) -> usize {
        self.existing_outputs.iter().map(|(_, n)| n).sum()
    }

    /// Directories ngiab-rs considers "outputs" for cleanup purposes. Only
    /// directories that are safe to empty are returned (see `safe_to_clean`).
    pub fn output_dirs(&self, data_dir: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(d) = &self.output_dir {
            v.push(d.clone());
        }
        v.push(data_dir.join(TROUTE_OUTPUT_DIR));
        v.dedup();
        v.retain(|d| safe_to_clean(data_dir, d));
        v
    }

    fn push(&mut self, level: Level, name: &str, detail: impl Into<String>) {
        self.checks.push(Check {
            level,
            name: name.into(),
            detail: detail.into(),
        });
    }
}

/// Where a path from the realization lives on this machine. Relative paths are
/// relative to the data folder. For container runs, `/data/...` (and the legacy
/// `/ngen/ngen/data/...`) refer to the mounted data folder; other absolute paths
/// live inside the image and can't be checked (`None`).
pub fn host_path(data_dir: &Path, p: &str, kind: BackendKind) -> Option<PathBuf> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return Some(data_dir.join(normalise_rel(p)));
    }
    if kind.is_container() {
        for prefix in [CONTAINER_DATA, "/ngen/ngen/data"] {
            if let Ok(rest) = path.strip_prefix(prefix) {
                return Some(data_dir.join(rest));
            }
        }
        return None;
    }
    Some(path.to_path_buf())
}

/// Only empty directories strictly inside the data folder that don't hold inputs.
pub fn safe_to_clean(data_dir: &Path, dir: &Path) -> bool {
    let (Ok(data), Ok(d)) = (data_dir.canonicalize(), dir.canonicalize()) else {
        // Not existing yet is fine (nothing to clean); just require it be inside.
        return dir.starts_with(data_dir) && dir != data_dir;
    };
    d.starts_with(&data)
        && d != data
        && !data.join("config").starts_with(&d)
        && !data.join("forcings").starts_with(&d)
        && !d.join("config").exists()
        && !d.join("realization.json").exists()
}

pub fn run(launcher: &Launcher, routing: bool) -> Preflight {
    let mut pf = Preflight::default();
    let data = &launcher.data_dir;
    let kind = launcher.kind;

    // --- data folder layout -------------------------------------------------
    if !data.is_dir() {
        pf.push(
            Level::Fail,
            "data folder",
            format!("{} does not exist", data.display()),
        );
        return pf;
    }
    pf.push(Level::Ok, "data folder", data.display().to_string());

    let config_dir = data.join("config");
    if !config_dir.is_dir() {
        pf.push(
            Level::Fail,
            "config/",
            "missing (needs realization.json and a .gpkg)",
        );
    }
    match count_files(&data.join("forcings"), 1) {
        Some(n) => pf.push(Level::Ok, "forcings/", format!("{n} files")),
        None => pf.push(
            Level::Warn,
            "forcings/",
            "missing - forcing path must point elsewhere",
        ),
    }
    if data.join("outputs").is_dir() {
        pf.push(Level::Ok, "outputs/", "present");
    } else {
        pf.push(Level::Info, "outputs/", "missing - will be created");
    }

    // --- realization --------------------------------------------------------
    let real_path = config_dir.join("realization.json");
    match Realization::load(&real_path) {
        Ok(r) => {
            pf.push(
                Level::Ok,
                "realization",
                format!(
                    "{} → {} every {}s",
                    r.start_time, r.end_time, r.output_interval
                ),
            );
            let models: Vec<_> = r.modules.iter().map(|m| m.name.as_str()).collect();
            pf.push(Level::Info, "models", models.join(" → "));
            let fmt_level = if r.is_csv() { Level::Ok } else { Level::Warn };
            let mut fmt_detail = format!("{} in {}", r.output_format, r.output_root);
            if !r.is_csv() {
                fmt_detail.push_str(" - routing & catchment plots need csv");
            }
            pf.push(fmt_level, "output format", fmt_detail);
            match host_path(data, &r.output_root, kind) {
                Some(d) => {
                    if !safe_to_clean(data, &d) {
                        pf.push(
                            Level::Warn,
                            "output_root",
                            format!("{} is not a sub-folder of the data folder; it will never be cleaned", r.output_root),
                        );
                    }
                    pf.output_dir = Some(d);
                }
                None => pf.push(
                    Level::Fail,
                    "output_root",
                    format!(
                        "{} is outside /data, outputs would be lost when the container exits",
                        r.output_root
                    ),
                ),
            }
            pf.realization = Some(r);
        }
        Err(e) => pf.push(Level::Fail, "realization", format!("{e:#}")),
    }

    // --- hydrofabric --------------------------------------------------------
    let gpkgs: Vec<PathBuf> = std::fs::read_dir(&config_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "gpkg"))
                .collect()
        })
        .unwrap_or_default();
    let mut first_divide = None;
    match gpkgs.as_slice() {
        [] => pf.push(Level::Fail, "hydrofabric", "no .gpkg in config/"),
        [g, rest @ ..] => {
            let name = g
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if rest.is_empty() {
                pf.push(Level::Ok, "hydrofabric", name);
            } else {
                pf.push(
                    Level::Warn,
                    "hydrofabric",
                    format!(
                        "{} .gpkg files in config/, tools will use {name}",
                        gpkgs.len()
                    ),
                );
            }
            match read_divides(g) {
                Ok((n, first)) => {
                    pf.push(Level::Ok, "catchments", format!("{n} divides"));
                    pf.divides = Some(n);
                    first_divide = first;
                }
                Err(e) => pf.push(
                    Level::Fail,
                    "catchments",
                    format!("cannot read divides: {e}"),
                ),
            }
            pf.gpkg = Some(g.clone());
        }
    }

    // --- realization referenced files --------------------------------------
    if let Some(r) = pf.realization.clone() {
        if let Some(fp) = &r.forcing_path {
            if fp.contains("{{") {
                pf.push(
                    Level::Info,
                    "forcing",
                    format!("{fp} (templated, not checked)"),
                );
            } else if let Some(hp) = host_path(data, fp, kind) {
                if hp.exists() {
                    pf.push(Level::Ok, "forcing", fp.clone());
                } else {
                    pf.push(Level::Fail, "forcing", format!("{fp} not found"));
                }
            }
        }
        for m in &r.modules {
            if let Some(init) = &m.init_config {
                let p = match &first_divide {
                    Some(id) => init.replace("{{id}}", id),
                    None => init.clone(),
                };
                if p.contains("{{") {
                    continue;
                }
                if let Some(hp) = host_path(data, &p, kind)
                    && !hp.exists()
                {
                    pf.push(
                        Level::Fail,
                        &format!("{} config", m.name),
                        format!("{p} not found"),
                    );
                }
            }
            // Library paths refer to the image when running in a container.
            if kind == BackendKind::Local
                && let Some(lib) = &m.library_file
                && !Path::new(lib).exists()
            {
                pf.push(
                    Level::Warn,
                    &format!("{} library", m.name),
                    format!("{lib} not found on this machine"),
                );
            }
        }
        if routing && !r.is_csv() {
            pf.push(
                Level::Warn,
                "routing",
                "will be skipped: needs csv catchment output",
            );
        }
    }

    // --- backend ------------------------------------------------------------
    let det = &launcher.detected;
    match kind {
        BackendKind::Local => {
            match &det.bmi_driver {
                Some(p) => pf.push(Level::Ok, "bmi-driver", p.display().to_string()),
                None => pf.push(Level::Fail, "bmi-driver", "not found on PATH"),
            }
            match &det.rs_route {
                Some(p) => pf.push(Level::Ok, "rs-route", p.display().to_string()),
                None if routing => {
                    pf.push(Level::Warn, "rs-route", "not found - routing unavailable")
                }
                None => pf.push(Level::Info, "rs-route", "not found"),
            }
        }
        _ => {
            match det.runtime(kind) {
                Some(r) if r.usable => pf.push(Level::Ok, kind.name(), det.summary(kind)),
                _ => pf.push(Level::Fail, kind.name(), det.summary(kind)),
            }
            match launcher.image_present() {
                Some(true) => pf.push(Level::Ok, "image", launcher.image.clone()),
                Some(false) => pf.push(
                    Level::Info,
                    "image",
                    format!("{} not present locally, will be pulled", launcher.image),
                ),
                None => {}
            }
        }
    }
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    pf.push(Level::Info, "cpus", format!("{cpus} available"));

    // --- existing outputs ---------------------------------------------------
    for dir in pf.output_dirs(data) {
        if let Some(n) = count_files(&dir, usize::MAX)
            && n > 0
        {
            pf.existing_outputs.push((dir, n));
        }
    }
    let n = pf.existing_output_files();
    if n > 0 {
        pf.push(
            Level::Info,
            "existing outputs",
            format!("{n} files from a previous run"),
        );
    }

    pf
}

fn read_divides(gpkg: &Path) -> rusqlite::Result<(usize, Option<String>)> {
    let conn =
        rusqlite::Connection::open_with_flags(gpkg, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM divides", [], |r| r.get(0))?;
    let first: Option<String> = conn
        .query_row("SELECT divide_id FROM divides LIMIT 1", [], |r| r.get(0))
        .ok();
    Ok((n as usize, first))
}

/// Count regular files below `dir` up to `max_depth` levels deep. `None` if `dir` isn't a dir.
pub fn count_files(dir: &Path, max_depth: usize) -> Option<usize> {
    if !dir.is_dir() {
        return None;
    }
    let mut n = 0;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth + 1 < max_depth {
                    stack.push((e.path(), depth + 1));
                }
            } else {
                n += 1;
            }
        }
    }
    Some(n)
}

/// Delete all files (not directories) below each of `dirs`. Returns the number removed.
pub fn clean_outputs(dirs: &[PathBuf]) -> std::io::Result<usize> {
    let mut removed = 0;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let ft = e.file_type()?;
                if ft.is_dir() {
                    stack.push(e.path());
                } else {
                    std::fs::remove_file(e.path())?;
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

/// Make sure the directories the tools write into exist (rs-route refuses to start otherwise).
/// `output_dir` is the host path of the realization's output_root.
pub fn ensure_output_dirs(data_dir: &Path, output_dir: Option<&Path>) -> std::io::Result<()> {
    if let Some(d) = output_dir {
        std::fs::create_dir_all(d)?;
    }
    std::fs::create_dir_all(data_dir.join(TROUTE_OUTPUT_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_path_mapping() {
        let d = Path::new("/home/me/run");
        assert_eq!(
            host_path(d, "./forcings/f.nc", BackendKind::Docker).unwrap(),
            PathBuf::from("/home/me/run/forcings/f.nc")
        );
        assert_eq!(
            host_path(d, "/ngen/ngen/data/config/x.yml", BackendKind::Docker).unwrap(),
            PathBuf::from("/home/me/run/config/x.yml")
        );
        assert!(host_path(d, "/dmod/x.so", BackendKind::Docker).is_none());
        assert!(host_path(d, "/dmod/x.so", BackendKind::Local).is_some());
        // component-wise prefix: /database is not /data
        assert!(host_path(d, "/database/x", BackendKind::Docker).is_none());
        // local runs take absolute paths literally
        assert_eq!(
            host_path(d, "/data/x", BackendKind::Local).unwrap(),
            PathBuf::from("/data/x")
        );
    }

    #[test]
    fn cleanup_guard() {
        let d = std::env::temp_dir().join(format!("ngiab-rs-guard-{}", std::process::id()));
        std::fs::create_dir_all(d.join("config")).unwrap();
        std::fs::create_dir_all(d.join("outputs/ngen")).unwrap();
        assert!(!safe_to_clean(&d, &d));
        assert!(!safe_to_clean(&d, &d.join(".")));
        assert!(!safe_to_clean(&d, &d.join("config")));
        assert!(!safe_to_clean(&d, d.parent().unwrap()));
        assert!(safe_to_clean(&d, &d.join("outputs/ngen")));
        assert!(safe_to_clean(&d, &d.join("outputs/not-yet")));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn count_and_clean() {
        let d = std::env::temp_dir().join(format!("ngiab-rs-clean-{}", std::process::id()));
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("x"), "1").unwrap();
        std::fs::write(d.join("a/y"), "1").unwrap();
        std::fs::write(d.join("a/b/z"), "1").unwrap();
        assert_eq!(count_files(&d, usize::MAX), Some(3));
        assert_eq!(count_files(&d, 1), Some(1));
        assert_eq!(clean_outputs(std::slice::from_ref(&d)).unwrap(), 3);
        assert_eq!(count_files(&d, usize::MAX), Some(0));
        assert!(d.join("a/b").is_dir());
        let _ = std::fs::remove_dir_all(d);
    }
}
