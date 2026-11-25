//! `ngiab-rs backend list | install | record` and the "set up a missing backend"
//! offer used by the CLI and the TUI (tasks.md B.7). `record` is used by the
//! installers.
//!
//! The offer is decided once, by [`offer`]:
//! - a backend that is only missing its local model files gets `models update`,
//!   run in-process ([`models_update`]);
//! - one that is installed but not running gets a start hint, never a reinstall;
//! - one that is not installed gets the installer, pinned to this version
//!   ([`run`]);
//! - nothing is ever offered inside a container, with `--auto`, or without a
//!   terminal on both stdin and stdout ([`can_prompt`]).

use crate::backend::{self, Availability, BackendKind, Detected};
use crate::cli::ModelsUpdateArgs;
use crate::home::InstallInfo;
use crate::{home, net};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

/// Fetch the installer and its `SHA256SUMS` from this base URL instead of the
/// GitHub release (mirrors and testing). install.sh/install.ps1 read the same
/// variable for their own downloads, and the child inherits it.
pub const DOWNLOAD_URL_ENV: &str = "NGIAB_RS_DOWNLOAD_URL";

/// What to offer when a backend can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    Nothing,
    /// Local has bmi-driver and rs-route but no usable model files: run
    /// `models update` in-process instead of the full installer.
    ModelsUpdate {
        reason: String,
    },
    /// Run the installer for one of these backends (preference order). With more
    /// than one, the user picks.
    Installer {
        kinds: Vec<BackendKind>,
        reason: String,
    },
    /// Installed but not running: how to start each one.
    StartHints(Vec<(BackendKind, String)>),
}

impl Offer {
    /// The backends this offer is about (for `declined_install`).
    pub fn kinds(&self) -> Vec<BackendKind> {
        match self {
            Offer::Nothing => vec![],
            Offer::ModelsUpdate { .. } => vec![BackendKind::Local],
            Offer::Installer { kinds, .. } => kinds.clone(),
            Offer::StartHints(h) => h.iter().map(|(k, _)| *k).collect(),
        }
    }

    /// The question to ask (`None` when there is nothing to confirm).
    pub fn question(&self) -> Option<String> {
        match self {
            Offer::ModelsUpdate { reason } => Some(format!(
                "Backend 'local' is not set up ({reason}). Download the model files now?"
            )),
            Offer::Installer { kinds, reason } => Some(match kinds.as_slice() {
                [k] => format!(
                    "Backend '{}' is not set up ({reason}). Run the installer now?",
                    k.name()
                ),
                _ => format!("No backend is set up ({reason}). Run the installer now?"),
            }),
            Offer::Nothing | Offer::StartHints(_) => None,
        }
    }

    /// What to tell the user when nothing is run: the command to run themselves,
    /// or how to start a stopped runtime.
    pub fn advice(&self) -> Vec<String> {
        match self {
            Offer::Nothing => vec![],
            Offer::ModelsUpdate { .. } => {
                vec!["run `ngiab-rs models update` to download the model files".into()]
            }
            Offer::Installer { kinds, .. } => vec![format!(
                "run `ngiab-rs backend install {}` to set it up",
                match kinds.as_slice() {
                    [k] => k.name().to_string(),
                    _ => format!(
                        "<{}>",
                        kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join("|")
                    ),
                }
            )],
            Offer::StartHints(h) => h
                .iter()
                .map(|(k, hint)| format!("{}: {hint}", k.name()))
                .collect(),
        }
    }
}

/// May we ask the user anything at all? Never with `--auto`, never inside a
/// container, and only with a terminal on both stdin and stdout.
pub fn can_prompt(auto: bool, stdin_tty: bool, stdout_tty: bool, in_container: bool) -> bool {
    !auto && stdin_tty && stdout_tty && !in_container
}

/// Local has its tools but not a usable bundle (the only thing missing is what
/// `models update` installs).
pub fn only_bundle_missing(d: &Detected) -> bool {
    d.local_platform
        && !d.own_image
        && !d.in_container
        && d.bmi_driver.is_some()
        && d.rs_route.is_some()
        && matches!(
            d.availability(BackendKind::Local),
            Availability::NotInstalled(_)
        )
}

