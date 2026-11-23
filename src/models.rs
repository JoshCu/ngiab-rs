//! `ngiab-rs models update | revert | status` (tasks.md B.4). Not implemented yet:
//! the subcommands are wired up so the CLI shape is final, and a later step fills
//! these in.

use crate::cli::ModelsUpdateArgs;
use std::process::ExitCode;

fn not_implemented(what: &str) -> anyhow::Result<ExitCode> {
    anyhow::bail!("`ngiab-rs models {what}` is not implemented yet")
}

/// `models update [--version R] [--file TAR] [--check] [--force] [-y]`.
pub fn update(_args: &ModelsUpdateArgs) -> anyhow::Result<ExitCode> {
    not_implemented("update")
}

/// `models revert [-y]`.
pub fn revert(_yes: bool) -> anyhow::Result<ExitCode> {
    not_implemented("revert")
}

/// `models status`.
pub fn status() -> anyhow::Result<ExitCode> {
    not_implemented("status")
}
