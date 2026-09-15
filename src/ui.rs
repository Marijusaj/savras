//! Rendering. Knows nothing about where jobs come from — it is handed a
//! `Snapshot` and a selection and draws them.

use chrono::Utc;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::agents;
use crate::app::{App, BoardRow, BoardView, Compose, Row, Tab};
use crate::board;
use crate::job::{age, Deploy, Job, Status};

/// Keep the summary as long as this many columns are left for it. Claude
/// Code's own panel keeps a truncated summary in a narrow pane, and a sidebar
/// with only names tells you far less, so the summary is the last thing to go.
const MIN_SUMMARY: usize = 8;
/// Below this width the resume command in the footer wraps onto a second line.
const WRAPS: u16 = 60;
/// At this width and above, the key footer has room for the rarer keys too.
const ROOMY: u16 = 58;
/// And at this one, for the board as well.
const WIDE: u16 = 68;
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
    // The board takes the place of the rows and the detail, not of the header:
    // what is waiting on you is still worth a line while you read.
    if app.board.is_some() {
        let chunks = Layout::vertical([
            Constraint::Length(2), // header
            Constraint::Min(1),    // board
            Constraint::Length(1), // keys
        ])
        .split(area);
        draw_header(frame, chunks[0], app);
        draw_board(frame, chunks[1], app);
        draw_footer(frame, chunks[2], app, hint);
        return;
    }

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

    // Wide enough for the longest name, plus the gutter when anything is
    // indented — an agent pays for its indent out of the column, and without
    // this the column is set by a name that is then too long to fit in it.
    let indented = (0..app.snapshot.jobs.len()).any(|i| app.under_lead(i));
    let name_width = (app
        .snapshot
        .jobs
        .iter()
        .map(|j| j.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 12)
        + if indented { GUTTER.chars().count() } else { 0 })
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
            Row::Board(i) => match app.board_rows.get(*i) {
                Some(board) => ListItem::new(board_line(
                    board,
                    app.board_tab(&board.repo),
                    name_width,
                    area.width,
                )),
                None => ListItem::new(Line::from("")),
            },
            Row::Shell(i) => ListItem::new(shell_line(
                &app.shell_name(*i),
                app.shell_detail(*i),
                app.shell_tab(*i),
                name_width,
                area.width,
            )),
            Row::Job(i) => {
                let job = &app.snapshot.jobs[*i];
                let under = app.under_lead(*i);
                ListItem::new(job_line(
                    job,
                    app.alerted(job),
                    app.tab(job),
                    under,
                    name_width,
                    area.width,
                ))
            }
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(Color::Indexed(236)));
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

