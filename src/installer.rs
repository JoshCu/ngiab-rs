//! `ngiab-rs backend list | install | record` (tasks.md B.7). Not implemented yet:
//! the subcommands are wired up so the CLI shape is final, and a later step fills
//! these in.

use crate::backend::BackendKind;
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

/// `backend record <kinds…>` (hidden; called by the installers).
pub fn record(_kinds: &[BackendKind]) -> anyhow::Result<ExitCode> {
    not_implemented("record")
}
