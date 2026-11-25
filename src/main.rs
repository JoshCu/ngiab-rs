mod app;
mod auto;
mod backend;
mod cli;
mod config;
mod eval;
mod home;
mod installer;
mod models;
mod net;
mod paths;
mod plan;
mod preflight;
mod pty;
mod selfupdate;
mod stage;
mod ui;

use app::App;
use backend::{Availability, BackendKind, Detected, Launcher};
use clap::Parser;
use cli::{BackendCmd, Cli, Cmd, ModelsCmd, RunArgs, SelfCmd};
use ratatui::crossterm::event::{self, Event};
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() -> ExitCode {
    // Windows: leftovers of an earlier `self update` (no-op elsewhere).
    selfupdate::cleanup_stale_binaries();
    let cli = Cli::parse();
    // `self` and `models` must not run `Detected::detect()`: the docker/podman
    // probes can take seconds.
    let res = match cli.cmd {
        None => return run_main(cli.run),
        Some(Cmd::Run(run)) => return run_main(run),
        Some(Cmd::SelfManage {
            cmd: SelfCmd::Update(a),
        }) => selfupdate::update(&a),
        Some(Cmd::Models { cmd }) => match cmd {
            ModelsCmd::Update(a) => models::update(&a),
            ModelsCmd::Revert { yes } => models::revert(yes),
            ModelsCmd::Status => models::status(),
        },
        Some(Cmd::Backend { cmd }) => match cmd {
            BackendCmd::List => installer::list(),
            BackendCmd::Install { kinds } => installer::install(&kinds),
            BackendCmd::Record { kinds } => installer::record(&kinds),
        },
    };
    res.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::FAILURE
    })
}

/// The default action: the TUI, or a headless run with `--auto`.
fn run_main(args: RunArgs) -> ExitCode {
    let mut detected = Detected::detect();

    // Inside the container the data folder is always mounted at /data.
    let path = args.path.clone().or_else(|| {
        let d = PathBuf::from(backend::CONTAINER_DATA);
        (detected.in_container && d.is_dir()).then_some(d)
    });

    // An explicitly chosen backend that can't work here is an error. One that is
    // only missing something (or not running) gets the setup offer (B.7) and then
    // goes on to the preflight checks, which say exactly what is still missing.
    if let Some(b) = args.backend
        && let Availability::Unsupported(r) = detected.availability(b)
    {
        eprintln!("backend '{}' is not available: {r}", b.name());
        return ExitCode::FAILURE;
    }
    let can_prompt = installer::can_prompt(
        args.auto,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        detected.in_container,
    );
    let declined = offer_setup_cli(args.backend, can_prompt, &mut detected);

    if args.auto {
        let Some(path) = path else {
            eprintln!("--auto needs a data folder: ngiab-rs <PATH> --auto");
            return ExitCode::FAILURE;
        };
        let data_dir = match std::path::absolute(&path) {
            Ok(p) if p.is_dir() => p,
            Ok(p) => {
                eprintln!("{} is not a directory", p.display());
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let Some(kind) = args.backend.or_else(|| detected.default_kind()) else {
            eprintln!("No backend available:");
            for k in BackendKind::ALL {
                if let Some(r) = detected.availability(k).reason() {
                    eprintln!("  {:<7} {r}", k.name());
                }
            }
            for line in installer::offer(&detected, None).advice() {
                eprintln!("{line}");
            }
            return ExitCode::FAILURE;
        };
        let launcher = Launcher::new(kind, args.image, data_dir, detected);
        let jobs = args
            .jobs
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .max(1);
        return auto::run(
            launcher,
            plan::RunOptions {
                jobs,
                routing: !args.no_routing,
            },
        );
    }

    if !std::io::stdout().is_terminal() {
        eprintln!(
            "ngiab-rs needs a terminal for interactive mode (use -it with docker), or pass --auto."
        );
        return ExitCode::FAILURE;
    }

    // SIGTERM/SIGHUP (terminal closed, `kill`) must still stop containers and restore
    // the terminal; SIGINT only arrives from outside since raw mode eats ctrl-c.
    // SIGINT has its own flag: while the TUI is suspended (shell, installer) a
    // ctrl-c is meant for the child, and `suspended` clears it afterwards.
    let term = Arc::new(AtomicBool::new(false));
    let sigint = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let signals = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP];
    // No SIGHUP on Windows; signal-hook maps these onto console control events.
    #[cfg(windows)]
    let signals = [signal_hook::consts::SIGTERM];
    for sig in signals {
        let _ = signal_hook::flag::register(sig, Arc::clone(&term));
    }
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&sigint));

    let mut app = App::new(path, detected, args.backend, args.image, args.jobs);
    // Don't ask again in the TUI what was just declined here.
    app.declined_install = declined;
    app.can_prompt = can_prompt;
    let res = run_tui(&mut app, &term, &sigint);
    // Stops any running step (kills its container) before we exit.
    app.shutdown();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// B.7, CLI side: offer to set up the chosen backend (or any backend, when none
