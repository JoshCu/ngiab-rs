//! Application state and input handling for the interactive TUI.

use crate::backend::{BackendKind, Detected, Launcher, Tool};
use crate::config::{Realization, State, TROUTE_OUTPUT_DIR};
use crate::eval::EvalState;
use crate::plan::{self, RunOptions, StepKind};
use crate::preflight::{self, Preflight};
use crate::pty::{PtyProcess, awaiting_answer, parse_progress};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    PathInput,
    Home,
    Run,
    Eval,
}

pub enum Popup {
    Cleanup { files: usize, dirs: Vec<(PathBuf, usize)>, plan: Vec<StepKind> },
    ConfirmCancel,
    Message { title: String, body: String },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    RunAll,
    RunBmi,
    RunRoute,
    Evaluate,
    Preflight,
    Pull,
    Shell,
    ChangeFolder,
    Quit,
}

impl MenuItem {
    pub fn label(self) -> &'static str {
        match self {
            MenuItem::RunAll => "Run simulation (bmi-driver → rs-route)",
            MenuItem::RunBmi => "Run bmi-driver only",
            MenuItem::RunRoute => "Run rs-route only",
            MenuItem::Evaluate => "Evaluate outputs",
            MenuItem::Preflight => "Re-run preflight checks",
            MenuItem::Pull => "Pull / update container image",
            MenuItem::Shell => "Open a shell in the data folder",
            MenuItem::ChangeFolder => "Change data folder",
            MenuItem::Quit => "Quit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed(String),
    Skipped(String),
    Cancelled,
}

pub struct StepState {
    pub kind: StepKind,
    pub status: StepStatus,
    pub started: Option<Instant>,
    pub elapsed: Option<Duration>,
    pub progress: Option<(u64, u64)>,
    /// Tool is waiting on a typed answer (bmi-driver's [y/N] prompt).
    pub awaiting: bool,
    pub proc: Option<PtyProcess>,
    pub command: Option<String>,
}

impl StepState {
    pub fn elapsed(&self) -> Option<Duration> {
        self.elapsed.or_else(|| self.started.map(|s| s.elapsed()))
    }
}

pub struct RunState {
    pub steps: Vec<StepState>,
    pub current: usize,
    pub view: usize,
    pub finished: bool,
    pub started: Instant,
    pub elapsed: Option<Duration>,
    opts: RunOptions,
    real: Option<Realization>,
}

impl RunState {
    pub fn succeeded(&self) -> bool {
        self.finished
            && self
                .steps
                .iter()
                .all(|s| matches!(s.status, StepStatus::Done | StepStatus::Skipped(_)))
            && self.steps.iter().any(|s| s.status == StepStatus::Done)
    }

    pub fn awaiting_input(&self) -> bool {
        !self.finished && self.steps.get(self.current).is_some_and(|s| s.awaiting)
    }
}

pub struct App {
    pub screen: Screen,
    pub popup: Option<Popup>,
    pub quit: bool,
    pub want_shell: bool,

    pub data_dir: Option<PathBuf>,
    pub path_input: String,
    pub path_error: Option<String>,

    pub detected: Detected,
    pub backend: Option<BackendKind>,
    pub image: String,
    pub jobs: usize,
    pub max_jobs: usize,

    pub preflight: Option<Preflight>,
    pub menu_idx: usize,
    pub run: Option<RunState>,
    pub eval: Option<EvalState>,
    /// Size (rows, cols) of the embedded terminal pane, updated each draw.
    pub pane: (u16, u16),
    pub status: Option<(String, Instant)>,
}

impl App {
    pub fn new(
        data_dir: Option<PathBuf>,
        detected: Detected,
        backend: Option<BackendKind>,
        image: String,
        jobs: Option<usize>,
    ) -> Self {
        let max_jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
        let backend = backend.or_else(|| detected.default_kind());
        let state = State::load();
        let mut app = Self {
            screen: Screen::PathInput,
            popup: None,
            quit: false,
            want_shell: false,
            data_dir: None,
            path_input: state
                .last_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            path_error: None,
            detected,
            backend,
            image,
            jobs: jobs.unwrap_or(max_jobs).max(1),
            max_jobs,
            preflight: None,
            menu_idx: 0,
            run: None,
            eval: None,
            pane: (24, 80),
            status: None,
        };
        if let Some(d) = data_dir {
            app.path_input = d.display().to_string();
            app.submit_path();
        }
        app
    }

