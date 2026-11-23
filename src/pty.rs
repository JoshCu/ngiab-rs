//! Run a command inside a pseudo-terminal and keep a vt100 model of its screen,
//! so tools that draw indicatif progress bars behave exactly as in a real terminal.

use crate::backend::{CommandSpec, kill_container};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

const SCROLLBACK: usize = 10_000;
/// Max time / bytes spent parsing output per poll so a chatty tool can't starve the UI.
const POLL_BUDGET: Duration = Duration::from_millis(15);
const POLL_BYTES: usize = 4 << 20;

pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    pub parser: vt100::Parser,
    pub spec: CommandSpec,
    size: (u16, u16),
    reader_done: bool,
    killed: bool,
    exit: Option<portable_pty::ExitStatus>,
    exited_at: Option<Instant>,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl PtyProcess {
    pub fn spawn(spec: CommandSpec, cwd: &Path, rows: u16, cols: u16) -> Result<Self> {
        let (rows, cols) = (rows.max(2), cols.max(10));
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning {}", spec.program))?;
        // Drop our copy of the slave so we see EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        // Bounded: if we fall behind, the reader blocks and the tool sees back-pressure.
        let (tx, rx) = sync_channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut parser = vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK);
        // Echo the command line so the user sees exactly what is being run.
        parser.process(format!("\x1b[2m$ {}\x1b[0m\r\n", spec.display()).as_bytes());

        Ok(Self {
            master: pair.master,
            writer,
            child,
            rx,
            parser,
            spec,
            size: (rows, cols),
            reader_done: false,
            killed: false,
            exit: None,
            exited_at: None,
        })
    }

    /// Drain pending output into the terminal model and reap the child.
    /// Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let start = Instant::now();
        let mut processed = 0;
        while processed < POLL_BYTES && start.elapsed() < POLL_BUDGET {
            match self.rx.try_recv() {
                Ok(bytes) => {
                    processed += bytes.len();
                    self.parser.process(&bytes);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reader_done = true;
                    break;
                }
            }
        }
        if self.exit.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit = Some(status);
            self.exited_at = Some(Instant::now());
            changed = true;
        }
        changed
    }

    /// Finished once the child exited and its output is fully drained (or a short
    /// grace period passed, in case a grandchild keeps the pty open).
    pub fn finished(&self) -> Option<&portable_pty::ExitStatus> {
        let exit = self.exit.as_ref()?;
        let drained = self.reader_done
            || self
                .exited_at
                .is_some_and(|t| t.elapsed() > Duration::from_millis(750));
        drained.then_some(exit)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(10));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.screen_mut().set_size(nz(rows), nz(cols));
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn kill(&mut self) {
        if self.exit.is_some() || self.killed {
            return;
        }
        self.killed = true;
        if let Some((rt, name)) = &self.spec.container {
            kill_container(rt, name);
        }
        let _ = self.child.kill();
        // Reap so we never signal a stale (possibly reused) pid again.
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit = Some(status);
            self.exited_at = Some(Instant::now());
        }
    }

    /// Contents of the live screen, ignoring any scrollback the user is viewing.
    pub fn live_contents(&mut self) -> String {
        let off = self.parser.screen().scrollback();
        if off == 0 {
            return self.parser.screen().contents();
        }
        self.parser.screen_mut().set_scrollback(0);
        let c = self.parser.screen().contents();
        self.parser.screen_mut().set_scrollback(off);
        c
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Scroll the view back by `delta` lines (positive = further back).
    pub fn scroll(&mut self, delta: isize) {
        let cur = self.parser.screen().scrollback() as isize;
        let new = (cur + delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(new);
    }

    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Progress parsing
// ---------------------------------------------------------------------------

/// Find the most recent `pos/len` counter on screen (indicatif renders
/// `... 123/456 (12/s) total` for bmi-driver and `... 12/34 nodes` for rs-route).
pub fn parse_progress(contents: &str) -> Option<(u64, u64)> {
    for line in contents.lines().rev() {
        if let Some(p) = last_fraction(line) {
            return Some(p);
        }
    }
    None
}

fn last_fraction(line: &str) -> Option<(u64, u64)> {
    let b = line.as_bytes();
    let mut found = None;
    for (i, &c) in b.iter().enumerate() {
        if c != b'/' || i == 0 {
            continue;
        }
        let mut s = i;
        while s > 0 && b[s - 1].is_ascii_digit() {
            s -= 1;
        }
        let mut e = i + 1;
        while e < b.len() && b[e].is_ascii_digit() {
            e += 1;
        }
        if s == i || e == i + 1 {
            continue;
        }
        // Reject things like paths or dates glued to other characters: "a1/2".
        if s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'/' || b[s - 1] == b'.') {
            continue;
        }
        if e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'/' || b[e] == b'.') {
            continue;
        }
        let pos: u64 = line[s..i].parse().ok()?;
        let len: u64 = line[i + 1..e].parse().ok()?;
        if len > 0 && pos <= len {
            found = Some((pos, len));
        }
    }
    found
}

