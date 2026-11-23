//! Detection of execution backends (local binaries, docker, podman) and
//! construction of the command lines used to run each tool.

use crate::models::{Manifest, version_lt};
use crate::paths::PathMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Mount point of the data folder inside the container.
pub const CONTAINER_DATA: &str = "/data";

/// Default image for the docker/podman backends. Pinned to this binary's version so
/// the tools in the image always match what this build of ngiab-rs expects.
pub const DEFAULT_IMAGE: &str = concat!("docker.io/joshcu/ngiab-rs:", env!("CARGO_PKG_VERSION"));

/// The local backend needs the Linux x86_64 (glibc) model bundle. Everywhere else
/// only docker/podman are offered (except inside our own image, see `Detected::detect`).
pub const LOCAL_SUPPORTED: bool = cfg!(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu"
));

/// The only target model bundles are built for.
pub const BUNDLE_TARGET: &str = "x86_64-unknown-linux-gnu";

/// Are we ourselves running inside a container (docker, podman, ...)?
#[cfg(unix)]
pub fn in_container() -> bool {
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some()
}

/// Are we ourselves running inside a container? Never true on Windows hosts.
#[cfg(not(unix))]
pub fn in_container() -> bool {
    false
}

/// Are we running inside our own image (or one built from the same NGIAB base)?
/// There `/dmod` is native whatever the arch, the arm64 image included. Checking for
/// `/dmod/shared_libs` keeps a generic container (toolbox, distrobox) on an
/// unsupported host from being treated as a local-capable platform.
fn in_own_image(in_container: bool) -> bool {
    in_container && Path::new("/dmod/shared_libs").is_dir()
}

/// Should `Detected::detect` look for local bmi-driver/rs-route at all? Only on a
/// platform the model bundle supports, or inside our own image.
fn probe_local(local_supported: bool, in_own_image: bool) -> bool {
    local_supported || in_own_image
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendKind {
    Local,
    Docker,
    Podman,
}

impl BackendKind {
    pub const ALL: [BackendKind; 3] =
        [BackendKind::Local, BackendKind::Docker, BackendKind::Podman];

    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Local => "local",
            BackendKind::Docker => "docker",
            BackendKind::Podman => "podman",
        }
    }

    pub fn is_container(self) -> bool {
        !matches!(self, BackendKind::Local)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    BmiDriver,
    RsRoute,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Tool::BmiDriver => "bmi-driver",
            Tool::RsRoute => "rs-route",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub version: String,
    /// Daemon/service reachable (docker info / podman info succeeded).
    pub usable: bool,
    /// Rootless engines already map container root to the invoking user.
    pub rootless: bool,
    pub note: Option<String>,
}