    pub fn launcher(&self) -> Option<Launcher> {
        Some(Launcher {
            kind: self.backend?,
            image: self.image.clone(),
            data_dir: self.data_dir.clone()?,
            detected: self.detected.clone(),
        })
    }

    pub fn realization(&self) -> Option<&Realization> {
        self.preflight.as_ref()?.realization.as_ref()
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn menu_items(&self) -> Vec<MenuItem> {
        let mut v = vec![
            MenuItem::RunAll,
            MenuItem::RunBmi,
            MenuItem::RunRoute,
            MenuItem::Evaluate,
            MenuItem::Preflight,
        ];
        if self.backend.is_some_and(BackendKind::is_container) {
            v.push(MenuItem::Pull);
        }
        v.extend([MenuItem::Shell, MenuItem::ChangeFolder, MenuItem::Quit]);
        v
    }

    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.finished)
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    fn submit_path(&mut self) {
        let raw = self.path_input.trim();
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| raw.into())
        } else {
            PathBuf::from(raw)
        };
        if raw.is_empty() {
            self.path_error = Some("enter a path".into());
            return;
        }
        match std::path::absolute(&expanded) {
            Ok(p) if p.is_dir() => {
                self.data_dir = Some(p.clone());
                self.path_error = None;
                State { last_path: Some(p) }.save();
                self.eval = None;
                self.refresh_preflight();
                self.screen = Screen::Home;
            }
            Ok(p) if !p.exists() => self.path_error = Some(format!("{} does not exist", p.display())),
            Ok(p) => self.path_error = Some(format!("{} is not a directory", p.display())),
            Err(e) => self.path_error = Some(format!("{}: {e}", expanded.display())),
        }
    }

    pub fn refresh_preflight(&mut self) {
        self.preflight = self.launcher().map(|l| preflight::run(&l, true));
        if self.preflight.is_none() && self.data_dir.is_some() {
            self.flash("no backend available: install bmi-driver, docker or podman");
        }
    }

    fn cycle_backend(&mut self) {
        let avail: Vec<BackendKind> =
            BackendKind::ALL.into_iter().filter(|k| self.detected.available(*k)).collect();
        if avail.is_empty() {
            self.flash("no backends available");
            return;
        }
        let pos = self.backend.and_then(|b| avail.iter().position(|k| *k == b));
        let next = avail[pos.map_or(0, |p| (p + 1) % avail.len())];
        if Some(next) != self.backend {
            self.backend = Some(next);
            self.refresh_preflight();
            self.flash(format!("backend: {}", next.name()));
        }
        let n = self.menu_items().len();
        self.menu_idx = self.menu_idx.min(n - 1);
    }

    fn activate(&mut self, item: MenuItem) {
        match item {
            MenuItem::RunAll => {
                self.request_run(vec![StepKind::Tool(Tool::BmiDriver), StepKind::Tool(Tool::RsRoute)])
            }
            MenuItem::RunBmi => self.request_run(vec![StepKind::Tool(Tool::BmiDriver)]),
            MenuItem::RunRoute => self.request_run(vec![StepKind::Tool(Tool::RsRoute)]),
            MenuItem::Evaluate => self.open_eval(),
            MenuItem::Preflight => {
                self.refresh_preflight();
                self.flash("preflight refreshed");
            }
            MenuItem::Pull => self.begin_run(vec![StepKind::Pull]),
            MenuItem::Shell => self.want_shell = true,
            MenuItem::ChangeFolder => {
                self.path_error = None;
                self.screen = Screen::PathInput;
            }
            MenuItem::Quit => self.quit = true,
        }
    }

    fn request_run(&mut self, plan: Vec<StepKind>) {
        let Some(pf) = &self.preflight else {
            self.popup = Some(Popup::Message {
                title: "Cannot run".into(),
                body: "No backend available. Install bmi-driver/rs-route locally, or docker/podman.".into(),
            });
            return;
        };
        if !pf.can_run() {
            let fails: Vec<String> = pf
                .checks
                .iter()
                .filter(|c| c.level == preflight::Level::Fail)
                .map(|c| format!("✗ {}: {}", c.name, c.detail))
                .collect();
            self.popup = Some(Popup::Message {
                title: "Preflight checks failed".into(),
                body: fails.join("\n"),
            });
            return;
        }
        let runs_bmi = plan.contains(&StepKind::Tool(Tool::BmiDriver));
        let files = pf.existing_output_files();
        if runs_bmi && files > 0 {
            self.popup = Some(Popup::Cleanup { files, dirs: pf.existing_outputs.clone(), plan });
        } else {
            self.begin_run(plan);
        }
    }

    fn begin_run(&mut self, plan: Vec<StepKind>) {
        let Some(launcher) = self.launcher() else { return };
        // Never hold output files open while tools rewrite them.
        self.eval = None;
        let real = self.realization().cloned();
        if plan.iter().any(|k| matches!(k, StepKind::Tool(_))) {
            let out = self.preflight.as_ref().and_then(|p| p.output_dir.clone());
            if let Err(e) = preflight::ensure_output_dirs(&launcher.data_dir, out.as_deref()) {
                self.popup = Some(Popup::Message {
                    title: "Cannot create output folders".into(),
                    body: e.to_string(),
                });
                return;
            }
        }
        let steps = plan
            .into_iter()
            .map(|kind| {
                let skip = match kind {
                    StepKind::Tool(Tool::RsRoute) => {
                        if real.as_ref().is_some_and(|r| !r.is_csv()) {
                            Some("needs csv catchment output".to_string())
                        } else if launcher.kind == BackendKind::Local && self.detected.rs_route.is_none() {
                            Some("rs-route not found".to_string())
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                StepState {
                    kind,
                    status: skip.map_or(StepStatus::Pending, StepStatus::Skipped),
                    started: None,
                    elapsed: None,
                    progress: None,
                    awaiting: false,
                    proc: None,
                    command: None,
                }
            })
            .collect();
        self.run = Some(RunState {
            steps,
            current: 0,
            view: 0,
            finished: false,
            started: Instant::now(),
            elapsed: None,
            opts: RunOptions { jobs: self.jobs, routing: true },
            real,
        });
        self.screen = Screen::Run;
        self.advance();
    }

    /// Start the next pending step, or mark the run finished.
    fn advance(&mut self) {
        let Some(launcher) = self.launcher() else { return };
        let pane = self.pane;
        let Some(run) = self.run.as_mut() else { return };
        while run.current < run.steps.len() {
            let step = &mut run.steps[run.current];
            if step.status != StepStatus::Pending {
                run.current += 1;
                continue;
            }
            let Some(spec) = plan::command_for(step.kind, &launcher, &run.opts, run.real.as_ref(), true)
            else {
                step.status = StepStatus::Skipped("not applicable".into());
                run.current += 1;
                continue;
            };
            step.command = Some(spec.display());
            step.started = Some(Instant::now());
            match PtyProcess::spawn(spec, &launcher.data_dir, pane.0, pane.1) {
                Ok(p) => {
                    step.proc = Some(p);
                    step.status = StepStatus::Running;
                    run.view = run.current;
                    return;
                }
                Err(e) => {
                    step.status = StepStatus::Failed(format!("{e:#}"));
                    step.elapsed = Some(Duration::ZERO);
                    Self::skip_rest(run, "previous step failed");
                    break;
                }
            }
        }
        self.finish_run();
    }

    fn skip_rest(run: &mut RunState, why: &str) {
        for s in run.steps.iter_mut() {
            if s.status == StepStatus::Pending {
                s.status = StepStatus::Skipped(why.into());
            }
        }
        run.current = run.steps.len();
    }

    fn finish_run(&mut self) {
        if let Some(run) = self.run.as_mut() {
            if run.finished {
                return;
            }
            run.finished = true;
            run.elapsed = Some(run.started.elapsed());
            let ok = run.succeeded();
            self.flash(if ok { "run finished ✓  (e: evaluate)" } else { "run did not complete" });
        }
        // Outputs changed: refresh counts and any open evaluation.
        self.refresh_preflight();
        self.eval = None;
    }

    /// Stop everything before the process exits.
    pub fn shutdown(&mut self) {
        if self.running() {
            self.cancel_run();
        }
        self.run = None;
    }

    fn cancel_run(&mut self) {
        if let Some(run) = self.run.as_mut() {
            if let Some(step) = run.steps.get_mut(run.current) {
                if let Some(p) = step.proc.as_mut() {
                    p.kill();
                }
                step.status = StepStatus::Cancelled;
                step.elapsed = step.started.map(|s| s.elapsed());
            }
            Self::skip_rest(run, "cancelled");
        }
        self.finish_run();
    }

    /// Called every loop iteration: pump PTY output, detect step completion.
    pub fn tick(&mut self) {
        if let Some((_, t)) = &self.status
            && t.elapsed() > Duration::from_secs(5) {
                self.status = None;
            }
        let pane = self.pane;
        let Some(run) = self.run.as_mut() else { return };
        for s in run.steps.iter_mut() {
            if let Some(p) = s.proc.as_mut() {
                p.resize(pane.0, pane.1);
            }
        }
        if run.finished {
            return;
        }
        let Some(step) = run.steps.get_mut(run.current) else { return };
        let Some(proc) = step.proc.as_mut() else { return };
        if proc.poll() {
            let live = proc.live_contents();
            if let Some(p) = parse_progress(&live) {
                step.progress = Some(p);
            }
            step.awaiting = awaiting_answer(&live);
        }
        if let Some(status) = proc.finished() {
            step.elapsed = step.started.map(|s| s.elapsed());
            step.awaiting = false;
            if status.success() {
                step.status = StepStatus::Done;
                if let Some((_, len)) = step.progress {
                    step.progress = Some((len, len));
                }
                run.current += 1;
                self.advance();
            } else {
                step.status = StepStatus::Failed(format!("exit code {}", status.exit_code()));
                Self::skip_rest(run, "previous step failed");
                self.finish_run();
            }
        }
    }

    fn open_eval(&mut self) {
        let Some(data) = self.data_dir.clone() else { return };
        let cat_dir = self
            .preflight
            .as_ref()
            .and_then(|p| p.output_dir.clone())
            .unwrap_or_else(|| data.join(crate::config::BMI_DEFAULT_OUTPUT_ROOT));
        if self.eval.is_none() {
            self.eval = Some(EvalState::new(data.join(TROUTE_OUTPUT_DIR), cat_dir));
        }
        self.screen = Screen::Eval;
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.popup.is_some() {
            self.on_popup_key(key);
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            if self.running() {
                self.popup = Some(Popup::ConfirmCancel);
            } else {
                self.quit = true;
            }
            return;
        }
        match self.screen {
            Screen::PathInput => self.on_path_key(key),
            Screen::Home => self.on_home_key(key),
            Screen::Run => self.on_run_key(key),
            Screen::Eval => self.on_eval_key(key),
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        match popup {
            Popup::Cleanup { dirs, plan, files } => match key.code {
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    let paths: Vec<PathBuf> = dirs.iter().map(|d| d.0.clone()).collect();
                    match preflight::clean_outputs(&paths) {
                        Ok(n) => {
                            self.flash(format!("deleted {n} files"));
                            self.begin_run(plan);
                        }
                        Err(e) => {
                            self.popup = Some(Popup::Message {
                                title: "Cleanup failed".into(),
                                body: e.to_string(),
                            })
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Enter => self.begin_run(plan),
                KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => {}
                _ => self.popup = Some(Popup::Cleanup { dirs, plan, files }),
            },
            Popup::ConfirmCancel => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.cancel_run(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.popup = Some(Popup::ConfirmCancel),
            },
            Popup::Message { .. } | Popup::Help => {}
        }
    }

    fn on_path_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.submit_path(),
            KeyCode::Esc => {
                if self.data_dir.is_some() {
                    self.screen = Screen::Home;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Backspace => {
                self.path_input.pop();
            }
            KeyCode::Char('u') if ctrl => self.path_input.clear(),
            KeyCode::Tab => self.complete_path(),
            KeyCode::Char(c) if !ctrl => self.path_input.push(c),
            _ => {}
        }
    }

    /// Shell-like Tab completion of directory names.
    fn complete_path(&mut self) {
        let input = self.path_input.clone();
        let (dir, prefix) = match input.rfind('/') {
            Some(i) => (input[..=i].to_string(), input[i + 1..].to_string()),
            None => ("./".to_string(), input.clone()),
        };
        let lookup = if let Some(rest) = dir.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| dir.clone().into())
        } else {
            PathBuf::from(&dir)
        };
        let Ok(rd) = std::fs::read_dir(&lookup) else { return };
        let mut matches: Vec<String> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(&prefix) && (!n.starts_with('.') || prefix.starts_with('.')))
            .collect();
        matches.sort();
        let Some(first) = matches.first().cloned() else { return };
        let common = matches.iter().fold(first.clone(), |acc, m| {
            acc.chars().zip(m.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect()
        });
        let base = if input.contains('/') { dir } else { String::new() };
        self.path_input = format!("{base}{common}");
        if matches.len() == 1 {
            self.path_input.push('/');
        } else {
            self.flash(matches.iter().take(8).cloned().collect::<Vec<_>>().join("  "));
        }
    }

    fn on_home_key(&mut self, key: KeyEvent) {
        let items = self.menu_items();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.menu_idx = self.menu_idx.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.menu_idx = (self.menu_idx + 1).min(items.len() - 1)
            }
            KeyCode::Enter => {
                if let Some(&item) = items.get(self.menu_idx) {
                    self.activate(item);
                }
            }
            KeyCode::Char('b') => self.cycle_backend(),
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Right => {
                self.jobs = (self.jobs + 1).min(self.max_jobs * 4)
            }
            KeyCode::Char('-') | KeyCode::Left => self.jobs = self.jobs.saturating_sub(1).max(1),
            KeyCode::Char('p') => self.activate(MenuItem::Preflight),
            KeyCode::Char('e') => self.activate(MenuItem::Evaluate),
            KeyCode::Char('r') => self.activate(MenuItem::RunAll),
            KeyCode::Char('s') => self.activate(MenuItem::Shell),
            KeyCode::Char('v') if self.run.is_some() => self.screen = Screen::Run,
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let i = c.to_digit(10).unwrap() as usize;
                if i >= 1 && i <= items.len() {
                    self.menu_idx = i - 1;
                    self.activate(items[i - 1]);
                }
            }
            _ => {}
        }
    }

    fn on_run_key(&mut self, key: KeyEvent) {
        let page = (self.pane.0 / 2).max(1) as isize;
        let Some(run) = self.run.as_mut() else {
            self.screen = Screen::Home;
            return;
        };
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // Navigation keys work whether or not the run is active.
        match key.code {
            KeyCode::PageUp => return scroll_view(run, page),
            KeyCode::PageDown => return scroll_view(run, -page),
            KeyCode::Up if shift => return scroll_view(run, 1),
            KeyCode::Down if shift => return scroll_view(run, -1),
            KeyCode::Tab => {
                let n = run.steps.len();
                run.view = (run.view + 1) % n;
                return;
            }
            _ => {}
        }
        if run.finished {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.screen = Screen::Home,
                KeyCode::Char('e') => self.open_eval(),
                KeyCode::Home => scroll_view(run, isize::MAX / 2),
                KeyCode::End => scroll_view(run, isize::MIN / 2),
                _ => {}
            }
            return;
        }
        // While running, forward typing to the tool (e.g. bmi-driver's [y/N] prompt).
        let bytes: Option<Vec<u8>> = match key.code {
            KeyCode::Char(c) => Some(c.to_string().into_bytes()),
            KeyCode::Enter => Some(b"\r".to_vec()),
            KeyCode::Backspace => Some(vec![0x7f]),
            KeyCode::Esc => None,
            _ => None,
        };
        if let (Some(b), Some(step)) = (bytes, run.steps.get_mut(run.current))
            && let Some(p) = step.proc.as_mut() {
                p.scroll(-(p.scrollback() as isize));
                p.write(&b);
            }
    }

    fn on_eval_key(&mut self, key: KeyEvent) {
        let Some(ev) = self.eval.as_mut() else {
            self.screen = Screen::Home;
            return;
        };
        if ev.filtering {
            match key.code {
                KeyCode::Enter => ev.filtering = false,
                KeyCode::Esc => {
                    ev.filtering = false;
                    ev.filter.clear();
                    ev.refilter();
                    ev.load_selected();
                }
                KeyCode::Backspace => {
                    ev.filter.pop();
                    ev.refilter();
                    ev.load_selected();
                }
                KeyCode::Char(c) => {
                    ev.filter.push(c);
                    ev.selected = 0;
                    ev.refilter();
                    ev.load_selected();
                }
                KeyCode::Up => ev.move_sel(-1),
                KeyCode::Down => ev.move_sel(1),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Home,
            KeyCode::Tab | KeyCode::BackTab => ev.switch_tab(),
            KeyCode::Up | KeyCode::Char('k') => ev.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => ev.move_sel(1),
            KeyCode::PageUp => ev.move_sel(-20),
            KeyCode::PageDown => ev.move_sel(20),
            KeyCode::Home | KeyCode::Char('g') => ev.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => ev.move_sel(isize::MAX / 2),
            KeyCode::Left | KeyCode::Char('h') => ev.cycle_var(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('v') => ev.cycle_var(1),
            KeyCode::Char('f') => ev.cycle_file(),
            KeyCode::Char('/') => ev.filtering = true,
            KeyCode::Char('r') => ev.reload(),
            _ => {}
        }
    }
}

fn scroll_view(run: &mut RunState, delta: isize) {
    if let Some(p) = run.steps.get_mut(run.view).and_then(|s| s.proc.as_mut()) {
        p.scroll(delta);
    }
}