/// Is the tool waiting on a yes/no answer (bmi-driver's mapping suggestion)?
pub fn awaiting_answer(contents: &str) -> bool {
    contents
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim_end().ends_with("[y/N]") || l.trim_end().ends_with("[Y/n]"))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

pub struct TermView<'a> {
    pub screen: &'a vt100::Screen,
}

fn conv(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

impl Widget for TermView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        let rows = rows.get().min(area.height);
        let cols = cols.get().min(area.width);
        for r in 0..rows {
            for c in 0..cols {
                let Some(cell) = self.screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default();
                if let Some(fg) = conv(cell.fgcolor()) {
                    style = style.fg(fg);
                }
                if let Some(bg) = conv(cell.bgcolor()) {
                    style = style.bg(bg);
                }
                let mut m = Modifier::empty();
                if cell.bold() {
                    m |= Modifier::BOLD;
                }
                if cell.dim() {
                    m |= Modifier::DIM;
                }
                if cell.italic() {
                    m |= Modifier::ITALIC;
                }
                if cell.underline() {
                    m |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    m |= Modifier::REVERSED;
                }
                style = style.add_modifier(m);
                let sym = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                if let Some(target) = buf.cell_mut((area.x + c, area.y + r)) {
                    target.set_symbol(sym).set_style(style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bmi_driver() {
        let s = "Found stuff\n⠁ [00:00:03] ━━━━━━╸──── 120/530 (40.1/s) total\n\n";
        assert_eq!(parse_progress(s), Some((120, 530)));
    }

    #[test]
    fn progress_rs_route() {
        let s = "Starting parallel wave-front routing...\n⠁ [00:00:01] [####>---] 12/53 nodes (3s)";
        assert_eq!(parse_progress(s), Some((12, 53)));
    }

    #[test]
    fn ignores_paths_and_dates() {
        assert_eq!(
            parse_progress("reading /data/outputs/1/2.csv 2010-01-01"),
            None
        );
        assert_eq!(parse_progress("cpu 1/0"), None);
    }

    #[test]
    fn prompt_detection() {
        assert!(awaiting_answer(
            "Add these mappings to realization.json? [y/N]\n\n"
        ));
        assert!(!awaiting_answer("running"));
    }

    #[test]
    fn poll_is_bounded_under_flood() {
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "yes 'Musk Cunge WARNING: Failure to converge on node 12345 timestep 99'".into(),
            ],
            container: None,
        };
        let mut p = PtyProcess::spawn(spec, Path::new("/"), 40, 150).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..5 {
            let t = Instant::now();
            p.poll();
            assert!(
                t.elapsed() < Duration::from_millis(250),
                "poll took {:?}",
                t.elapsed()
            );
        }
        p.kill();
    }

    #[test]
    fn pty_runs_command() {
        let spec = CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "printf 'hello\\n3/9 nodes\\n'".into()],
            container: None,
        };
        let mut p = PtyProcess::spawn(spec, Path::new("/"), 10, 40).unwrap();
        let start = Instant::now();
        while p.finished().is_none() && start.elapsed() < Duration::from_secs(5) {
            p.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(p.finished().unwrap().success());
        let c = p.screen().contents();
        assert!(c.contains("hello"), "{c}");
        assert_eq!(parse_progress(&c), Some((3, 9)));
    }
}