/// Can a backend be used right now, and if not, why (tasks.md C.7)?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Something is missing that an install would provide (the reason names it).
    NotInstalled(String),
    /// Installed but its daemon/machine is not reachable: start it, don't reinstall.
    NotRunning(String),
    /// Can't work on this platform at all.
    Unsupported(String),
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }

    /// Why the backend can't be used (`None` when it can).
    pub fn reason(&self) -> Option<&str> {
        match self {
            Availability::Available => None,
            Availability::NotInstalled(r)
            | Availability::NotRunning(r)
            | Availability::Unsupported(r) => Some(r),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Detected {
    pub bmi_driver: Option<PathBuf>,
    pub rs_route: Option<PathBuf>,
    pub docker: Option<RuntimeInfo>,
    pub podman: Option<RuntimeInfo>,
    /// We are ourselves running inside a container.
    pub in_container: bool,
    /// Local runs are possible on this platform (`LOCAL_SUPPORTED`, or our own image).
    pub local_platform: bool,
    /// Inside our own image: `/dmod` is real and needs no bundle.
    pub own_image: bool,
    /// Where bundle paths (`/dmod/...`) live for local runs: `home::container_dir()`
    /// when it exists, or `/` inside our own image (identity mapping).
    pub model_root: Option<PathBuf>,
    /// The bundle's manifest, when `model_root` is a bundle with a readable one.
    pub models: Option<Manifest>,
    /// Why the manifest could not be read (bundle dir present but broken).
    pub models_error: Option<String>,
    /// Host glibc version (Linux gnu only).
    pub glibc: Option<String>,
}

impl Detected {
    pub fn detect() -> Self {
        let in_container = in_container();
        let own_image = in_own_image(in_container);
        let local_platform = probe_local(LOCAL_SUPPORTED, own_image);
        let mut d = Self {
            in_container,
            local_platform,
            own_image,
            glibc: host_glibc(),
            ..Default::default()
        };
        if local_platform {
            d.bmi_driver = find_executable(&["bmi-driver"]);
            d.rs_route = find_executable(&["rs-route", "rs_route"]);
            if own_image {
                d.model_root = Some(PathBuf::from("/"));
            } else if let Ok(dir) = crate::home::container_dir()
                && dir.is_dir()
            {
                let dir = std::path::absolute(&dir).unwrap_or(dir);
                match Manifest::load(&dir) {
                    Ok(m) => d.models = Some(m),
                    Err(e) => d.models_error = Some(format!("{e:#}")),
                }
                d.model_root = Some(dir);
            }
        }
        // Inside a container we never try to nest another container.
        if !in_container {
            d.docker = probe_runtime("docker");
            d.podman = probe_runtime("podman");
        }
        d
    }

    pub fn runtime(&self, kind: BackendKind) -> Option<&RuntimeInfo> {
        match kind {
            BackendKind::Local => None,
            BackendKind::Docker => self.docker.as_ref(),
            BackendKind::Podman => self.podman.as_ref(),
        }
    }

    /// Can `kind` run a simulation, and if not, why?
    pub fn availability(&self, kind: BackendKind) -> Availability {
        match kind {
            BackendKind::Local => self.local_availability(),
            _ if self.in_container => {
                Availability::Unsupported("not available inside a container".into())
            }
            _ => match self.runtime(kind) {
                None => Availability::NotInstalled(format!("{} not installed", kind.name())),
                Some(r) if !r.usable => Availability::NotRunning(format!(
                    "{} is not running: {}",
                    kind.name(),
                    r.note.as_deref().unwrap_or("not usable")
                )),
                Some(_) => Availability::Available,
            },
        }
    }

    fn local_availability(&self) -> Availability {
        if !self.local_platform {
            return Availability::Unsupported(
                "not supported on this platform; use docker or podman".into(),
            );
        }
        if !self.own_image
            && let (Some(have), Some(m)) = (&self.glibc, &self.models)
            && !m.min_glibc.is_empty()
            && version_lt(have, &m.min_glibc)
        {
            return Availability::Unsupported(format!(
                "glibc too old ({have} < {} required by the model files); use docker or podman",
                m.min_glibc
            ));
        }
        let mut missing = vec![];
        if self.bmi_driver.is_none() {
            missing.push("bmi-driver");
        }
        if self.rs_route.is_none() {
            missing.push("rs-route");
        }
        let bundle = if self.own_image {
            None
        } else if let Some(m) = &self.models
            && !m.target.is_empty()
            && m.target != BUNDLE_TARGET
        {
            missing.push("model files for this machine");
            Some(format!(
                "installed ones are for {}; run `ngiab-rs models update`",
                m.target
            ))
        } else if self.model_root.is_none() {
            missing.push("model files");
            Some("run `ngiab-rs models update`".to_string())
        } else if let Some(e) = &self.models_error {
            missing.push("a readable model manifest");
            Some(format!("{e}; run `ngiab-rs models update`"))
        } else {
            None
        };
        if missing.is_empty() {
            return Availability::Available;
        }
        let mut r = format!("missing {}", missing.join(", "));
        if let Some(b) = bundle {
            r.push_str(&format!(" ({b})"));
        }
        Availability::NotInstalled(r)
    }

    /// Is this backend [`Availability::Available`]?
    pub fn available(&self, kind: BackendKind) -> bool {
        self.availability(kind).is_available()
    }

    /// Preference local > docker > podman, among the available backends only.
    pub fn default_kind(&self) -> Option<BackendKind> {
        BackendKind::ALL.into_iter().find(|k| self.available(*k))
    }

    /// Bundle prefixes mapped for local runs (the manifest's, else the defaults).
    pub fn model_prefixes(&self) -> Vec<String> {
        Manifest::prefixes_or_default(self.models.as_ref())
    }

    pub fn local_tool(&self, tool: Tool) -> Option<&PathBuf> {
        match tool {
            Tool::BmiDriver => self.bmi_driver.as_ref(),
            Tool::RsRoute => self.rs_route.as_ref(),
        }
    }

    pub fn summary(&self, kind: BackendKind) -> String {
        match kind {
            BackendKind::Local => {
                let b = self
                    .bmi_driver
                    .as_ref()
                    .map_or("missing".into(), |p| p.display().to_string());
                let r = self
                    .rs_route
                    .as_ref()
                    .map_or("missing".into(), |p| p.display().to_string());
                let m = if self.own_image {
                    "image /dmod".to_string()
                } else {
                    self.models
                        .as_ref()
                        .map_or("missing".into(), |m| m.label().to_string())
                };
                format!("bmi-driver: {b}, rs-route: {r}, models: {m}")
            }
            _ => match self.runtime(kind) {
                None => format!("{} not installed", kind.name()),
                Some(r) => {
                    let mut s = format!("{} {}", kind.name(), r.version);
                    if r.rootless {
                        s.push_str(" (rootless)");
                    }
                    if let Some(n) = &r.note {
                        s.push_str(" - ");
                        s.push_str(n);
                    }
                    s
                }
            },
        }
    }
}

/// A fully resolved command ready to spawn.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment variables set for the child (on top of our own env).
    pub env: Vec<(String, String)>,
    /// For container runs: (runtime, container name) so the run can be killed.
    pub container: Option<(String, String)>,
}

impl CommandSpec {
    /// A spec with no extra env and no container.
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: vec![],
            container: None,
        }
    }

    /// Copy-pasteable shell form: `K=V ... program args...`.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", shell_quote(v)))
            .collect();
        parts.push(shell_quote(&self.program));
        parts.extend(self.args.iter().map(|a| shell_quote(a)));
        parts.join(" ")
    }

    /// Apply program, args and env to a `std::process::Command`.
    pub fn to_command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        c
    }
}

