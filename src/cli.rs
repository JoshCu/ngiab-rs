//! Command-line interface: the default action (run / TUI) plus the `self`,
//! `models`, `backend` and `run` subcommands.
//!
//! `args_conflicts_with_subcommands` keeps `ngiab-rs DATA_DIR [run flags]` working:
//! the top-level arguments are the run arguments, and a subcommand is only
//! recognised in first position. Every run flag lives on [`RunArgs`].

use crate::backend::{self, BackendKind};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

const AFTER_HELP: &str = "\
A data folder named like a subcommand (self, models, backend, run) is parsed as that
subcommand. Write it as a path or use the explicit `run` subcommand instead:
  ngiab-rs ./models
  ngiab-rs run models";

/// NextGen In A Box (Rust): run bmi-driver + rs-route and inspect the results.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    args_conflicts_with_subcommands = true,
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Manage the ngiab-rs binary itself
    #[command(name = "self")]
    SelfManage {
        #[command(subcommand)]
        cmd: SelfCmd,
    },
    /// Manage the model files used by the local backend (Linux x86_64)
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Inspect and install execution backends (local, docker, podman)
    Backend {
        #[command(subcommand)]
        cmd: BackendCmd,
    },
    /// Run on a data folder (the default when no subcommand is given)
    #[command(after_help = AFTER_HELP)]
    Run(RunArgs),
}

/// Everything the default action (TUI or `--auto`) accepts. New run flags go here.
#[derive(Args, Debug, Clone, PartialEq)]
pub struct RunArgs {
    /// Data folder containing config/, forcings/ and outputs/
    pub path: Option<PathBuf>,

    /// Non-interactive: run preflight checks, bmi-driver, then rs-route and exit
    #[arg(long)]
    pub auto: bool,

