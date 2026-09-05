//! Rendering. Knows nothing about where jobs come from — it is handed a
//! `Snapshot` and a selection and draws them.

use chrono::Utc;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::app::{App, Row};
use crate::job::{age, Job, Status};

/// Keep the summary as long as this many columns are left for it. Claude
/// Code's own panel keeps a truncated summary in a narrow pane, and a sidebar
/// with only names tells you far less, so the summary is the last thing to go.
const MIN_SUMMARY: usize = 8;
/// Below this width the resume command in the footer wraps onto a second line.
const WRAPS: u16 = 60;
/// Below this height the detail footer is dropped to keep rows visible.
const SHORT: u16 = 16;

pub fn draw(frame: &mut Frame, app: &mut App) {
    draw_in(frame, frame.area(), app, true);
}

/// Draw the panel into `area`. `focused` is false when the panel is a column
/// beside a working pane that currently has the keyboard.
pub fn draw_in(frame: &mut Frame, area: Rect, app: &mut App, focused: bool) {
    let show_detail = area.height >= SHORT && app.selected_job().is_some();

    let chunks = Layout::vertical([
        Constraint::Length(2),                                // header
        Constraint::Min(1),                                   // rows
        Constraint::Length(detail_height(area, show_detail)), // detail
        Constraint::Length(1),                                // keys
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    draw_rows(frame, chunks[1], app);
    if show_detail {
        draw_detail(frame, chunks[2], app);
    }
    draw_footer(frame, chunks[3], app, focused);
}

/// The detail footer needs an extra line in narrow panes, where the resume
/// command wraps rather than fitting on one.
fn detail_height(area: Rect, show_detail: bool) -> u16 {
    if !show_detail {
        0
    } else if area.width < WRAPS {
        6
    } else {
        4
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let snap = &app.snapshot;
    let counts = Line::from(vec![
        count_span(snap.count(Status::NeedsInput), "needs input", Color::Yellow),
        Span::raw(" · "),
        count_span(snap.count(Status::Working), "working", Color::Cyan),
        Span::raw(" · "),
        count_span(snap.count(Status::Done), "done", Color::DarkGray),
    ]);

    let title = Line::from(vec![
        Span::styled(
            "SAVRAS",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if app.watching {
                "  ◦ live"
            } else {
                "  ◦ polling"
            },
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    frame.render_widget(Paragraph::new(vec![title, counts]), area);
}

fn count_span(n: usize, label: &str, color: Color) -> Span<'static> {
    let style = if n == 0 {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(color)
    };
    Span::styled(format!("{n} {label}"), style)
}

fn draw_rows(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.snapshot.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "No Claude Code sessions.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Start one and it appears here.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, area);
        return;
    }

    let name_width = app
        .snapshot
        .jobs
        .iter()
        .map(|j| j.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 12)
        .min((area.width / 3) as usize) as u16;

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| match row {
            Row::Heading(status) => ListItem::new(Line::from(Span::styled(
                status.heading().to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ))),
            Row::Job(i) => ListItem::new(job_line(&app.snapshot.jobs[*i], name_width, area.width)),
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(Color::Indexed(236)));
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn job_line(job: &Job, name_width: u16, total_width: u16) -> Line<'static> {
    let age = age(job.updated_at, Utc::now());
    let color = badge_color(job);

    let mut spans = vec![
        Span::styled(
            "✳ ",
            Style::default().fg(match job.status {
                Status::NeedsInput => Color::Yellow,
                Status::Working => color,
                Status::Done => Color::Green,
            }),
        ),
        Span::styled(
            pad(&job.name, name_width as usize),
            Style::default().fg(Color::Black).bg(color),
        ),
    ];

    // Columns already spent: bullet + name + gap + age.
    let fixed = 2 + name_width + 2 + 4;
    let room = (total_width.saturating_sub(fixed)) as usize;
    if room >= MIN_SUMMARY {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(&job.summary, room),
            Style::default().fg(Color::Gray),
        ));
    }

    // Right-align the age against the pane edge.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad_to = (total_width as usize).saturating_sub(used + age.chars().count());
    spans.push(Span::raw(" ".repeat(pad_to)));
    spans.push(Span::styled(age, Style::default().fg(Color::DarkGray)));

    Line::from(spans)
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let Some(job) = app.selected_job() else {
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(job.short_cwd(), Style::default().fg(Color::Blue)),
        Span::styled(
            format!("  {} tokens", thousands(job.tokens)),
            Style::default().fg(Color::DarkGray),
        ),
    ])];

    if !job.links.is_empty() {
        let links = job
            .links
            .iter()
            .map(|l| format!("{}#{}", l.kind, l.id))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(Span::styled(
            links,
            Style::default().fg(Color::Magenta),
        )));
    }

    lines.push(Line::from(Span::styled(
        job.resume_command(),
        Style::default().fg(Color::Green),
    )));

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::ZERO);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let text = match (&app.error, focused) {
        (Some(err), _) => Span::styled(
            truncate(err, area.width as usize),
            Style::default().fg(Color::Red),
        ),
        (None, true) => Span::styled(
            "↑↓ move · r refresh · q quit",
            Style::default().fg(Color::DarkGray),
        ),
        // The panel is a column beside a pane that has the keyboard.
        (None, false) => Span::styled("ctrl-g to focus", Style::default().fg(Color::DarkGray)),
    };
    frame.render_widget(Paragraph::new(Line::from(text)), area);
}

