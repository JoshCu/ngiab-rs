//! Model bundle manifest (tasks.md A.6 / B.4 / C.1) and the `ngiab-rs models
//! update | revert | status` subcommands.
//!
//! Only these entries of `~/.ngiab` (shared with NGIAB_data_preprocess) are ever
//! touched here: `container/`, `container.new/`, `container.tmp/`,
//! `container.bak/`, `.lock` and `container.new.tar.gz.part`.
//!
//! Every state change is a rename on the same file system, so an interrupted
//! command leaves a state that [`recover`] can put right on the next run:
//!
//! - update: extract into `container.new/`, then `rm -rf container.bak`,
//!   `container` → `container.bak`, `container.new` → `container`.
//! - revert: `container` → `container.tmp`, `container.bak` → `container`,
//!   `container.tmp` → `container.bak`.

use crate::cli::{self, ModelsUpdateArgs};
use crate::{backend, home, net};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
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
    /// Entries are normalised to `/x[/y]` without a trailing `/`; anything that isn't
    /// absolute, or is `/` itself (which would map every path), is dropped.
    pub fn prefixes_or_default(m: Option<&Self>) -> Vec<String> {
        let listed: Vec<String> = m
            .map(|m| {
                m.prefixes
                    .iter()
                    .map(|p| p.trim().trim_end_matches('/').to_string())
                    .filter(|p| p.starts_with('/') && !p.contains("//"))
                    .collect()
            })
            .unwrap_or_default();
        if listed.is_empty() {
            DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect()
        } else {
            listed
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

/// The bundle tarball and its sibling manifest, as attached to `models-*` releases.
pub const BUNDLE_ASSET: &str = "ngiab-models-x86_64-unknown-linux-gnu.tar.gz";
pub const MANIFEST_ASSET: &str = "ngiab-models-x86_64-unknown-linux-gnu.json";
const SUMS_ASSET: &str = "SHA256SUMS";
const TAG_PREFIX: &str = "models-";

const PLATFORM_MESSAGE: &str =
    "model files are only used by the local backend on Linux x86_64; docker/podman need nothing";

/// The entries of `~/.ngiab` this module may touch.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    root: PathBuf,
    container: PathBuf,
    new: PathBuf,
    tmp: PathBuf,
    bak: PathBuf,
    lock: PathBuf,
    /// The downloaded tarball, kept under its `.part` name until it is extracted.
    part: PathBuf,
}

impl Layout {
    fn from_env() -> Result<Self> {
        let new = home::container_new_dir()?;
        Ok(Self {
            root: home::ngiab_home()?,
            container: home::container_dir()?,
            part: net::part_path(&new.with_file_name("container.new.tar.gz")),
            new,
            tmp: home::container_tmp_dir()?,
            bak: home::container_bak_dir()?,
            lock: home::lock_path()?,
        })
    }

    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            container: root.join("container"),
            new: root.join("container.new"),
            tmp: root.join("container.tmp"),
            bak: root.join("container.bak"),
            lock: root.join(".lock"),
            part: root.join("container.new.tar.gz.part"),
        }
    }
}

/// Does anything (file, dir or dangling symlink) exist at `p`?
fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// Remove `p` whatever it is, without following symlinks. Missing is fine.
fn remove_any(p: &Path) -> Result<()> {
    let Ok(meta) = p.symlink_metadata() else {
        return Ok(());
    };
    let res = if meta.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    };
    res.with_context(|| format!("removing {}", p.display()))
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

/// Exclusive hold on `~/.ngiab/.lock`; released when dropped (the file is closed).
struct Lock {
    _file: File,
}

fn lock(l: &Layout) -> Result<Lock> {
    fs::create_dir_all(&l.root).with_context(|| format!("creating {}", l.root.display()))?;
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&l.lock)
        .with_context(|| format!("opening {}", l.lock.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock { _file: file }),
        Err(fs::TryLockError::WouldBlock) => bail!(
            "another `ngiab-rs models` command is running ({} is locked); try again when it finishes",
            l.lock.display()
        ),
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", l.lock.display()))
        }
    }
}

