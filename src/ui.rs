//! Rendering for every screen.

use crate::app::{App, MenuItem, Popup, Screen, StepStatus};
use crate::eval::{EvalState, Tab, fmt_time};
use crate::plan::StepKind;
use crate::preflight::Level;
use crate::pty::TermView;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Chart, Clear, Dataset, Gauge, GraphType, List, ListItem,
    ListState, Paragraph, Wrap,
};
use std::time::Duration;

const ACCENT: Color = Color::Rgb(0x3a, 0xa7, 0xe8);
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, app);
    match app.screen {
        Screen::PathInput => draw_path_input(f, body, app),
        Screen::Home => draw_home(f, body, app),
        Screen::Run => draw_run(f, body, app),
        Screen::Eval => draw_eval(f, body, app),
    }
    draw_footer(f, footer, app);
    if let Some(p) = &app.popup {
        draw_popup(f, p);
    }
}

fn level_style(l: Level) -> Style {
    match l {
        Level::Ok => Style::new().fg(Color::Green),
        Level::Info => Style::new().fg(ACCENT),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::Fail => Style::new().fg(Color::Red).bold(),
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m{:02}s", s / 3600, (s / 60) % 60, s % 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(" NGIAB-RS ").bold().fg(ACCENT))
        .title(
            Line::from(" NextGen In A Box · Rust ")
                .right_aligned()
                .fg(DIM),
        );
    let data = app
        .data_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".into());
    let backend = app
        .backend
        .map_or("none".to_string(), |b| b.name().to_string());
    let mut spans = vec![
        Span::raw(" data ").fg(DIM),
        Span::raw(data).bold(),
        Span::raw("   backend ").fg(DIM),
        Span::raw(backend).fg(Color::Cyan).bold(),
        Span::raw("   jobs ").fg(DIM),
        Span::raw(app.jobs.to_string()).fg(Color::Cyan).bold(),
    ];
    if app.backend.is_some_and(|b| b.is_container()) {
        spans.push(Span::raw("   image ").fg(DIM));
        spans.push(Span::raw(app.image.clone()));
    }
    if app.detected.in_container {
        spans.push(Span::raw("   (in container)").fg(DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = match app.screen {
        Screen::PathInput => &[
            ("enter", "select"),
            ("tab", "complete"),
            ("ctrl-u", "clear"),
            ("esc", "back/quit"),
        ],
        Screen::Home => &[
            ("↑↓/1-9", "menu"),
            ("enter", "select"),
            ("b", "backend"),
            ("+/-", "jobs"),
            ("r", "run"),
            ("e", "evaluate"),
            ("p", "preflight"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Screen::Run if app.running() => &[
            ("ctrl-c", "cancel"),
            ("pgup/pgdn", "scroll"),
            ("tab", "switch step"),
            ("keys", "sent to tool"),
        ],
        Screen::Run => &[
            ("q/enter", "home"),
            ("e", "evaluate"),
            ("pgup/pgdn", "scroll"),
            ("tab", "switch step"),
        ],
        Screen::Eval => &[
            ("tab", "hydrograph/catchment"),
            ("↑↓", "select"),
            ("←→", "variable"),
            ("/", "filter"),
            ("f", "next file"),
            ("r", "reload"),
            ("q", "back"),
        ],
    };
    let mut spans = Vec::new();
    if let Some((msg, _)) = &app.status {
        spans.push(Span::raw(format!(" {msg} ")).black().on_yellow());
        spans.push(Span::raw(" "));
    }
    for (k, d) in keys {
        spans.push(Span::raw(format!(" {k} ")).black().on_dark_gray());
        spans.push(Span::raw(format!(" {d}  ")).fg(DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------

fn draw_path_input(f: &mut Frame, area: Rect, app: &App) {
    let [_, mid, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(14),
        Constraint::Fill(2),
    ])
    .areas(area);
    let [_, mid, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Max(90),
        Constraint::Fill(1),
    ])
    .areas(mid);
    let mut lines = vec![
        Line::from("Select a data folder").bold(),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  → ").fg(Color::Yellow),
            Span::raw("config/").fg(Color::Green),
            Span::raw("   realization.json + hydrofabric .gpkg").fg(DIM),
        ]),
        Line::from(vec![
            Span::raw("  → ").fg(Color::Yellow),
            Span::raw("forcings/").fg(Color::Blue),
            Span::raw(" meteorological forcing data").fg(DIM),
        ]),
        Line::from(vec![
            Span::raw("  → ").fg(Color::Yellow),
            Span::raw("outputs/").fg(Color::Magenta),
            Span::raw("  simulation results (created if missing)").fg(DIM),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw(" path: ").fg(DIM),
            Span::raw(app.path_input.clone()).bold(),
            Span::raw("█").fg(ACCENT),
        ]),
        Line::raw(""),
    ];
    if let Some(e) = &app.path_error {
        lines.push(Line::from(format!(" ✗ {e}")).red());
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Data folder ");
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        mid,
    );
}

// ---------------------------------------------------------------------------

fn draw_home(f: &mut Frame, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(46), Constraint::Min(30)]).areas(area);
    let [menu_area, backend_area] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(7)]).areas(left);

    let items: Vec<ListItem> = app
        .menu_items()
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut style = Style::new();
            if matches!(m, MenuItem::RunAll | MenuItem::RunBmi | MenuItem::RunRoute)
                && !app.preflight.as_ref().is_some_and(|p| p.can_run())
            {
                style = style.fg(DIM);
            }
            ListItem::new(Line::from(vec![
                Span::raw(format!("{} ", i + 1)).fg(DIM),
                Span::styled(m.label(), style),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.menu_idx));
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Menu "),
        )
        .highlight_style(Style::new().bg(ACCENT).fg(Color::Black).bold())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, menu_area, &mut state);

    // Backends
    let mut lines = Vec::new();
    for k in crate::backend::BackendKind::ALL {
        let avail = app.detected.available(k);
        let selected = app.backend == Some(k);
        let mark = if selected {
            "●"
        } else if avail {
            "○"
        } else {
            "✗"
        };
        let style = if selected {
            Style::new().fg(Color::Cyan).bold()
        } else if avail {
            Style::new()
        } else {
            Style::new().fg(DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {mark} {:<7}", k.name()), style),
            Span::raw(app.detected.summary(k)).fg(DIM),
        ]));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Backends (b to switch) ");
    f.render_widget(Paragraph::new(lines).block(block), backend_area);

    // Preflight
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Preflight ");
    let Some(pf) = &app.preflight else {
        f.render_widget(
            Paragraph::new("No backend selected - install bmi-driver/rs-route, docker or podman.")
                .block(block)
                .wrap(Wrap { trim: true }),
            right,
        );
        return;
    };
    let mut lines: Vec<Line> = pf
        .checks
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::styled(format!(" {} ", c.level.symbol()), level_style(c.level)),
                Span::raw(format!("{:<18}", c.name)).bold(),
                Span::raw(c.detail.clone()),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    let (msg, style) = match pf.worst() {
        Level::Fail => (
            "Fix the ✗ items above before running.",
            level_style(Level::Fail),
        ),
        Level::Warn => ("Ready to run (with warnings).", level_style(Level::Warn)),
        _ => ("Ready to run.", level_style(Level::Ok).bold()),
    };
    lines.push(Line::styled(format!(" {msg}"), style));
    if let Some(run) = &app.run {
        let s = if run.finished {
            if run.succeeded() {
                "last run: succeeded (v to view)"
            } else {
                "last run: failed/cancelled (v to view)"
            }
        } else {
            "run in progress (v to view)"
        };
        lines.push(Line::from(format!(" {s}")).fg(DIM));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        right,
    );
}

// ---------------------------------------------------------------------------

fn draw_run(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(run) = &app.run else { return };
    let n = run.steps.len() as u16;
    let [steps_area, term_area] =
        Layout::vertical([Constraint::Length(n * 2 + 2), Constraint::Min(4)]).areas(area);

    // Step list with gauges
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Run ")
        .title(
            Line::from(match (run.finished, run.succeeded()) {
                (false, _) => format!(" running {} ", fmt_dur(run.started.elapsed())),
                (true, true) => format!(
                    " finished in {} ✓ ",
                    fmt_dur(run.elapsed.unwrap_or_default())
                ),
                (true, false) => " stopped ".to_string(),
            })
            .right_aligned(),
        );
    let inner = block.inner(steps_area);
    f.render_widget(block, steps_area);
    let rows = Layout::vertical(vec![Constraint::Length(2); run.steps.len()]).split(inner);
    for (i, (step, row)) in run.steps.iter().zip(rows.iter()).enumerate() {
        let [label_area, gauge_area] =
            Layout::horizontal([Constraint::Length(44), Constraint::Min(10)]).areas(*row);
        let (sym, style, note) = match &step.status {
            StepStatus::Pending => ("·", Style::new().fg(DIM), String::new()),
            StepStatus::Running => ("▶", Style::new().fg(Color::Cyan).bold(), String::new()),
            StepStatus::Done => ("✓", Style::new().fg(Color::Green), String::new()),
            StepStatus::Failed(e) => ("✗", Style::new().fg(Color::Red).bold(), e.clone()),
            StepStatus::Skipped(e) => ("–", Style::new().fg(DIM), format!("skipped: {e}")),
            StepStatus::Cancelled => ("✗", Style::new().fg(Color::Yellow), "cancelled".into()),
        };
        let viewing = if i == run.view { "◀ viewing" } else { "" };
        let elapsed = step.elapsed().map(fmt_dur).unwrap_or_default();
        let label = vec![
            Line::from(vec![
                Span::styled(format!(" {sym} {:<11}", step.kind.label()), style),
                Span::raw(format!("{elapsed:>9}  ")),
                Span::raw(viewing).fg(ACCENT),
            ]),
            Line::from(format!("   {note}")).fg(DIM),
        ];
        f.render_widget(Paragraph::new(label), label_area);

        let gauge_area = Rect {
            height: 1,
            ..gauge_area
        };
        let (ratio, text) = match (step.progress, &step.status) {
            (_, StepStatus::Done) => (
                1.0,
                step.progress
                    .map_or("done".into(), |(_, l)| format!("{l}/{l}")),
            ),
            (Some((p, l)), _) => (
                p as f64 / l as f64,
                format!("{p}/{l}  {:.0}%", 100.0 * p as f64 / l as f64),
            ),
            (None, StepStatus::Running) => (0.0, "starting…".into()),
            _ => (0.0, String::new()),
        };
        let unit = match step.kind {
            StepKind::Tool(crate::backend::Tool::BmiDriver) => " catchments",
            StepKind::Tool(crate::backend::Tool::RsRoute) => " nodes",
            StepKind::Pull => "",
        };
        let color = match step.status {
            StepStatus::Done => Color::Green,
            StepStatus::Failed(_) | StepStatus::Cancelled => Color::Red,
            _ => ACCENT,
        };
        let label = if step.progress.is_some() {
            format!("{text}{unit}")
        } else {
            text
        };
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::new().fg(color).bg(Color::Rgb(30, 30, 40)))
                .ratio(ratio.clamp(0.0, 1.0))
                .label(label)
                .use_unicode(true),
            gauge_area,
        );
    }

    // Embedded terminal
    let step = &run.steps[run.view.min(run.steps.len() - 1)];
    let mut title = format!(" {} ", step.kind.label());
    if let Some(p) = &step.proc
        && p.scrollback() > 0
    {
        title.push_str(&format!("[scrolled back {} lines] ", p.scrollback()));
    }
    let awaiting = run.awaiting_input() && run.view == run.current;
    let border = if awaiting { Color::Yellow } else { DIM };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(title);
    if awaiting {
        block = block.title(
            Line::from(" tool is asking a question - type your answer and press enter ")
                .right_aligned()
                .black()
                .on_yellow(),
        );
    }
    let inner = block.inner(term_area);
    f.render_widget(block, term_area);
    app.pane = (inner.height.max(2), inner.width.max(10));
    let Some(run) = &app.run else { return };
    let step = &run.steps[run.view.min(run.steps.len() - 1)];
    match &step.proc {
        Some(p) => f.render_widget(TermView { screen: p.screen() }, inner),
        None => {
            let msg = match &step.status {
                StepStatus::Skipped(e) => format!("skipped: {e}"),
                StepStatus::Failed(e) => format!("failed to start: {e}"),
                _ => "waiting…".into(),
            };
            f.render_widget(Paragraph::new(msg).fg(DIM), inner);
        }
    }
}

