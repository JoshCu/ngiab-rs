//! What to run, in which order, with which arguments.

use crate::backend::{BackendKind, CommandSpec, Launcher, Tool};
use crate::config::{Realization, TROUTE_OUTPUT_DIR};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Pull,
    /// Local runs: write the realization (and configs) with host paths (stage.rs).
    Stage,
    Tool(Tool),
}

impl StepKind {
    pub fn label(self) -> &'static str {
        match self {
            StepKind::Pull => "pull image",
            StepKind::Stage => "stage configs",
            StepKind::Tool(Tool::BmiDriver) => "bmi-driver",
            StepKind::Tool(Tool::RsRoute) => "rs-route",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub jobs: usize,
    pub routing: bool,
}

/// Arguments for `tool`, using `root` as the data folder path the tool will see.
/// `route_input` is rs-route's `-i` (bmi-driver's output_root as the tool sees it);
/// `staged` is a staged realization for bmi-driver's `--config` (local runs).
pub fn tool_args(
    tool: Tool,
    root: &str,
    jobs: usize,
    route_input: Option<String>,
    staged: Option<&str>,
) -> Vec<String> {
    let join = |rel: &str| format!("{}/{}", root.trim_end_matches('/'), rel);
    match tool {
        Tool::BmiDriver => {
            let mut a = vec![root.to_string()];
            if let Some(s) = staged {
                a.extend(["--config".into(), s.to_string()]);
            }
            a.extend(["-j".into(), jobs.to_string()]);
            a
        }
        Tool::RsRoute => {
            let mut a = vec![root.to_string(), "-n".into(), jobs.to_string()];
            // bmi-driver writes to realization.output_root (default outputs/bmi-driver)
            // while rs-route reads outputs/ngen by default, so always be explicit.
            if let Some(input) = route_input {
                a.extend(["-i".into(), input]);
            }
            a.extend(["-o".into(), join(TROUTE_OUTPUT_DIR)]);
            a
        }
    }
}

/// rs-route's `-i`: the realization's output_root as the tool sees it. Containers
/// take absolute paths as they are (they are container paths) and relative ones
/// under `/data`; local runs map it through `PathMap` (`/data/...` and
/// `/ngen/ngen/data/...` are the data folder).
pub fn route_input(launcher: &Launcher, real: &Realization) -> String {
    let root = launcher.data_root();
    if launcher.kind == BackendKind::Local {
        if let Some(p) = launcher.path_map().resolve(&real.output_root).host() {
            return p.display().to_string();
        }
    } else if real.output_root.starts_with('/') {
        return real.output_root.clone();
    }
    format!("{}/{}", root.trim_end_matches('/'), real.output_root)
}

/// The command for a step. `None` for steps that are not commands (`Stage`) or don't
/// apply (`Pull` on local). `staged` is the staged realization, if staging wrote one.
pub fn command_for(
    kind: StepKind,
    launcher: &Launcher,
    opts: &RunOptions,
    real: Option<&Realization>,
    staged: Option<&Path>,
    tty: bool,
) -> Option<CommandSpec> {
    match kind {
        StepKind::Pull => launcher.pull_command(),
        StepKind::Stage => None,
        StepKind::Tool(tool) => {
            // tool_args joins with '/', which is only right for container roots
            // (local is Linux-only, see backend::LOCAL_SUPPORTED).
            debug_assert!(launcher.kind.is_container() || cfg!(unix));
            let input = real.map(|r| route_input(launcher, r));
            let staged = staged
                .filter(|_| launcher.kind == BackendKind::Local)
                .map(|p| p.display().to_string());
            let args = tool_args(
                tool,
                &launcher.data_root(),
                opts.jobs,
                input,
                staged.as_deref(),
            );
            let tag = match tool {
                Tool::BmiDriver => "bmi",
                Tool::RsRoute => "route",
            };
            Some(launcher.tool_command(tool, args, tty, tag))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backend::{DEFAULT_IMAGE, Detected};

    fn real(output_root: &str) -> Realization {
        Realization {
            start_time: String::new(),
            end_time: String::new(),
            output_interval: 3600,
            output_root: output_root.into(),
            output_format: "csv".into(),
            forcing_path: None,
            modules: vec![],
        }
    }

    #[test]
    fn route_args_use_output_root() {
        let docker = Launcher::new(
            BackendKind::Docker,
            DEFAULT_IMAGE.into(),
            "/home/me/run".into(),
            Detected::default(),
        );
        let input = route_input(&docker, &real("outputs/ngen"));
        let a = tool_args(Tool::RsRoute, "/data", 8, Some(input), None);
        assert_eq!(
            a,
            vec![
                "/data",
                "-n",
                "8",
                "-i",
                "/data/outputs/ngen",
                "-o",
                "/data/outputs/troute"
            ]
        );
        let b = tool_args(Tool::BmiDriver, "/data/", 3, None, None);
        assert_eq!(b, vec!["/data/", "-j", "3"]);
        assert_eq!(
            route_input(&docker, &real("/ngen/ngen/data/outputs/ngen")),
            "/ngen/ngen/data/outputs/ngen"
        );
    }

    #[test]
    fn local_route_input_and_staged_config() {
        let local = Launcher::new(
            BackendKind::Local,
            DEFAULT_IMAGE.into(),
            "/home/me/my run".into(),
            Detected::default(),
        );
        for root in [
            "/ngen/ngen/data/outputs/ngen",
            "/data/outputs/ngen",
            "outputs/ngen",
        ] {
            assert_eq!(
                route_input(&local, &real(root)),
                "/home/me/my run/outputs/ngen"
            );
        }
        assert_eq!(route_input(&local, &real("/scratch/out")), "/scratch/out");
        let b = tool_args(
            Tool::BmiDriver,
            "/home/me/my run",
            4,
            None,
            Some("/home/me/my run/.ngiab-rs/stage/realization.json"),
        );
        assert_eq!(
            b,
            vec![
                "/home/me/my run",
                "--config",
                "/home/me/my run/.ngiab-rs/stage/realization.json",
                "-j",
                "4"
            ]
        );
    }
}