/// Put right whatever an interrupted update or revert left behind (call under the
/// lock). Returns a line per action taken.
fn recover(l: &Layout) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if !exists(&l.container) {
        if exists(&l.tmp) {
            // Revert stopped after `container` -> `container.tmp`: undo that step.
            rename(&l.tmp, &l.container)?;
            done.push("restored container/ from container.tmp/ (interrupted revert)".into());
        } else if exists(&l.bak) {
            // Update stopped between `container` -> `container.bak` and
            // `container.new` -> `container`: roll back to the previous bundle.
            rename(&l.bak, &l.container)?;
            done.push("restored container/ from container.bak/ (interrupted update)".into());
        }
    } else if exists(&l.tmp) {
        if !exists(&l.bak) {
            // Revert stopped before `container.tmp` -> `container.bak`: finish it.
            rename(&l.tmp, &l.bak)?;
            done.push("moved container.tmp/ to container.bak/ (interrupted revert)".into());
        } else {
            done.push(format!(
                "warning: left {} alone: container/ and container.bak/ both exist, so it is \
                 not clear what it holds; remove it to allow `models revert`",
                l.tmp.display()
            ));
        }
    }
    if exists(&l.new) {
        remove_any(&l.new)?;
        done.push("removed an incomplete container.new/".into());
    }
    if exists(&l.part) {
        remove_any(&l.part)?;
        done.push("removed a partial download (container.new.tar.gz.part)".into());
    }
    Ok(done)
}

/// Lock, then recover, printing what recovery did.
fn lock_and_recover(l: &Layout) -> Result<Lock> {
    let guard = lock(l)?;
    for line in recover(l)? {
        eprintln!("{line}");
    }
    Ok(guard)
}

/// `models-2026.11.23` or `2026.11.23` -> `models-2026.11.23`.
fn release_tag(v: &str) -> Result<String> {
    let v = v.trim();
    let date = v.strip_prefix(TAG_PREFIX).unwrap_or(v);
    ensure!(
        !date.is_empty()
            && date
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "invalid model release {v:?}: expected models-YYYY.MM.DD[.N] or YYYY.MM.DD[.N]"
    );
    Ok(format!("{TAG_PREFIX}{date}"))
}

/// Is model release `a` older than `b`? Both `models-YYYY.MM.DD[.N]`.
pub fn release_lt(a: &str, b: &str) -> bool {
    let date = |s: &str| s.strip_prefix(TAG_PREFIX).unwrap_or(s).to_string();
    version_lt(&date(a), &date(b))
}

/// The newest non-draft `models-*` release in `releases`.
fn pick_newest(releases: Vec<net::Release>) -> Option<net::Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft && r.tag_name.starts_with(TAG_PREFIX))
        .reduce(|best, r| {
            if release_lt(&best.tag_name, &r.tag_name) {
                r
            } else {
                best
            }
        })
}

/// The newest `models-*` release of `repo`.
pub fn newest_release(repo: &str) -> Result<net::Release> {
    pick_newest(net::github_releases(repo)?)
        .with_context(|| format!("{repo} has no models-* release"))
}

fn asset<'a>(rel: &'a net::Release, name: &str) -> Result<&'a net::Asset> {
    rel.asset(name)
        .with_context(|| format!("release {} has no {name} asset", rel.tag_name))
}

fn host_glibc_note(min: &str) -> Option<String> {
    let host = backend::host_glibc()?;
    (!min.is_empty() && version_lt(&host, min)).then(|| {
        format!(
            "warning: these model files need glibc {min} but this system has {host}; the local \
             backend will not work here, use docker or podman instead"
        )
    })
}

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Where the tarball comes from, with what is known about it up front.
struct Source {
    /// The release's (or the sibling `.json`'s) manifest, if any.
    manifest: Option<Manifest>,
    /// Expected tarball hash: from `SHA256SUMS` and/or the manifest's `tar_sha256`.
    sha256: Option<String>,
    kind: SourceKind,
}

enum SourceKind {
    File(PathBuf),
    Url { url: String, sums: String },
}

impl Source {
    fn label(&self) -> String {
        match (&self.manifest, &self.kind) {
            (Some(m), _) if !m.release.is_empty() => m.release.clone(),
            (_, SourceKind::File(p)) => p.display().to_string(),
            (_, SourceKind::Url { url, .. }) => url.clone(),
        }
    }

    fn installed_from(&self) -> String {
        match &self.kind {
            SourceKind::File(p) => p.display().to_string(),
            SourceKind::Url { url, .. } => url.clone(),
        }
    }
}

/// `foo.tar.gz` -> `foo.json` (or `foo.tgz` -> `foo.json`).
fn sibling_json(tar: &Path) -> PathBuf {
    let name = tar.file_name().unwrap_or_default().to_string_lossy();
    let stem = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".tgz"))
        .unwrap_or(&name);
    tar.with_file_name(format!("{stem}.json"))
}

