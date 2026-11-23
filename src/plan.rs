//! What to run, in which order, with which arguments.

use crate::backend::{CommandSpec, Launcher, Tool};
use crate::config::{Realization, TROUTE_OUTPUT_DIR};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Pull,
    Tool(Tool),
}

impl StepKind {
    pub fn label(self) -> &'static str {
        match self {
            StepKind::Pull => "pull image",
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
pub fn tool_args(tool: Tool, root: &str, jobs: usize, real: Option<&Realization>) -> Vec<String> {
    let join = |rel: &str| format!("{}/{}", root.trim_end_matches('/'), rel);
    match tool {
        Tool::BmiDriver => vec![root.to_string(), "-j".into(), jobs.to_string()],
        Tool::RsRoute => {
            let mut a = vec![root.to_string(), "-n".into(), jobs.to_string()];
            // bmi-driver writes to realization.output_root (default outputs/bmi-driver)
            // while rs-route reads outputs/ngen by default, so always be explicit.
            if let Some(r) = real {
                let input = if r.output_root.starts_with('/') {
                    r.output_root.clone()
                } else {
                    join(&r.output_root)
                };
                a.extend(["-i".into(), input]);
            }
            a.extend(["-o".into(), join(TROUTE_OUTPUT_DIR)]);
            a
        }
    }
}

pub fn command_for(
    kind: StepKind,
    launcher: &Launcher,
    opts: &RunOptions,
    real: Option<&Realization>,
    tty: bool,
) -> Option<CommandSpec> {
    match kind {
        StepKind::Pull => launcher.pull_command(),
        StepKind::Tool(tool) => {
            let args = tool_args(tool, &launcher.data_root(), opts.jobs, real);
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

    #[test]
    fn route_args_use_output_root() {
        let real = Realization {
            start_time: String::new(),
            end_time: String::new(),
            output_interval: 3600,
            output_root: "outputs/ngen".into(),
            output_format: "csv".into(),
            forcing_path: None,
            modules: vec![],
        };
        let a = tool_args(Tool::RsRoute, "/data", 8, Some(&real));
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
        let b = tool_args(Tool::BmiDriver, "/data/", 3, Some(&real));
        assert_eq!(b, vec!["/data/", "-j", "3"]);
    }
}
