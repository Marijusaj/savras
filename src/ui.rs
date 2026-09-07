//! Rendering. Knows nothing about where jobs come from — it is handed a
//! `Snapshot` and a selection and draws them.

use chrono::Utc;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::agents;
use crate::app::{App, Row, Tab};
use crate::job::{age, Job, Status};

/// Keep the summary as long as this many columns are left for it. Claude
/// Code's own panel keeps a truncated summary in a narrow pane, and a sidebar
/// with only names tells you far less, so the summary is the last thing to go.
const MIN_SUMMARY: usize = 8;
/// Below this width the resume command in the footer wraps onto a second line.
const WRAPS: u16 = 60;
/// At this width and above, the key footer has room for the rarer keys too.
const ROOMY: u16 = 58;
/// Below this height the detail footer is dropped to keep rows visible.
const SHORT: u16 = 16;

/// How the panel is being used, which is all the renderer needs to know to
/// offer the right keys.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hint<'a> {
    /// Running on its own, in its own tab.
    Standalone,
    /// A column beside a working pane, holding the keyboard.
    Focused,
    /// A column beside a working pane that has the keyboard.
    Background,
    /// Focused, with a question open: the key that was pressed closes
    /// something for good, so it asks before it does. Carries the question,
    /// which names what is about to go.
    Confirming(&'a str),
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    draw_in(frame, frame.area(), app, Hint::Standalone);
}