fn file_source(path: &Path) -> Result<Source> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    ensure!(path.is_file(), "{} is not a file", path.display());
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let json = sibling_json(&path);
    let manifest = if json.is_file() {
        let text =
            fs::read_to_string(&json).with_context(|| format!("reading {}", json.display()))?;
        Some(Manifest::parse(&text).with_context(|| format!("in {}", json.display()))?)
    } else {
        None
    };
    let mut sha256 = manifest.as_ref().and_then(|m| m.tar_sha256.clone());
    let sums = path.with_file_name(SUMS_ASSET);
    if let Ok(text) = fs::read_to_string(&sums)
        && let Ok(want) = net::expected_sha256(&text, &name)
    {
        if let Some(have) = &sha256 {
            ensure!(
                have.eq_ignore_ascii_case(&want),
                "{} and {} disagree about the hash of {name}",
                json.display(),
                sums.display()
            );
        }
        sha256 = Some(want);
    }
    if sha256.is_none() {
        eprintln!(
            "warning: {} is unverified (no {} with tar_sha256 and no {SUMS_ASSET} next to it)",
            path.display(),
            json.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok(Source {
        manifest,
        sha256,
        kind: SourceKind::File(path),
    })
}

fn release_source(repo: &str, version: Option<&str>) -> Result<Source> {
    let rel = match version {
        Some(v) => net::github_release(repo, net::ReleaseRef::Tag(&release_tag(v)?))?,
        None => newest_release(repo)?,
    };
    let tar = asset(&rel, BUNDLE_ASSET)?;
    let json = asset(&rel, MANIFEST_ASSET)?;
    let sums = net::get_text(&asset(&rel, SUMS_ASSET)?.browser_download_url)
        .with_context(|| format!("fetching {SUMS_ASSET} of {}", rel.tag_name))?;
    let want = net::expected_sha256(&sums, BUNDLE_ASSET)?;
    let json_text = net::get_text(&json.browser_download_url)
        .with_context(|| format!("fetching {MANIFEST_ASSET} of {}", rel.tag_name))?;
    let json_want = net::expected_sha256(&sums, MANIFEST_ASSET)?;
    let json_got = net::sha256_bytes(json_text.as_bytes());
    ensure!(
        json_got == json_want,
        "checksum mismatch for {MANIFEST_ASSET}: expected {json_want}, got {json_got}"
    );
    let manifest = Manifest::parse(&json_text).with_context(|| format!("in {MANIFEST_ASSET}"))?;
    if let Some(t) = &manifest.tar_sha256 {
        ensure!(
            t.eq_ignore_ascii_case(&want),
            "{MANIFEST_ASSET} and {SUMS_ASSET} of {} disagree about the tarball hash",
            rel.tag_name
        );
    }
    Ok(Source {
        manifest: Some(manifest),
        sha256: Some(want),
        kind: SourceKind::Url {
            url: tar.browser_download_url.clone(),
            sums,
        },
    })
}

/// What `models update` should do, given the installed and offered bundles.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    UpToDate,
    Install,
    /// An older release than the installed one: needs confirmation.
    Downgrade,
}

