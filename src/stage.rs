//! Staging for the local backend (tasks.md C.3).
//!
//! NGIAB realizations and model configs name container paths (`/dmod/...`,
//! `/ngen/ngen/extern/...`, `/data/...`). For a local run they are rewritten into a
//! staging copy under `<data>/.ngiab-rs/stage/`, and bmi-driver gets `--config` with
//! the staged realization. The user's `config/` is never modified, except by
//! [`write_back`] when bmi-driver itself added variable mappings to the realization.

use crate::backend::BackendKind;
use crate::paths::PathMap;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// ngiab-rs' own folder inside a data folder. Output cleanup never touches it.
pub const NGIAB_RS_DIR: &str = ".ngiab-rs";

/// `<data>/.ngiab-rs/stage`, wiped and recreated on every local run.
pub fn stage_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(NGIAB_RS_DIR).join("stage")
}

/// Bytes scanned for a NUL to decide a config file is binary (then never rewritten).
const BINARY_SNIFF: usize = 8192;

/// Shared progress for a staging run (read by the TUI from another thread).
#[derive(Debug, Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn get(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

/// The result of [`stage`] when something needed rewriting.
#[derive(Debug, Clone)]
pub struct Staged {
    /// `<data>/.ngiab-rs/stage/realization.json`, for bmi-driver's `--config`.
    pub realization: PathBuf,
    /// Realization string values rewritten.
    pub values: usize,
    /// Per-catchment config files written with rewritten text.
    pub rewrote: usize,
    /// Per-catchment config files linked to the original (nothing to rewrite).
    pub linked: usize,
    /// Hash of the staged realization as written, to spot bmi-driver's write-back.
    pub original_hash: u64,
    /// For [`write_back`]: JSON pointer -> (staged value, the user's value) for every
    /// value staging changed.
    restores: HashMap<String, (String, String)>,
    /// Staged string -> the user's string (for values bmi-driver adds or moves).
    reverse: HashMap<String, String>,
    /// The user's realization as staged, and the hash of its text, so a file edited
    /// during the run is never overwritten.
    original: Value,
    user_hash: u64,
}

impl Staged {
    /// One line for the user: what was staged, and where.
    pub fn summary(&self) -> String {
        format!(
            "{} realization paths and {} config files rewritten ({} linked) in {}",
            self.values,
            self.rewrote,
            self.linked,
            self.realization
                .parent()
                .map_or(String::new(), |p| p.display().to_string())
        )
    }
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// Does `s` contain something that breaks an unquoted YAML or namelist scalar when
/// it is spliced into a config file (`" #"`, `": "` or quotes)?
pub fn text_hazard(s: &str) -> bool {
    s.contains(" #") || s.contains(": ") || s.contains('"') || s.contains('\'')
}

/// Remove the stage dir (refusing to follow a symlinked `.ngiab-rs`).
fn wipe_stage_dir(data_dir: &Path) -> Result<()> {
    let own = data_dir.join(NGIAB_RS_DIR);
    if let Ok(m) = std::fs::symlink_metadata(&own)
        && !m.is_dir()
    {
        anyhow::bail!(
            "{} is not a directory (a symlink or file); remove it first",
            own.display()
        );
    }
    let dir = stage_dir(data_dir);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?
        }
        Ok(_) => {
            std::fs::remove_file(&dir).with_context(|| format!("removing {}", dir.display()))?
        }
        Err(_) => {}
    }
    Ok(())
}

/// Every string value in `v`, with its JSON pointer and object key.
fn walk_strings(
    v: &mut Value,
    ptr: &mut String,
    f: &mut impl FnMut(&str, Option<&str>, &mut String),
) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let len = ptr.len();
                ptr.push('/');
                ptr.push_str(&k.replace('~', "~0").replace('/', "~1"));
                if let Value::String(s) = child {
                    f(ptr, Some(k), s);
                } else {
                    walk_strings(child, ptr, f);
                }
                ptr.truncate(len);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                let len = ptr.len();
                ptr.push('/');
                ptr.push_str(&i.to_string());
                if let Value::String(s) = child {
                    f(ptr, None, s);
                } else {
                    walk_strings(child, ptr, f);
                }
                ptr.truncate(len);
            }
        }
        _ => {}
    }
}