/// works) before the run, then re-detect. Asks at most once, only when
/// `can_prompt`; otherwise, and when declined, it prints what to run instead.
/// Returns what the user declined, so the TUI doesn't ask again.
fn offer_setup_cli(
    chosen: Option<BackendKind>,
    can_prompt: bool,
    detected: &mut Detected,
) -> HashSet<BackendKind> {
    let mut declined = HashSet::new();
    // What is wrong and what to run instead.
    let explain = |d: &Detected, offer: &installer::Offer| {
        match chosen {
            Some(b) => {
                if let Some(r) = d.availability(b).reason() {
                    eprintln!("warning: backend '{}' is not ready: {r}", b.name());
                }
            }
            None if d.default_kind().is_none() => eprintln!("warning: no backend is available"),
            None => {}
        }
        for line in offer.advice() {
            eprintln!("  {line}");
        }
    };
    let offer = installer::offer(detected, chosen);
    let Some(question) = offer.question() else {
        // Nothing to run for the user (e.g. a start hint) or nothing wrong. With no
        // backend chosen, `--auto` and the TUI say it themselves.
        if chosen.is_some() {
            explain(detected, &offer);
        }
        return declined;
    };
    if !can_prompt {
        if chosen.is_some() {
            explain(detected, &offer);
        }
        return declined;
    }
    if !cli::confirm(&question, false).unwrap_or(false) {
        explain(detected, &offer);
        declined.extend(offer.kinds());
        return declined;
    }
    let res = match &offer {
        installer::Offer::ModelsUpdate { .. } => installer::models_update(detected),
        installer::Offer::Installer { kinds, .. } => {
            // A single backend is installed directly; with a choice, the installer's
            // own menu asks which.
            let kinds: &[BackendKind] = if kinds.len() == 1 { kinds } else { &[] };
            installer::run(kinds).map(|status| {
                if !status.success() {
                    eprintln!("the installer reported a problem ({status})");
                }
            })
        }
        installer::Offer::Nothing | installer::Offer::StartHints(_) => Ok(()),
    };
    if let Err(e) = res {
        eprintln!("error: {e:#}");
    }
    *detected = Detected::detect();
    // Still not usable: say why once more (and don't ask again in the TUI).
    let after = installer::offer(detected, chosen);
    if after != installer::Offer::Nothing {
        explain(detected, &after);
        declined.extend(after.kinds());
    }
    declined
}

fn run_tui(app: &mut App, term: &AtomicBool, sigint: &AtomicBool) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        while !app.quit && !term.load(Ordering::Relaxed) && !sigint.load(Ordering::Relaxed) {
            app.tick();
            terminal.draw(|f| ui::draw(f, app))?;
            // Faster refresh while a tool is producing output.
            let timeout = if app.running() {
                Duration::from_millis(33)
            } else {
                Duration::from_millis(200)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Resize(..) => {}
                    _ => {}
                }
                // Drain any further queued events before redrawing (fast typing / paste).
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(k) = event::read()? {
                        app.on_key(k);
                    }
                }
            }
            if app.want_shell {
                app.want_shell = false;
                suspended(&mut terminal, sigint, || run_shell(app))?;
                app.refresh_preflight();
            }
            let installer = app.want_installer.take();
            let models = std::mem::take(&mut app.want_models_update);
            if installer.is_some() || models {
                let kinds = installer.unwrap_or_default();
                let detected = app.detected.clone();
                suspended(&mut terminal, sigint, || {
                    run_setup(&kinds, models, &detected)
                })?;
                let mut tried = kinds;
                if models {
                    tried.insert(0, BackendKind::Local);
                }
                app.after_setup(&tried);
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}

/// The one way to leave the TUI for a while (tasks.md "TUI plumbing"): restore
/// the terminal, run `f` on the plain terminal, then re-enter raw mode and the
/// alternate screen. The shell, the installer and `models update` use it (and the
/// D.3 folder dialog and D.5 mouse capture are meant to hook in here). A ctrl-c
/// typed while suspended went to the child, so it must not quit the TUI. If
/// resuming fails the error propagates and `run_tui` restores the terminal.
fn suspended<T>(
    terminal: &mut ratatui::DefaultTerminal,
    sigint: &AtomicBool,
    f: impl FnOnce() -> T,
) -> anyhow::Result<T> {
    ratatui::restore();
    let out = f();
    sigint.store(false, Ordering::Relaxed);
    use ratatui::crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    ratatui::crossterm::execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        Clear(ClearType::All)
    )?;
    // A fresh `Terminal` (blank back buffer) redraws everything on the next frame.
    // Not `terminal.clear()`: that queries the cursor position and fails when the
    // terminal doesn't answer in time.
    *terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    Ok(out)
}

/// Runs on the plain terminal (via `suspended`): `models update` in-process for
/// a local backend that only lacks its model files, then the installer for
/// `kinds`. Waits for Enter so the output can be read before the TUI returns.
fn run_setup(kinds: &[BackendKind], models: bool, detected: &Detected) {
    if models {
        println!("\x1b[36mDownloading the model files for the local backend.\x1b[0m");
        if let Err(e) = installer::models_update(detected) {
            eprintln!("error: {e:#}");
        }
    }
    if !kinds.is_empty() {
        match installer::run(kinds) {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("the installer reported a problem ({s})"),
            Err(e) => eprintln!("error: {e:#}"),
        }
    }
    print!("\n\x1b[36mPress Enter to return to ngiab-rs.\x1b[0m ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn run_shell(app: &App) {
    let Some(l) = app.launcher().or_else(|| {
        // Shell is still useful without a tool backend (e.g. to inspect files).
        Some(Launcher::new(
            BackendKind::Local,
            app.image.clone(),
            app.data_dir.clone()?,
            app.detected.clone(),
        ))
    }) else {
        return;
    };
    let spec = l.shell_command();
    println!(
        "\x1b[36mStarting a shell in {} - type `exit` to return to ngiab-rs.\x1b[0m",
        l.data_dir.display()
    );
    println!("\x1b[2m$ {}\x1b[0m", spec.display());
    let status = spec.to_command().current_dir(&l.data_dir).status();
    if let Err(e) = status {
        eprintln!("could not start shell: {e}");
        std::thread::sleep(Duration::from_secs(2));
    }
}
