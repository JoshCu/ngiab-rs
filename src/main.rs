mod app;
mod auto;
mod backend;
mod config;
mod eval;
mod plan;
mod preflight;
mod pty;
mod ui;

use app::App;
use backend::{BackendKind, Detected, Launcher};
use clap::Parser;
use ratatui::crossterm::event::{self, Event};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// NextGen In A Box (Rust): run bmi-driver + rs-route and inspect the results.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Data folder containing config/, forcings/ and outputs/
    path: Option<PathBuf>,

    /// Non-interactive: run preflight checks, bmi-driver, then rs-route and exit
    #[arg(long)]
    auto: bool,

    /// Number of CPUs to use (bmi-driver -j / rs-route -n). Defaults to all CPUs.
    #[arg(
        short = 'j',
        long = "jobs",
        visible_alias = "num-cpus",
        value_name = "N"
    )]
    jobs: Option<usize>,

    /// Where to run the tools. Default: local binaries > docker > podman
    #[arg(short, long, value_enum, env = "NGIAB_RS_BACKEND")]
    backend: Option<BackendKind>,

    /// Container image used by the docker/podman backends
    #[arg(long, env = "NGIAB_RS_IMAGE", default_value = backend::DEFAULT_IMAGE)]
    image: String,

    /// Only run bmi-driver (auto mode)
    #[arg(long)]
    no_routing: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let detected = Detected::detect();

    // Inside the container the data folder is always mounted at /data.
    let path = cli.path.clone().or_else(|| {
        let d = PathBuf::from(backend::CONTAINER_DATA);
        (detected.in_container && d.is_dir()).then_some(d)
    });

    if let Some(b) = cli.backend
        && !detected.available(b)
    {
        eprintln!(
            "backend '{}' is not available: {}",
            b.name(),
            detected.summary(b)
        );
        return ExitCode::FAILURE;
    }

    if cli.auto {
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
        let Some(kind) = cli.backend.or_else(|| detected.default_kind()) else {
            eprintln!("No backend found: install bmi-driver + rs-route, docker, or podman.");
            return ExitCode::FAILURE;
        };
        let launcher = Launcher {
            kind,
            image: cli.image,
            data_dir,
            detected,
        };
        let jobs = cli
            .jobs
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .max(1);
        return auto::run(
            launcher,
            plan::RunOptions {
                jobs,
                routing: !cli.no_routing,
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

    let mut app = App::new(path, detected, cli.backend, cli.image, cli.jobs);
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
        Some(Launcher {
            kind: BackendKind::Local,
            image: app.image.clone(),
            data_dir: app.data_dir.clone()?,
            detected: app.detected.clone(),
        })
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