fn plan(
    installed: Option<&Manifest>,
    offered_release: Option<&str>,
    offered_sha: Option<&str>,
    force: bool,
) -> Plan {
    let Some(inst) = installed else {
        return Plan::Install;
    };
    let same_sha = matches!((inst.tar_sha256.as_deref(), offered_sha),
        (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
    if same_sha && !force {
        return Plan::UpToDate;
    }
    match offered_release {
        Some(r) if !r.is_empty() && !inst.release.is_empty() && release_lt(r, &inst.release) => {
            Plan::Downgrade
        }
        _ => Plan::Install,
    }
}

/// `models update [--version R] [--file TAR] [--check] [--force] [-y]`.
pub fn update(args: &ModelsUpdateArgs) -> Result<ExitCode> {
    if !backend::LOCAL_SUPPORTED {
        println!("{PLATFORM_MESSAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let l = Layout::from_env()?;
    update_in(&l, args, &home::repo())
}

fn update_in(l: &Layout, args: &ModelsUpdateArgs, repo: &str) -> Result<ExitCode> {
    let _lock = lock_and_recover(l)?;
    let mut source = match &args.file {
        Some(f) => file_source(f)?,
        None => release_source(repo, args.version.as_deref())?,
    };
    if let SourceKind::File(p) = &source.kind {
        // The file is hashed anyway (to verify it or to compare with the installed one).
        let got = net::sha256_file(p)?;
        if let Some(want) = &source.sha256 {
            ensure!(
                got.eq_ignore_ascii_case(want),
                "checksum mismatch for {}: expected {want}, got {got}",
                p.display()
            );
        }
        source.sha256 = Some(got);
    }
    if let Some(note) = source
        .manifest
        .as_ref()
        .and_then(|m| host_glibc_note(&m.min_glibc))
    {
        eprintln!("{note}");
    }

    let installed = Manifest::load(&l.container).ok();
    let label = source.label();
    let plan = plan(
        installed.as_ref(),
        source.manifest.as_ref().map(|m| m.release.as_str()),
        source.sha256.as_deref(),
        args.force,
    );
    let current = installed
        .as_ref()
        .map_or("nothing", |m| m.label())
        .to_string();
    if plan == Plan::UpToDate {
        println!("model files are up to date ({current})");
        return Ok(ExitCode::SUCCESS);
    }
    if args.check {
        println!("model update available: {current} -> {label}");
        return Ok(ExitCode::from(cli::EXIT_UPDATE_AVAILABLE));
    }
    if plan == Plan::Downgrade
        && !cli::confirm(
            &format!("Replace {current} with the older {label}?"),
            args.yes,
        )?
    {
        println!("cancelled");
        return Ok(ExitCode::FAILURE);
    }

    // Fetch.
    let tarball = match &source.kind {
        SourceKind::File(p) => p.clone(),
        SourceKind::Url { url, sums } => {
            println!("downloading {label}");
            net::download_part(url, &l.part, BUNDLE_ASSET, true)?;
            net::verify_sha256sums(sums, BUNDLE_ASSET, &l.part)?;
            l.part.clone()
        }
    };

    let res = extract_and_swap(l, &tarball, &source);
    // The downloaded tarball is never kept (a local --file is the user's).
    let _ = remove_any(&l.part);
    let res = res.inspect_err(|_| {
        let _ = remove_any(&l.new);
    })?;

    println!("installed {} (was {current})", res.label());
    if exists(&l.bak) {
        println!(
            "the previous bundle is kept in {} (`ngiab-rs models revert` swaps back)",
            l.bak.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Extract `tarball` into `container.new/`, write the final manifest and swap it in.
fn extract_and_swap(l: &Layout, tarball: &Path, source: &Source) -> Result<Manifest> {
    remove_any(&l.new)?;
    println!("extracting into {}", l.new.display());
    net::extract_tar_gz(tarball, &l.new)?;
    let mut m = Manifest::load(&l.new).context("the tarball has no usable manifest.json")?;
    ensure!(
        m.target == backend::BUNDLE_TARGET,
        "the bundle is for {:?}, not {}",
        m.target,
        backend::BUNDLE_TARGET
    );
    if let Some(src) = &source.manifest
        && !src.release.is_empty()
        && !m.release.is_empty()
    {
        ensure!(
            src.release == m.release,
            "the manifest says {} but the tarball holds {}",
            src.release,
            m.release
        );
    }
    if source.manifest.is_none()
        && let Some(note) = host_glibc_note(&m.min_glibc)
    {
        eprintln!("{note}");
    }
    m.tar_sha256 = source.sha256.clone();
    m.installed_at = Some(now_utc());
    m.installed_from = Some(source.installed_from());
    write_manifest(&l.new, &m)?;
    swap_in(l)?;
    Ok(m)
}

fn write_manifest(bundle: &Path, m: &Manifest) -> Result<()> {
    let path = Manifest::path(bundle);
    let tmp = path.with_extension("json.part");
    let json = serde_json::to_string_pretty(m)? + "\n";
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    rename(&tmp, &path)
}

/// `rm -rf container.bak`, `container` -> `container.bak`, `container.new` -> `container`.
fn swap_in(l: &Layout) -> Result<()> {
    if exists(&l.bak) {
        // The manifest goes first, so a half-deleted backup is never offered by revert.
        let _ = fs::remove_file(Manifest::path(&l.bak));
        remove_any(&l.bak)?;
    }
    let had = exists(&l.container);
    if had {
        rename(&l.container, &l.bak)?;
    }
    if let Err(e) = rename(&l.new, &l.container) {
        if had && let Err(back) = rename(&l.bak, &l.container) {
            return Err(e.context(format!(
                "and moving the previous bundle back failed too ({back:#}); the next \
                 `ngiab-rs models` command restores it"
            )));
        }
        return Err(e);
    }
    Ok(())
}

/// `models revert [-y]`.
pub fn revert(yes: bool) -> Result<ExitCode> {
    if !backend::LOCAL_SUPPORTED {
        println!("{PLATFORM_MESSAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    revert_in(&Layout::from_env()?, yes)
}

fn revert_in(l: &Layout, yes: bool) -> Result<ExitCode> {
    let _lock = lock_and_recover(l)?;
    let back = match (exists(&l.container), Manifest::load(&l.bak)) {
        (true, Ok(m)) => m,
        _ => {
            eprintln!(
                "nothing to revert (no previous bundle in {})",
                l.bak.display()
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    ensure!(
        !exists(&l.tmp),
        "{} is in the way; remove it and try again",
        l.tmp.display()
    );
    let current = Manifest::load(&l.container)
        .map(|m| m.label().to_string())
        .unwrap_or_else(|_| "the installed bundle".into());
    if !cli::confirm(
        &format!("Swap {current} with the backup {}?", back.label()),
        yes,
    )? {
        println!("cancelled");
        return Ok(ExitCode::FAILURE);
    }
    rename(&l.container, &l.tmp)?;
    if let Err(e) = rename(&l.bak, &l.container) {
        if let Err(again) = rename(&l.tmp, &l.container) {
            return Err(e.context(format!(
                "and restoring the installed bundle failed too ({again:#}); the next \
                 `ngiab-rs models` command restores it"
            )));
        }
        return Err(e);
    }
    if let Err(e) = rename(&l.tmp, &l.bak) {
        eprintln!("warning: {e:#}; the next `ngiab-rs models` command finishes the swap");
    }
    println!(
        "reverted to {} ({current} is now the backup; run `ngiab-rs models revert` again to swap back)",
        back.label()
    );
    Ok(ExitCode::SUCCESS)
}

/// `models status`.
pub fn status() -> Result<ExitCode> {
    if !backend::LOCAL_SUPPORTED {
        println!("{PLATFORM_MESSAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let l = Layout::from_env()?;
    // Recover first when possible; a running update just means the state is in flux.
    // The lock is released before the (slow) network lookup in `print_status`.
    match lock_and_recover(&l) {
        Ok(guard) => drop(guard),
        Err(e) => eprintln!("note: {e:#}"),
    }
    print_status(&l, Some(&home::repo()));
    Ok(ExitCode::SUCCESS)
}

fn describe(dir: &Path) -> String {
    if !exists(dir) {
        return "none".into();
    }
    match Manifest::load(dir) {
        Ok(m) => {
            let mut s = m.label().to_string();
            if !m.source_image.is_empty() {
                s += &format!(" from {}", m.source_image);
            }
            if let Some(at) = &m.installed_at {
                s += &format!(", installed {at}");
            }
            s
        }
        Err(e) => format!("unusable ({e:#})"),
    }
}

fn print_status(l: &Layout, repo: Option<&str>) {
    let installed = Manifest::load(&l.container).ok();
    println!("installed: {}", describe(&l.container));
    if exists(&l.container) {
        println!("  path:    {}", l.container.display());
        println!("  size:    {}", human_size(dir_size(&l.container)));
    }
    if let Some(from) = installed.as_ref().and_then(|m| m.installed_from.as_deref()) {
        println!("  from:    {from}");
    }
    println!("backup:    {}", describe(&l.bak));
    if exists(&l.bak) {
        println!("  size:    {}", human_size(dir_size(&l.bak)));
    }
    if let Some(repo) = repo {
        match newest_release(repo) {
            Ok(r) => {
                let note = match &installed {
                    Some(m) if m.release == r.tag_name => " (installed)",
                    Some(m) if release_lt(&m.release, &r.tag_name) => {
                        " (newer: run `ngiab-rs models update`)"
                    }
                    None => " (run `ngiab-rs models update` to install it)",
                    _ => "",
                };
                println!("newest:    {}{note}", r.tag_name);
            }
            Err(e) => println!("newest:    unknown ({e:#})"),
        }
    }
    let host = backend::host_glibc();
    let min = installed
        .as_ref()
        .map(|m| m.min_glibc.as_str())
        .unwrap_or("");
    match (host, min) {
        (None, _) => println!("glibc:     unknown"),
        (Some(h), "") => println!("glibc:     {h}"),
        (Some(h), min) if version_lt(&h, min) => println!(
            "glibc:     {h}, INCOMPATIBLE: the model files need {min}; use docker or podman"
        ),
        (Some(h), min) => println!("glibc:     {h} (model files need {min}: ok)"),
    }
}

/// Bytes used by the files under `dir` (symlinks not followed).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

fn human_size(b: u64) -> String {
    let mib = b as f64 / (1024.0 * 1024.0);
    if mib >= 1024.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{mib:.1} MiB")
    }
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
        // `/`, empty and relative entries would map everything (or nothing): dropped
        let odd = Manifest {
            prefixes: vec!["/".into(), "".into(), "dmod".into(), "/dmod/".into()],
            ..Default::default()
        };
        assert_eq!(Manifest::prefixes_or_default(Some(&odd)), vec!["/dmod"]);
        let none_valid = Manifest {
            prefixes: vec!["/".into()],
            ..Default::default()
        };
        assert_eq!(Manifest::prefixes_or_default(Some(&none_valid)).len(), 2);
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

    // --- models update / revert / recovery -------------------------------------

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ngiab-rs-models-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    /// A bundle tarball for `release` (entries start with `./`, like the real one),
    /// plus its sibling `.json`; returns the tarball path.
    fn make_bundle(dir: &Path, release: &str, payload: &str) -> PathBuf {
        use flate2::write::GzEncoder;
        fs::create_dir_all(dir).unwrap();
        let m = Manifest {
            schema: 1,
            release: release.into(),
            target: backend::BUNDLE_TARGET.into(),
            min_glibc: "2.17".into(),
            prefixes: vec!["/dmod".into()],
            ..Default::default()
        };
        let tar_path = dir.join(BUNDLE_ASSET);
        let mut b = tar::Builder::new(GzEncoder::new(
            File::create(&tar_path).unwrap(),
            flate2::Compression::fast(),
        ));
        let json = serde_json::to_vec(&m).unwrap();
        for (name, data) in [
            ("./manifest.json", json.as_slice()),
            ("./dmod/shared_libs/libcfebmi.so.1.0.0", payload.as_bytes()),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        }
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(
            &mut h,
            "./dmod/shared_libs/libcfebmi.so",
            "libcfebmi.so.1.0.0",
        )
        .unwrap();
        b.into_inner().unwrap().finish().unwrap();
        let mut sib = m.clone();
        sib.tar_sha256 = Some(net::sha256_file(&tar_path).unwrap());
        fs::write(dir.join(MANIFEST_ASSET), serde_json::to_vec(&sib).unwrap()).unwrap();
        tar_path
    }

    #[cfg(unix)]
    fn file_args(tar: &Path) -> ModelsUpdateArgs {
        ModelsUpdateArgs {
            version: None,
            file: Some(tar.to_path_buf()),
            check: false,
            force: false,
            yes: true,
        }
    }

    fn release_of(dir: &Path) -> String {
        Manifest::load(dir).unwrap().release
    }

    #[cfg(unix)]
    fn entries(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn release_tags_and_ordering() {
        assert_eq!(release_tag("2026.11.23").unwrap(), "models-2026.11.23");
        assert_eq!(
            release_tag("models-2026.11.23.1").unwrap(),
            "models-2026.11.23.1"
        );
        assert!(release_tag("models-").is_err());
        assert!(release_tag("v0.2.0").is_err());
        assert!(release_tag("2026..1").is_err());
        assert!(release_lt("models-2026.11.23", "models-2026.11.23.1"));
        assert!(release_lt("models-2026.9.30", "models-2026.11.1"));
        assert!(!release_lt("models-2026.11.23", "models-2026.11.23"));
        let rel = |t: &str, draft| net::Release {
            tag_name: t.into(),
            draft,
            prerelease: false,
            assets: vec![],
        };
        let newest = pick_newest(vec![
            rel("v0.2.0", false),
            rel("models-2026.11.23", false),
            rel("models-2026.12.01", true),
            rel("models-2026.11.23.1", false),
            rel("models-2026.2.1", false),
        ]);
        assert_eq!(newest.unwrap().tag_name, "models-2026.11.23.1");
        assert!(pick_newest(vec![rel("v0.2.0", false)]).is_none());
    }

    #[test]
    fn update_plans() {
        let inst = Manifest {
            release: "models-2026.11.23".into(),
            tar_sha256: Some("AA".into()),
            ..Default::default()
        };
        assert_eq!(plan(None, None, None, false), Plan::Install);
        assert_eq!(
            plan(Some(&inst), Some("models-2026.12.01"), Some("aa"), false),
            Plan::UpToDate
        );
        assert_eq!(
            plan(Some(&inst), Some("models-2026.11.23"), Some("aa"), true),
            Plan::Install
        );
        assert_eq!(
            plan(Some(&inst), Some("models-2026.12.01"), Some("bb"), false),
            Plan::Install
        );
        assert_eq!(
            plan(Some(&inst), Some("models-2026.11.01"), Some("bb"), false),
            Plan::Downgrade
        );
        assert_eq!(plan(Some(&inst), None, Some("bb"), false), Plan::Install);
    }

    #[test]
    fn sibling_json_names() {
        assert_eq!(
            sibling_json(Path::new("/x/ngiab-models-x86_64-unknown-linux-gnu.tar.gz")),
            Path::new("/x/ngiab-models-x86_64-unknown-linux-gnu.json")
        );
        assert_eq!(sibling_json(Path::new("/x/b.tgz")), Path::new("/x/b.json"));
    }

    #[test]
    fn layout_matches_home() {
        // Whatever NGIAB_HOME is in the test environment, the names must agree.
        if let (Ok(l), Ok(root)) = (Layout::from_env(), home::ngiab_home()) {
            assert_eq!(l, Layout::at(&root));
        }
    }

    #[test]
    fn lock_is_exclusive() {
        let root = tmp("lock");
        let l = Layout::at(&root);
        let held = lock(&l).unwrap();
        let e = lock(&l).err().expect("second lock must fail");
        assert!(
            format!("{e:#}").contains("another `ngiab-rs models` command"),
            "{e:#}"
        );
        drop(held);
        lock(&l).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn update_update_revert_revert() {
        let root = tmp("cycle");
        let l = Layout::at(&root);
        // Something NGIAB_data_preprocess owns: must survive everything.
        fs::write(root.join("preprocessor"), "keep").unwrap();
        let a = make_bundle(
            &root
                .join("../")
                .join(format!("ngiab-rs-models-{}-src-a", std::process::id())),
            "models-2026.11.23",
            "A",
        );
        let b = make_bundle(
            &root
                .join("../")
                .join(format!("ngiab-rs-models-{}-src-b", std::process::id())),
            "models-2026.12.01",
            "B",
        );

        // Fresh install.
        assert_eq!(
            update_in(&l, &file_args(&a), "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        let m = Manifest::load(&l.container).unwrap();
        assert_eq!(m.release, "models-2026.11.23");
        assert_eq!(
            m.tar_sha256.as_deref(),
            Some(net::sha256_file(&a).unwrap().as_str())
        );
        assert!(m.installed_at.is_some());
        assert_eq!(m.installed_from.as_deref(), Some(a.to_str().unwrap()));
        assert!(!l.bak.exists());
        let link = l.container.join("dmod/shared_libs/libcfebmi.so");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("libcfebmi.so.1.0.0")
        );
        assert_eq!(fs::read_to_string(&link).unwrap(), "A");

        // Same tarball again: up to date, nothing moves.
        assert_eq!(
            update_in(&l, &file_args(&a), "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        assert!(!l.bak.exists());
        // --check on a different one reports exit 10 and changes nothing.
        let mut check = file_args(&b);
        check.check = true;
        assert_eq!(
            update_in(&l, &check, "none/none").unwrap(),
            ExitCode::from(10)
        );
        assert_eq!(release_of(&l.container), "models-2026.11.23");

        // Update: A becomes the backup.
        assert_eq!(
            update_in(&l, &file_args(&b), "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(release_of(&l.container), "models-2026.12.01");
        assert_eq!(release_of(&l.bak), "models-2026.11.23");
        // Forced reinstall of B: still exactly one backup (now B).
        let mut force = file_args(&b);
        force.force = true;
        assert_eq!(
            update_in(&l, &force, "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(release_of(&l.bak), "models-2026.12.01");
        assert_eq!(
            entries(&root),
            vec![".lock", "container", "container.bak", "preprocessor"]
        );

        // Downgrade to A needs -y (no terminal in tests).
        let mut no_yes = file_args(&a);
        no_yes.yes = false;
        assert!(update_in(&l, &no_yes, "none/none").is_err());
        assert_eq!(
            update_in(&l, &file_args(&a), "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(release_of(&l.container), "models-2026.11.23");
        assert_eq!(release_of(&l.bak), "models-2026.12.01");

        // Revert swaps; revert again swaps back.
        assert_eq!(revert_in(&l, true).unwrap(), ExitCode::SUCCESS);
        assert_eq!(release_of(&l.container), "models-2026.12.01");
        assert_eq!(release_of(&l.bak), "models-2026.11.23");
        assert_eq!(revert_in(&l, true).unwrap(), ExitCode::SUCCESS);
        assert_eq!(release_of(&l.container), "models-2026.11.23");
        assert_eq!(release_of(&l.bak), "models-2026.12.01");
        assert!(revert_in(&l, false).is_err(), "no -y and no terminal");
        assert_eq!(fs::read_to_string(&link).unwrap(), "A");

        // A corrupted tarball (sibling json no longer matches) is refused untouched.
        let bad_dir = b.parent().unwrap().join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        let bad = bad_dir.join(BUNDLE_ASSET);
        fs::copy(&b, &bad).unwrap();
        fs::copy(
            b.with_file_name(MANIFEST_ASSET),
            bad_dir.join(MANIFEST_ASSET),
        )
        .unwrap();
        {
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(&bad)
                .unwrap()
                .write_all(b"x")
                .unwrap();
        }
        let e = update_in(&l, &file_args(&bad), "none/none").unwrap_err();
        assert!(format!("{e:#}").contains("checksum mismatch"), "{e:#}");
        assert_eq!(release_of(&l.container), "models-2026.11.23");
        assert_eq!(
            fs::read_to_string(root.join("preprocessor")).unwrap(),
            "keep"
        );

        // Without a sibling json the file is installed unverified.
        let lone_dir = b.parent().unwrap().join("lone");
        fs::create_dir_all(&lone_dir).unwrap();
        let lone = lone_dir.join("bundle.tar.gz");
        fs::copy(&b, &lone).unwrap();
        assert_eq!(
            update_in(&l, &file_args(&lone), "none/none").unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(release_of(&l.container), "models-2026.12.01");

        for d in [&root, a.parent().unwrap(), b.parent().unwrap()] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn nothing_to_revert() {
        let root = tmp("norevert");
        let l = Layout::at(&root);
        assert_eq!(revert_in(&l, true).unwrap(), ExitCode::FAILURE);
        fs::create_dir_all(&l.container).unwrap();
        fs::create_dir_all(&l.bak).unwrap(); // no manifest: not a usable backup
        assert_eq!(revert_in(&l, true).unwrap(), ExitCode::FAILURE);
        assert!(l.container.exists() && l.bak.exists());
        let _ = fs::remove_dir_all(&root);
    }

    fn bundle_dir(dir: &Path, release: &str) {
        fs::create_dir_all(dir).unwrap();
        let m = Manifest {
            schema: 1,
            release: release.into(),
            ..Default::default()
        };
        write_manifest(dir, &m).unwrap();
    }

    #[test]
    fn crash_recovery() {
        let root = tmp("recover");
        let l = Layout::at(&root);
        fs::write(root.join("preprocessor"), "keep").unwrap();

        // Interrupted extraction / download: container intact, leftovers removed.
        bundle_dir(&l.container, "cur");
        bundle_dir(&l.new, "half");
        fs::write(&l.part, "partial").unwrap();
        let done = recover(&l).unwrap();
        assert_eq!(done.len(), 2, "{done:?}");
        assert_eq!(release_of(&l.container), "cur");
        assert!(!l.new.exists() && !l.part.exists());

        // Update interrupted between the two renames: roll back to the old bundle.
        rename(&l.container, &l.bak).unwrap();
        bundle_dir(&l.new, "next");
        recover(&l).unwrap();
        assert_eq!(release_of(&l.container), "cur");
        assert!(!l.bak.exists() && !l.new.exists());

        // Revert interrupted after step 1: undo it.
        bundle_dir(&l.bak, "old");
        rename(&l.container, &l.tmp).unwrap();
        recover(&l).unwrap();
        assert_eq!(release_of(&l.container), "cur");
        assert_eq!(release_of(&l.bak), "old");
        assert!(!l.tmp.exists());

        // Revert interrupted after step 2: finish it.
        rename(&l.container, &l.tmp).unwrap();
        rename(&l.bak, &l.container).unwrap();
        recover(&l).unwrap();
        assert_eq!(release_of(&l.container), "old");
        assert_eq!(release_of(&l.bak), "cur");
        assert!(!l.tmp.exists());

        // All three present: ambiguous, left alone (and revert refuses).
        bundle_dir(&l.tmp, "mystery");
        let done = recover(&l).unwrap();
        assert!(done[0].starts_with("warning"), "{done:?}");
        assert!(l.tmp.exists());
        assert!(revert_in(&l, true).is_err());
        fs::remove_dir_all(&l.tmp).unwrap();

        // Nothing to do on a clean state.
        assert!(recover(&l).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(root.join("preprocessor")).unwrap(),
            "keep"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
        let root = tmp("size");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/f"), [0u8; 100]).unwrap();
        fs::write(root.join("g"), [0u8; 23]).unwrap();
        assert_eq!(dir_size(&root), 123);
        let _ = fs::remove_dir_all(&root);
    }
}