/// Claude Code stores a colour name per job; fall back to a stable colour
/// derived from the name so unnamed or new-coloured jobs still read distinctly.
fn badge_color(job: &Job) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Red,
    ];
    match job.color.as_deref() {
        Some("cyan") => Color::Cyan,
        Some("magenta") => Color::Magenta,
        Some("green") => Color::Green,
        Some("yellow") => Color::Yellow,
        Some("blue") => Color::Blue,
        Some("red") => Color::Red,
        Some("white") => Color::White,
        Some("gray") | Some("grey") => Color::Gray,
        _ => {
            let sum: usize = job.name.bytes().map(|b| b as usize).sum();
            PALETTE[sum % PALETTE.len()]
        }
    }
}

fn pad(s: &str, width: usize) -> String {
    let mut out = truncate(s, width);
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::testing::Fixture;
    use ratatui::backend::TestBackend;

    /// Renders into a real terminal buffer and returns it as plain lines.
    fn render(fixture: &Fixture, w: u16, h: u16) -> Vec<String> {
        let mut app = App::new(fixture.0.clone());
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn three_jobs() -> Fixture {
        Fixture::new("ui")
            .job(
                "aaa",
                r#"{"state":"working","name":"ASK","color":"cyan","detail":"d",
                    "needs":"answer: which one?","cwd":"/tmp/repo","tokens":11547,
                    "sessionId":"sess-ask"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"ROADMAP","detail":"compiling the plan",
                    "sessionId":"sess-road"}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"FIN","output":{"result":"shipped it"},
                    "children":[{"id":"357","kind":"pr","href":"http://x/357"}],
                    "sessionId":"sess-fin"}"#,
            )
    }

    #[test]
    fn wide_panel_shows_every_group_and_column() {
        let lines = render(&three_jobs(), 80, 24);
        let text = lines.join("\n");

        assert!(text.contains("SAVRAS"));
        assert!(text.contains("1 needs input"));
        assert!(text.contains("1 working"));
        assert!(text.contains("1 done"));
        for heading in ["Needs input", "Working", "Completed"] {
            assert!(text.contains(heading), "missing heading {heading}");
        }
        assert!(text.contains("answer: which one?"));
        assert!(text.contains("shipped it"));
    }

    #[test]
    fn name_and_summary_do_not_run_together() {
        // The longest name defines the column, so it is the one at risk of
        // touching the summary next to it.
        let lines = render(&three_jobs(), 80, 24);
        let row = lines
            .iter()
            .find(|l| l.contains("ROADMAP"))
            .expect("ROADMAP row");
        assert!(
            row.contains("ROADMAP  compiling the plan"),
            "name column collided with the summary: {row:?}"
        );
    }

    #[test]
    fn age_is_right_aligned_against_the_pane_edge() {
        let lines = render(&three_jobs(), 80, 24);
        let row = lines.iter().find(|l| l.contains("ROADMAP")).unwrap();
        // Fixtures carry no updatedAt, so the age renders as "-".
        assert!(row.ends_with('-'), "age not flush right: {row:?}");
        assert_eq!(row.chars().count(), 80);
    }

    #[test]
    fn a_sidebar_width_pane_keeps_a_truncated_summary() {
        // 44 columns is the default side-panel width. A column of bare names
        // says far less than a clipped sentence, so a long summary is
        // truncated rather than dropped — the trade Claude Code's panel makes.
        let fixture = Fixture::new("ui-sidebar").job(
            "aaa",
            r#"{"state":"working","name":"ROADMAP",
                "detail":"58 tier-A items on origin/main at ba75f62 across seven groups"}"#,
        );
        let lines = render(&fixture, 44, 26);
        let row = lines.iter().find(|l| l.contains("ROADMAP")).unwrap();

        assert!(
            row.contains("58 tier-A"),
            "summary dropped too early: {row:?}"
        );
        assert!(
            row.contains('…'),
            "long summary should be truncated: {row:?}"
        );
        assert_eq!(row.chars().count(), 44, "row must not overflow: {row:?}");
        assert!(row.ends_with('-'), "age must stay flush right: {row:?}");
    }

    #[test]
    fn a_very_narrow_pane_finally_drops_the_summary() {
        // Below a readable slice, the summary is worse than nothing.
        let lines = render(&three_jobs(), 18, 26);
        let text = lines.join("\n");
        assert!(text.contains("ROADMAP") || text.contains("ROADM"));
        assert!(!text.contains("compiling"));
    }

    #[test]
    fn narrow_panel_still_shows_a_usable_resume_command() {
        let lines = render(&three_jobs(), 34, 26);
        let joined: String = lines
            .join("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("claude --resume sess-ask"),
            "resume command was clipped: {joined:?}"
        );
    }

    #[test]
    fn detail_shows_the_selected_job_only() {
        let lines = render(&three_jobs(), 80, 24);
        let text = lines.join("\n");
        // Selection starts on the first job, which is the one needing input.
        assert!(text.contains("claude --resume sess-ask"));
        assert!(!text.contains("sess-fin"));
        assert!(text.contains("11,547 tokens"));
    }

    #[test]
    fn a_short_pane_drops_the_detail_footer_to_keep_rows_visible() {
        let lines = render(&three_jobs(), 80, 12);
        let text = lines.join("\n");
        assert!(!text.contains("claude --resume"));
        assert!(text.contains("ROADMAP"), "rows must survive a short pane");
    }

    #[test]
    fn empty_state_is_explained_not_blank() {
        let fixture = Fixture::new("ui-empty");
        let text = render(&fixture, 80, 24).join("\n");
        assert!(text.contains("No Claude Code sessions"));
    }
}
