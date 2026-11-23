//! Pre-run validation of the data folder, realization and backend.

use crate::backend::{BackendKind, Launcher};
use crate::config::{Realization, TROUTE_OUTPUT_DIR};
use crate::models::{Manifest, version_lt};
use crate::paths::{PathMap, Resolved};
use crate::stage::{NGIAB_RS_DIR, text_hazard};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// Local runs: the model bundle's manifest.
    pub models: Option<Manifest>,
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

/// Where a path from the realization lives on this machine (`None` if inside the
/// image, or a bundle path without a bundle). See `paths::PathMap::resolve`.
pub fn host_path(map: &PathMap, p: &str) -> Option<PathBuf> {
    map.resolve(p).host()
}

/// Folders in the data folder that are never cleaned, nor anything inside them:
/// inputs, ngiab-rs' own state (stage dir) and evaluation results.
const KEEP: [&str; 4] = ["config", "forcings", NGIAB_RS_DIR, "eval"];

/// Only directories strictly inside the data folder that don't hold inputs, and are
/// neither inside nor above a [`KEEP`] folder.
pub fn safe_to_clean(data_dir: &Path, dir: &Path) -> bool {
    let keeps = |data: &Path, d: &Path| {
        KEEP.iter()
            .all(|k| !data.join(k).starts_with(d) && !d.starts_with(data.join(k)))
    };
    let (Ok(data), Ok(d)) = (data_dir.canonicalize(), dir.canonicalize()) else {
        // Not existing yet is fine (nothing to clean); just require it be inside,
        // with no `..` that could lead back into a kept folder once it exists.
        let dotdot = dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
        return !dotdot && dir.starts_with(data_dir) && dir != data_dir && keeps(data_dir, dir);
    };
    d.starts_with(&data)
        && d != data
        && keeps(&data, &d)
        && !d.join("config").exists()
        && !d.join("realization.json").exists()
}