// ---------------------------------------------------------------------------

fn draw_eval(f: &mut Frame, area: Rect, app: &App) {
    let Some(ev) = &app.eval else { return };
    let [list_area, chart_area] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(30)]).areas(area);

    // Item list
    let tab_title = Line::from(vec![
        Span::styled(" Hydrograph ", tab_style(ev.tab == Tab::Hydrograph)),
        Span::raw("│"),
        Span::styled(" Catchment ", tab_style(ev.tab == Tab::Catchment)),
    ]);
    let count = format!(" {} ", ev.visible.len());
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(tab_title)
        .title_bottom(Line::from(count).right_aligned());
    if ev.filtering || !ev.filter.is_empty() {
        let cursor = if ev.filtering { "█" } else { "" };
        block =
            block.title_bottom(Line::from(format!(" /{}{cursor} ", ev.filter)).fg(Color::Yellow));
    }
    let inner_h = list_area.height.saturating_sub(2) as usize;
    // Only build list items for the visible window (catchment dirs can be huge).
    let start = ev
        .selected
        .saturating_sub(inner_h / 2)
        .min(ev.visible.len().saturating_sub(inner_h));
    let items: Vec<ListItem> = ev
        .visible
        .iter()
        .skip(start)
        .take(inner_h)
        .map(|&i| ListItem::new(ev.item_label(i)))
        .collect();
    let mut state =
        ListState::default().with_selected((!ev.visible.is_empty()).then(|| ev.selected - start));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(ACCENT).fg(Color::Black).bold())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, list_area, &mut state);

    draw_chart(f, chart_area, ev);
}