/// A repository's board, first under its heading.
///
/// Marked like any tab — filled in front, hollow behind — because it is one,
/// and with its own glyph when it is not open, so a board never reads as a
/// session. The count is flush right, where a session's age sits.
fn board_line(board: &BoardRow, tab: Tab, name_width: u16, total_width: u16) -> Line<'static> {
    let mark = match tab {
        Tab::Front => Span::styled(
            "▶ ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Tab::Behind => Span::styled("▷ ", Style::default().fg(Color::Indexed(245))),
        Tab::None => Span::styled("≡ ", Style::default().fg(Color::Blue)),
    };
    let name = pad("board", (name_width as usize).max(5));
    let count = board.count.to_string();
    let gap =
        (total_width as usize).saturating_sub(2 + name.chars().count() + count.chars().count());
    Line::from(vec![
        mark,
        Span::styled(name, Style::default().fg(Color::Blue)),
        Span::raw(" ".repeat(gap)),
        Span::styled(count, Style::default().fg(Color::DarkGray)),
    ])
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
    under_lead: bool,
    name_width: u16,
    total_width: u16,
) -> Line<'static> {
    // Measured from the session's start, not its last word: see `created_at`.
    // A session with no start recorded falls back to freshness rather than
    // showing nothing, since the older files have only that.
    let age = age(job.created_at.or(job.updated_at), Utc::now());
    let color = badge_color(job);

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

    // The session you are typing into is named in white, plain, against the
    // colour every other row wears. The `▶` says the same thing, but it is one
    // glyph in a column that already carries four meanings, and the name is
    // what the eye lands on.
    // A parallel agent is indented under its lead, and pays for the gutter out
    // of its own name rather than out of the row: the columns to the right are
    // read down a list, so they have to stay where they are. Two columns is
    // enough — the shape is read before the names are.
    let named = (name_width as usize).saturating_sub(if under_lead {
        GUTTER.chars().count()
    } else {
        0
    });
    let name = if tab == Tab::Front {
        Span::styled(
            pad(&job.name, named),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            pad(&job.name, named),
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = vec![mark];
    if under_lead {
        spans.push(Span::styled(
            GUTTER,
            Style::default().fg(Color::Indexed(240)),
        ));
    }
    spans.push(name);

    // What is left after the mark and the name, spent in the order these
    // things are worth: how long it has been open, what it is, what its pull
    // request is doing, how much context is gone — and only then the sentence,
    // which is the one that can be said elsewhere. A 44-column panel holds all
    // of them and no summary; a wide one holds the summary as well; a very
    // narrow one keeps the name and the age and gives up the rest in that
    // order, rather than truncating everything into uselessness.
    let mut room = (total_width.saturating_sub(2 + name_width)) as usize;
    let mut spend = |cost: usize| -> bool {
        let can = room >= cost;
        if can {
            room -= cost;
        }
        can
    };

    let age_shown = spend(1 + age.chars().count());
    let word_shown = spend(1 + WORD_WIDTH);
    let deploy = deploy_text(job);
    let deploy_shown = !deploy.is_empty() && spend(1 + deploy.chars().count());
    let percent = job.context_percent();
    let ctx_shown = percent.is_some() && spend(1 + CTX_WIDTH);

    if word_shown {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            pad(job.word(), WORD_WIDTH),
            Style::default()
                .fg(word_color(job))
                .add_modifier(Modifier::BOLD),
        ));
    }

    if room >= MIN_SUMMARY + 2 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(&job.summary, room - 2),
            Style::default().fg(Color::Gray),
        ));
    }

    // Everything from here is right-aligned against the pane edge.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let tail = usize::from(deploy_shown) * (1 + deploy.chars().count())
        + usize::from(ctx_shown) * (1 + CTX_WIDTH)
        + usize::from(age_shown) * (1 + age.chars().count());
    spans.push(Span::raw(
        " ".repeat((total_width as usize).saturating_sub(used + tail)),
    ));

    if deploy_shown {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            deploy,
            Style::default().fg(deploy_color(job.deploy)),
        ));
    }
    if let (true, Some(percent)) = (ctx_shown, percent) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{percent:>3}%"),
            // Past about here a session is close to compacting, which is worth
            // seeing before it happens rather than after.
            Style::default().fg(if percent >= 85 {
                Color::Indexed(203)
            } else {
                Color::DarkGray
            }),
        ));
    }
    if age_shown {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(age, Style::default().fg(age_color(job))));
    }

    Line::from(spans)
}

/// What an agent's row is indented by, drawn in the dimmest grey that is still
/// a line: the eye should find the shape without reading it.
const GUTTER: &str = "└ ";
/// `WORKING` is the longest of the four, and they read as a column only if
/// they are one.
const WORD_WIDTH: usize = 7;
/// `100%`.
const CTX_WIDTH: usize = 4;

/// The pull request, said in the width a sidebar has: the word and the number.
fn deploy_text(job: &Job) -> String {
    let Some(link) = job.links.first() else {
        return String::new();
    };
    match job.deploy {
        // A pull request nothing is known about is still worth its number —
        // that is what the row said before any of this existed.
        None => format!("#{}", link.id),
        Some(deploy) => format!("{} #{}", deploy.word(), link.id),
    }
}

fn deploy_color(deploy: Option<Deploy>) -> Color {
    match deploy {
        Some(Deploy::Broken) => Color::Indexed(203),
        Some(Deploy::Ready) => Color::Indexed(114),
        Some(Deploy::Checks) => Color::Indexed(179),
        Some(Deploy::Merged) => Color::Indexed(141),
        Some(Deploy::Closed) => Color::DarkGray,
        Some(Deploy::Open) | None => Color::Indexed(141),
    }
}

fn word_color(job: &Job) -> Color {
    if job.failed {
        return Color::Indexed(203);
    }
    match job.status {
        Status::NeedsInput => Color::Indexed(179),
        Status::Working => Color::Indexed(117),
        Status::Done => Color::Indexed(114),
    }
}

