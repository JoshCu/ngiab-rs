//! Application state and input handling for the interactive TUI.

use crate::backend::{Availability, BackendKind, Detected, Launcher, Tool};
use crate::config::{Realization, State, TROUTE_OUTPUT_DIR};
use crate::eval::EvalState;
use crate::installer::{self, Offer};
use crate::plan::{self, RunOptions, StepKind};
use crate::preflight::{self, Preflight};
use crate::pty::{PtyProcess, awaiting_answer, parse_progress};
use crate::stage::{self, Staged};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// A staging run (local backend) on a background thread.
pub struct StageJob {
    pub progress: Arc<stage::Progress>,
    rx: Receiver<anyhow::Result<Option<Staged>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    PathInput,
    Home,
    Run,
    Eval,
}

pub enum Popup {
    Cleanup {
        files: usize,
        dirs: Vec<(PathBuf, usize)>,
        plan: Vec<StepKind>,
    },
    ConfirmCancel,
    /// "Set up the missing backend now?" (B.7): the installer or `models update`.
    OfferSetup(Offer),
    /// Checkbox list of backends to install.
    PickBackends {
        items: Vec<(BackendKind, bool)>,
        cursor: usize,
    },
    Message {
        title: String,
        body: String,
    },
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
    InstallBackend,
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
            MenuItem::InstallBackend => "Install backend…",
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
    /// Steps without a terminal (staging): their running job and a text to show.
    pub job: Option<StageJob>,
    pub note: Option<String>,
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
    /// Local runs: the staged realization bmi-driver gets as `--config`.
    pub staged: Option<Staged>,
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
    /// Run the installer for these backends (main.rs suspends the TUI for it).
    pub want_installer: Option<Vec<BackendKind>>,
    /// Run `models update` in-process (main.rs suspends the TUI for it).
    pub want_models_update: bool,
    /// Backends whose setup offer was declined: not offered again this session
    /// (the menu's "Install backend…" still works).
    pub declined_install: HashSet<BackendKind>,
    /// Setup offers need a terminal on stdin and stdout (B.7).
    pub can_prompt: bool,

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
            want_installer: None,
            want_models_update: false,
            declined_install: HashSet::new(),
            can_prompt: false,
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
        Some(Launcher::new(
            self.backend?,
            self.image.clone(),
            self.data_dir.clone()?,
            self.detected.clone(),
        ))
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
        if self.can_prompt && !installer::candidates(&self.detected).is_empty() {
            v.push(MenuItem::InstallBackend);
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
        let expanded = if let Some(rest) = strip_home_prefix(raw) {
            dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| raw.into())
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
            Ok(p) if !p.exists() => {
                self.path_error = Some(format!("{} does not exist", p.display()))
            }
            Ok(p) => self.path_error = Some(format!("{} is not a directory", p.display())),
            Err(e) => self.path_error = Some(format!("{}: {e}", expanded.display())),
        }
    }

    pub fn refresh_preflight(&mut self) {
        self.preflight = self.launcher().map(|l| preflight::run(&l, true));
        if self.preflight.is_none() && self.data_dir.is_some() {
            self.flash("no backend available: see the Backends panel");
        }
    }

    /// Switch to the next backend that can work on this platform. Backends that are
    /// only missing something can be selected, so preflight explains what to fix;
    /// unsupported ones are listed (greyed, with the reason) but skipped.
    fn cycle_backend(&mut self) {
        let avail: Vec<BackendKind> = BackendKind::ALL
            .into_iter()
            .filter(|k| !matches!(self.detected.availability(*k), Availability::Unsupported(_)))
            .collect();
        if avail.is_empty() {
            self.flash("no backends available");
            return;
        }
        let pos = self
            .backend
            .and_then(|b| avail.iter().position(|k| *k == b));
        let next = avail[pos.map_or(0, |p| (p + 1) % avail.len())];
        if Some(next) != self.backend {
            self.backend = Some(next);
            self.refresh_preflight();
            match self.detected.availability(next).reason() {
                None => self.flash(format!("backend: {}", next.name())),
                Some(r) => self.flash(format!("backend: {} (not ready: {r})", next.name())),
            }
        }
        let n = self.menu_items().len();
        self.menu_idx = self.menu_idx.min(n - 1);
    }

    fn activate(&mut self, item: MenuItem) {
        match item {
            MenuItem::RunAll => self.request_run(vec![
                StepKind::Tool(Tool::BmiDriver),
                StepKind::Tool(Tool::RsRoute),
            ]),
            MenuItem::RunBmi => self.request_run(vec![StepKind::Tool(Tool::BmiDriver)]),
            MenuItem::RunRoute => self.request_run(vec![StepKind::Tool(Tool::RsRoute)]),
            MenuItem::Evaluate => self.open_eval(),
            MenuItem::Preflight => {
                self.refresh_preflight();
                self.flash("preflight refreshed");
            }
            MenuItem::Pull => self.begin_run(vec![StepKind::Pull]),
            MenuItem::InstallBackend => self.open_backend_picker(None),
            MenuItem::Shell => self.want_shell = true,
            MenuItem::ChangeFolder => {
                self.path_error = None;
                self.screen = Screen::PathInput;
            }
            MenuItem::Quit => self.quit = true,
        }
    }

    /// The setup offer for the selected backend (or for any, when none works),
    /// unless it can't be asked here or was declined already.
    fn setup_offer(&self) -> Option<Offer> {
        if !self.can_prompt || self.running() {
            return None;
        }
        let offer = installer::offer(&self.detected, self.backend);
        offer.question()?;
        let kinds = offer.kinds();
        (!kinds.iter().all(|k| self.declined_install.contains(k))).then_some(offer)
    }

    /// What to run by hand when the selected backend (or every backend) is not
    /// usable: the install/start advice, as popup lines.
    fn setup_advice(&self) -> Vec<String> {
        installer::offer(&self.detected, self.backend)
            .advice()
            .into_iter()
            .map(|l| format!("→ {l}"))
            .collect()
    }

    fn request_run(&mut self, plan: Vec<StepKind>) {
        let ready = self.backend.is_some_and(|b| self.detected.available(b));
        if !ready && let Some(offer) = self.setup_offer() {
            self.popup = Some(Popup::OfferSetup(offer));
            return;
        }
        let Some(pf) = &self.preflight else {
            let mut body = vec!["No backend available:".to_string()];
            for k in BackendKind::ALL {
                if let Some(r) = self.detected.availability(k).reason() {
                    body.push(format!("  {:<7} {r}", k.name()));
                }
            }
            body.extend(self.setup_advice());
            self.popup = Some(Popup::Message {
                title: "Cannot run".into(),
                body: body.join("\n"),
            });
            return;
        };
        if !pf.can_run() {
            let mut fails: Vec<String> = pf
                .checks
                .iter()
                .filter(|c| c.level == preflight::Level::Fail)
                .map(|c| format!("✗ {}: {}", c.name, c.detail))
                .collect();
            if !ready {
                fails.extend(self.setup_advice());
            }
            self.popup = Some(Popup::Message {
                title: "Preflight checks failed".into(),
                body: fails.join("\n"),
            });
            return;
        }
        let runs_bmi = plan.contains(&StepKind::Tool(Tool::BmiDriver));
        let files = pf.existing_output_files();
        if runs_bmi && files > 0 {
            self.popup = Some(Popup::Cleanup {
                files,
                dirs: pf.existing_outputs.clone(),
                plan,
            });
        } else {
            self.begin_run(plan);
        }
    }

    fn begin_run(&mut self, plan: Vec<StepKind>) {
        let Some(launcher) = self.launcher() else {
            return;
        };
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
        // Local runs rewrite container paths into a staged realization first.
        let mut plan = plan;
        if launcher.kind == BackendKind::Local
            && let Some(i) = plan
                .iter()
                .position(|k| *k == StepKind::Tool(Tool::BmiDriver))
        {
            plan.insert(i, StepKind::Stage);
        }
        let steps = plan
            .into_iter()
            .map(|kind| {
                let skip = match kind {
                    StepKind::Tool(Tool::RsRoute) => {
                        if real.as_ref().is_some_and(|r| !r.is_csv()) {
                            Some("needs csv catchment output".to_string())
                        } else if launcher.kind == BackendKind::Local
                            && self.detected.rs_route.is_none()
                        {
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
                    job: None,
                    note: None,
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
            opts: RunOptions {
                jobs: self.jobs,
                routing: true,
            },
            real,
            staged: None,
        });
        self.screen = Screen::Run;
        self.advance();
    }

    /// Start the next pending step, or mark the run finished.
    fn advance(&mut self) {
        let Some(launcher) = self.launcher() else {
            return;
        };
        let pane = self.pane;
        let gpkg = self.preflight.as_ref().and_then(|p| p.gpkg.clone());
        let Some(run) = self.run.as_mut() else { return };
        while run.current < run.steps.len() {
            let step = &mut run.steps[run.current];
            if step.status != StepStatus::Pending {
                run.current += 1;
                continue;
            }
            if step.kind == StepKind::Stage {
                step.job = Some(start_stage(&launcher, gpkg));
                step.started = Some(Instant::now());
                step.status = StepStatus::Running;
                step.command = Some(format!(
                    "stage configs into {}",
                    stage::stage_dir(&launcher.data_dir).display()
                ));
                run.view = run.current;
                return;
            }
            let staged = run.staged.as_ref().map(|s| s.realization.as_path());
            let Some(spec) = plan::command_for(
                step.kind,
                &launcher,
                &run.opts,
                run.real.as_ref(),
                staged,
                true,
            ) else {
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
            self.flash(if ok {
                "run finished ✓  (e: evaluate)"
            } else {
                "run did not complete"
            });
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
        let mut was_bmi = false;
        if let Some(run) = self.run.as_mut() {
            if let Some(step) = run.steps.get_mut(run.current) {
                if let Some(p) = step.proc.as_mut() {
                    p.kill();
                }
                if let Some(job) = step.job.take() {
                    job.progress.cancel.store(true, Ordering::Relaxed);
                }
                was_bmi = step.kind == StepKind::Tool(Tool::BmiDriver)
                    && step.status == StepStatus::Running;
                step.status = StepStatus::Cancelled;
                step.elapsed = step.started.map(|s| s.elapsed());
            }
            Self::skip_rest(run, "cancelled");
        }
        if was_bmi {
            self.write_back_after_bmi();
        }
        self.finish_run();
    }

    /// After bmi-driver ran on a staged realization: apply any mappings it added to
    /// the user's `config/realization.json` (C.3.4).
    fn write_back_after_bmi(&mut self) {
        let (Some(launcher), Some(staged)) = (
            self.launcher(),
            self.run.as_ref().and_then(|r| r.staged.as_ref()),
        ) else {
            return;
        };
        match stage::write_back(staged, &launcher.path_map(), &launcher.data_dir) {
            Ok(Some(bak)) => {
                self.popup = Some(Popup::Message {
                    title: "realization.json updated".into(),
                    body: format!(
                        "bmi-driver's new variable mappings were applied to config/realization.json.\n\
                         The previous version is saved as {}.",
                        bak.display()
                    ),
                })
            }
            Ok(None) => {}
            Err(e) => {
                self.popup = Some(Popup::Message {
                    title: "Could not update realization.json".into(),
                    body: format!("{e:#}"),
                })
            }
        }
    }

    /// Poll a running staging step; on completion start the next step.
    fn poll_stage(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        let Some(step) = run.steps.get_mut(run.current) else {
            return;
        };
        let Some(job) = step.job.as_ref() else { return };
        let (done, total) = job.progress.get();
        if total > 0 {
            step.progress = Some((done as u64, total as u64));
        }
        step.note = Some(format!("staging config files: {done}/{total}"));
        let res = match job.rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(r) => r,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("staging stopped unexpectedly")),
        };
        step.job = None;
        step.elapsed = step.started.map(|s| s.elapsed());
        match res {
            Ok(staged) => {
                step.status = StepStatus::Done;
                step.note = Some(match &staged {
                    Some(s) => s.summary(),
                    None => "nothing to rewrite: bmi-driver uses config/realization.json".into(),
                });
                run.staged = staged;
                run.current += 1;
                self.advance();
            }
            Err(e) => {
                step.status = StepStatus::Failed(format!("{e:#}"));
                step.note = Some(format!("staging failed: {e:#}"));
                Self::skip_rest(run, "previous step failed");
                self.finish_run();
            }
        }
    }

    /// Called every loop iteration: pump PTY output, detect step completion.
    pub fn tick(&mut self) {
        if let Some((_, t)) = &self.status
            && t.elapsed() > Duration::from_secs(5)
        {
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
        let Some(step) = run.steps.get_mut(run.current) else {
            return;
        };
        if step.job.is_some() {
            self.poll_stage();
            return;
        }
        let Some(proc) = step.proc.as_mut() else {
            return;
        };
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
            let was_bmi = step.kind == StepKind::Tool(Tool::BmiDriver);
            if status.success() {
                step.status = StepStatus::Done;
                if let Some((_, len)) = step.progress {
                    step.progress = Some((len, len));
                }
                run.current += 1;
                if was_bmi {
                    self.write_back_after_bmi();
                }
                self.advance();
            } else {
                step.status = StepStatus::Failed(format!("exit code {}", status.exit_code()));
                Self::skip_rest(run, "previous step failed");
                if was_bmi {
                    self.write_back_after_bmi();
                }
                self.finish_run();
            }
        }
    }

    fn open_eval(&mut self) {
        let Some(data) = self.data_dir.clone() else {
            return;
        };
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
        let Some(popup) = self.popup.take() else {
            return;
        };
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
            Popup::OfferSetup(offer) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match offer {
                    Offer::ModelsUpdate { .. } => self.want_models_update = true,
                    Offer::Installer { kinds, .. } if kinds.len() == 1 => {
                        self.want_installer = Some(kinds)
                    }
                    Offer::Installer { kinds, .. } => self.open_backend_picker(Some(kinds)),
                    Offer::Nothing | Offer::StartHints(_) => {}
                },
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.declined_install.extend(offer.kinds());
                    if let Some(a) = offer.advice().first() {
                        self.flash(format!("later: {a}"));
                    }
                }
                _ => self.popup = Some(Popup::OfferSetup(offer)),
            },
            Popup::PickBackends {
                mut items,
                mut cursor,
            } => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => cursor = cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        cursor = (cursor + 1).min(items.len().saturating_sub(1))
                    }
                    KeyCode::Char(' ') | KeyCode::Char('x') => {
                        if let Some(it) = items.get_mut(cursor) {
                            it.1 = !it.1;
                        }
                    }
                    KeyCode::Enter => {
                        let picked: Vec<BackendKind> = items
                            .iter()
                            .filter(|(_, on)| *on)
                            .map(|(k, _)| *k)
                            .collect();
                        if picked.is_empty() {
                            self.flash("select a backend with space, or esc to cancel");
                        } else {
                            self.start_setup(&picked);
                            return;
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return,
                    _ => {}
                }
                self.popup = Some(Popup::PickBackends { items, cursor });
            }
            Popup::Message { .. } | Popup::Help => {}
        }
    }

    /// The "Install backend…" checkbox list: `kinds` (default: every backend that
    /// is not installed). The selected backend is pre-checked, or the only one.
    fn open_backend_picker(&mut self, kinds: Option<Vec<BackendKind>>) {
        let kinds = kinds.unwrap_or_else(|| installer::candidates(&self.detected));
        if kinds.is_empty() {
            self.flash("every backend that works here is already installed");
            return;
        }
        let items: Vec<(BackendKind, bool)> = kinds
            .iter()
            .map(|k| (*k, kinds.len() == 1 || self.backend == Some(*k)))
            .collect();
        let cursor = items.iter().position(|(_, on)| *on).unwrap_or(0);
        self.popup = Some(Popup::PickBackends { items, cursor });
    }

    /// Queue the setup of `picked` for main.rs: a local backend that only lacks its
    /// model files gets `models update`, everything else the installer.
    fn start_setup(&mut self, picked: &[BackendKind]) {
        let bundle_only = installer::only_bundle_missing(&self.detected);
        let mut kinds = picked.to_vec();
        if bundle_only && kinds.contains(&BackendKind::Local) {
            kinds.retain(|k| *k != BackendKind::Local);
            self.want_models_update = true;
        }
        if !kinds.is_empty() {
            self.want_installer = Some(kinds);
        }
    }

    /// After the installer / `models update` ran: re-detect, switch to a backend
    /// that now works if the selected one still doesn't, and re-run preflight.
    pub fn after_setup(&mut self, tried: &[BackendKind]) {
        self.detected = Detected::detect();
        let selected_ok = self.backend.is_some_and(|b| self.detected.available(b));
        if !selected_ok
            && let Some(k) = tried
                .iter()
                .copied()
                .find(|k| self.detected.available(*k))
                .or_else(|| self.detected.default_kind())
        {
            self.backend = Some(k);
        }
        // Whatever is still missing is not offered again automatically.
        for k in tried {
            if !self.detected.available(*k) {
                self.declined_install.insert(*k);
            }
        }
        self.refresh_preflight();
        let n = self.menu_items().len();
        self.menu_idx = self.menu_idx.min(n - 1);
        let (ok, missing): (Vec<_>, Vec<_>) =
            tried.iter().partition(|k| self.detected.available(**k));
        let names = |v: &[&BackendKind]| v.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ");
        match (ok.is_empty(), missing.is_empty()) {
            (_, true) => self.flash(format!("ready: {}", names(&ok))),
            (true, false) => self.flash(format!("still not usable: {}", names(&missing))),
            (false, false) => self.flash(format!(
                "ready: {}; still not usable: {}",
                names(&ok),
                names(&missing)
            )),
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

    /// Shell-like Tab completion of directory names. (Superseded by the D.2 picker.)
    fn complete_path(&mut self) {
        let input = self.path_input.clone();
        let (dir, prefix) = match input.rfind(is_sep) {
            Some(i) => (input[..=i].to_string(), input[i + 1..].to_string()),
            None => ("./".to_string(), input.clone()),
        };
        let lookup = if let Some(rest) = strip_home_prefix(&dir) {
            dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| dir.clone().into())
        } else {
            PathBuf::from(&dir)
        };
        let Ok(rd) = std::fs::read_dir(&lookup) else {
            return;
        };
        let mut matches: Vec<String> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(&prefix) && (!n.starts_with('.') || prefix.starts_with('.')))
            .collect();
        matches.sort();
        let Some(first) = matches.first().cloned() else {
            return;
        };
        let common = matches.iter().fold(first.clone(), |acc, m| {
            acc.chars()
                .zip(m.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect()
        });
        let base = if input.contains(is_sep) {
            dir
        } else {
            String::new()
        };
        self.path_input = format!("{base}{common}");
        if matches.len() == 1 {
            self.path_input.push(std::path::MAIN_SEPARATOR);
        } else {
            self.flash(
                matches
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("  "),
            );
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
            && let Some(p) = step.proc.as_mut()
        {
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

/// Stage on a background thread so a large domain doesn't freeze the UI.
fn start_stage(launcher: &Launcher, gpkg: Option<PathBuf>) -> StageJob {
    let progress = Arc::new(stage::Progress::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let map = launcher.path_map();
    let data = launcher.data_dir.clone();
    let p = Arc::clone(&progress);
    std::thread::spawn(move || {
        let res = (|| {
            let ids = match gpkg {
                Some(g) => preflight::divide_ids(&g)
                    .map_err(|e| anyhow::anyhow!("reading divides from {}: {e}", g.display()))?,
                None => vec![],
            };
            stage::stage(&map, &data, &ids, &p)
        })();
        let _ = tx.send(res);
    });
    StageJob { progress, rx }
}

fn scroll_view(run: &mut RunState, delta: isize) {
    if let Some(p) = run.steps.get_mut(run.view).and_then(|s| s.proc.as_mut()) {
        p.scroll(delta);
    }
}

/// Path separators accepted in typed paths (`/` everywhere, also `\` on Windows).
fn is_sep(c: char) -> bool {
    c == '/' || c == std::path::MAIN_SEPARATOR
}

/// `~/rest` (or `~\rest` on Windows) -> `rest`.
fn strip_home_prefix(p: &str) -> Option<&str> {
    p.strip_prefix('~')
        .filter(|r| r.starts_with(is_sep))
        .map(|r| &r[1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// Nothing installed; docker selected.
    fn app(can_prompt: bool) -> App {
        let d = Detected {
            local_platform: crate::backend::LOCAL_SUPPORTED,
            ..Default::default()
        };
        let mut a = App::new(None, d, Some(BackendKind::Docker), "img".into(), Some(1));
        a.can_prompt = can_prompt;
        a
    }

    #[test]
    fn offer_needs_a_terminal() {
        let mut a = app(false);
        assert!(a.setup_offer().is_none());
        assert!(!a.menu_items().contains(&MenuItem::InstallBackend));
        a.request_run(vec![StepKind::Tool(Tool::BmiDriver)]);
        let Some(Popup::Message { body, .. }) = &a.popup else {
            panic!("expected a message")
        };
        assert!(body.contains("ngiab-rs backend install docker"), "{body}");
    }

    #[test]
    fn declined_offer_is_not_repeated() {
        let mut a = app(true);
        assert!(a.menu_items().contains(&MenuItem::InstallBackend));
        a.request_run(vec![StepKind::Tool(Tool::BmiDriver)]);
        assert!(matches!(a.popup, Some(Popup::OfferSetup(_))));
        a.on_key(key(KeyCode::Char('n')));
        assert!(a.popup.is_none());
        assert!(a.declined_install.contains(&BackendKind::Docker));
        assert!(a.want_installer.is_none());
        // Asking to run again shows the plain message, not the offer.
        a.request_run(vec![StepKind::Tool(Tool::BmiDriver)]);
        assert!(matches!(a.popup, Some(Popup::Message { .. })));
    }

    #[test]
    fn accepted_offer_queues_the_installer() {
        let mut a = app(true);
        a.request_run(vec![StepKind::Tool(Tool::BmiDriver)]);
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.want_installer, Some(vec![BackendKind::Docker]));
        assert!(a.popup.is_none());
    }

    #[test]
    fn picker_toggles_and_splits_bundle_only_local() {
        let mut a = app(true);
        a.activate(MenuItem::InstallBackend);
        let Some(Popup::PickBackends { items, .. }) = &a.popup else {
            panic!("expected the picker")
        };
        // Only the selected backend starts checked.
        assert!(
            items
                .iter()
                .all(|(k, on)| *on == (*k == BackendKind::Docker))
        );
        // Enter with nothing checked keeps the picker open.
        a.on_key(key(KeyCode::Char(' ')));
        let n = items_checked(&a);
        if n == 0 {
            a.on_key(key(KeyCode::Enter));
            assert!(matches!(a.popup, Some(Popup::PickBackends { .. })));
        }
        a.on_key(key(KeyCode::Esc));
        assert!(a.popup.is_none());

        if crate::backend::LOCAL_SUPPORTED {
            let mut a = app(true);
            a.detected.bmi_driver = Some("/bin/bmi-driver".into());
            a.detected.rs_route = Some("/bin/rs-route".into());
            a.start_setup(&[BackendKind::Local, BackendKind::Podman]);
            assert!(a.want_models_update);
            assert_eq!(a.want_installer, Some(vec![BackendKind::Podman]));
        }
    }

    fn items_checked(a: &App) -> usize {
        match &a.popup {
            Some(Popup::PickBackends { items, .. }) => items.iter().filter(|(_, on)| *on).count(),
            _ => 0,
        }
    }
}