/// Draw the panel into `area`.
pub fn draw_in(frame: &mut Frame, area: Rect, app: &mut App, hint: Hint<'_>) {
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
    draw_footer(frame, chunks[3], app, hint);
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

    let mut title = vec![
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
    ];
    // Which session is in the pane beside this panel. It goes first, in the
    // session's own colour, because "which one am I typing into" is the
    // question you ask most often and the pane itself does not reliably say.
    if let Some(name) = app.front_name() {
        title.push(Span::raw("  "));
        title.push(Span::styled(
            format!("▶ {name}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // What the ping was about, held on screen until you go to it: a sound you
    // half-heard from another room is no use without the name.
    if app.alert_count() > 0 {
        title.push(Span::styled(
            format!("  ● {}", app.alert_count()),
            Style::default()
                .fg(Color::Indexed(220))
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Sessions you have open in tabs behind this one. Each is a live pane
    // costing memory and a running `claude attach`, so the count is worth
    // carrying where you can see it.
    if app.behind_count() > 0 {
        title.push(Span::styled(
            format!("  ▷ {}", app.behind_count()),
            Style::default().fg(Color::Indexed(245)),
        ));
    }
    let title = Line::from(title);

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
    // With panes of your own open there is a list even when there are no
    // sessions in it — those rows are where the flip keys land.
    if app.snapshot.is_empty() && app.shells() < 2 {
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
            Row::Spacer => ListItem::new(Line::from("")),
            // A repository is a place rather than a state, so it is drawn as
            // one: the same weight as a status heading, in the blue the detail
            // footer already uses for a path.
            Row::Repo(i) => ListItem::new(Line::from(Span::styled(
                app.groups
                    .get(*i)
                    .cloned()
                    .unwrap_or_else(|| "elsewhere".to_string()),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))),
            Row::Shell(i) => ListItem::new(shell_line(
                &app.shell_name(*i),
                app.shell_detail(*i),
                app.shell_tab(*i),
                name_width,
                area.width,
            )),
            Row::Job(i) => {
                let job = &app.snapshot.jobs[*i];
                ListItem::new(job_line(
                    job,
                    app.alerted(job),
                    app.tab(job),
                    name_width,
                    area.width,
                ))
            }
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(Color::Indexed(236)));
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

/// One pane of your own: the shell Savras started with, or one you added.
///
/// It carries the same markers as a session — filled for the pane you are in,
/// hollow for one running behind it — because it is the same kind of thing,
/// and the point of showing it at all is that flipping walks through it.
fn shell_line(
    name: &str,
    detail: &str,
    tab: Tab,
    name_width: u16,
    total_width: u16,
) -> Line<'static> {
    let mark = if tab == Tab::Front {
        Span::styled(
            "▶ ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("▷ ", Style::default().fg(Color::Indexed(245)))
    };
    let mut spans = vec![
        mark,
        Span::styled(
            pad(name, name_width as usize),
            Style::default().fg(Color::Indexed(245)),
        ),
    ];
    // Whatever room the name left, the way a session's summary uses it. Most
    // programs set no title at all, and then the row is just the name.
    let left = (total_width as usize).saturating_sub(name_width as usize + 3);
    if !detail.is_empty() && left > 0 {
        spans.push(Span::styled(
            format!(" {}", truncate(detail, left)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// One session's row. `alerted` means it pinged and you have not been to it —
/// the sound is gone in a second, so the panel has to keep pointing.
fn job_line(
    job: &Job,
    alerted: bool,
    tab: Tab,
    name_width: u16,
    total_width: u16,
) -> Line<'static> {
    let age = age(job.updated_at, Utc::now());
    let color = badge_color(job);
    // Claude Code shows the pull request a session produced; it is often the
    // one thing you want from a finished job.
    let link = job
        .links
        .first()
        .map(|l| format!(" #{} ", l.id))
        .unwrap_or_default();

    // The marker sits where the status star does, so it costs no width in a
    // panel that has none to spare, and a filled dot against a star is a
    // difference you can see without reading.
    //
    // A pinged session outranks an open one: the ping is the thing you have
    // not dealt with, and going to a session clears its mark anyway, so the
    // two rarely collide.
    let mark = if alerted {
        Span::styled(
            "● ",
            Style::default()
                .fg(Color::Indexed(220))
                .add_modifier(Modifier::BOLD),
        )
    } else if tab == Tab::Front {
        Span::styled(
            "▶ ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else if tab == Tab::Behind {
        // Open, running, and not on your screen — the state worth seeing.
        Span::styled("▷ ", Style::default().fg(Color::Indexed(245)))
    } else {
        Span::styled(
            "✳ ",
            Style::default().fg(match job.status {
                Status::NeedsInput => Color::Indexed(179),
                Status::Working => Color::Indexed(117),
                Status::Done => Color::Indexed(114),
            }),
        )
    };

    let mut spans = vec![
        mark,
        Span::styled(
            pad(&job.name, name_width as usize),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    // Columns already spent: bullet + name + gap + link + age.
    let fixed = 2 + name_width + 2 + link.chars().count() as u16 + 4;
    let room = (total_width.saturating_sub(fixed)) as usize;
    if room >= MIN_SUMMARY {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(&job.summary, room),
            Style::default().fg(Color::Gray),
        ));
    }

    // Right-align the link and age against the pane edge.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let tail = link.chars().count() + age.chars().count();
    spans.push(Span::raw(
        " ".repeat((total_width as usize).saturating_sub(used + tail)),
    ));
    if !link.is_empty() {
        spans.push(Span::styled(link, Style::default().fg(Color::Indexed(141))));
    }
    spans.push(Span::styled(age, Style::default().fg(Color::DarkGray)));

    Line::from(spans)
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let Some(job) = app.selected_job() else {
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(job.short_cwd(), Style::default().fg(Color::Blue)),
        // Grouping puts every session in a repository under one heading, and
        // two of them may be different working copies of it. The path above
        // says which, but only if you read all of it; this says it at a
        // glance, where the question comes up.
        Span::styled(
            if job.in_worktree() { "  worktree" } else { "" },
            Style::default().fg(Color::Indexed(140)),
        ),
        Span::styled(
            format!("  {} tokens", thousands(job.tokens)),
            Style::default().fg(Color::DarkGray),
        ),
    ])];

    // Where this session sits in its group. The detail footer is the right
    // place for it: the rows have no width to spare, and "who commands whom"
    // is a question you ask about one session at a time.
    if let Some(group) = agents::group_of(&app.snapshot, job) {
        lines.push(Line::from(Span::styled(
            truncate(&group.describe(job), area.width as usize),
            Style::default().fg(Color::Indexed(140)),
        )));
    }

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
        job.open_command_line(),
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

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, hint: Hint<'_>) {
    let text = match (&app.error, hint) {
        (Some(err), _) => Span::styled(
            truncate(err, area.width as usize),
            Style::default().fg(Color::Red),
        ),
        (None, Hint::Standalone) => Span::styled(
            // Solo has no working pane, so nothing here opens or closes one.
            // `a` is the exception worth the width: starting an agent needs no
            // pane, and a key nobody is told about is a key nobody presses.
            truncate(
                if area.width >= ROOMY {
                    "↑↓ move · s group · a agent · r refresh · q quit"
                } else {
                    "↑↓ move · a agent · q quit"
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        (None, Hint::Focused) => Span::styled(
            truncate(
                // A 44-column footer holds about forty characters, so the
                // arrows and esc — which nobody needs telling — give up their
                // place to the keys you would otherwise never find. Past that
                // there is room for the two rarer ones; `n` is also offered in
                // the other footer, as ctrl-t, which is where you are standing
                // when you want it.
                if area.width >= ROOMY {
                    "enter open · n tab · d delete · x close · a agent · Q quit"
                } else {
                    "enter open · d delete · x close · Q quit"
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        (None, Hint::Confirming(question)) => Span::styled(
            truncate(question, area.width as usize),
            Style::default().fg(Color::Yellow),
        ),
        // The chord is the one key worth advertising from the working pane:
        // it is the only thing you do without coming here first.
        (None, Hint::Background) => Span::styled(
            truncate(
                &match app.switch_hint() {
                    Some(keys) => format!("ctrl-g focus · {keys} tabs · ctrl-t new"),
                    None => "ctrl-g focus · ctrl-t new tab".to_string(),
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(text)), area);
}

/// Claude Code stores a colour name per job. These are the names it uses, and
/// the 256-colour approximations that match how its own panel looks.
///
/// They are Claude Code's colours darkened by about a fifth, which reads better
/// against a dark terminal; every one still clears 9.5:1 against the bold black
/// text on top of it.
///
/// A name Savras does not recognise falls back to a neutral, never to a colour
/// from the palette: a wrong colour reads as meaning something, and a job
/// silently shown as "red" is worse than one shown as plain.
fn badge_color(job: &Job) -> Color {
    match job.color.as_deref() {
        Some("cyan") => Color::Indexed(74),
        Some("blue") => Color::Indexed(68),
        Some("green") => Color::Indexed(71),
        Some("yellow") => Color::Indexed(137),
        Some("orange") => Color::Indexed(173),
        Some("red") => Color::Indexed(167),
        Some("pink") => Color::Indexed(168),
        Some("purple") | Some("magenta") => Color::Indexed(98),
        Some("white") => Color::Indexed(247),
        Some("gray") | Some("grey") => Color::Indexed(242),
        _ => Color::Indexed(246),
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
    use crate::app::{App, Front, Shell};
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

    /// Same, but with a session marked as having pinged.
    fn render_alerted(fixture: &Fixture, short: &str, w: u16, h: u16) -> Vec<String> {
        let mut app = App::new(fixture.0.clone());
        app.alert([short.to_string()]);
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

    #[test]
    fn the_header_says_which_session_the_pane_is_showing() {
        // Claude Code does not reliably draw its own name where you can see
        // it, so a pane full of output looks like any other pane. The panel
        // always knows which session it is, so it always says.
        let fixture = three_jobs();
        let mut app = App::new(fixture.0.clone());
        app.set_tabs(
            Front::Session("bbb".into()),
            vec!["aaa".into(), "bbb".into()],
            vec![Shell {
                name: "shell".to_string(),
                detail: String::new(),
            }],
        );
        let mut terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let header: String = (0..44)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 0))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();

        assert!(header.contains("▶ ROADMAP"), "header was: {header}");
    }

    #[test]
    fn open_tabs_are_marked_and_the_one_in_front_stands_apart() {
        // A session running in a tab behind the one you are looking at is the
        // state worth being able to see: it is live, and it is not on screen.
        let fixture = three_jobs();
        let mut app = App::new(fixture.0.clone());
        app.set_tabs(
            Front::Session("bbb".into()),
            vec!["aaa".into(), "bbb".into()],
            vec![Shell {
                name: "shell".to_string(),
                detail: String::new(),
            }],
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines: Vec<String> = (0..24)
            .map(|y| {
                (0..60)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .unwrap()
                            .symbol()
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect();
        let text = lines.join("\n");

        let front = lines.iter().find(|l| l.contains("ROADMAP")).unwrap();
        assert!(front.contains('▶'), "the tab in front: {front}");
        let behind = lines.iter().find(|l| l.contains("ASK")).unwrap();
        assert!(behind.contains('▷'), "a tab behind it: {behind}");
        let closed = lines.iter().find(|l| l.contains("FIN")).unwrap();
        assert!(closed.contains('✳'), "a session with no tab: {closed}");

        assert!(
            text.contains("▷ 2"),
            "the header counts the panes behind you, the shell included"
        );
    }

    #[test]
    fn the_focused_footer_advertises_the_keys_you_would_not_guess() {
        // A key nobody can see is a key nobody has. The footer is the only
        // place the panel says what it can do, and it truncates at the panel
        // width — so the keys that need announcing have to come first.
        let fixture = three_jobs();
        let mut app = App::new(fixture.0.clone());
        let mut terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
        terminal
            .draw(|frame| draw_in(frame, frame.area(), &mut app, Hint::Focused))
            .unwrap();
        let text: String = (0..24)
            .map(|y| {
                (0..44)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .unwrap()
                            .symbol()
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // A 44-column panel gets the four that matter most, deleting included:
        // it is the one key here that cannot be undone, so it must not be the
        // one you find out about by accident.
        assert!(text.contains("d delete"), "deleting a session: {text}");
        assert!(text.contains("x close"), "closing a tab: {text}");
        assert!(text.contains("enter open"));
        assert!(text.contains("Q quit"), "quitting: {text}");

        // Given room, the rarer two come back.
        let mut wide = Terminal::new(TestBackend::new(70, 24)).unwrap();
        wide.draw(|frame| draw_in(frame, frame.area(), &mut app, Hint::Focused))
            .unwrap();
        let last: String = (0..70)
            .map(|x| {
                wide.backend()
                    .buffer()
                    .cell((x, 23))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(last.contains("a agent"), "starting an agent: {last}");
        assert!(last.contains("n tab"), "a tab of your own: {last}");
    }

    #[test]
    fn the_switch_key_is_offered_only_once_it_would_go_somewhere() {
        // Advertising a key that does nothing is worse than advertising none:
        // you press it, nothing happens, and you conclude it is broken.
        let render_background = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
            terminal
                .draw(|frame| draw_in(frame, frame.area(), app, Hint::Background))
                .unwrap();
            (0..24)
                .map(|y| {
                    (0..44)
                        .map(|x| {
                            terminal
                                .backend()
                                .buffer()
                                .cell((x, y))
                                .unwrap()
                                .symbol()
                                .to_string()
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // An empty panel: nowhere to flip to, so the keys belong to the shell
        // and are not advertised.
        let empty = Fixture::new("ui-switch-empty");
        let mut app = App::new(empty.0.clone());
        app.set_switch(Some("ctrl-w/s".into()));
        assert!(
            !render_background(&mut app).contains("ctrl-w/s"),
            "with no sessions at all there is nowhere to flip to"
        );

        // One session is already somewhere to go — you need never have opened
        // a pane, because flipping walks the panel's rows, not your panes.
        let mut app = App::new(three_jobs().0.clone());
        app.set_switch(Some("ctrl-w/s".into()));
        let text = render_background(&mut app);
        assert!(text.contains("ctrl-w/s tabs"), "{text}");
    }

    #[test]
    fn the_detail_says_where_a_session_sits_in_its_group() {
        // Rows have no width for it, and "who commands whom" is a question you
        // ask about one session at a time — so it lives in the detail footer.
        let fixture = Fixture::new("ui-group")
            .job(
                "aaa",
                r#"{"state":"working","name":"LEAD","detail":"planning","cwd":"/tmp/r"}"#,
            )
            .job(
                "bbb",
                r#"{"state":"working","name":"LEAD-2","detail":"building","cwd":"/tmp/r"}"#,
            );
        let text = render(&fixture, 60, 24).join("\n");
        assert!(
            text.contains("leads LEAD-2"),
            "the lead's row must say who it commands:\n{text}"
        );
    }

    #[test]
    fn a_session_in_no_group_says_nothing_about_groups() {
        let text = render(&three_jobs(), 60, 24).join("\n");
        assert!(!text.contains("leads "));
        assert!(!text.contains("parallel agent"));
    }

    #[test]
    fn a_panel_with_no_working_pane_marks_no_tabs() {
        // `svr solo` has nowhere to open a session, so nothing is ever in
        // front and the stars are left alone.
        let lines = render(&three_jobs(), 60, 24);
        let text = lines.join("\n");
        assert!(!text.contains('▶'));
        assert!(!text.contains('▷'));
    }

    #[test]
    fn the_session_that_pinged_is_marked_and_counted() {
        // Hearing a ping and not knowing which of four waiting sessions made
        // it is the same as not being told at all.
        let lines = render_alerted(&three_jobs(), "aaa", 60, 24);
        let text = lines.join("\n");

        assert!(text.contains("● 1"), "the header says how many are waiting");
        let asking = lines
            .iter()
            .find(|l| l.contains("ASK"))
            .expect("the asking session is on screen");
        assert!(asking.contains('●'), "marked: {asking}");

        let working = lines.iter().find(|l| l.contains("ROADMAP")).unwrap();
        assert!(working.contains('✳'), "unmarked sessions keep the star");
        assert!(!working.contains('●'));
    }

    #[test]
    fn with_nothing_waiting_the_header_says_nothing_extra() {
        let text = render(&three_jobs(), 60, 24).join("\n");
        assert!(!text.contains('●'));
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