/// Can the installer set up `k` here? Only what is `NotInstalled`, never inside a
/// container, and `local` only on the platform the model files are built for.
fn installable(d: &Detected, k: BackendKind) -> bool {
    !d.in_container
        && (k != BackendKind::Local || (backend::LOCAL_SUPPORTED && d.local_platform))
        && matches!(d.availability(k), Availability::NotInstalled(_))
}

/// The backends the "Install backend…" picker lists: everything not installed
/// that can be set up here (local included when only its model files are
/// missing; picking it then runs `models update`).
pub fn candidates(d: &Detected) -> Vec<BackendKind> {
    BackendKind::ALL
        .into_iter()
        .filter(|k| installable(d, *k))
        .collect()
}

/// What to offer for `chosen` (`--backend` / the TUI selection), or, with no
/// backend chosen, when nothing at all is available.
pub fn offer(d: &Detected, chosen: Option<BackendKind>) -> Offer {
    if d.in_container {
        return Offer::Nothing;
    }
    let os = std::env::consts::OS;
    match chosen {
        Some(k) => match d.availability(k) {
            Availability::Available | Availability::Unsupported(_) => Offer::Nothing,
            Availability::NotRunning(r) => Offer::StartHints(vec![(k, start_hint(k, &r, os))]),
            Availability::NotInstalled(reason) => {
                if k == BackendKind::Local && only_bundle_missing(d) {
                    Offer::ModelsUpdate { reason }
                } else if installable(d, k) {
                    Offer::Installer {
                        kinds: vec![k],
                        reason,
                    }
                } else {
                    Offer::Nothing
                }
            }
        },
        None if d.default_kind().is_some() => Offer::Nothing,
        None => {
            if only_bundle_missing(d) {
                let reason = d
                    .availability(BackendKind::Local)
                    .reason()
                    .unwrap_or_default()
                    .to_string();
                return Offer::ModelsUpdate { reason };
            }
            // A stopped runtime is fixed by starting it, not by installing another.
            let stopped: Vec<_> = BackendKind::ALL
                .into_iter()
                .filter_map(|k| match d.availability(k) {
                    Availability::NotRunning(r) => Some((k, start_hint(k, &r, os))),
                    _ => None,
                })
                .collect();
            if !stopped.is_empty() {
                return Offer::StartHints(stopped);
            }
            let kinds = candidates(d);
            if kinds.is_empty() {
                return Offer::Nothing;
            }
            let reason = kinds
                .iter()
                .filter_map(|k| Some(format!("{}: {}", k.name(), d.availability(*k).reason()?)))
                .collect::<Vec<_>>()
                .join("; ");
            Offer::Installer { kinds, reason }
        }
    }
}

/// How to start a runtime that is installed but not reachable. `note` is the
/// availability reason (the first line of `docker info`'s error).
pub fn start_hint(k: BackendKind, note: &str, os: &str) -> String {
    let lower = note.to_ascii_lowercase();
    match (k, os) {
        (BackendKind::Docker, "linux") if lower.contains("permission denied") => {
            "add yourself to the docker group (`sudo usermod -aG docker $USER`), then log out \
             and back in"
                .into()
        }
        (BackendKind::Docker, "linux") => {
            "start the daemon: `sudo systemctl start docker` (or open Docker Desktop)".into()
        }
        (BackendKind::Docker, _) => "open Docker Desktop and wait until it is running".into(),
        (BackendKind::Podman, "linux") => {
            "podman needs no daemon on Linux; run `podman info` to see what is wrong".into()
        }
        (BackendKind::Podman, _) => {
            "start the podman machine: `podman machine start` (run `podman machine init` \
             first if there is none)"
                .into()
        }
        (BackendKind::Local, _) => String::new(),
    }
}

/// The `models update` arguments for the in-process offer: the newest bundle, and
/// a forced reinstall when the installed one is for another machine.
pub fn models_update_args(d: &Detected) -> ModelsUpdateArgs {
    ModelsUpdateArgs {
        version: None,
        file: None,
        check: false,
        force: d
            .models
            .as_ref()
            .is_some_and(|m| !m.target.is_empty() && m.target != backend::BUNDLE_TARGET),
        yes: false,
    }
}