/// Quote `s` for a POSIX shell if it contains anything beyond a safe character set.
fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=,+@%^".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Everything needed to turn a tool invocation into a command line.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub kind: BackendKind,
    pub image: String,
    pub data_dir: PathBuf,
    pub detected: Detected,
    /// Local runs: where bundle paths live (see `Detected::model_root`).
    pub model_root: Option<PathBuf>,
}

impl Launcher {
    pub fn new(kind: BackendKind, image: String, data_dir: PathBuf, detected: Detected) -> Self {
        Self {
            kind,
            image,
            data_dir,
            model_root: detected.model_root.clone(),
            detected,
        }
    }

    /// How realization/config paths map onto this machine for this backend.
    pub fn path_map(&self) -> PathMap {
        PathMap {
            kind: self.kind,
            data_dir: self.data_dir.clone(),
            model_root: self.model_root.clone(),
            prefixes: self.detected.model_prefixes(),
        }
    }

    /// Local runs: the env the model libraries need. `LD_LIBRARY_PATH` gets the
    /// bundle's shared_libs in front (the libraries also have `$ORIGIN` RPATHs, but
    /// user libraries bmi-driver dlopens may need bundled ones too), and
    /// `OMP_NUM_THREADS=1` unless set, as in the Dockerfile.
    pub fn local_env(&self) -> Vec<(String, String)> {
        let mut env = vec![];
        if let Some(root) = &self.model_root {
            let libs = root.join("dmod").join("shared_libs").display().to_string();
            let v = match std::env::var("LD_LIBRARY_PATH") {
                Ok(old) if !old.is_empty() => format!("{libs}:{old}"),
                _ => libs,
            };
            env.push(("LD_LIBRARY_PATH".into(), v));
        }
        if std::env::var_os("OMP_NUM_THREADS").is_none() {
            env.push(("OMP_NUM_THREADS".into(), "1".into()));
        }
        env
    }