/// `p` with `.` and `..` resolved lexically (no filesystem access).
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(c);
                }
                // `/..` is `/`
            }
            _ => out.push(c),
        }
    }
    out
}

/// Where a config file is staged: its path relative to the data folder, or (for
/// files elsewhere) its absolute path without the leading `/`. Always a plain
/// relative path (no `..`), so it stays inside the staging dir it is joined to.
fn stage_rel(data_dir: &Path, host: &Path) -> PathBuf {
    let host = lexical(host);
    match host.strip_prefix(lexical(data_dir)) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => host
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .collect(),
    }
}

#[cfg(unix)]
fn link_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

/// Local runs are Unix-only; a copy keeps this compiling (and correct) elsewhere.
#[cfg(not(unix))]
fn link_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

fn place(dst: &Path, content: Option<&str>, src: &Path) -> Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match content {
        Some(text) => std::fs::write(dst, text),
        None => link_file(src, dst),
    }
    .with_context(|| format!("staging {}", dst.display()))
}

/// A config file's rewritten text, or `None` to link it (binary, not UTF-8, or no
/// container paths in it).
fn rewritten(map: &PathMap, bytes: &[u8]) -> Option<String> {
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return None;
    }
    map.rewrite_text(std::str::from_utf8(bytes).ok()?)
}

/// Stage one `init_config` template for every divide. Returns the staged template
/// when at least one file needed rewriting (all other files are then symlinked
/// next to it), else `None` and nothing is written.
fn stage_init(
    map: &PathMap,
    data_dir: &Path,
    dest_root: &Path,
    template: &str,
    divide_ids: &[String],
    progress: &Progress,
    counts: &mut (usize, usize),
) -> Result<Option<String>> {
    let Some(template_host) = map.resolve(template).host() else {
        return Ok(None);
    };
    let staged_template = dest_root.join(stage_rel(data_dir, &template_host));
    let files: Vec<String> = if template.contains("{{id}}") {
        divide_ids
            .iter()
            .map(|id| template.replace("{{id}}", id))
            .collect()
    } else {
        vec![template.to_string()]
    };
    // Files seen before the first one that needs rewriting: linked only once we
    // know the template has to be staged at all.
    let mut pending: Vec<(PathBuf, PathBuf)> = vec![];
    let mut seen: std::collections::HashSet<PathBuf> = Default::default();
    let mut staging = false;
    for f in files {
        if progress.cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
        let Some(src) = map.resolve(&f).host() else {
            continue;
        };
        // Missing per-catchment files are skipped: preflight reports the first one.
        let Ok(bytes) = std::fs::read(&src) else {
            continue;
        };
        let dst = dest_root.join(stage_rel(data_dir, &src));
        // The same file twice (duplicate divide ids): already handled.
        if !seen.insert(dst.clone()) {
            continue;
        }
        let text = rewritten(map, &bytes);
        if text.is_none() && !staging {
            pending.push((src, dst));
            continue;
        }
        if !staging {
            staging = true;
            for (s, d) in pending.drain(..) {
                place(&d, None, &s)?;
                counts.1 += 1;
            }
        }
        place(&dst, text.as_deref(), &src)?;
        if text.is_some() {
            counts.0 += 1;
        } else {
            counts.1 += 1;
        }
    }
    Ok(staging.then(|| staged_template.display().to_string()))
}

