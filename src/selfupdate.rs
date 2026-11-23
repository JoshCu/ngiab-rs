//! `ngiab-rs self update` (tasks.md B.3). Not implemented yet: the subcommand is
//! wired up so the CLI shape is final, and a later step fills this in.

use crate::cli::SelfUpdateArgs;
use std::process::ExitCode;

/// `self update [--version V] [--check] [-y]`.
pub fn update(_args: &SelfUpdateArgs) -> anyhow::Result<ExitCode> {
    anyhow::bail!(
        "`ngiab-rs self update` is not implemented yet; reinstall with the installer or `cargo install` instead"
    )
}
