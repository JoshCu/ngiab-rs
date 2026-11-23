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
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn main() -> ExitCode {
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
    let detected = Detected::detect();

    // Inside the container the data folder is always mounted at /data.
    let path = args.path.clone().or_else(|| {
        let d = PathBuf::from(backend::CONTAINER_DATA);
        (detected.in_container && d.is_dir()).then_some(d)
    });

    // An explicitly chosen backend that can't work here is an error. One that is
    // only missing something (or not running) goes on to the preflight checks,
    // which say exactly what is missing and how to fix it.
    if let Some(b) = args.backend {
        match detected.availability(b) {
            Availability::Available => {}
            Availability::Unsupported(r) => {
                eprintln!("backend '{}' is not available: {r}", b.name());
                return ExitCode::FAILURE;
            }
            Availability::NotInstalled(r) | Availability::NotRunning(r) => {
                eprintln!("warning: backend '{}' is not ready: {r}", b.name());
            }
        }
    }

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
    let term = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let signals = [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ];
    // No SIGHUP on Windows; signal-hook maps these onto console control events.
    #[cfg(windows)]
    let signals = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT];
    for sig in signals {
        let _ = signal_hook::flag::register(sig, Arc::clone(&term));
    }

    let mut app = App::new(path, detected, args.backend, args.image, args.jobs);
    let res = run_tui(&mut app, &term);
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

fn run_tui(app: &mut App, term: &AtomicBool) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        while !app.quit && !term.load(Ordering::Relaxed) {
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
                ratatui::restore();
                run_shell(app);
                terminal = ratatui::init();
                terminal.clear()?;
                app.refresh_preflight();
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
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
