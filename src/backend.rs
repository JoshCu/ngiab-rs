//! Detection of execution backends (local binaries, docker, podman) and
//! construction of the command lines used to run each tool.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Mount point of the data folder inside the container.
pub const CONTAINER_DATA: &str = "/data";

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

#[derive(Debug, Clone, Default)]
pub struct Detected {
    pub bmi_driver: Option<PathBuf>,
    pub rs_route: Option<PathBuf>,
    pub docker: Option<RuntimeInfo>,
    pub podman: Option<RuntimeInfo>,
    /// We are ourselves running inside a container.
    pub in_container: bool,
}

impl Detected {
    pub fn detect() -> Self {
        let in_container = Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || std::env::var_os("container").is_some();
        let bmi_driver = find_executable(&["bmi-driver"]);
        let rs_route = find_executable(&["rs-route", "rs_route"]);
        // Inside a container we never try to nest another container.
        let (docker, podman) = if in_container {
            (None, None)
        } else {
            (probe_runtime("docker"), probe_runtime("podman"))
        };
        Self {
            bmi_driver,
            rs_route,
            docker,
            podman,
            in_container,
        }
    }

    pub fn runtime(&self, kind: BackendKind) -> Option<&RuntimeInfo> {
        match kind {
            BackendKind::Local => None,
            BackendKind::Docker => self.docker.as_ref(),
            BackendKind::Podman => self.podman.as_ref(),
        }
    }

    /// Can this backend run (at least) bmi-driver?
    pub fn available(&self, kind: BackendKind) -> bool {
        match kind {
            BackendKind::Local => self.bmi_driver.is_some(),
            _ => self.runtime(kind).is_some_and(|r| r.usable),
        }
    }

    /// Preference: local > docker > podman.
    pub fn default_kind(&self) -> Option<BackendKind> {
        BackendKind::ALL.into_iter().find(|k| self.available(*k))
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
                format!("bmi-driver: {b}, rs-route: {r}")
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
    /// For container runs: (runtime, container name) so the run can be killed.
    pub container: Option<(String, String)>,
}

impl CommandSpec {
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') {
                s.push_str(&format!("'{a}'"));
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

/// Everything needed to turn a tool invocation into a command line.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub kind: BackendKind,
    pub image: String,
    pub data_dir: PathBuf,
    pub detected: Detected,
}

impl Launcher {
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
            BackendKind::Local => CommandSpec {
                program: self
                    .detected
                    .local_tool(tool)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| tool.binary().to_string()),
                args: tool_args,
                container: None,
            },
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
                    program: runtime.clone(),
                    args,
                    container: Some((runtime, name)),
                }
            }
        }
    }

    pub fn pull_command(&self) -> Option<CommandSpec> {
        self.kind.is_container().then(|| CommandSpec {
            program: self.kind.name().into(),
            args: vec!["pull".into(), self.image.clone()],
            container: None,
        })
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
            CommandSpec {
                program: self.kind.name().into(),
                args,
                container: None,
            }
        } else {
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if Path::new("/bin/bash").exists() {
                        "/bin/bash".into()
                    } else {
                        "/bin/sh".into()
                    }
                });
            CommandSpec {
                program: shell,
                args: vec![],
                container: None,
            }
        }
    }

    /// Bind-mount the data folder at /data. `--mount` (unlike `-v`) copes with ':' in
    /// paths; on SELinux-enforcing hosts label separation is disabled for the run
    /// rather than relabelling the user's folder.
    fn mount_args(&self) -> Vec<String> {
        let src = self.data_dir.display().to_string();
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
            // SAFETY: getuid/getgid are always successful and have no side effects.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            vec![
                "--user".into(),
                format!("{uid}:{gid}"),
                "-e".into(),
                "HOME=/tmp".into(),
            ]
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

fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce").is_ok_and(|s| s.trim() == "1")
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
                text.contains("\"rootless\":true") || unsafe { libc::geteuid() } != 0
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

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Search PATH (plus a few common install dirs) for the first of `names`.
pub fn find_executable(names: &[&str]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push("/usr/local/bin".into());
    for name in names {
        for d in &dirs {
            let p = d.join(name);
            if let Ok(m) = p.metadata()
                && m.is_file()
                && m.permissions().mode() & 0o111 != 0
            {
                return Some(p);
            }
        }
    }
    None
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

    fn launcher(kind: BackendKind) -> Launcher {
        Launcher {
            kind,
            image: "docker.io/joshcu/ngiab-rs:latest".into(),
            data_dir: "/home/me/run 1".into(),
            detected: Detected {
                bmi_driver: Some("/opt/bin/bmi-driver".into()),
                docker: Some(RuntimeInfo {
                    version: "1".into(),
                    usable: true,
                    rootless: true,
                    note: None,
                }),
                ..Default::default()
            },
        }
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
        assert!(joined.contains(
            "--entrypoint /usr/local/bin/rs-route docker.io/joshcu/ngiab-rs:latest /data"
        ));
        assert!(c.args.contains(&"-t".to_string()));
        // rootless docker -> no --user mapping
        assert!(!c.args.contains(&"--user".to_string()));
        assert!(c.container.is_some());
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
        d.bmi_driver = Some("/x".into());
        assert_eq!(d.default_kind(), Some(BackendKind::Local));
    }
}