    /// Number of CPUs to use (bmi-driver -j / rs-route -n). Defaults to all CPUs.
    #[arg(
        short = 'j',
        long = "jobs",
        visible_alias = "num-cpus",
        value_name = "N"
    )]
    pub jobs: Option<usize>,

    /// Where to run the tools. Default: local binaries > docker > podman
    #[arg(short, long, value_enum, env = "NGIAB_RS_BACKEND")]
    pub backend: Option<BackendKind>,

    /// Container image used by the docker/podman backends
    #[arg(long, env = "NGIAB_RS_IMAGE", default_value = backend::DEFAULT_IMAGE)]
    pub image: String,

    /// Only run bmi-driver (auto mode)
    #[arg(long)]
    pub no_routing: bool,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum SelfCmd {
    /// Update ngiab-rs to the newest (or a given) release
    Update(SelfUpdateArgs),
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct SelfUpdateArgs {
    /// Release to install, with or without a leading `v` (default: newest)
    #[arg(long, value_name = "V")]
    pub version: Option<String>,

    /// Only check: exit 0 when up to date, 10 when an update is available
    #[arg(long)]
    pub check: bool,

    /// Don't ask for confirmation (e.g. for a downgrade)
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum ModelsCmd {
    /// Download and install the newest (or a given) model bundle
    Update(ModelsUpdateArgs),
    /// Swap the installed bundle with the previous one (running it twice re-applies the update)
    Revert {
        /// Don't ask for confirmation
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Show the installed, backup and newest available model bundles
    Status,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct ModelsUpdateArgs {
    /// Release to install: `models-YYYY.MM.DD[.N]` or just `YYYY.MM.DD[.N]` (default: newest)
    #[arg(long, value_name = "RELEASE")]
    pub version: Option<String>,

    /// Install from a local tarball instead of downloading
    #[arg(long, value_name = "TAR")]
    pub file: Option<PathBuf>,

    /// Only report whether an update is available
    #[arg(long)]
    pub check: bool,

    /// Reinstall even when the installed bundle is the same
    #[arg(long)]
    pub force: bool,

    /// Don't ask for confirmation
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
}

#[derive(Subcommand, Debug, PartialEq)]
pub enum BackendCmd {
    /// Show every backend, whether it can be used here, and the model bundle status
    List,
    /// Run the installer (pinned to this version) to set up the given backends
    Install {
        #[arg(required = true, value_enum, value_name = "BACKEND")]
        kinds: Vec<BackendKind>,
    },
    /// Record the installed backends in install.json (used by the installers)
    #[command(hide = true)]
    Record {
        #[arg(required = true, value_enum, value_name = "BACKEND")]
        kinds: Vec<BackendKind>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        let argv = std::iter::once("ngiab-rs").chain(args.iter().copied());
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    /// The run args of a parse that must not have picked a subcommand.
    fn run_args(args: &[&str]) -> RunArgs {
        let cli = parse(args);
        assert!(cli.cmd.is_none(), "{args:?} parsed as {:?}", cli.cmd);
        cli.run
    }

    #[test]
    fn debug_assert() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_args_is_the_default_action() {
        let r = run_args(&[]);
        assert_eq!(r.path, None);
        assert!(!r.auto);
        assert_eq!(r.jobs, None);
        assert!(!r.no_routing);
    }

    #[test]
    fn positional_path() {
        assert_eq!(run_args(&["/x"]).path, Some(PathBuf::from("/x")));
    }

    #[test]
    fn path_then_flags() {
        let r = run_args(&["/x", "--auto", "-j", "4"]);
        assert_eq!(r.path, Some(PathBuf::from("/x")));
        assert!(r.auto);
        assert_eq!(r.jobs, Some(4));
    }

    #[test]
    fn flags_then_path() {
        let r = run_args(&["--auto", "/x"]);
        assert_eq!(r.path, Some(PathBuf::from("/x")));
        assert!(r.auto);
    }

    #[test]
    fn docker_cmd_and_headless_invocation() {
        // Dockerfile CMD ["/data"] and `docker run ... /data --auto -j 8`.
        assert_eq!(run_args(&["/data"]).path, Some(PathBuf::from("/data")));
        let r = run_args(&["/data", "--auto", "-j", "8"]);
        assert_eq!(r.path, Some(PathBuf::from("/data")));
        assert!(r.auto);
        assert_eq!(r.jobs, Some(8));
        let r = run_args(&["/data", "--num-cpus", "2", "--no-routing"]);
        assert_eq!(r.jobs, Some(2));
        assert!(r.no_routing);
    }

    #[test]
    fn self_update_check() {
        let cli = parse(&["self", "update", "--check"]);
        let Some(Cmd::SelfManage {
            cmd: SelfCmd::Update(a),
        }) = cli.cmd
        else {
            panic!("{:?}", cli.cmd)
        };
        assert!(a.check);
        assert!(!a.yes);
        assert_eq!(a.version, None);

        let cli = parse(&["self", "update", "--version", "v0.2.0", "-y"]);
        let Some(Cmd::SelfManage {
            cmd: SelfCmd::Update(a),
        }) = cli.cmd
        else {
            panic!("{:?}", cli.cmd)
        };
        assert_eq!(a.version.as_deref(), Some("v0.2.0"));
        assert!(a.yes);
    }

    #[test]
    fn models_subcommands() {
        let cli = parse(&["models", "revert"]);
        assert_eq!(
            cli.cmd.map(|c| match c {
                Cmd::Models { cmd } => cmd,
                other => panic!("{other:?}"),
            }),
            Some(ModelsCmd::Revert { yes: false })
        );

        let cli = parse(&["models", "status"]);
        assert!(matches!(
            cli.cmd,
            Some(Cmd::Models {
                cmd: ModelsCmd::Status
            })
        ));

        let cli = parse(&[
            "models",
            "update",
            "--version",
            "2026.11.23",
            "--file",
            "b.tar.gz",
            "--force",
        ]);
        let Some(Cmd::Models {
            cmd: ModelsCmd::Update(a),
        }) = cli.cmd
        else {
            panic!("{:?}", cli.cmd)
        };
        assert_eq!(a.version.as_deref(), Some("2026.11.23"));
        assert_eq!(a.file, Some(PathBuf::from("b.tar.gz")));
        assert!(a.force && !a.check && !a.yes);
    }

    #[test]
    fn folder_named_like_a_subcommand() {
        // `./models` is a path, not the `models` subcommand.
        assert_eq!(
            run_args(&["./models"]).path,
            Some(PathBuf::from("./models"))
        );

        let cli = parse(&["run", "models"]);
        let Some(Cmd::Run(r)) = cli.cmd else {
            panic!("{:?}", cli.cmd)
        };
        assert_eq!(r.path, Some(PathBuf::from("models")));

        let cli = parse(&["run", "/x", "--auto", "-j", "3"]);
        let Some(Cmd::Run(r)) = cli.cmd else {
            panic!("{:?}", cli.cmd)
        };
        assert_eq!(r.path, Some(PathBuf::from("/x")));
        assert!(r.auto);
        assert_eq!(r.jobs, Some(3));
    }

    #[test]
    fn backend_install_kinds() {
        let cli = parse(&["backend", "install", "docker", "podman"]);
        assert!(matches!(
            cli.cmd,
            Some(Cmd::Backend { cmd: BackendCmd::Install { ref kinds } })
                if kinds == &[BackendKind::Docker, BackendKind::Podman]
        ));
        assert!(matches!(
            parse(&["backend", "list"]).cmd,
            Some(Cmd::Backend {
                cmd: BackendCmd::List
            })
        ));
        assert!(matches!(
            parse(&["backend", "record", "local"]).cmd,
            Some(Cmd::Backend { cmd: BackendCmd::Record { ref kinds } })
                if kinds == &[BackendKind::Local]
        ));
    }

    #[test]
    fn rejects_bad_input() {
        let bad: &[&[&str]] = &[
            // Run flags don't combine with a subcommand.
            &["--auto", "models", "status"],
            &["backend", "install"],
            &["backend", "install", "kubernetes"],
            &["models"],
            &["self"],
            &["/x", "/y"],
        ];
        for args in bad {
            let argv = std::iter::once("ngiab-rs").chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{args:?} should fail");
        }
    }

    #[test]
    fn help_mentions_folders_named_like_subcommands() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("ngiab-rs ./models"), "{help}");
        assert!(help.contains("ngiab-rs run models"), "{help}");
    }
}