/// A session open for hours is the thing the age column exists to say, so it
/// stops being grey once it is worth remarking on.
fn age_color(job: &Job) -> Color {
    let hours = job
        .created_at
        .map(|start| (Utc::now() - start).num_hours())
        .unwrap_or(0);
    match hours {
        h if h >= 8 => Color::Indexed(203),
        h if h >= 4 => Color::Indexed(179),
        _ => Color::DarkGray,
    }
}

/// The board, in place of the rows: what the agents said to each other, and
/// what you are saying back.
///
/// Oldest at the top and newest at the bottom, the way a conversation reads,
/// with the view held at the bottom unless the cursor has gone above it.
fn draw_board(frame: &mut Frame, area: Rect, app: &App) {
    let Some(view) = app.board.as_ref() else {
        return;
    };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(if view.compose.is_some() { 2 } else { 0 }),
    ])
    .split(area);

    draw_board_title(frame, chunks[0], view);
    draw_messages(frame, chunks[1], view, "Press c to create one");
    if let Some(compose) = &view.compose {
        draw_compose(frame, chunks[2], view, compose);
    }
}

/// A repository's board as a tab in the working pane.
///
/// The conversation fills the pane, the line being written sits under it with
/// the terminal's own cursor in it, and the last line says what the keys do —
/// because this is the one tab where typing is not typing into a program.
pub fn draw_board_tab(frame: &mut Frame, area: Rect, view: &BoardView, focused: bool) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(if view.exists { 2 } else { 0 }),
        Constraint::Length(1),
    ])
    .split(area);

    draw_board_title(frame, chunks[0], view);
    draw_messages(frame, chunks[1], view, "Press enter to create one");
    if view.exists {
        draw_tab_compose(frame, chunks[2], view, focused);
    }
    let hint = match (&view.repo, view.exists) {
        (Some(_), true) => {
            "type to post · ↑↓ pick a message to answer · enter send · esc clear · ctrl-g panel"
                .to_string()
        }
        (Some(_), false) => format!("enter creates a board for {} · ctrl-g panel", view.name()),
        (None, _) => "ctrl-g panel".to_string(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&hint, chunks[3].width as usize),
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[3],
    );
}