fn tab_style(active: bool) -> Style {
    if active {
        Style::new().fg(Color::Black).bg(ACCENT).bold()
    } else {
        Style::new().fg(DIM)
    }
}

fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if a != 0.0 && (a >= 1e5 || a < 1e-3) {
        format!("{v:.3e}")
    } else {
        format!("{v:.3}")
    }
}

fn draw_chart(f: &mut Frame, area: Rect, ev: &EvalState) {
    let [info_area, chart_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(5)]).areas(area);

    // Info / stats panel
    let source = match ev.tab {
        Tab::Hydrograph => ev
            .nc_files
            .get(ev.nc_idx)
            .map(|p| {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if ev.nc_files.len() > 1 {
                    format!("{name} ({}/{}, f: next)", ev.nc_idx + 1, ev.nc_files.len())
                } else {
                    name
                }
            })
            .unwrap_or_else(|| format!("no .nc files in {}", ev.troute_dir.display())),
        Tab::Catchment => {
            if ev.cats.is_empty() {
                format!("no cat-*.csv in {}", ev.cat_dir.display())
            } else {
                ev.cat_dir.display().to_string()
            }
        }
    };
    let vars: Vec<String> = match ev.tab {
        Tab::Hydrograph => ev
            .troute
            .as_ref()
            .map(|t| t.vars.clone())
            .unwrap_or_default(),
        Tab::Catchment => ev
            .csv
            .as_ref()
            .map(|c| c.columns.clone())
            .unwrap_or_default(),
    };
    let cur = ev.current_var();
    let mut var_spans = vec![Span::raw(" variable ").fg(DIM)];
    for v in &vars {
        let style = if Some(v) == cur.as_ref() {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::new().fg(DIM)
        };
        var_spans.push(Span::styled(format!(" {v} "), style));
        var_spans.push(Span::raw(" "));
    }
    let mut lines = vec![
        Line::from(vec![Span::raw(" source ").fg(DIM), Span::raw(source)]),
        Line::from(var_spans),
    ];
    if let Some(st) = ev.series.as_ref().and_then(|s| s.stats()) {
        lines.push(Line::from(vec![
            Span::raw(" min ").fg(DIM),
            Span::raw(fmt_num(st.min)),
            Span::raw("  mean ").fg(DIM),
            Span::raw(fmt_num(st.mean)),
            Span::raw("  max ").fg(DIM),
            Span::raw(fmt_num(st.max)).bold(),
            Span::raw(" @ ").fg(DIM),
            Span::raw(fmt_time(st.peak_t)),
            Span::raw(format!("  ({} values)", st.n)).fg(DIM),
        ]));
    }
    f.render_widget(Paragraph::new(lines), info_area);

    let block = Block::bordered().border_type(BorderType::Rounded);
    let Some(series) = &ev.series else {
        let msg = ev
            .error
            .clone()
            .unwrap_or_else(|| "nothing selected".into());
        f.render_widget(
            Paragraph::new(msg)
                .fg(DIM)
                .block(block)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            chart_area,
        );
        return;
    };
    let Some(st) = series.stats() else {
        f.render_widget(
            Paragraph::new("series has no valid values")
                .fg(DIM)
                .block(block),
            chart_area,
        );
        return;
    };
    let width = chart_area.width.saturating_sub(12).max(10) as usize;
    let data = series.decimated(width * 2);
    let (t0, t1) = (
        *series.t.first().unwrap_or(&0) as f64,
        *series.t.last().unwrap_or(&1) as f64,
    );
    let t1 = if t1 <= t0 { t0 + 1.0 } else { t1 };
    let (mut lo, mut hi) = (st.min, st.max);
    if (hi - lo).abs() < f64::EPSILON {
        lo -= 1.0;
        hi += 1.0;
    }
    let pad = (hi - lo) * 0.05;
    let (lo, hi) = (
        if st.min >= 0.0 {
            (lo - pad).max(0.0)
        } else {
            lo - pad
        },
        hi + pad,
    );

    let x_labels: Vec<Span> = (0..=3)
        .map(|i| {
            let t = t0 + (t1 - t0) * i as f64 / 3.0;
            Span::raw(fmt_time(t as i64).chars().take(10).collect::<String>())
        })
        .collect();
    let y_labels: Vec<Span> = (0..=4)
        .map(|i| Span::raw(fmt_num(lo + (hi - lo) * i as f64 / 4.0)))
        .collect();
    let y_title = if series.units.is_empty() {
        cur.clone().unwrap_or_default()
    } else {
        series.units.clone()
    };
    let dataset = Dataset::default()
        .name(series.title.clone())
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::new().fg(Color::Cyan))
        .data(&data);
    let chart = Chart::new(vec![dataset])
        .block(block.title(Line::from(format!(" {} ", series.title)).bold()))
        .x_axis(
            Axis::default()
                .style(Style::new().fg(DIM))
                .bounds([t0, t1])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title(Span::raw(y_title).fg(DIM))
                .style(Style::new().fg(DIM))
                .bounds([lo, hi])
                .labels(y_labels),
        )
        .legend_position(None);
    f.render_widget(chart, chart_area);
}