/// Number of config files [`stage`] will look at for the data folder's realization
/// (0 if it can't be read).
pub fn files_to_stage(data_dir: &Path, divides: usize) -> usize {
    std::fs::read_to_string(data_dir.join("config").join("realization.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map_or(0, |v| file_count(&v, divides))
}

/// Number of config files [`stage`] will look at (for progress and messages).
pub fn file_count(real: &Value, divides: usize) -> usize {
    let mut n = 0;
    let mut v = real.clone();
    walk_strings(&mut v, &mut String::new(), &mut |_, key, s| {
        if key == Some("init_config") && !s.is_empty() {
            n += if s.contains("{{id}}") { divides } else { 1 };
        }
    });
    n
}

/// Stage `config/realization.json` and the configs it names for a local run.
///
/// Wipes `<data>/.ngiab-rs/stage/` first. Every realization string that starts with
/// a mapped prefix is rewritten; each `init_config` whose files mention a container
/// path is staged (rewritten files written, the rest symlinked) and pointed at the
/// staged copy, keeping `{{id}}`. Returns `None` when nothing needs rewriting (and
/// for container backends): bmi-driver then runs without `--config`.
pub fn stage(
    map: &PathMap,
    data_dir: &Path,
    divide_ids: &[String],
    progress: &Progress,
) -> Result<Option<Staged>> {
    if map.kind != BackendKind::Local {
        return Ok(None);
    }
    wipe_stage_dir(data_dir)?;
    let real_path = data_dir.join("config").join("realization.json");
    let text = std::fs::read_to_string(&real_path)
        .with_context(|| format!("reading {}", real_path.display()))?;
    let mut real: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", real_path.display()))?;
    let original = real.clone();
    progress
        .total
        .store(file_count(&real, divide_ids.len()), Ordering::Relaxed);

    // 1. Realization values with a mapped prefix.
    let mut reverse: HashMap<String, String> = HashMap::new();
    let mut restores: HashMap<String, (String, String)> = HashMap::new();
    let mut values = 0;
    let mut inits: Vec<(String, String)> = vec![];
    walk_strings(&mut real, &mut String::new(), &mut |ptr, key, s| {
        // The user's template, not its rewritten form: a host path that happens to
        // start with a mapped prefix (a data folder under `/data/...`) must not be
        // mapped a second time when the files are resolved.
        if key == Some("init_config") && !s.is_empty() {
            inits.push((ptr.to_string(), s.clone()));
        }
        if let Some(new) = map.rewrite_str(s) {
            reverse.entry(new.clone()).or_insert_with(|| s.clone());
            restores.insert(ptr.to_string(), (new.clone(), s.clone()));
            *s = new;
            values += 1;
        }
    });

    // 2. Per-catchment configs.
    let dir = stage_dir(data_dir);
    let mut counts = (0, 0);
    for (i, (ptr, template)) in inits.iter().enumerate() {
        let dest_root = dir.join("init").join(i.to_string());
        if let Some(staged) = stage_init(
            map,
            data_dir,
            &dest_root,
            template,
            divide_ids,
            progress,
            &mut counts,
        )? {
            let user = original
                .pointer(ptr)
                .and_then(Value::as_str)
                .unwrap_or(template)
                .to_string();
            reverse.insert(staged.clone(), user.clone());
            restores.insert(ptr.clone(), (staged.clone(), user));
            if let Some(v) = real.pointer_mut(ptr) {
                *v = Value::String(staged);
            }
        }
    }

    if values == 0 && counts == (0, 0) {
        return Ok(None);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let out = serde_json::to_string_pretty(&real)? + "\n";
    let staged_real = dir.join("realization.json");
    std::fs::write(&staged_real, &out)
        .with_context(|| format!("writing {}", staged_real.display()))?;
    Ok(Some(Staged {
        realization: staged_real,
        values,
        rewrote: counts.0,
        linked: counts.1,
        original_hash: hash_bytes(out.as_bytes()),
        restores,
        reverse,
        original,
        user_hash: hash_bytes(text.as_bytes()),
    }))
}

/// After bmi-driver ran with a staged realization: if it rewrote it (the "Add these
/// mappings to realization.json?" prompt), reverse-map the result to container
/// paths, save `config/realization.json.bak` and replace `config/realization.json`.
/// Returns the backup path when the user's file was updated.
///
/// Every value staging changed goes back to the user's string (by JSON pointer, so
/// a host path the user wrote literally is never touched); values bmi-driver added
/// are reverse-mapped by value. If the user's file changed during the run it is
/// left alone and this fails, naming the staged file.
pub fn write_back(staged: &Staged, map: &PathMap, data_dir: &Path) -> Result<Option<PathBuf>> {
    let Ok(bytes) = std::fs::read(&staged.realization) else {
        return Ok(None);
    };
    if hash_bytes(&bytes) == staged.original_hash {
        return Ok(None);
    }
    let mut v: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", staged.realization.display()))?;
    walk_strings(&mut v, &mut String::new(), &mut |ptr, _, s| {
        if let Some((new, user)) = staged.restores.get(ptr)
            && new == s
        {
            *s = user.clone();
        } else if staged.original.pointer(ptr).and_then(Value::as_str) == Some(s.as_str()) {
            // The user's own value, unchanged.
        } else if let Some(user) = staged.reverse.get(s.as_str()) {
            *s = user.clone();
        } else if let Some(user) = map.unrewrite_str(s) {
            *s = user;
        }
    });
    // Never write a path into the stage dir (wiped on the next run) into the user's
    // realization; keep their file and point at the staged one instead.
    let stage = stage_dir(data_dir).display().to_string();
    let mut leaked = None;
    walk_strings(&mut v, &mut String::new(), &mut |ptr, _, s| {
        if leaked.is_none() && s.contains(&stage) {
            leaked = Some(format!("{ptr} = {s}"));
        }
    });
    if let Some(l) = leaked {
        anyhow::bail!(
            "bmi-driver's update could not be mapped back to container paths ({l}), so \
             config/realization.json was left unchanged; its version is {}",
            staged.realization.display()
        );
    }
    let real = data_dir.join("config").join("realization.json");
    let current = std::fs::read(&real).with_context(|| format!("reading {}", real.display()))?;
    if hash_bytes(&current) != staged.user_hash {
        anyhow::bail!(
            "{} changed during the run, so bmi-driver's update was not applied; \
             its version (with local paths) is {}",
            real.display(),
            staged.realization.display()
        );
    }
    let bak = data_dir.join("config").join("realization.json.bak");
    std::fs::copy(&real, &bak)
        .with_context(|| format!("backing up {} to {}", real.display(), bak.display()))?;
    let tmp = data_dir
        .join("config")
        .join(".realization.json.ngiab-rs.part");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &real).with_context(|| format!("replacing {}", real.display()))?;
    Ok(Some(bak))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ngiab-rs-stage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn map(data: &Path, root: &Path) -> PathMap {
        PathMap {
            kind: BackendKind::Local,
            data_dir: data.into(),
            model_root: Some(root.into()),
            prefixes: vec!["/dmod".into(), "/ngen/ngen/extern".into()],
        }
    }

    const REAL: &str = r#"{"global":{"formulations":[{"name":"bmi_multi","params":{
        "model_type_name":"bmi_multi","init_config":"","modules":[
        {"name":"bmi_c++","params":{"model_type_name":"SLOTH","init_config":"/dev/null",
          "library_file":"/dmod/shared_libs/libslothmodel.so"}},
        {"name":"bmi_fortran","params":{"model_type_name":"NoahOWP",
          "library_file":"/dmod/shared_libs/libsurfacebmi.so",
          "init_config":"./config/cat_config/NOAH-OWP-M/{{id}}.input"}},
        {"name":"bmi_c","params":{"model_type_name":"CFE",
          "library_file":"/dmod/shared_libs/libcfebmi.so.1.0.0",
          "init_config":"./config/cat_config/CFE/{{id}}.ini"}}]}}],
        "forcing":{"path":"./forcings/forcings.nc"}},
        "time":{"start_time":"2010-01-01 00:00:00","end_time":"2010-01-02 00:00:00"},
        "output_root":"/ngen/ngen/data/outputs/ngen"}"#;

    fn data_folder(name: &str) -> (PathBuf, PathBuf, Vec<String>) {
        let base = tmp(name);
        let data = base.join("my data");
        let root = base.join("ngiab home").join("container");
        let noah = data.join("config/cat_config/NOAH-OWP-M");
        let cfe = data.join("config/cat_config/CFE");
        std::fs::create_dir_all(&noah).unwrap();
        std::fs::create_dir_all(&cfe).unwrap();
        std::fs::write(data.join("config/realization.json"), REAL).unwrap();
        let ids: Vec<String> = vec!["cat-1".into(), "cat-2".into(), "cat-3".into()];
        for id in &ids {
            std::fs::write(
                noah.join(format!("{id}.input")),
                "&parameters\n  parameter_dir      = \"/dmod/datasets/static/\"\n/\n",
            )
            .unwrap();
            std::fs::write(cfe.join(format!("{id}.ini")), "soil_params.b=4.05\n").unwrap();
        }
        // a binary file is never rewritten, even with a container path in it
        std::fs::write(noah.join("cat-3.input"), b"\0/dmod/x").unwrap();
        (data, root, ids)
    }

    #[test]
    fn stages_values_and_init_configs() {
        let (data, root, ids) = data_folder("full");
        let m = map(&data, &root);
        let before = std::fs::read(data.join("config/realization.json")).unwrap();
        let p = Progress::default();
        let s = stage(&m, &data, &ids, &p).unwrap().unwrap();
        // SLOTH /dev/null + 3 Noah-OWP + 3 CFE
        assert_eq!(p.get(), (7, 7));
        assert_eq!(s.realization, stage_dir(&data).join("realization.json"));
        assert_eq!((s.rewrote, s.linked), (2, 1));
        // 3 library files + output_root
        assert_eq!(s.values, 4);
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        let mods = &v["global"]["formulations"][0]["params"]["modules"];
        let root_s = root.display().to_string();
        assert_eq!(
            mods[0]["params"]["library_file"],
            format!("{root_s}/dmod/shared_libs/libslothmodel.so")
        );
        assert_eq!(mods[0]["params"]["init_config"], "/dev/null");
        // CFE files have no container paths: template untouched
        assert_eq!(
            mods[2]["params"]["init_config"],
            "./config/cat_config/CFE/{{id}}.ini"
        );
        let noah_t = mods[1]["params"]["init_config"].as_str().unwrap();
        assert!(
            noah_t.ends_with("/config/cat_config/NOAH-OWP-M/{{id}}.input"),
            "{noah_t}"
        );
        assert!(noah_t.starts_with(&stage_dir(&data).display().to_string()));
        let staged1 = std::fs::read_to_string(noah_t.replace("{{id}}", "cat-1")).unwrap();
        assert!(staged1.contains(&format!("\"{root_s}/dmod/datasets/static/\"")));
        let staged3 = PathBuf::from(noah_t.replace("{{id}}", "cat-3"));
        assert!(
            std::fs::symlink_metadata(&staged3)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            v["output_root"],
            data.join("outputs/ngen").display().to_string()
        );
        // the user's config is untouched
        assert_eq!(
            std::fs::read(data.join("config/realization.json")).unwrap(),
            before
        );

        // no write-back when bmi-driver didn't change the staged file
        assert_eq!(write_back(&s, &m, &data).unwrap(), None);
        assert!(!data.join("config/realization.json.bak").exists());

        // bmi-driver accepted mappings: write back, reverse-mapped, with a .bak
        let mut v2 = v.clone();
        v2["global"]["formulations"][0]["params"]["modules"][1]["params"]["variables_names_map"] =
            serde_json::json!({"PRCPNONC": "precip_rate"});
        std::fs::write(&s.realization, serde_json::to_string(&v2).unwrap()).unwrap();
        let bak = write_back(&s, &m, &data).unwrap().unwrap();
        assert_eq!(std::fs::read(&bak).unwrap(), before);
        let text = std::fs::read_to_string(data.join("config/realization.json")).unwrap();
        assert!(!text.contains(".ngiab-rs"), "{text}");
        assert!(!text.contains(&root_s), "{text}");
        let back: Value = serde_json::from_str(&text).unwrap();
        let mods = &back["global"]["formulations"][0]["params"]["modules"];
        assert_eq!(
            mods[1]["params"]["init_config"],
            "./config/cat_config/NOAH-OWP-M/{{id}}.input"
        );
        assert_eq!(
            mods[2]["params"]["library_file"],
            "/dmod/shared_libs/libcfebmi.so.1.0.0"
        );
        assert_eq!(back["output_root"], "/ngen/ngen/data/outputs/ngen");
        assert_eq!(
            mods[1]["params"]["variables_names_map"]["PRCPNONC"],
            "precip_rate"
        );

        // a file edited during the run is never overwritten
        let s2 = stage(&m, &data, &ids, &Progress::default())
            .unwrap()
            .unwrap();
        std::fs::write(&s2.realization, serde_json::to_string(&v2).unwrap()).unwrap();
        std::fs::write(data.join("config/realization.json"), "{}").unwrap();
        assert!(write_back(&s2, &m, &data).is_err());
        assert_eq!(
            std::fs::read_to_string(data.join("config/realization.json")).unwrap(),
            "{}"
        );
        std::fs::write(data.join("config/realization.json"), &before).unwrap();

        // re-staging wipes the previous stage
        let marker = stage_dir(&data).join("stale");
        std::fs::write(&marker, "x").unwrap();
        stage(&m, &data, &ids, &Progress::default())
            .unwrap()
            .unwrap();
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn nothing_to_rewrite_means_no_stage() {
        let base = tmp("none");
        std::fs::create_dir_all(base.join("config/cfg")).unwrap();
        std::fs::write(
            base.join("config/realization.json"),
            r#"{"global":{"formulations":[{"name":"bmi_c","params":{
               "library_file":"/opt/lib/libcfe.so","init_config":"./config/cfg/{{id}}.ini"}}]},
               "time":{"start_time":"a","end_time":"b"}}"#,
        )
        .unwrap();
        std::fs::write(base.join("config/cfg/cat-1.ini"), "x=/opt/y\n").unwrap();
        std::fs::create_dir_all(stage_dir(&base)).unwrap();
        let m = map(&base, Path::new("/b"));
        let ids = vec!["cat-1".to_string(), "cat-missing".to_string()];
        assert!(
            stage(&m, &base, &ids, &Progress::default())
                .unwrap()
                .is_none()
        );
        assert!(!stage_dir(&base).exists());
        // container backends never stage
        let docker = PathMap {
            kind: BackendKind::Docker,
            ..m
        };
        assert!(
            stage(&docker, &base, &ids, &Progress::default())
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn single_file_template_and_symlinked_own_dir() {
        let base = tmp("single");
        std::fs::create_dir_all(base.join("config")).unwrap();
        std::fs::write(
            base.join("config/realization.json"),
            r#"{"global":{"formulations":[{"name":"bmi_c","params":{
               "library_file":"/opt/lib/x.so","init_config":"/ngen/ngen/data/config/one.yml"}}]},
               "time":{"start_time":"a","end_time":"b"}}"#,
        )
        .unwrap();
        std::fs::write(
            base.join("config/one.yml"),
            "run_dir: /ngen/ngen/extern/m\n",
        )
        .unwrap();
        let m = map(&base, Path::new("/b"));
        let s = stage(&m, &base, &[], &Progress::default())
            .unwrap()
            .unwrap();
        assert_eq!(s.rewrote, 1);
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        let init = v["global"]["formulations"][0]["params"]["init_config"]
            .as_str()
            .unwrap();
        assert_eq!(
            init,
            stage_dir(&base)
                .join("init/0/config/one.yml")
                .display()
                .to_string()
        );
        assert_eq!(
            std::fs::read_to_string(init).unwrap(),
            "run_dir: /b/ngen/ngen/extern/m\n"
        );

        // a symlinked .ngiab-rs is refused rather than followed
        let elsewhere = tmp("single-elsewhere");
        std::fs::remove_dir_all(base.join(NGIAB_RS_DIR)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join(NGIAB_RS_DIR)).unwrap();
        assert!(stage(&m, &base, &[], &Progress::default()).is_err());
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn write_back_keeps_user_literals() {
        let base = tmp("literal");
        std::fs::create_dir_all(base.join("config")).unwrap();
        let root = base.join("bundle");
        let literal = format!("{}/dmod/shared_libs/mine.so", root.display());
        let real = serde_json::json!({
            "global": {"formulations": [{"name": "bmi_multi", "params": {"modules": [
                {"name": "bmi_c", "params": {"library_file": "/dmod/shared_libs/libcfebmi.so"}},
                {"name": "bmi_c", "params": {"library_file": literal}}]}}]},
            "time": {"start_time": "a", "end_time": "b"}});
        std::fs::write(base.join("config/realization.json"), real.to_string()).unwrap();
        let m = map(&base, &root);
        let s = stage(&m, &base, &[], &Progress::default())
            .unwrap()
            .unwrap();
        let mut v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        v["global"]["formulations"][0]["params"]["modules"][0]["params"]["variables_names_map"] =
            serde_json::json!({"a": "b"});
        std::fs::write(&s.realization, v.to_string()).unwrap();
        write_back(&s, &m, &base).unwrap().unwrap();
        let back: Value = serde_json::from_str(
            &std::fs::read_to_string(base.join("config/realization.json")).unwrap(),
        )
        .unwrap();
        let mods = &back["global"]["formulations"][0]["params"]["modules"];
        assert_eq!(
            mods[0]["params"]["library_file"],
            "/dmod/shared_libs/libcfebmi.so"
        );
        assert_eq!(mods[1]["params"]["library_file"], literal.as_str());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A data folder whose host path starts with a mapped prefix (a server's
    /// `/data/...`): the init_config template must be resolved from the user's
    /// string, not from its already rewritten form (which would be mapped twice).
    #[test]
    fn host_path_under_a_mapped_prefix_is_not_mapped_twice() {
        let base = tmp("twice");
        std::fs::create_dir_all(base.join("config")).unwrap();
        std::fs::write(
            base.join("config/realization.json"),
            r#"{"global":{"formulations":[{"name":"bmi_c","params":{
               "library_file":"/opt/lib/x.so","init_config":"/ngen/ngen/data/config/one.yml"}}]},
               "time":{"start_time":"a","end_time":"b"}}"#,
        )
        .unwrap();
        std::fs::write(base.join("config/one.yml"), "run_dir: /dmod/m\n").unwrap();
        // Stand-in for `/data`: a bundle prefix that the data folder itself is under.
        let top = format!(
            "/{}",
            base.components()
                .find_map(|c| match c {
                    std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                    _ => None,
                })
                .unwrap()
        );
        let m = PathMap {
            prefixes: vec!["/dmod".into(), top],
            ..map(&base, Path::new("/b"))
        };
        let s = stage(&m, &base, &[], &Progress::default())
            .unwrap()
            .unwrap();
        assert_eq!(s.rewrote, 1);
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        let init = v["global"]["formulations"][0]["params"]["init_config"]
            .as_str()
            .unwrap();
        assert!(
            init.starts_with(&stage_dir(&base).display().to_string()),
            "{init}"
        );
        assert_eq!(
            std::fs::read_to_string(init).unwrap(),
            "run_dir: /b/dmod/m\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `..` in an init_config template never puts a staged file outside the stage
    /// dir (where it could overwrite one of the user's files).
    #[test]
    fn parent_dirs_in_templates_stay_inside_the_stage_dir() {
        let base = tmp("dotdot");
        let data = base.join("a/b/c/d/data");
        std::fs::create_dir_all(data.join("config")).unwrap();
        std::fs::create_dir_all(base.join("ext")).unwrap();
        std::fs::write(base.join("ext/cat-1.ini"), "p=/dmod/x\n").unwrap();
        // the user's file where the unnormalised staged path would have landed
        let victim = base.join("a/b/c/d/ext/cat-1.ini");
        std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
        std::fs::write(&victim, "mine\n").unwrap();
        std::fs::write(
            data.join("config/realization.json"),
            r#"{"global":{"formulations":[{"name":"bmi_c","params":{
               "init_config":"../../../../../ext/{{id}}.ini"}}]},
               "time":{"start_time":"a","end_time":"b"}}"#,
        )
        .unwrap();
        let m = map(&data, Path::new("/b"));
        let s = stage(&m, &data, &["cat-1".into()], &Progress::default())
            .unwrap()
            .unwrap();
        assert_eq!(s.rewrote, 1);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "mine\n");
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        let init = v["global"]["formulations"][0]["params"]["init_config"]
            .as_str()
            .unwrap();
        assert!(!init.contains(".."), "{init}");
        assert!(
            init.starts_with(&stage_dir(&data).display().to_string()),
            "{init}"
        );
        assert_eq!(
            std::fs::read_to_string(init.replace("{{id}}", "cat-1")).unwrap(),
            "p=/b/dmod/x\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn lexical_paths() {
        assert_eq!(lexical(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(lexical(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(lexical(Path::new("../a/../../b")), PathBuf::from("../../b"));
        assert_eq!(
            stage_rel(Path::new("/d/x"), Path::new("/d/x/../../e/f")),
            PathBuf::from("e/f")
        );
        assert_eq!(
            stage_rel(Path::new("/d/x"), Path::new("/d/x/./config/f")),
            PathBuf::from("config/f")
        );
    }

    /// Staged values that moved (or that bmi-driver changed) are still mapped back,
    /// and a stage-dir path that can't be mapped back is never written to the
    /// user's file.
    #[test]
    fn write_back_maps_moved_values_and_refuses_stage_paths() {
        let (data, root, ids) = data_folder("moved");
        let m = map(&data, &root);
        let before = std::fs::read(data.join("config/realization.json")).unwrap();
        let s = stage(&m, &data, &ids, &Progress::default())
            .unwrap()
            .unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&s.realization).unwrap()).unwrap();
        // swap two staged library paths: pointers no longer match their values
        let mut v2 = v.clone();
        let mods = &mut v2["global"]["formulations"][0]["params"]["modules"];
        let a = mods[0]["params"]["library_file"].clone();
        mods[0]["params"]["library_file"] = mods[2]["params"]["library_file"].clone();
        mods[2]["params"]["library_file"] = a;
        std::fs::write(&s.realization, v2.to_string()).unwrap();
        write_back(&s, &m, &data).unwrap().unwrap();
        let text = std::fs::read_to_string(data.join("config/realization.json")).unwrap();
        assert!(!text.contains(&root.display().to_string()), "{text}");
        let back: Value = serde_json::from_str(&text).unwrap();
        let mods = &back["global"]["formulations"][0]["params"]["modules"];
        assert_eq!(
            mods[0]["params"]["library_file"],
            "/dmod/shared_libs/libcfebmi.so.1.0.0"
        );
        assert_eq!(
            mods[2]["params"]["library_file"],
            "/dmod/shared_libs/libslothmodel.so"
        );

        // a stage-dir path with no reverse mapping: the user's file is kept
        std::fs::write(data.join("config/realization.json"), &before).unwrap();
        let s = stage(&m, &data, &ids, &Progress::default())
            .unwrap()
            .unwrap();
        let mut v3 = v.clone();
        v3["extra"] = Value::String(stage_dir(&data).join("init/9/x.ini").display().to_string());
        std::fs::write(&s.realization, v3.to_string()).unwrap();
        let e = write_back(&s, &m, &data).unwrap_err();
        assert!(format!("{e:#}").contains("left unchanged"), "{e:#}");
        assert_eq!(
            std::fs::read(data.join("config/realization.json")).unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn hazards() {
        assert!(text_hazard("/home/a: b"));
        assert!(text_hazard("/home/a #b"));
        assert!(text_hazard("/home/it's"));
        assert!(!text_hazard("/home/my data/x"));
    }
}