/// `board · savras`, and how much is on it.
fn draw_board_title(frame: &mut Frame, area: Rect, view: &BoardView) {
    let mut said = format!(" · {}", view.name());
    if view.exists {
        said.push_str(&format!(
            " · {} message{}",
            view.messages.len(),
            if view.messages.len() == 1 { "" } else { "s" }
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "board",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(&said, (area.width as usize).saturating_sub(5)),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        area,
    );
}

/// The conversation, oldest at the top and newest at the bottom, held at the
/// bottom unless the cursor has gone above it — or, where there is nothing to
/// show, a sentence saying why. `create` is how this screen makes a board.
fn draw_messages(frame: &mut Frame, area: Rect, view: &BoardView, create: &str) {
    if !view.exists || view.messages.is_empty() {
        // A repository without a board is the common case, and a blank screen
        // would read as a board nobody has written on. Say which it is.
        let said = match (&view.repo, view.exists) {
            (None, _) => "No session of this machine's is selected, so there is no \
                          repository to show a board for."
                .to_string(),
            (Some(_), false) => format!(
                "{} has no board. {create}, and the agents working there can read \
                 it and post to it.",
                view.name()
            ),
            (Some(_), true) => "Nothing on the board yet.".to_string(),
        };
        frame.render_widget(
            Paragraph::new(said)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let (lines, selected) = board_lines(view, area.width as usize);
    let mut top = lines.len().saturating_sub(area.height as usize);
    if let Some(start) = selected {
        top = top.min(start);
    }
    frame.render_widget(
        Paragraph::new(lines).scroll((top.min(u16::MAX as usize) as u16, 0)),
        area,
    );
}

/// The line being written in a board tab — always there, since typing is how
/// the tab is used — with what it will be: a new message, or an answer to the
/// one picked. The cursor is the terminal's own, so it blinks where you type.
fn draw_tab_compose(frame: &mut Frame, area: Rect, view: &BoardView, focused: bool) {
    let width = area.width as usize;
    let text = view.compose.as_ref().map_or("", |c| c.text.as_str());
    let answering = match &view.compose {
        Some(compose) => compose.re.as_ref(),
        None => view.selected.and_then(|i| view.messages.get(i)),
    };
    let whither = match answering {
        Some(m) => format!("to {} · answering {}", view.name(), m.from),
        None => format!("to {} as {}", view.name(), board::OWNER),
    };
    let room = width.saturating_sub(3);
    let chars: Vec<char> = text.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    let shown_width = shown.chars().count() as u16;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                truncate(&whither, width),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(vec![
                Span::styled("› ", Style::default().fg(Color::Yellow)),
                Span::raw(shown),
            ]),
        ]),
        area,
    );
    if focused && area.height >= 2 && area.width > 2 {
        frame.set_cursor_position((area.x + 2 + shown_width, area.y + 1));
    }
}

/// Every message as the lines it takes at this width, and the line the
/// selected one starts on.
///
/// An answer is marked with who it answers rather than moved under its
/// question: the list keeps the order things were said in, and a thread
/// rebuilt out of order hides that the answer came an hour later.
fn board_lines(view: &BoardView, width: usize) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = Vec::new();
    let mut selected = None;
    for (i, m) in view.messages.iter().enumerate() {
        let start = lines.len();
        let mut head = vec![
            Span::styled(
                m.at.with_timezone(&chrono::Local)
                    .format("%H:%M ")
                    .to_string(),
                Style::default().fg(Color::DarkGray),
            ),
            // The owner in the colour Savras wears, so what you said stands
            // apart from what the agents said.
            Span::styled(
                m.from.clone(),
                Style::default()
                    .fg(if m.from == board::OWNER {
                        Color::Magenta
                    } else {
                        Color::White
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(re) = &m.re {
            let whom = view
                .messages
                .iter()
                .find(|q| &q.id == re)
                .map_or_else(|| "earlier".to_string(), |q| q.from.clone());
            head.push(Span::styled(
                format!(" ↳ {whom}"),
                Style::default().fg(Color::Indexed(140)),
            ));
        }
        lines.push(Line::from(head));
        for row in wrap(&m.text, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(
                format!("  {row}"),
                Style::default().fg(Color::Gray),
            )));
        }
        if view.selected == Some(i) {
            for line in &mut lines[start..] {
                *line = std::mem::take(line).style(Style::default().bg(Color::Indexed(236)));
            }
            selected = Some(start);
        }
    }
    (lines, selected)
}

/// The message being written: where it is going, then the end of the words,
/// which is where you are typing.
fn draw_compose(frame: &mut Frame, area: Rect, view: &BoardView, compose: &Compose) {
    let width = area.width as usize;
    let whither = match &compose.re {
        Some(m) => format!("to {} · answering {}", view.name(), m.from),
        None => format!("to {} as {}", view.name(), board::OWNER),
    };
    let chars: Vec<char> = compose.text.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(width.saturating_sub(3))..]
        .iter()
        .collect();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                truncate(&whither, width),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(vec![
                Span::styled("› ", Style::default().fg(Color::Yellow)),
                Span::raw(shown),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]),
        ]),
        area,
    );
}