    /// The data folder as the tool will see it.
    pub fn data_root(&self) -> String {
        if self.kind.is_container() {
            CONTAINER_DATA.to_string()
        } else {
            self.data_dir.display().to_string()
        }
    }

    /// Build the command for `tool` with `tool_args` (which should already use
    /// `data_root()` for any paths). `tty` adds `-t` for container runs.
    pub fn tool_command(
        &self,
        tool: Tool,
        tool_args: Vec<String>,
        tty: bool,
        tag: &str,
    ) -> CommandSpec {
        match self.kind {
            BackendKind::Local => {
                let mut spec = CommandSpec::new(
                    self.detected
                        .local_tool(tool)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| tool.binary().to_string()),
                    tool_args,
                );
                if tool == Tool::BmiDriver {
                    spec.env = self.local_env();
                }
                spec
            }
            kind => {
                let runtime = kind.name().to_string();
                let name = format!("ngiab-rs-{}-{}", std::process::id(), tag);
                let mut args = vec!["run".into(), "--rm".into(), "-i".into()];
                if tty {
                    args.push("-t".into());
                }
                // --init: the tool isn't PID 1, so ctrl-c / SIGTERM actually reach it.
                args.extend(["--init".into(), "--name".into(), name.clone()]);
                args.extend(self.user_args());
                args.extend(self.mount_args());
                args.extend([
                    "-w".into(),
                    CONTAINER_DATA.into(),
                    "--entrypoint".into(),
                    format!("/usr/local/bin/{}", tool.binary()),
                    self.image.clone(),
                ]);
                args.extend(tool_args);
                CommandSpec {
                    container: Some((runtime.clone(), name)),
                    ..CommandSpec::new(runtime, args)
                }
            }
        }
    }

    pub fn pull_command(&self) -> Option<CommandSpec> {
        self.kind
            .is_container()
            .then(|| CommandSpec::new(self.kind.name(), vec!["pull".into(), self.image.clone()]))
    }

    /// Interactive shell in the data folder (host shell or inside the image).
    pub fn shell_command(&self) -> CommandSpec {
        if self.kind.is_container() {
            let mut args = vec!["run".into(), "--rm".into(), "-it".into(), "--init".into()];
            args.extend(self.user_args());
            args.extend(self.mount_args());
            args.extend([
                "-w".into(),
                CONTAINER_DATA.into(),
                "--entrypoint".into(),
                "/bin/bash".into(),
                self.image.clone(),
            ]);
            CommandSpec::new(self.kind.name(), args)
        } else {
            // The env bmi-driver gets, so its echoed command can be re-run by hand.
            let mut spec = CommandSpec::new(host_shell(), vec![]);
            spec.env = self.local_env();
            spec
        }
    }

    /// Bind-mount the data folder at /data. `--mount` (unlike `-v`) copes with ':' in
    /// paths; on SELinux-enforcing hosts label separation is disabled for the run
    /// rather than relabelling the user's folder.
    fn mount_args(&self) -> Vec<String> {
        let src = strip_verbatim(&self.data_dir.display().to_string());
        let src = if src.contains(',') || src.contains('"') {
            format!("\"source={}\"", src.replace('"', "\"\""))
        } else {
            format!("source={src}")
        };
        let mut v = vec![
            "--mount".into(),
            format!("type=bind,{src},target={CONTAINER_DATA}"),
        ];
        if selinux_enforcing() {
            v.extend(["--security-opt".into(), "label=disable".into()]);
        }
        v
    }

    /// Docker (rootful) runs as root by default which leaves root-owned output files
    /// in the user's folder; run as the invoking user instead.
    fn user_args(&self) -> Vec<String> {
        let rootless = self.detected.runtime(self.kind).is_some_and(|r| r.rootless);
        if self.kind == BackendKind::Docker && !rootless {
            host_user_args()
        } else {
            vec![]
        }
    }

    pub fn image_present(&self) -> Option<bool> {
        if !self.kind.is_container() {
            return None;
        }
        run_with_timeout(
            Command::new(self.kind.name()).args(["image", "inspect", &self.image]),
            Duration::from_secs(10),
        )
        .map(|o| o.status.success())
    }
}