pub fn run(launcher: &Launcher, routing: bool) -> Preflight {
    let mut pf = Preflight::default();
    let data = &launcher.data_dir;
    let kind = launcher.kind;
    let map = launcher.path_map();
    let local = kind == BackendKind::Local;

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
            match host_path(&map, &r.output_root) {
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
            } else if let Some(hp) = host_path(&map, fp) {
                if hp.exists() {
                    pf.push(Level::Ok, "forcing", fp.clone());
                } else {
                    pf.push(Level::Fail, "forcing", format!("{fp} not found"));
                }
            }
        }
        let mut libs: Vec<(String, PathBuf)> = vec![];
        // Config files that staging will rewrite for a local run (estimated from the
        // first divide), and container paths in them that don't exist here.
        let mut staged_files = 0usize;
        for m in &r.modules {
            if m.adapter
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case("bmi_python"))
            {
                pf.push(
                    Level::Fail,
                    &format!("{} module", m.name),
                    "Python BMI models are not supported (bmi-driver is built without Python)",
                );
            }
            if let Some(init) = &m.init_config {
                let p = match &first_divide {
                    Some(id) => init.replace("{{id}}", id),
                    None => init.clone(),
                };
                if p.contains("{{") {
                    continue;
                }
                match host_path(&map, &p) {
                    Some(hp) if !hp.exists() => pf.push(
                        Level::Fail,
                        &format!("{} config", m.name),
                        format!("{p} not found"),
                    ),
                    Some(hp) if local => {
                        if let Ok(text) = std::fs::read_to_string(&hp) {
                            if map.rewrite_text(&text).is_some() {
                                staged_files += if init.contains("{{id}}") {
                                    pf.divides.unwrap_or(1)
                                } else {
                                    1
                                };
                            }
                            scan_init_paths(&mut pf, &map, &m.name, &p, &text);
                        }
                    }
                    _ => {}
                }
            }
            // Library paths refer to the image when running in a container.
            if local && let Some(lib) = &m.library_file {
                check_library(&mut pf, launcher, &map, &m.name, lib, &mut libs);
            }
        }
        if local && !libs.is_empty() {
            check_linkage(&mut pf, launcher, &libs);
        }
        if staged_files > 0 {
            pf.push(
                Level::Info,
                "local paths",
                format!("{staged_files} config files will be rewritten for local paths"),
            );
            for (what, p) in [
                ("model files", launcher.model_root.as_deref()),
                ("data folder", Some(data.as_path())),
            ] {
                if let Some(p) = p
                    && text_hazard(&p.display().to_string())
                {
                    pf.push(
                        Level::Warn,
                        "local paths",
                        format!(
                            "the {what} path {} contains \": \", \" #\" or a quote, which can break unquoted YAML/namelist values",
                            p.display()
                        ),
                    );
                }
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
            if let Some(reason) = det.availability(kind).reason()
                && !det.local_platform
            {
                pf.push(Level::Fail, "local", reason.to_string());
            }
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
            if det.local_platform {
                check_models(&mut pf, launcher);
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

/// C.5.1 + C.5.3: the model bundle and glibc (local runs).
fn check_models(pf: &mut Preflight, launcher: &Launcher) {
    let det = &launcher.detected;
    if det.own_image {
        pf.push(Level::Ok, "models", "image files (/dmod)");
        return;
    }
    let Some(root) = &launcher.model_root else {
        pf.push(
            Level::Fail,
            "models",
            "model files not installed: run `ngiab-rs models update`",
        );
        return;
    };
    match (&det.models, &det.models_error) {
        (Some(m), _) => {
            pf.push(
                Level::Ok,
                "models",
                format!("{} ({})", m.label(), root.display()),
            );
            if let Some(have) = &det.glibc
                && !m.min_glibc.is_empty()
                && version_lt(have, &m.min_glibc)
            {
                pf.push(
                    Level::Fail,
                    "glibc",
                    format!(
                        "glibc {have} < {} required by the model libraries; use docker or podman",
                        m.min_glibc
                    ),
                );
            }
            pf.models = Some(m.clone());
        }
        (None, e) => pf.push(
            Level::Warn,
            "models",
            format!(
                "{}: manifest unreadable ({}); try `ngiab-rs models update`",
                root.display(),
                e.as_deref().unwrap_or("unknown error")
            ),
        ),
    }
}

/// C.5.2: a local module library must exist on this machine.
fn check_library(
    pf: &mut Preflight,
    launcher: &Launcher,
    map: &PathMap,
    module: &str,
    lib: &str,
    libs: &mut Vec<(String, PathBuf)>,
) {
    let name = format!("{module} library");
    match map.resolve(lib) {
        Resolved::Host(p) if p.exists() => {
            if !libs.iter().any(|(_, q)| *q == p) {
                libs.push((module.to_string(), p));
            }
        }
        Resolved::Host(p) if map.is_model_path(lib) => {
            let detail = if launcher.detected.own_image {
                format!("{lib} is not in the image")
            } else {
                let release = launcher
                    .detected
                    .models
                    .as_ref()
                    .map_or("model files", |m| m.label());
                format!(
                    "{lib} not in model bundle {release} ({}; try `ngiab-rs models update`)",
                    p.display()
                )
            };
            pf.push(Level::Fail, &name, detail);
        }
        Resolved::Host(_) => pf.push(Level::Fail, &name, format!("{lib} not found")),
        // No bundle: the models check already fails with the fix.
        Resolved::Unmapped(_) | Resolved::InImage => {}
    }
}

/// C.5.2: `ldd` each library with the run's `LD_LIBRARY_PATH`; any `not found`
/// dependency fails (this is what catches SUMMA).
fn check_linkage(pf: &mut Preflight, launcher: &Launcher, libs: &[(String, PathBuf)]) {
    let Some(ldd) = crate::backend::find_executable(&["ldd"]) else {
        pf.push(
            Level::Info,
            "libraries",
            "ldd not found, dependency check skipped",
        );
        return;
    };
    let env = launcher.local_env();
    let mut ok = 0;
    for (module, lib) in libs {
        let mut cmd = std::process::Command::new(&ldd);
        cmd.arg(lib).envs(env.iter().map(|(k, v)| (k, v)));
        let Some(out) = crate::backend::run_with_timeout(&mut cmd, Duration::from_secs(10)) else {
            pf.push(
                Level::Info,
                &format!("{module} library"),
                "ldd did not answer, dependency check skipped",
            );
            continue;
        };
        let missing = ldd_not_found(&String::from_utf8_lossy(&out.stdout));
        if missing.is_empty() {
            ok += 1;
        } else {
            pf.push(
                Level::Fail,
                &format!("{module} library"),
                format!(
                    "{} needs libraries that are missing: {}",
                    lib.file_name().unwrap_or_default().to_string_lossy(),
                    missing.join(", ")
                ),
            );
        }
    }
    if ok > 0 {
        pf.push(
            Level::Ok,
            "libraries",
            format!("{ok} found, dependencies resolve"),
        );
    }
}

/// Sonames on `libfoo.so.1 => not found` lines of `ldd` output.
fn ldd_not_found(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.contains("not found"))
        .filter_map(|l| l.split("=>").next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// C.5.5: container paths in a (first-divide) init config that don't exist here.
fn scan_init_paths(pf: &mut Preflight, map: &PathMap, module: &str, file: &str, text: &str) {
    for token in map.container_paths_in(text) {
        if let Resolved::Host(p) = map.resolve(&token)
            && !p.exists()
        {
            let where_ = if map.is_model_path(&token) {
                " in the model files"
            } else {
                ""
            };
            pf.push(
                Level::Warn,
                &format!("{module} config"),
                format!("{token} (from {file}) not found{where_}"),
            );
        }
    }
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

/// Every `divide_id` in the hydrofabric (staging expands `{{id}}` for each).
pub fn divide_ids(gpkg: &Path) -> rusqlite::Result<Vec<String>> {
    let conn =
        rusqlite::Connection::open_with_flags(gpkg, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT divide_id FROM divides")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
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
                    // Never ngiab-rs' own state, wherever an output dir points.
                    if e.file_name() != NGIAB_RS_DIR {
                        stack.push(e.path());
                    }
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
        // a not-yet-existing dir that leads back into config/ is refused
        assert!(!safe_to_clean(
            &d,
            &d.join("outputs/missing/../../config/x")
        ));
        // ngiab-rs' own folder and eval/ are never cleaned
        std::fs::create_dir_all(d.join(".ngiab-rs/stage")).unwrap();
        std::fs::create_dir_all(d.join("eval/ngiab-rs")).unwrap();
        assert!(!safe_to_clean(&d, &d.join(".ngiab-rs")));
        assert!(!safe_to_clean(&d, &d.join(".ngiab-rs/stage")));
        assert!(!safe_to_clean(&d, &d.join("eval")));
        assert!(!safe_to_clean(&d, &d.join("eval/ngiab-rs/usgs")));
        assert!(!safe_to_clean(&d, &d.join("eval/new")));
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
        std::fs::create_dir_all(d.join("a/.ngiab-rs")).unwrap();
        std::fs::write(d.join("a/.ngiab-rs/keep"), "1").unwrap();
        assert_eq!(clean_outputs(std::slice::from_ref(&d)).unwrap(), 0);
        assert!(d.join("a/.ngiab-rs/keep").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn ldd_output() {
        let out = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibsundials_cvode.so.6 => not found\n\
                   \tlibnetcdff.so.7 => not found\n\tlibc.so.6 => /lib/libc.so.6 (0x7f)\n";
        assert_eq!(
            ldd_not_found(out),
            vec!["libsundials_cvode.so.6", "libnetcdff.so.7"]
        );
        assert!(ldd_not_found("\tlibc.so.6 => /lib/libc.so.6\n").is_empty());
    }
}
