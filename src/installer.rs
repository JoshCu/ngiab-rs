//! `ngiab-rs backend list | install | record` (tasks.md B.7). `list` and `install`
//! are not implemented yet: they are wired up so the CLI shape is final, and a later
//! step fills them in. `record` is used by the installers.

use crate::backend::BackendKind;
use crate::home::InstallInfo;
use std::process::ExitCode;

fn not_implemented(what: &str) -> anyhow::Result<ExitCode> {
    anyhow::bail!("`ngiab-rs backend {what}` is not implemented yet")
}

/// `backend list`.
pub fn list() -> anyhow::Result<ExitCode> {
    not_implemented("list")
}

/// `backend install <kinds…>`.
pub fn install(_kinds: &[BackendKind]) -> anyhow::Result<ExitCode> {
    not_implemented("install")
}

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