/// Run `models update` in-process (the offer for a missing local bundle).
pub fn models_update(d: &Detected) -> Result<()> {
    let code = crate::models::update(&models_update_args(d))?;
    if code != ExitCode::SUCCESS {
        bail!("`ngiab-rs models update` did not complete");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// backend list

/// `backend list`.
pub fn list() -> Result<ExitCode> {
    print!("{}", render_list(&Detected::detect()));
    Ok(ExitCode::SUCCESS)
}

fn status_word(a: &Availability) -> &'static str {
    match a {
        Availability::Available => "available",
        Availability::NotInstalled(_) => "not installed",
        Availability::NotRunning(_) => "not running",
        Availability::Unsupported(_) => "unsupported",
    }
}

fn render_list(d: &Detected) -> String {
    let os = std::env::consts::OS;
    let mut out = format!("backends ({}):\n", env!("NGIAB_TARGET"));
    for k in BackendKind::ALL {
        let a = d.availability(k);
        let detail = match a.reason() {
            None => d.summary(k),
            Some(r) => r.to_string(),
        };
        out += &format!("  {:<7} {:<14} {detail}\n", k.name(), status_word(&a));
        let o = offer(d, Some(k));
        // The missing-bundle reason already names `models update`.
        let advice = match o {
            Offer::ModelsUpdate { .. } => vec![],
            o => o.advice(),
        };
        for line in advice {
            out += &format!("  {:<7} {:<14} {line}\n", "", "");
        }
    }
    out += "\nplatform support:\n";
    let here = if backend::LOCAL_SUPPORTED {
        "supported here"
    } else {
        "not supported here"
    };
    out += &format!(
        "  local   Linux x86_64 (glibc) only, {here}{}\n",
        if d.own_image {
            " (inside the ngiab-rs image: yes)"
        } else {
            ""
        }
    );
    out += &format!(
        "  docker  Linux, macOS, Windows{}\n",
        if d.in_container {
            " (not inside a container)"
        } else {
            ""
        }
    );
    out += &format!(
        "  podman  Linux, macOS, Windows{}\n",
        if d.in_container {
            " (not inside a container)"
        } else {
            ""
        }
    );
    out += &format!("\nmodel files (local backend): {}\n", bundle_status(d));
    if d.in_container {
        out += "\nrunning inside a container: nothing is installed from here.\n";
    } else if os == "linux" && d.glibc.is_some() && !d.own_image {
        out += &format!("host glibc: {}\n", d.glibc.as_deref().unwrap_or("?"));
    }
    out
}

fn bundle_status(d: &Detected) -> String {
    if d.own_image {
        return "provided by the image (/dmod)".into();
    }
    if !d.local_platform {
        return "not used on this platform".into();
    }
    let root = d
        .model_root
        .as_ref()
        .map_or(String::new(), |r| format!(" at {}", r.display()));
    match (&d.models, &d.models_error, &d.model_root) {
        (Some(m), _, _) => {
            let mut s = m.label().to_string();
            if !m.target.is_empty() {
                s += &format!(" for {}", m.target);
            }
            if !m.min_glibc.is_empty() {
                s += &format!(", needs glibc >= {}", m.min_glibc);
            }
            s + &root
        }
        (None, Some(e), _) => format!("unusable{root} ({e}); run `ngiab-rs models update`"),
        (None, None, Some(_)) => format!("no manifest{root}; run `ngiab-rs models update`"),
        (None, None, None) => "not installed; run `ngiab-rs models update`".into(),
    }
}

// ---------------------------------------------------------------------------
// backend install / the installer re-run

/// `backend install <kinds…>`: run the pinned installer, then report what works.
pub fn install(kinds: &[BackendKind]) -> Result<ExitCode> {
    let mut want: Vec<BackendKind> = vec![];
    for k in kinds {
        if !want.contains(k) {
            want.push(*k);
        }
    }
    if let Some(reason) = refusal(&want, backend::in_container(), backend::LOCAL_SUPPORTED) {
        bail!("{reason}");
    }
    let status = run(&want)?;
    if !status.success() {
        eprintln!("the installer reported a problem ({status}); checking what works now");
    }
    let d = Detected::detect();
    let mut ok = true;
    println!();
    for k in &want {
        let a = d.availability(*k);
        match a.reason() {
            None => println!("{}: available", k.name()),
            Some(r) => {
                ok = false;
                println!("{}: {} ({r})", k.name(), status_word(&a));
                for line in offer(&d, Some(*k)).advice() {
                    println!("  {line}");
                }
            }
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Why `backend install` must not run for `kinds` here (`None`: go ahead).
fn refusal(kinds: &[BackendKind], in_container: bool, local_supported: bool) -> Option<String> {
    if in_container {
        return Some(
            "backends can't be installed from inside a container; run the installer on the host"
                .into(),
        );
    }
    if kinds.contains(&BackendKind::Local) && !local_supported {
        return Some(
            "the local backend only works on Linux x86_64; install docker or podman instead".into(),
        );
    }
    None
}

/// `install.sh`, or `install.ps1` on Windows.
fn script_name() -> &'static str {
    if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    }
}

/// Where `asset` comes from: `$NGIAB_RS_DOWNLOAD_URL/asset` when set, else the
/// release of this very version (the installer must match the binary).
fn asset_url(base_override: Option<&str>, repo: &str, version: &str, asset: &str) -> String {
    match base_override.map(|b| b.trim().trim_end_matches('/')) {
        Some(b) if !b.is_empty() => format!("{b}/{asset}"),
        _ => net::release_download_url(repo, &format!("v{version}"), asset),
    }
}

/// The directory holding this binary when it is named `ngiab-rs[.exe]`: the
/// installer's `--install-dir`, so `--skip-binary` finds this very binary even
/// when it is not on `PATH`.
fn own_install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    (exe.file_stem()? == "ngiab-rs").then(|| exe.parent().map(Path::to_path_buf))?
}

/// A fresh temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let p =
            std::env::temp_dir().join(format!("ngiab-rs-installer-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&p).with_context(|| format!("creating {}", p.display()))?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The installer command line (`kinds` empty: let the installer's menu ask).
fn installer_command(
    script: &Path,
    kinds: &[BackendKind],
    version: &str,
    install_dir: Option<&Path>,
) -> Command {
    let list = kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join(",");
    if cfg!(windows) {
        // install.ps1 takes environment variables only (no param() block).
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .env("NGIAB_RS_SKIP_BINARY", "1")
            .env("NGIAB_RS_VERSION", version);
        if !list.is_empty() {
            c.env("NGIAB_RS_BACKENDS", &list);
        }
        if let Some(d) = install_dir {
            c.env("NGIAB_RS_INSTALL_DIR", d);
        }
        c
    } else {
        let mut c = Command::new("sh");
        c.arg(script)
            .arg("--skip-binary")
            .args(["--version", version]);
        if !list.is_empty() {
            c.args(["--backends", &list]);
        }
        if let Some(d) = install_dir {
            c.arg("--install-dir").arg(d);
        }
        c
    }
}

/// Download the installer pinned to this version, verify it against that
/// release's `SHA256SUMS`, and run it with inherited stdio to set up `kinds`
/// (empty: the installer asks). Returns its exit status; the caller re-detects.
pub fn run(kinds: &[BackendKind]) -> Result<ExitStatus> {
    let version = env!("CARGO_PKG_VERSION");
    let base = std::env::var(DOWNLOAD_URL_ENV).ok();
    let repo = home::repo();
    let name = script_name();
    let url = asset_url(base.as_deref(), &repo, version, name);
    let sums_url = asset_url(base.as_deref(), &repo, version, "SHA256SUMS");

    let tmp = TempDir::new()?;
    let script = tmp.0.join(name);
    println!("downloading {url}");
    net::download(&url, &script, false)?;
    let sums = net::get_text(&sums_url)?;
    net::verify_sha256sums(&sums, name, &script)
        .with_context(|| format!("verifying {name} against {sums_url}"))?;

    let mut cmd = installer_command(&script, kinds, version, own_install_dir().as_deref());
    println!(
        "running the ngiab-rs {version} installer{}",
        if kinds.is_empty() {
            String::new()
        } else {
            format!(
                " for {}",
                kinds
                    .iter()
                    .map(|k| k.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let status = cmd
        .status()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    Ok(status)
}

// ---------------------------------------------------------------------------
// backend record

/// `backend record <kinds…>` (hidden; called by the installers after setting up
/// `kinds`): adds them to `install.json` and records this binary's version, target
/// and path. A missing `install_method` becomes `installer`.
pub fn record(kinds: &[BackendKind]) -> anyhow::Result<ExitCode> {
    let mut info = InstallInfo::load();
    let exe = std::env::current_exe().ok();
    let exe = exe.map(|e| e.canonicalize().unwrap_or(e));
    apply_record(&mut info, kinds, exe, &now_utc());
    info.save()?;
    println!(
        "recorded backends: {} ({})",
        info.backends.join(", "),
        crate::home::install_json()?.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn apply_record(
    info: &mut InstallInfo,
    kinds: &[BackendKind],
    exe: Option<std::path::PathBuf>,
    now: &str,
) {
    for k in kinds {
        let name = k.name().to_string();
        if !info.backends.contains(&name) {
            info.backends.push(name);
        }
    }
    // Keep a stable order: local, docker, podman, then anything unknown.
    let rank = |b: &String| {
        BackendKind::ALL
            .iter()
            .position(|k| k.name() == b)
            .unwrap_or(usize::MAX)
    };
    info.backends.sort_by_key(rank);
    info.schema = 1;
    info.ngiab_rs_version = env!("CARGO_PKG_VERSION").into();
    info.target = env!("NGIAB_TARGET").into();
    if exe.is_some() {
        info.bin_path = exe;
    }
    if info.install_method.is_empty() {
        info.install_method = "installer".into();
    }
    if info.repo.is_empty() {
        info.repo = crate::home::repo();
    }
    if info.installed_at.is_empty() {
        info.installed_at = now.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backend::RuntimeInfo;
    use crate::models::Manifest;

    fn rt(usable: bool, note: Option<&str>) -> Option<RuntimeInfo> {
        Some(RuntimeInfo {
            version: "1".into(),
            usable,
            rootless: false,
            note: note.map(Into::into),
        })
    }

    fn bundle(target: &str) -> Manifest {
        Manifest {
            schema: 1,
            release: "models-2026.11.23".into(),
            target: target.into(),
            ..Default::default()
        }
    }

    /// A Linux x86_64 host with nothing installed.
    fn bare() -> Detected {
        Detected {
            local_platform: backend::LOCAL_SUPPORTED,
            ..Default::default()
        }
    }

    fn with_tools(mut d: Detected) -> Detected {
        d.bmi_driver = Some("/bin/bmi-driver".into());
        d.rs_route = Some("/bin/rs-route".into());
        d
    }

    #[test]
    fn prompts_only_interactively_outside_containers() {
        assert!(can_prompt(false, true, true, false));
        assert!(!can_prompt(true, true, true, false), "--auto never prompts");
        assert!(!can_prompt(false, false, true, false), "stdin not a tty");
        assert!(!can_prompt(false, true, false, false), "stdout not a tty");
        assert!(!can_prompt(false, true, true, true), "inside a container");
    }

    #[test]
    fn chosen_backend_not_installed_gets_the_installer() {
        let mut d = bare();
        d.podman = rt(true, None);
        let o = offer(&d, Some(BackendKind::Docker));
        assert_eq!(
            o,
            Offer::Installer {
                kinds: vec![BackendKind::Docker],
                reason: "docker not installed".into()
            }
        );
        let q = o.question().unwrap();
        assert!(
            q.starts_with("Backend 'docker' is not set up (docker not installed)"),
            "{q}"
        );
        assert_eq!(
            o.advice(),
            vec!["run `ngiab-rs backend install docker` to set it up"]
        );
        // Something else works, so with no backend chosen nothing is offered.
        assert_eq!(offer(&d, None), Offer::Nothing);
        assert_eq!(offer(&d, Some(BackendKind::Podman)), Offer::Nothing);
    }

    #[test]
    fn stopped_runtime_gets_a_start_hint_not_a_reinstall() {
        let mut d = bare();
        d.docker = rt(false, Some("Cannot connect to the Docker daemon"));
        let o = offer(&d, Some(BackendKind::Docker));
        let Offer::StartHints(h) = &o else {
            panic!("{o:?}")
        };
        assert_eq!(h[0].0, BackendKind::Docker);
        assert_eq!(o.question(), None);
        // With nothing available at all, the stopped one wins over installing another.
        let o = offer(&d, None);
        assert!(
            matches!(o, Offer::StartHints(ref h) if h.len() == 1),
            "{o:?}"
        );
    }

    #[test]
    fn start_hints_per_platform() {
        let docker = BackendKind::Docker;
        assert!(start_hint(docker, "permission denied while trying", "linux").contains("usermod"));
        assert!(start_hint(docker, "Cannot connect", "linux").contains("systemctl start docker"));
        assert!(start_hint(docker, "x", "macos").contains("Docker Desktop"));
        assert!(start_hint(docker, "x", "windows").contains("Docker Desktop"));
        let podman = BackendKind::Podman;
        assert!(start_hint(podman, "x", "macos").contains("podman machine start"));
        assert!(start_hint(podman, "x", "windows").contains("podman machine start"));
        assert!(start_hint(podman, "x", "linux").contains("podman info"));
    }

    #[test]
    fn nothing_is_offered_inside_a_container() {
        let mut d = bare();
        d.in_container = true;
        for k in [None, Some(BackendKind::Local), Some(BackendKind::Docker)] {
            assert_eq!(offer(&d, k), Offer::Nothing, "{k:?}");
        }
        assert!(candidates(&d).is_empty());
        assert!(refusal(&[BackendKind::Docker], true, true).is_some());
    }

    #[test]
    fn unsupported_is_never_offered() {
        let mut d = bare();
        d.local_platform = false;
        assert_eq!(offer(&d, Some(BackendKind::Local)), Offer::Nothing);
        assert!(!candidates(&d).contains(&BackendKind::Local));
        assert!(refusal(&[BackendKind::Local], false, false).is_some());
        assert_eq!(refusal(&[BackendKind::Docker], false, false), None);
        assert_eq!(refusal(&[BackendKind::Local], false, true), None);
    }

    #[test]
    fn nothing_available_offers_every_installable_backend() {
        let d = bare();
        let o = offer(&d, None);
        let Offer::Installer { kinds, reason } = &o else {
            panic!("{o:?}")
        };
        if backend::LOCAL_SUPPORTED {
            assert_eq!(kinds, &BackendKind::ALL.to_vec());
        } else {
            assert_eq!(kinds, &[BackendKind::Docker, BackendKind::Podman]);
        }
        assert!(reason.contains("docker: docker not installed"), "{reason}");
        assert!(o.question().unwrap().starts_with("No backend is set up"));
        assert!(
            o.advice()[0].contains("backend install <"),
            "{:?}",
            o.advice()
        );
        assert_eq!(candidates(&d), *kinds);
    }

    #[test]
    fn missing_bundle_only_offers_models_update() {
        if !backend::LOCAL_SUPPORTED {
            return;
        }
        let d = with_tools(bare());
        assert!(only_bundle_missing(&d));
        let o = offer(&d, Some(BackendKind::Local));
        assert!(matches!(o, Offer::ModelsUpdate { .. }), "{o:?}");
        assert!(o.question().unwrap().contains("models update"));
        assert_eq!(o.kinds(), vec![BackendKind::Local]);
        // Also when nothing at all is available and no backend is chosen.
        assert!(matches!(offer(&d, None), Offer::ModelsUpdate { .. }));
        // Local is still listed by the picker (picking it runs models update).
        assert!(candidates(&d).contains(&BackendKind::Local));
        assert!(!models_update_args(&d).force);

        // A bundle for another machine is replaced, forcibly.
        let mut d = with_tools(bare());
        d.model_root = Some("/home/u/.ngiab/container".into());
        d.models = Some(bundle("aarch64-unknown-linux-gnu"));
        assert!(matches!(
            offer(&d, Some(BackendKind::Local)),
            Offer::ModelsUpdate { .. }
        ));
        assert!(models_update_args(&d).force);

        // Missing binaries need the full installer.
        let mut d = bare();
        d.bmi_driver = Some("/bin/bmi-driver".into());
        assert!(!only_bundle_missing(&d));
        assert!(matches!(
            offer(&d, Some(BackendKind::Local)),
            Offer::Installer { .. }
        ));

        // Everything present: nothing to offer.
        let mut d = with_tools(bare());
        d.model_root = Some("/home/u/.ngiab/container".into());
        d.models = Some(bundle(backend::BUNDLE_TARGET));
        assert!(!only_bundle_missing(&d));
        assert_eq!(offer(&d, Some(BackendKind::Local)), Offer::Nothing);
        assert_eq!(offer(&d, None), Offer::Nothing);
    }

    #[test]
    fn installer_urls_are_pinned() {
        assert_eq!(
            asset_url(None, "JoshCu/ngiab-rs", "0.2.0", "install.sh"),
            "https://github.com/JoshCu/ngiab-rs/releases/download/v0.2.0/install.sh"
        );
        assert_eq!(
            asset_url(Some("http://localhost:8000/"), "x/y", "0.2.0", "SHA256SUMS"),
            "http://localhost:8000/SHA256SUMS"
        );
        assert_eq!(
            asset_url(Some("  "), "x/y", "0.2.0", "install.ps1"),
            "https://github.com/x/y/releases/download/v0.2.0/install.ps1"
        );
    }

    #[test]
    fn installer_command_line() {
        let c = installer_command(
            Path::new("/tmp/i/install.sh"),
            &[BackendKind::Docker, BackendKind::Podman],
            "0.2.0",
            Some(Path::new("/home/u/.local/bin")),
        );
        let args: Vec<_> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(windows) {
            assert_eq!(c.get_program(), "powershell");
            assert!(args.contains(&"Bypass".to_string()));
            let envs: Vec<_> = c
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            assert!(envs.contains(&("NGIAB_RS_BACKENDS".into(), Some("docker,podman".into()))));
            assert!(envs.contains(&("NGIAB_RS_SKIP_BINARY".into(), Some("1".into()))));
        } else {
            assert_eq!(c.get_program(), "sh");
            assert_eq!(
                args,
                [
                    "/tmp/i/install.sh",
                    "--skip-binary",
                    "--version",
                    "0.2.0",
                    "--backends",
                    "docker,podman",
                    "--install-dir",
                    "/home/u/.local/bin"
                ]
            );
            // No kinds: the installer's own menu asks.
            let c = installer_command(Path::new("/tmp/i/install.sh"), &[], "0.2.0", None);
            assert!(!c.get_args().any(|a| a == "--backends"));
        }
    }

    #[test]
    fn list_shows_status_support_and_bundle() {
        let mut d = bare();
        d.docker = rt(false, Some("permission denied"));
        let out = render_list(&d);
        assert!(out.contains("docker  not running"), "{out}");
        assert!(out.contains("podman  not installed"), "{out}");
        assert!(out.contains("ngiab-rs backend install podman"), "{out}");
        assert!(out.contains("platform support:"), "{out}");
        assert!(out.contains("model files (local backend):"), "{out}");
        if backend::LOCAL_SUPPORTED {
            assert!(
                out.contains("not installed; run `ngiab-rs models update`"),
                "{out}"
            );
        }
    }

    #[test]
    fn record_merges_backends() {
        let mut info = InstallInfo {
            backends: vec!["podman".into()],
            installed_at: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        apply_record(
            &mut info,
            &[BackendKind::Docker, BackendKind::Local, BackendKind::Podman],
            Some("/home/u/.local/bin/ngiab-rs".into()),
            "2026-11-23T00:00:00Z",
        );
        assert_eq!(info.backends, vec!["local", "docker", "podman"]);
        assert_eq!(info.install_method, "installer");
        assert_eq!(info.installed_at, "2026-01-01T00:00:00Z");
        assert_eq!(
            info.bin_path.as_deref(),
            Some(std::path::Path::new("/home/u/.local/bin/ngiab-rs"))
        );

        let mut fresh = InstallInfo {
            install_method: "cargo".into(),
            ..Default::default()
        };
        apply_record(&mut fresh, &[BackendKind::Docker], None, "now");
        assert_eq!(fresh.backends, vec!["docker"]);
        assert_eq!(fresh.install_method, "cargo");
        assert_eq!(fresh.installed_at, "now");
    }
}
