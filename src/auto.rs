//! `--auto`: preflight, then bmi-driver, then rs-route, no menus.
//! The tools inherit our terminal so their own progress bars are shown directly.

use crate::backend::{Launcher, Tool};
use crate::plan::{self, RunOptions, StepKind};
use crate::preflight::{self, Level};
use std::io::IsTerminal;
use std::process::{Command, ExitCode};
use std::time::Instant;

fn color(level: Level) -> &'static str {
    match level {
        Level::Ok => "\x1b[32m",
        Level::Info => "\x1b[36m",
        Level::Warn => "\x1b[33m",
        Level::Fail => "\x1b[1;31m",
    }
}

pub fn run(launcher: Launcher, opts: RunOptions) -> ExitCode {
    let tty = std::io::stderr().is_terminal();
    let (bold, reset) = if tty { ("\x1b[1m", "\x1b[0m") } else { ("", "") };
    eprintln!("{bold}ngiab-rs auto mode{reset}  data={}  backend={}  jobs={}",
        launcher.data_dir.display(), launcher.kind.name(), opts.jobs);

    let pf = preflight::run(&launcher, opts.routing);
    for c in &pf.checks {
        let (c0, c1) = if tty { (color(c.level), reset) } else { ("", "") };
        eprintln!("  {c0}{}{c1} {:<18} {}", c.level.symbol(), c.name, c.detail);
    }
    if !pf.can_run() {
        eprintln!("Preflight failed, not running.");
        return ExitCode::FAILURE;
    }
    let real = pf.realization.as_ref();
    if let Err(e) = preflight::ensure_output_dirs(&launcher.data_dir, pf.output_dir.as_deref()) {
        eprintln!("Cannot create output folders: {e}");
        return ExitCode::FAILURE;
    }

    let mut steps = vec![StepKind::Tool(Tool::BmiDriver)];
    if opts.routing {
        if real.is_some_and(|r| !r.is_csv()) {
            eprintln!("Skipping rs-route: realization output_format is not csv.");
        } else if !launcher.kind.is_container() && launcher.detected.rs_route.is_none() {
            eprintln!("Skipping rs-route: binary not found.");
        } else {
            steps.push(StepKind::Tool(Tool::RsRoute));
        }
    }

    let total = Instant::now();
    let stdin_tty = std::io::stdin().is_terminal();
    for kind in steps {
        let Some(spec) = plan::command_for(kind, &launcher, &opts, real, stdin_tty && tty) else { continue };
        eprintln!("\n{bold}▶ {}{reset}  {}", kind.label(), spec.display());
        let start = Instant::now();
        let status = Command::new(&spec.program).args(&spec.args).current_dir(&launcher.data_dir).status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("{bold}✓ {}{reset} finished in {:.1}s", kind.label(), start.elapsed().as_secs_f64())
            }
            Ok(s) => {
                eprintln!("✗ {} failed ({s})", kind.label());
                return ExitCode::from(s.code().unwrap_or(1).clamp(1, 255) as u8);
            }
            Err(e) => {
                eprintln!("✗ could not start {}: {e}", spec.program);
                return ExitCode::FAILURE;
            }
        }
    }
    eprintln!("\n{bold}Run completed in {:.1}s.{reset} Outputs are in {}/outputs",
        total.elapsed().as_secs_f64(), launcher.data_dir.display());
    ExitCode::SUCCESS
}