/// Docker Desktop / podman machine want a plain `C:\...` or `\\server\share` path,
/// never a verbatim `\\?\` one. `\\?\UNC\server\share` becomes `\\server\share`.
/// Not cfg-gated so it is tested everywhere; such prefixes never occur on Unix.
fn strip_verbatim(src: &str) -> String {
    if let Some(rest) = src.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = src.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        src.to_string()
    }
}

#[cfg(unix)]
fn host_user_args() -> Vec<String> {
    // SAFETY: getuid/getgid are always successful and have no side effects.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    vec![
        "--user".into(),
        format!("{uid}:{gid}"),
        "-e".into(),
        "HOME=/tmp".into(),
    ]
}

/// Docker Desktop on Windows already maps file ownership; there is no uid to pass.
#[cfg(not(unix))]
fn host_user_args() -> Vec<String> {
    vec![]
}

/// The user's interactive shell on the host.
#[cfg(unix)]
fn host_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if Path::new("/bin/bash").exists() {
                "/bin/bash".into()
            } else {
                "/bin/sh".into()
            }
        })
}

/// The user's interactive shell on the host: `%COMSPEC%`, else PowerShell.
#[cfg(windows)]
fn host_shell() -> String {
    std::env::var("COMSPEC")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "powershell.exe".into())
}

/// The host's glibc version (`gnu_get_libc_version`).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn host_glibc() -> Option<String> {
    // SAFETY: returns a pointer to a static NUL-terminated string.
    let v = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
    Some(v.to_string_lossy().into_owned())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn host_glibc() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce").is_ok_and(|s| s.trim() == "1")
}

#[cfg(not(target_os = "linux"))]
fn selinux_enforcing() -> bool {
    false
}

/// Kill a running container by name (best effort, used on cancel / exit).
pub fn kill_container(runtime: &str, name: &str) {
    let _ = run_with_timeout(
        Command::new(runtime).args(["kill", name]),
        Duration::from_secs(10),
    );
}

fn probe_runtime(bin: &str) -> Option<RuntimeInfo> {
    find_executable(&[bin])?;
    let version = run_with_timeout(Command::new(bin).arg("--version"), Duration::from_secs(5))
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    // "Docker version 27.3.1, build ..." / "podman version 5.0.2"
    let version = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("?")
        .trim_end_matches(',')
        .to_string();

    let info = run_with_timeout(
        Command::new(bin).args(["info", "--format", "{{json .}}"]),
        Duration::from_secs(10),
    );
    match info {
        Some(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).to_lowercase();
            let rootless = if bin == "podman" {
                text.contains("\"rootless\":true") || !running_as_root()
            } else {
                text.contains("rootless")
            };
            Some(RuntimeInfo {
                version,
                usable: true,
                rootless,
                note: None,
            })
        }
        Some(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let note = err
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("not usable")
                .trim();
            Some(RuntimeInfo {
                version,
                usable: false,
                rootless: false,
                note: Some(truncate(note, 80)),
            })
        }
        None => Some(RuntimeInfo {
            version,
            usable: false,
            rootless: false,
            note: Some("daemon did not respond".into()),
        }),
    }
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid is always successful and has no side effects.
    unsafe { libc::geteuid() == 0 }
}