/// Word-wrap to `width` columns, breaking a word only when it is longer than a
/// whole line. Counted in characters, like every other width here.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        let used = row.chars().count();
        if used > 0 && used + 1 + word.len() > width {
            rows.push(std::mem::take(&mut row));
        }
        while word.len() > width {
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            rows.push(word.drain(..width).collect());
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.extend(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
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
    ])];
    // A session on another machine reports no token count — the file it comes
    // from has none. "0 tokens" would read as a session that has spent
    // nothing, which is a different and untrue thing.
    if job.context() > 0 {
        lines[0].spans.push(Span::styled(
            // The same number the row's percentage is computed from, so the
            // two cannot be read against each other and disagree.
            format!("  {} tokens", thousands(job.context())),
            Style::default().fg(Color::DarkGray),
        ));
    }

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
        // A question outranks an error, whichever arrived first: the error is
        // news you have already been given, and the question is the one line
        // that says what the next keystroke will do. A machine that cannot be
        // reached says so every time it retries, and that must not be what
        // swallows "new tab: 1 here · 2 <the machine>".
        (_, Hint::Confirming(question)) => Span::styled(
            truncate(question, area.width as usize),
            Style::default().fg(Color::Yellow),
        ),
        (Some(err), _) => Span::styled(
            truncate(err, area.width as usize),
            Style::default().fg(Color::Red),
        ),
        // The board has keys of its own, and while a message is being written
        // every letter is a letter — so the footer says how to get out.
        (None, Hint::Focused | Hint::Standalone)
            if app.board.as_ref().is_some_and(|v| v.compose.is_some()) =>
        {
            Span::styled(
                truncate("enter post · esc cancel", area.width as usize),
                Style::default().fg(Color::DarkGray),
            )
        }
        // Only the keys that would do something: `p` on a repository with no
        // board is a key pressed to no effect, which reads as a broken one.
        (None, Hint::Focused | Hint::Standalone) if app.board.is_some() => Span::styled(
            truncate(
                match app.board.as_ref() {
                    Some(view) if view.exists => "p post · r reply · esc back",
                    Some(view) if view.repo.is_some() => "c create a board · esc back",
                    _ => "esc back",
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        (None, Hint::Standalone) => Span::styled(
            // Solo has no working pane, so nothing here opens or closes one.
            // `a` is the exception worth the width: starting an agent needs no
            // pane, and a key nobody is told about is a key nobody presses.
            truncate(
                if area.width >= ROOMY {
                    "↑↓ move · b board · s group · a agent · q quit"
                } else {
                    "↑↓ move · a agent · q quit"
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        // A board row has verbs of its own, and none of a session's.
        (None, Hint::Focused) if app.selected_board().is_some() => Span::styled(
            truncate(
                "enter open · c clean · d delete · x close",
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
                //
                // `a` is offered only where it would do something: an agent is
                // started on this machine, in the lead's own directory, so
                // there is none to start for a session on another box. A key
                // you press to no effect reads as a broken key.
                match (
                    area.width,
                    app.selected_job()
                        .is_some_and(|job| job.machine_tag().is_some()),
                ) {
                    (w, false) if w >= WIDE => {
                        "enter open · n tab · b board · d delete · x close · a agent · Q quit"
                    }
                    (w, true) if w >= WIDE => {
                        "enter open · n tab · b board · d delete · x close · Q quit"
                    }
                    (w, false) if w >= ROOMY => {
                        "enter open · n tab · d delete · x close · a agent · Q quit"
                    }
                    (w, true) if w >= ROOMY => "enter open · n tab · d delete · x close · Q quit",
                    _ => "enter open · d delete · x close · Q quit",
                },
                area.width as usize,
            ),
            Style::default().fg(Color::DarkGray),
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
        if i > 0 && (s.len() - i).is_multiple_of(3) {
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
            row.contains("ROADMAP WORKING  compiling the plan"),
            "name column collided with what follows it: {row:?}"
        );
    }

    #[test]
    fn the_session_you_are_typing_into_is_named_in_white() {
        // Every other row wears a colour badge, so the one you are in is the
        // one *without* it. The `▶` says the same thing, but it is one glyph
        // in a column already carrying four meanings, and the name is what
        // the eye lands on.
        let fixture = three_jobs();
        let mut app = App::new(fixture.0.clone());
        app.set_tabs(Front::Session("bbb".into()), vec!["bbb".into()], vec![]);
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Find the row each name starts on, and look at the name's first cell.
        let cell_of = |name: &str| {
            for y in 0..24u16 {
                let line: String = (0..60)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect();
                if let Some(at) = line.find(name) {
                    return buffer.cell((at as u16, y)).unwrap().clone();
                }
            }
            panic!("no row for {name}");
        };

        let front = cell_of("ROADMAP"); // "bbb", the one in the pane
        assert_eq!(front.fg, Color::White, "the name should be plain white");
        assert_eq!(front.bg, Color::Reset, "and wear no badge");

        let other = cell_of("ASK");
        assert_ne!(other.bg, Color::Reset, "every other name keeps its badge");
    }

    #[test]
    fn agents_are_drawn_under_their_lead_when_grouped_by_repository() {
        let fixture = Fixture::new("ui-under")
            .job(
                "a",
                r#"{"state":"working","name":"BOOKS","cwd":"/tmp","detail":"d"}"#,
            )
            .job(
                "b",
                r#"{"state":"working","name":"BOOKS-2","cwd":"/tmp","detail":"d"}"#,
            );

        let mut app = App::new(fixture.0.clone());
        app.set_group_by(crate::app::GroupBy::Repo);
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> = (0..24)
            .map(|y| {
                (0..60)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect();

        // Rows only — the detail footer names the group too, and "leads
        // BOOKS-2" is not a row.
        let row = |want: &str| {
            lines
                .iter()
                .position(|l| l.contains("WORKING") && l.contains(want))
                .unwrap_or_else(|| panic!("no row for {want} in {lines:#?}"))
        };
        let lead = row("BOOKS  ");
        let agent = row("BOOKS-2");
        assert!(agent > lead, "the agent belongs under its lead");
        assert!(
            lines[agent].contains("└ BOOKS-2"),
            "an agent is indented: {:?}",
            lines[agent]
        );
        assert!(
            !lines[lead].contains('└'),
            "the lead is not: {:?}",
            lines[lead]
        );

        // The columns to the right are read down a list, so the gutter comes
        // out of the name rather than shifting everything along.
        // Counted in characters, not bytes: `└` is three of the latter, and
        // that difference is exactly the bug this alignment is guarding.
        let ends = |l: &String| l.trim_end().chars().count();
        assert_eq!(ends(&lines[lead]), ends(&lines[agent]));

        // Grouped by status the hierarchy is not drawn at all: a lead and its
        // agent can be in different groups, and an indent pointing at a row
        // under another heading would be a lie.
        let mut app = App::new(fixture.0.clone());
        app.set_group_by(crate::app::GroupBy::Status);
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text: String = (0..24)
            .flat_map(|y| {
                (0..60).map(move |x| (x, y)).map(|(x, y)| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .unwrap()
                        .symbol()
                        .to_string()
                })
            })
            .collect();
        assert!(!text.contains('└'), "no gutter when grouped by status");
    }

    #[test]
    fn the_row_says_what_the_session_is_in_one_word() {
        let lines = render(&three_jobs(), 80, 24);
        let word = |name: &str| {
            let row = lines.iter().find(|l| l.contains(name)).expect(name).clone();
            row
        };
        assert!(word("ROADMAP").contains("WORKING"), "{}", word("ROADMAP"));
        assert!(word("ASK").contains("WAITING"), "{}", word("ASK"));
        assert!(word("FIN").contains("DONE"), "{}", word("FIN"));
    }

    #[test]
    fn the_columns_are_kept_and_the_sentence_is_what_gives_way() {
        // 44 columns is the default sidebar. The four columns are fixed and
        // the summary takes whatever is left, so what a session *is* survives
        // at any width and the account of what it is doing is clipped — the
        // trade this build makes, and the reverse of the old row's.
        let fixture = Fixture::new("ui-vitals-narrow").job(
            "aaa",
            r#"{"state":"working","name":"BOOKS","tokens":76000,
                "respawnFlags":["--model","claude-opus-5"],
                "children":[{"id":"411","kind":"pr","href":"https://x/pull/411"}],
                "detail":"a sentence far too long for a sidebar to hold"}"#,
        );
        let lines = render(&fixture, 44, 26);
        let row = lines.iter().find(|l| l.contains("BOOKS")).unwrap();

        assert!(row.contains("WORKING"), "{row:?}");
        assert!(row.contains("#411"), "{row:?}");
        assert!(row.contains("38%"), "76k of a 200k window: {row:?}");
        assert!(row.contains('…'), "the summary should be clipped: {row:?}");
        assert!(!row.contains("to hold"), "clipped, not fitted: {row:?}");
        assert_eq!(row.chars().count(), 44);
    }

    #[test]
    fn a_long_name_costs_the_summary_rather_than_a_column() {
        // The name column is as wide as the longest name, so a wide name is
        // exactly the case where something has to go. It is the sentence.
        let fixture = Fixture::new("ui-vitals-wide-name").job(
            "aaa",
            r#"{"state":"working","name":"BOOKS-LEG-33","tokens":76000,
                "children":[{"id":"411","kind":"pr","href":"https://x/pull/411"}],
                "detail":"running the matcher tests"}"#,
        );
        let lines = render(&fixture, 44, 26);
        let row = lines.iter().find(|l| l.contains("BOOKS")).unwrap();

        assert!(row.contains("WORKING"), "{row:?}");
        assert!(row.contains("#411"), "{row:?}");
        assert!(
            !row.contains("matcher"),
            "the summary should be gone: {row:?}"
        );
        assert_eq!(row.chars().count(), 44);
    }

    #[test]
    fn a_pull_request_says_what_it_is_doing_when_the_cache_knows() {
        // Claude Code refreshes this file itself, so the column costs one
        // small read per scan and no network at all.
        let fixture = Fixture::new("ui-vitals-pr")
            .job(
                "aaa",
                r#"{"state":"done","name":"BOOKS",
                    "children":[{"id":"411","kind":"pr","href":"https://x/pull/411"}]}"#,
            )
            .job(
                "bbb",
                r#"{"state":"done","name":"LEDGER",
                    "children":[{"id":"412","kind":"pr","href":"https://x/pull/412"}]}"#,
            )
            .job(
                "ccc",
                r#"{"state":"done","name":"COSTS",
                    "children":[{"id":"413","kind":"pr","href":"https://x/pull/413"}]}"#,
            )
            .pr_cache(
                r#"{"https://x/pull/411":{"state":"OPEN","checks":{"passed":5,"failed":0,"pending":0}},
                    "https://x/pull/412":{"state":"MERGED","checks":{"passed":5,"failed":0,"pending":0}},
                    "https://x/pull/413":{"state":"OPEN","checks":{"passed":2,"failed":1,"pending":0}}}"#,
            );
        let lines = render(&fixture, 70, 26);
        let row = |name: &str| lines.iter().find(|l| l.contains(name)).expect(name).clone();

        assert!(row("BOOKS").contains("READY #411"), "{}", row("BOOKS"));
        assert!(row("LEDGER").contains("MERGED #412"), "{}", row("LEDGER"));
        // A failing check outranks everything else true of an open pull
        // request: it is the only one of these asking for something.
        assert!(row("COSTS").contains("FAILED #413"), "{}", row("COSTS"));
    }

    #[test]
    fn the_age_is_counted_from_the_start_not_the_last_word() {
        // A working session rewrites `updatedAt` every few seconds, so
        // freshness reads `8s` for as long as it runs, however long that is.
        // How long it has been *open* is the number that changes what you do.
        let now = Utc::now();
        let started = now - chrono::Duration::hours(3);
        let fixture = Fixture::new("ui-vitals-age").job(
            "aaa",
            &format!(
                r#"{{"state":"working","name":"BOOKS",
                     "createdAt":"{}","updatedAt":"{}"}}"#,
                started.to_rfc3339(),
                now.to_rfc3339()
            ),
        );
        let lines = render(&fixture, 60, 26);
        let row = lines.iter().find(|l| l.contains("BOOKS")).unwrap();

        assert!(row.trim_end().ends_with("3h"), "{row:?}");
    }

    #[test]
    fn a_million_token_model_is_measured_against_a_million() {
        // The denominator is the model, and `[1m]` in its name is what says
        // so. Reading the same 76k against 200k would cry compaction at a
        // session that has spent under a tenth of its context.
        let fixture = Fixture::new("ui-vitals-1m").job(
            "aaa",
            r#"{"state":"working","name":"BOOKS","tokens":76000,
                "respawnFlags":["--model","claude-opus-5[1m]"]}"#,
        );
        let lines = render(&fixture, 60, 26);
        let row = lines.iter().find(|l| l.contains("BOOKS")).unwrap();
        // 76k of a million is 7.6%, and rounds the way the session's own
        // status line rounds it.
        assert!(row.contains("8%"), "{row:?}");
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

    /// The panel with its board open, focused, as plain lines.
    fn render_board(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| draw_in(frame, frame.area(), app, Hint::Focused))
            .unwrap();
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

    /// A session in `/tmp`, and — when `made` — a board for it holding the two
    /// messages this feature was first tested with.
    fn a_board(tag: &str, made: bool) -> (Fixture, std::path::PathBuf) {
        let f = Fixture::new(tag).job(
            "aaa",
            r#"{"state":"working","name":"SAVRAS-8","cwd":"/tmp"}"#,
        );
        let dir = f.0.parent().unwrap().join("boards");
        if made {
            let boards = board::Boards::at(dir.clone());
            let repo = board::topic_of(std::path::Path::new("/tmp"));
            boards.create(&board::Owner::at_the_panel(), &repo).unwrap();
            let first = boards.post("SAVRAS-8", &repo, None, "msg").unwrap();
            boards
                .post(
                    "ROADMAP",
                    &repo,
                    Some(first.id),
                    "received — the hook delivered it into my turn unprompted, so the round trip works",
                )
                .unwrap();
        }
        (f, dir)
    }

    #[test]
    fn the_board_takes_the_place_of_the_rows_and_says_who_is_answered() {
        let (f, dir) = a_board("ui-board", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();
        let lines = render_board(&mut app, 44, 24);
        let text = lines.join("\n");

        assert!(text.contains("board · tmp"), "{text}");
        assert!(!text.contains("WORKING"), "the rows give way: {text}");
        assert!(text.contains("SAVRAS"), "the header stays: {text}");
        assert!(text.contains("ROADMAP ↳ SAVRAS-8"), "{text}");
        assert!(!text.contains("· all"), "there is no lane to name: {text}");
        // Wrapped at the panel's width rather than clipped at it.
        assert!(text.contains("unprompted"), "{text}");
        assert!(text.contains("works"), "{text}");
        assert!(text.contains("p post"), "the keys are offered: {text}");
        for line in &lines {
            assert!(line.chars().count() <= 44, "overflowed: {line:?}");
        }
    }

    #[test]
    fn a_repository_without_a_board_says_so_and_offers_to_make_one() {
        let (f, dir) = a_board("ui-board-none", false);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();
        let lines = render_board(&mut app, 44, 24);
        let text = lines.join("\n");

        assert!(text.contains("tmp has no board"), "{text}");
        assert!(text.contains("c create a board"), "{text}");
        assert!(!text.contains("p post"), "nowhere to post: {text}");
        for line in &lines {
            assert!(line.chars().count() <= 44, "overflowed: {line:?}");
        }
    }

    #[test]
    fn a_message_being_written_says_where_it_is_going() {
        let (f, dir) = a_board("ui-board-compose", true);
        let mut app = App::new(f.0.clone());
        app.board_dir = dir;
        app.open_board();
        let view = app.board.as_mut().unwrap();
        view.compose(true);
        view.type_text("on it");
        let text = render_board(&mut app, 44, 24).join("\n");

        assert!(text.contains("to tmp · answering ROADMAP"), "{text}");
        assert!(text.contains("› on it"), "{text}");
        assert!(text.contains("enter post · esc cancel"), "{text}");
    }

    #[test]
    fn a_board_row_sits_under_its_repository_and_says_how_much_is_on_it() {
        let (_f, dir) = a_board("ui-board-row", true);
        let mut app = App::new(_f.0.clone());
        app.board_dir = dir;
        app.set_group_by(crate::app::GroupBy::Repo);
        let lines = render_board(&mut app, 44, 24);

        let heading = lines
            .iter()
            .position(|l| l.trim() == "tmp")
            .unwrap_or_else(|| panic!("no heading for tmp: {lines:#?}"));
        let row = &lines[heading + 1];
        assert!(row.starts_with("≡ board"), "{lines:#?}");
        assert!(row.ends_with('2'), "two messages, flush right: {row:?}");
        assert!(row.chars().count() <= 44, "{row:?}");
    }

    /// A board as a tab, drawn alone into a terminal of its own.
    fn render_tab(view: &BoardView, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| draw_board_tab(frame, frame.area(), view, true))
            .unwrap();
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
    fn a_board_tab_is_a_conversation_with_a_line_to_write_in_under_it() {
        let (_f, dir) = a_board("ui-board-tab", true);
        let repo = board::topic_of(std::path::Path::new("/tmp"));
        let mut view = BoardView::new(dir.clone(), Some(repo));

        let text = render_tab(&view, 70, 16).join("\n");
        assert!(text.contains("board · tmp · 2 messages"), "{text}");
        assert!(text.contains("ROADMAP ↳ SAVRAS-8"), "{text}");
        assert!(text.contains("to tmp as owner"), "{text}");
        assert!(text.contains("type to post"), "the keys are said: {text}");

        // Picked, the line says who it answers, and the words are under it.
        view.step(-1);
        view.type_text("on it");
        let text = render_tab(&view, 70, 16).join("\n");
        assert!(text.contains("answering ROADMAP"), "{text}");
        assert!(text.contains("› on it"), "{text}");

        let none = BoardView::new(dir, Some("/nowhere/books".to_string()));
        let text = render_tab(&none, 70, 16).join("\n");
        assert!(text.contains("books has no board"), "{text}");
        assert!(text.contains("enter creates a board for books"), "{text}");
    }

    #[test]
    fn a_long_word_is_broken_and_a_short_line_is_not() {
        assert_eq!(
            wrap("the round trip works", 10),
            ["the round", "trip works"]
        );
        assert_eq!(wrap("abcdefghij-klm", 5), ["abcde", "fghij", "-klm"]);
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn empty_state_is_explained_not_blank() {
        let fixture = Fixture::new("ui-empty");
        let text = render(&fixture, 80, 24).join("\n");
        assert!(text.contains("No Claude Code sessions"));
    }
}