// ---------------------------------------------------------------------------

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(r);
    r
}

fn draw_popup(f: &mut Frame, p: &Popup) {
    let (title, text, color): (String, Text, Color) = match p {
        Popup::Cleanup { files, dirs, .. } => {
            let mut lines = vec![
                Line::from(format!(
                    "{files} files already exist in the output folders:"
                )),
                Line::raw(""),
            ];
            for (d, n) in dirs {
                lines.push(Line::from(format!("  {}  ({n} files)", d.display())).fg(Color::Cyan));
            }
            lines.extend([
                Line::raw(""),
                Line::from(vec![
                    Span::raw(" d ").black().on_red(),
                    Span::raw(" delete them and run a fresh simulation"),
                ]),
                Line::from(vec![
                    Span::raw(" k ").black().on_green(),
                    Span::raw(" keep them and run (files get overwritten)"),
                ]),
                Line::from(vec![
                    Span::raw(" esc ").black().on_dark_gray(),
                    Span::raw(" cancel"),
                ]),
            ]);
            ("Existing outputs".into(), Text::from(lines), Color::Yellow)
        }
        Popup::ConfirmCancel => (
            "Cancel run?".into(),
            Text::from(vec![
                Line::from("Stop the running step? Partial outputs will be left in place."),
                Line::raw(""),
                Line::from(vec![
                    Span::raw(" y ").black().on_red(),
                    Span::raw(" stop   "),
                    Span::raw(" n ").black().on_dark_gray(),
                    Span::raw(" keep running"),
                ]),
            ]),
            Color::Red,
        ),
        Popup::Message { title, body } => {
            let mut t = Text::from(body.clone());
            t.push_line(Line::raw(""));
            t.push_line(Line::from("press any key").fg(DIM));
            (title.clone(), t, Color::Yellow)
        }
        Popup::Help => (
            "Help".into(),
            Text::from(vec![
                Line::from("ngiab-rs runs bmi-driver then rs-route on a NextGen data folder.")
                    .bold(),
                Line::raw(""),
                Line::from("Backends: local binaries, docker or podman (b to switch)."),
                Line::from("Container runs mount the data folder at /data and run as your user."),
                Line::from("While a tool runs its terminal is shown live; anything you type"),
                Line::from("is sent to it (e.g. bmi-driver's unit-mapping [y/N] prompt)."),
                Line::raw(""),
                Line::from("Evaluate: hydrographs from outputs/troute/*.nc (flow/velocity/depth)"),
                Line::from("and any variable from the catchment cat-*.csv files."),
                Line::raw(""),
                Line::from(
                    "CLI:  ngiab-rs [DATA_DIR] [--auto] [-j N] [--backend local|docker|podman]",
                ),
                Line::raw(""),
                Line::from("press any key").fg(DIM),
            ]),
            ACCENT,
        ),
    };
    // Size to content, clamped to the screen; account for lines that will wrap.
    let screen = f.area();
    let w = (text.width().max(title.len() + 4) as u16 + 4)
        .min(screen.width.saturating_sub(2))
        .max(10);
    let inner_w = w.saturating_sub(2).max(1) as usize;
    let wrapped: usize = text
        .lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(inner_w))
        .sum();
    let h = (wrapped as u16 + 2).min(screen.height);
    let area = centered(screen, w, h);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(format!(" {title} ")).bold().fg(color));
    f.render_widget(
        Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::new().add_modifier(Modifier::empty())),
        area,
    );
}