/// No uid 0 on Windows: podman there only reports rootless via `podman info`.
#[cfg(not(unix))]
fn running_as_root() -> bool {
    true
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Search PATH (plus a few common install dirs) for the first of `names`.
pub fn find_executable(names: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo").join("bin"));
    }
    #[cfg(unix)]
    dirs.push("/usr/local/bin".into());
    for name in names {
        for d in &dirs {
            if let Some(p) = executable_in(d, name) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(unix)]
fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    let m = p.metadata().ok()?;
    (m.is_file() && m.permissions().mode() & 0o111 != 0).then_some(p)
}

/// Windows: `name` itself, then `name` + each `PATHEXT` extension (`.EXE`, `.CMD`, ...).
#[cfg(windows)]
fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    std::iter::once(String::new())
        .chain(
            pathext
                .split(';')
                .filter(|e| !e.is_empty())
                .map(str::to_string),
        )
        .map(|ext| dir.join(format!("{name}{ext}")))
        .find(|p| p.is_file())
}

/// Run a command capturing output, giving up after `timeout`.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_verbatim_prefixes() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\me\run"), r"C:\Users\me\run");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\run"),
            r"\\server\share\run"
        );
        assert_eq!(strip_verbatim(r"C:\Users\me\run"), r"C:\Users\me\run");
        assert_eq!(strip_verbatim(r"\\server\share"), r"\\server\share");
        assert_eq!(strip_verbatim("/home/me/run"), "/home/me/run");
    }

    #[test]
    fn local_probe_gating() {
        assert!(probe_local(true, false));
        assert!(probe_local(true, true));
        // arm64 (or other unsupported) host: only inside our own image
        assert!(probe_local(false, true));
        assert!(!probe_local(false, false));
        // a generic container (toolbox/distrobox) is not our image
        assert!(!in_own_image(false));
    }

    fn launcher(kind: BackendKind) -> Launcher {
        Launcher::new(
            kind,
            DEFAULT_IMAGE.into(),
            "/home/me/run 1".into(),
            Detected {
                bmi_driver: Some("/opt/bin/bmi-driver".into()),
                docker: Some(RuntimeInfo {
                    version: "1".into(),
                    usable: true,
                    rootless: true,
                    note: None,
                }),
                model_root: Some("/home/me/.ngiab/container".into()),
                ..Default::default()
            },
        )
    }

    /// Local runs are Unix-only; host paths here are joined with `/`.
    #[cfg(unix)]
    #[test]
    fn local_bmi_driver_env() {
        let l = launcher(BackendKind::Local);
        let c = l.tool_command(Tool::BmiDriver, vec![], false, "x");
        let ld = c.env.iter().find(|(k, _)| k == "LD_LIBRARY_PATH").unwrap();
        assert!(
            ld.1.starts_with("/home/me/.ngiab/container/dmod/shared_libs"),
            "{ld:?}"
        );
        // rs-route needs none of it; container runs never get host env
        assert!(
            l.tool_command(Tool::RsRoute, vec![], false, "x")
                .env
                .is_empty()
        );
        let d = launcher(BackendKind::Docker);
        assert!(
            d.tool_command(Tool::BmiDriver, vec![], false, "x")
                .env
                .is_empty()
        );
        // no bundle: no LD_LIBRARY_PATH
        let mut bare = launcher(BackendKind::Local);
        bare.model_root = None;
        assert!(
            !bare
                .tool_command(Tool::BmiDriver, vec![], false, "x")
                .env
                .iter()
                .any(|(k, _)| k == "LD_LIBRARY_PATH")
        );
    }

    #[test]
    fn local_command() {
        let l = launcher(BackendKind::Local);
        assert_eq!(l.data_root(), "/home/me/run 1");
        let c = l.tool_command(
            Tool::BmiDriver,
            vec![l.data_root(), "-j".into(), "4".into()],
            true,
            "x",
        );
        assert_eq!(c.program, "/opt/bin/bmi-driver");
        assert_eq!(c.args, vec!["/home/me/run 1", "-j", "4"]);
        assert!(c.container.is_none());
    }

    #[test]
    fn docker_command() {
        let l = launcher(BackendKind::Docker);
        let c = l.tool_command(Tool::RsRoute, vec![l.data_root()], true, "route");
        assert_eq!(c.program, "docker");
        let joined = c.args.join(" ");
        assert!(joined.contains("--mount type=bind,source=/home/me/run 1,target=/data"));
        assert!(c.args.contains(&"--init".to_string()));
        assert!(joined.contains(&format!(
            "--entrypoint /usr/local/bin/rs-route docker.io/joshcu/ngiab-rs:{} /data",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(c.args.contains(&"-t".to_string()));
        // rootless docker -> no --user mapping
        assert!(!c.args.contains(&"--user".to_string()));
        assert!(c.container.is_some());
    }

    #[test]
    fn display_prints_env_first_and_quotes() {
        let mut c = CommandSpec::new(
            "/opt/bin/bmi-driver",
            vec![
                "/home/me/run 1".into(),
                "-j".into(),
                "4".into(),
                "it's".into(),
            ],
        );
        assert_eq!(
            c.display(),
            r"/opt/bin/bmi-driver '/home/me/run 1' -j 4 'it'\''s'"
        );
        c.env = vec![
            ("LD_LIBRARY_PATH".into(), "/home/me/.ngiab/x y".into()),
            ("OMP_NUM_THREADS".into(), "1".into()),
        ];
        assert_eq!(
            c.display(),
            "LD_LIBRARY_PATH='/home/me/.ngiab/x y' OMP_NUM_THREADS=1 \
             /opt/bin/bmi-driver '/home/me/run 1' -j 4 'it'\\''s'"
        );
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn default_image_is_pinned_to_crate_version() {
        assert_eq!(
            DEFAULT_IMAGE,
            format!("docker.io/joshcu/ngiab-rs:{}", env!("CARGO_PKG_VERSION"))
        );
        assert!(!DEFAULT_IMAGE.ends_with(":latest"));
    }

    #[cfg(unix)]
    #[test]
    fn to_command_applies_env() {
        let mut c = CommandSpec::new("sh", vec!["-c".into(), "printf %s \"$NGIAB_Y\"".into()]);
        c.env.push(("NGIAB_Y".into(), "ok".into()));
        let out = c.to_command().output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    }

    /// The Dockerfile's build-arg defaults must match TOOL_VERSIONS, and the base
    /// image must be pinned to a vX.Y.Z tag (models.yml reads it with sed).
    #[test]
    fn dockerfile_pins_match_tool_versions() {
        let dockerfile = include_str!("../Dockerfile");
        let versions = include_str!("../TOOL_VERSIONS");
        let arg = |key: &str| {
            dockerfile
                .lines()
                .find_map(|l| l.strip_prefix(&format!("ARG {key}=")))
                .unwrap_or_else(|| panic!("Dockerfile has no ARG {key}="))
                .trim()
                .to_string()
        };
        let mut n = 0;
        for line in versions.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line.split_once('=').expect("KEY=value");
            assert_eq!(arg(k), v, "Dockerfile ARG {k} differs from TOOL_VERSIONS");
            n += 1;
        }
        assert_eq!(n, 2);
        let base = arg("NGIAB_BASE");
        let tag = base.strip_prefix("awiciroh/ngiab:v").expect(&base);
        assert_eq!(
            tag.split('.').filter(|p| p.parse::<u32>().is_ok()).count(),
            3,
            "{base}"
        );
    }

    #[test]
    fn default_preference() {
        let mut d = Detected::default();
        assert_eq!(d.default_kind(), None);
        d.podman = Some(RuntimeInfo {
            version: "5".into(),
            usable: true,
            rootless: true,
            note: None,
        });
        assert_eq!(d.default_kind(), Some(BackendKind::Podman));
        d.docker = Some(RuntimeInfo {
            version: "27".into(),
            usable: true,
            rootless: false,
            note: None,
        });
        assert_eq!(d.default_kind(), Some(BackendKind::Docker));
        // bmi-driver alone (no rs-route, no bundle) falls through to docker
        d.local_platform = true;
        d.bmi_driver = Some("/x".into());
        assert_eq!(d.default_kind(), Some(BackendKind::Docker));
        d.rs_route = Some("/y".into());
        assert_eq!(d.default_kind(), Some(BackendKind::Docker));
        d.model_root = Some("/b".into());
        d.models = Some(Manifest::default());
        assert_eq!(d.default_kind(), Some(BackendKind::Local));
    }

    fn local_ready() -> Detected {
        Detected {
            local_platform: true,
            bmi_driver: Some("/x".into()),
            rs_route: Some("/y".into()),
            model_root: Some("/b".into()),
            models: Some(Manifest {
                min_glibc: "2.34".into(),
                ..Default::default()
            }),
            glibc: Some("2.39".into()),
            ..Default::default()
        }
    }

    #[test]
    fn local_availability_reasons() {
        let ok = local_ready();
        assert_eq!(ok.availability(BackendKind::Local), Availability::Available);

        let d = Detected {
            local_platform: false,
            ..local_ready()
        };
        let a = d.availability(BackendKind::Local);
        assert!(matches!(a, Availability::Unsupported(_)), "{a:?}");
        assert!(
            a.reason()
                .unwrap()
                .contains("not supported on this platform")
        );

        let d = Detected {
            glibc: Some("2.31".into()),
            ..local_ready()
        };
        let a = d.availability(BackendKind::Local);
        assert!(matches!(a, Availability::Unsupported(_)), "{a:?}");
        assert!(a.reason().unwrap().contains("glibc too old"));

        let d = Detected {
            model_root: None,
            models: None,
            rs_route: None,
            ..local_ready()
        };
        let a = d.availability(BackendKind::Local);
        assert!(matches!(a, Availability::NotInstalled(_)), "{a:?}");
        let r = a.reason().unwrap();
        assert!(r.contains("rs-route") && r.contains("model files"), "{r}");
        assert!(r.contains("ngiab-rs models update"), "{r}");
        assert!(!r.contains("bmi-driver"), "{r}");

        let d = Detected {
            models: Some(Manifest {
                target: "aarch64-unknown-linux-gnu".into(),
                ..Default::default()
            }),
            ..local_ready()
        };
        let a = d.availability(BackendKind::Local);
        assert!(a.reason().unwrap().contains("aarch64"), "{a:?}");

        let d = Detected {
            models: None,
            models_error: Some("bad json".into()),
            ..local_ready()
        };
        let a = d.availability(BackendKind::Local);
        assert!(a.reason().unwrap().contains("bad json"), "{a:?}");

        // our own image: /dmod is real, no bundle needed
        let d = Detected {
            own_image: true,
            in_container: true,
            model_root: Some("/".into()),
            models: None,
            glibc: Some("2.1".into()),
            ..local_ready()
        };
        assert_eq!(d.availability(BackendKind::Local), Availability::Available);
        assert!(matches!(
            d.availability(BackendKind::Docker),
            Availability::Unsupported(_)
        ));
    }

    #[test]
    fn runtime_availability() {
        let mut d = Detected::default();
        assert!(matches!(
            d.availability(BackendKind::Docker),
            Availability::NotInstalled(_)
        ));
        d.docker = Some(RuntimeInfo {
            version: "27".into(),
            usable: false,
            rootless: false,
            note: Some("Cannot connect to the Docker daemon".into()),
        });
        let a = d.availability(BackendKind::Docker);
        assert!(matches!(a, Availability::NotRunning(_)), "{a:?}");
        assert!(a.reason().unwrap().contains("Cannot connect"));
        assert!(!d.available(BackendKind::Docker));
    }
}
