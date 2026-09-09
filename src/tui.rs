//! The popup: one flat list of units, keyboard only, quits on `q`.
//!
//! Every geometry decision comes from the frame Herdr hands us. The declared `90% × 80%` loses two
//! rows and two columns to the popup's own chrome, and the percentage is of the window rather than
//! the host pane, so no size may be inferred from anything but the frame itself.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::json;

use crate::client::{Endpoint, Link, Target};
use crate::form::Form;
use crate::manifest::{LocalUnit, Project};
use crate::peek::{self, Peek};
use crate::project::Resolution;
use crate::readable;
use crate::rows::{self, Row};
use crate::state;
use crate::store::{Identity, Store};
use crate::tail;
use crate::terminal;
use crate::unit;
use crate::view::{Statuses, View};

const KIND_WIDTH: usize = 6;
const STATE_WIDTH: usize = 8;
const TIMING_WIDTH: usize = 9;
const NAME_MIN_WIDTH: usize = 8;
const GAP: usize = 1;

const CURSOR: &str = "\u{276f}";
const INDENT: &str = "  ";
/// What a notice wears so it cannot be mistaken for a row, and what a wrapped one is indented by.
const NOTICE_MARK: &str = "! ";
const DAEMON_SKEWED: &str = "daemon skewed";
const NO_DAEMON: &str = "no daemon";
const NO_VERB: &str = "s, x and r act on unit rows; ↹ unfolds a repo";
const NO_DOCKER_LOG: &str = "a docker service has no log file of ours to overlay";
const NO_DOCKER_TERMINAL: &str = "a docker service has no terminal of ours to type at";
const NO_REPO_TERMINAL: &str = "a repo row is not a unit; expand it with tab";

/// How long the loop waits for a keystroke while a peek is open. Following has to happen without one,
/// and an idle peek costs one wakeup a tick and no redraw, because an unchanged frame writes nothing.
const PEEK_POLL: Duration = Duration::from_millis(120);
const PEEK_KEYS: &str = "esc rows   ↑↓ scroll   ⇞⇟ page   f follow";

pub fn run() -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the tui needs a terminal on stdin and stdout",
        ));
    }
    install_panic_hook();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let outcome = event_loop();
    restore();
    outcome
}

fn event_loop() -> io::Result<()> {
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // The daemon first, then the project. The link is held for the popup's whole life, which is also what keeps an idle daemon from exiting underneath it.
    let mut link = Endpoint::spelled_out().open();
    let daemon = daemon_report(&link);
    let Resolution {
        project,
        complaint: trouble,
    } = Resolution::resolve();
    // The included manifests are read here, once: a repo row has to say what is inside it before
    // anything is unfolded, and reading a file is all that takes.
    let mut view = project.map(View::of);
    let mut notice: Option<String> = None;
    let mut form: Option<Form> = None;
    let mut statuses = Statuses::new();
    let mut peek: Option<Peek> = None;
    let mut cursor = 0;
    let mut refresh = true;

    loop {
        // Nothing ticks: the states are re-read once per keystroke, which is the only moment anything
        // is redrawn anyway. A peek scrolling is not such a moment — a status read runs `compose ps`,
        // which no scroll may pay for.
        if refresh {
            if let (Some(view), Ok(link)) = (&view, &mut link) {
                // One read per manifest on screen, and none for a repo still folded: the states of a
                // project nobody is looking at are not worth a `compose ps`.
                for (owner, project) in view.on_screen() {
                    match link.status(project) {
                        Ok(fresh) => {
                            statuses.insert(owner, fresh);
                        }
                        Err(complaint) => notice = Some(complaint),
                    }
                }
            }
            refresh = false;
        }
        let rows = match &view {
            Some(view) => view.rows(&statuses),
            None => Vec::new(),
        };
        cursor = cursor.min(rows.len().saturating_sub(1));

        let said = notices(
            view.as_ref(),
            &daemon,
            trouble.as_deref(),
            notice.as_deref(),
        );
        terminal.draw(|frame| match (&mut peek, &form) {
            (Some(peek), _) => draw_peek(frame, peek),
            (None, Some(form)) => draw_form(frame, form, notice.as_deref()),
            (None, None) => draw(frame, view.as_ref(), &rows, cursor, &said, &daemon),
        })?;
        // A peek waits only so long for a keystroke, so following happens without one. The rows have
        // nothing to show that a keystroke did not cause, and wait for one for as long as it takes.
        if let Some(open) = peek.as_mut()
            && !event::poll(PEEK_POLL)?
        {
            open.pump();
            continue;
        }
        // Kitty keyboard enhancement, when the host terminal has it on, reports releases too.
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if let Some(open) = peek.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    peek = None;
                    // The rows have been standing still behind the peek.
                    refresh = true;
                }
                KeyCode::Char('f') => open.toggle_follow(),
                KeyCode::Up | KeyCode::Char('k') => open.scroll(-1),
                KeyCode::Down | KeyCode::Char('j') => open.scroll(1),
                KeyCode::PageUp => open.page(-1),
                KeyCode::PageDown => open.page(1),
                _ => {}
            }
            continue;
        }

        if let Some(open) = form.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    form = None;
                    notice = None;
                }
                KeyCode::Backspace => open.backspace(),
                KeyCode::Enter => match open
                    .to_config()
                    .and_then(|config| crate::agent::save(&config).map(|()| config))
                {
                    Ok(config) => {
                        notice = Some(format!("saved: {}", config.command.join(" ")));
                        form = None;
                    }
                    Err(complaint) => notice = Some(complaint),
                },
                KeyCode::Char(c) => open.insert(c),
                _ => {}
            }
            continue;
        }

        refresh = true;
        match key.code {
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('c') => {
                form = Some(Form::open());
                notice = None;
            }
            KeyCode::Char('g') if view.is_none() => {
                // The popup owns the keyboard while it lives, so handing over means leaving.
                let launching =
                    notices(view.as_ref(), &daemon, trouble.as_deref(), Some(LAUNCHING));
                terminal
                    .draw(|frame| draw(frame, view.as_ref(), &rows, cursor, &launching, &daemon))?;
                match hand_to_agent() {
                    Ok(()) => return Ok(()),
                    Err(complaint) => notice = Some(complaint),
                }
            }
            KeyCode::Up | KeyCode::Char('k') => cursor = stepped(cursor, -1, rows.len()),
            KeyCode::Down | KeyCode::Char('j') => cursor = stepped(cursor, 1, rows.len()),
            KeyCode::Tab | KeyCode::BackTab => {
                notice = match (&mut view, rows.get(cursor)) {
                    (Some(view), Some(row)) => view.toggle(row).err(),
                    _ => Some(crate::view::NOT_A_REPO.to_string()),
                };
            }
            KeyCode::Char(verb @ ('s' | 'x' | 'r')) => {
                notice = perform(verb, &mut link, view.as_ref(), &rows, cursor);
            }
            KeyCode::Char('L') => match open_peek(view.as_ref(), &rows, cursor) {
                Ok(open) => {
                    peek = Some(open);
                    notice = None;
                }
                Err(complaint) => notice = Some(complaint),
            },
            KeyCode::Char('O') => match open_overlay(view.as_ref(), &rows, cursor) {
                // The overlay outlives us; the popup would own the keyboard if it stayed.
                Ok(()) => return Ok(()),
                Err(complaint) => notice = Some(complaint),
            },
            KeyCode::Char('A') => match open_attach(view.as_ref(), &rows, cursor) {
                // The pane wants the keyboard, and only one of us can have it.
                Ok(()) => return Ok(()),
                Err(complaint) => notice = Some(complaint),
            },
            _ => {}
        }
    }
}

/// The peek over the list: heading, the lines themselves, and what the keys are. `esc` brings the rows
/// back — the popup itself never goes anywhere.
fn draw_peek(frame: &mut Frame, peek: &mut Peek) {
    let dim = Style::default().fg(Color::DarkGray);
    let width = frame.area().width as usize;
    let notices = notice_lines(
        &peek
            .trouble()
            .map(Notice::warn)
            .into_iter()
            .collect::<Vec<_>>(),
        width,
        notice_room(frame.area().height),
    );
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(notices.len() as u16 + 1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            peek.heading().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        areas[0],
    );

    let shown = peek.view(areas[1].height as usize, areas[1].width as usize);
    // No `Wrap`: a long line is clipped, which is what a peek does with one.
    frame.render_widget(
        Paragraph::new(shown.iter().map(painted).collect::<Vec<_>>()),
        areas[1],
    );

    let footer = Line::from(vec![
        Span::styled(PEEK_KEYS, dim),
        Span::raw("   "),
        Span::styled(
            match peek.following() {
                true => "following".to_string(),
                false => format!("paused {}", peek.position()),
            },
            dim,
        ),
    ]);
    frame.render_widget(Paragraph::new(above(notices, footer)), areas[2]);
}

fn draw_form(frame: &mut Frame, form: &Form, notice: Option<&str>) {
    let dim = Style::default().fg(Color::DarkGray);
    let notices = notice_lines(
        &notice.map(Notice::warn).into_iter().collect::<Vec<_>>(),
        frame.area().width as usize,
        notice_room(frame.area().height),
    );
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(notices.len() as u16 + 1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Agent used to write a manifest",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        areas[0],
    );

    let body = vec![
        field_line(form),
        Line::raw(""),
        Line::from(Span::styled(
            format!(
                "run as: <command> {} <project>",
                crate::agent::SKILL_COMMAND
            ),
            dim,
        )),
        Line::from(Span::styled(
            format!("saved to {}", crate::agent::config_path().display()),
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(body), areas[1]);

    let footer = Line::from(Span::styled("⏎ save   esc cancel", dim));
    frame.render_widget(Paragraph::new(above(notices, footer)), areas[2]);
}

fn field_line(form: &Form) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let shown = if form.command.is_empty() {
        "what to run; may be a wrapper".to_string()
    } else {
        format!("{}\u{2588}", form.command)
    };
    let style = if form.command.is_empty() {
        dim
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(" \u{203a} command  ", dim),
        Span::styled(shown, style),
    ])
}

/// The cursor never wraps and never leaves the list.
fn stepped(cursor: usize, delta: isize, len: usize) -> usize {
    match len {
        0 => 0,
        _ if delta < 0 => cursor.saturating_sub(delta.unsigned_abs()),
        len => (cursor + delta.unsigned_abs()).min(len - 1),
    }
}

/// What the cursor row acts on, and which project it belongs to — the row's own, which for an unfolded
/// row is the included repo rather than the one being looked at. An included repo's own row is the only
/// one no verb reaches: it stands for a whole manifest of its own.
fn selected<'a>(
    view: &'a View,
    rows: &[Row],
    cursor: usize,
) -> Result<(&'a Project, Target<'a>), String> {
    let row = rows
        .get(cursor)
        .ok_or_else(|| "nothing here to act on".to_string())?;
    if row.repo {
        return Err(NO_VERB.to_string());
    }
    let project = view.project(row.owner).ok_or_else(|| NO_VERB.to_string())?;
    Target::of(project, row.kind, &row.name).map(|target| (project, target))
}

/// Every verb is a request to the daemon; the popup itself spawns nothing and signals nothing.
fn perform(
    verb: char,
    link: &mut Result<Link, String>,
    view: Option<&View>,
    rows: &[Row],
    cursor: usize,
) -> Option<String> {
    let view = view?;
    let (project, target) = match selected(view, rows, cursor) {
        Ok(selected) => selected,
        Err(complaint) => return Some(complaint),
    };
    let link = match link {
        Ok(link) => link,
        Err(complaint) => return Some(complaint.clone()),
    };
    let asked = match verb {
        's' => link.start(project, &target),
        'x' => link.stop(project, &target),
        _ => link.restart(project, &target),
    };
    match asked {
        Ok(note) => note,
        Err(complaint) => Some(complaint),
    }
}

/// The log the row under the cursor would overlay, or why it has none. Only our own local units keep
/// a log file; a docker service's output is compose's and a repo row is not a unit at all.
fn log_path(store: &Store, view: &View, rows: &[Row], cursor: usize) -> Result<PathBuf, String> {
    let row = rows
        .get(cursor)
        .ok_or_else(|| "nothing here to act on".to_string())?;
    match row.kind {
        unit::DOCKER => return Err(NO_DOCKER_LOG.to_string()),
        unit::LOCAL => {}
        _ => return Err(peek::NO_REPO_LOG.to_string()),
    }
    // The log belongs to the project that owns the unit, so an unfolded row overlays the included
    // repo's own log rather than one under the including project's key.
    let project = view
        .project(row.owner)
        .ok_or_else(|| peek::NO_REPO_LOG.to_string())?;
    let identity = Identity {
        path: project.root.clone(),
        name: project.name.clone(),
    };
    let path = store
        .slot(&identity)
        .log_path(&unit::key(unit::LOCAL, &row.name));
    if !path.exists() {
        return Err(format!("no log yet for {}: start it first", row.name));
    }
    Ok(path)
}

/// The peek `L` opens on the row under the cursor. Nothing here can close the popup: a row with no log
/// of its own is a notice in the footer, not an empty screen.
fn open_peek(view: Option<&View>, rows: &[Row], cursor: usize) -> Result<Peek, String> {
    let view = view.ok_or_else(|| "no project here".to_string())?;
    let row = rows
        .get(cursor)
        .ok_or_else(|| "nothing here to peek".to_string())?;
    let project = view
        .project(row.owner)
        .ok_or_else(|| peek::NO_REPO_LOG.to_string())?;
    peek::Source::of(&Store::at(state::root()), project, row.kind, &row.name)?.open()
}

/// Herdr runs the pane the manifest declares; all this call adds is which log it should follow. No
/// `cwd` is passed: the declared argv is relative, and naming a cwd is measured to resolve it there
/// instead of in the plugin root, where the binary actually is.
fn open_overlay(view: Option<&View>, rows: &[Row], cursor: usize) -> Result<(), String> {
    let view = view.ok_or_else(|| "no project here".to_string())?;
    let path = log_path(&Store::at(state::root()), view, rows, cursor)?;
    crate::herdr::request(
        "plugin.pane.open",
        json!({
            "plugin_id": tail::PLUGIN_ID,
            "entrypoint": tail::ENTRYPOINT,
            "env": {tail::LOG_ENV: path.to_string_lossy()},
            "focus": true,
        }),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// The unit the row under the cursor would be typed at, or why there is none. Decided before anything
/// is opened, because a pane onto nothing is worse than a sentence in the footer.
fn attachable<'a>(
    view: Option<&'a View>,
    rows: &[Row],
    cursor: usize,
) -> Result<(&'a Project, &'a LocalUnit), String> {
    let view = view.ok_or_else(|| "no project here".to_string())?;
    let row = rows
        .get(cursor)
        .ok_or_else(|| "nothing here to type at".to_string())?;
    match row.kind {
        unit::DOCKER => return Err(NO_DOCKER_TERMINAL.to_string()),
        unit::LOCAL => {}
        _ => return Err(NO_REPO_TERMINAL.to_string()),
    }
    let project = view
        .project(row.owner)
        .ok_or_else(|| NO_REPO_TERMINAL.to_string())?;
    let unit = project
        .local
        .iter()
        .find(|unit| unit.name == row.name)
        .ok_or_else(|| format!("no local unit named {}", row.name))?;
    if let Some(problem) = &unit.problem {
        return Err(problem.clone());
    }
    if !unit.tty {
        return Err(terminal::runs_on_a_pipe(&unit.name));
    }
    // The daemon would refuse a unit it is not the parent of, and it is the row that can say why.
    if row.state != unit::State::Up.label() {
        return Err(format!("{} is not running; start it first", unit.name));
    }
    Ok((project, unit))
}

/// The attach pane `A` opens. Herdr runs the pane the manifest declares; all this call adds is which
/// unit it should type at.
fn open_attach(view: Option<&View>, rows: &[Row], cursor: usize) -> Result<(), String> {
    let (project, unit) = attachable(view, rows, cursor)?;
    crate::herdr::request(
        "plugin.pane.open",
        json!({
            "plugin_id": tail::PLUGIN_ID,
            "entrypoint": crate::attach::ENTRYPOINT,
            "env": {
                crate::attach::PROJECT_ENV: project.root.to_string_lossy(),
                crate::attach::UNIT_ENV: unit.name,
            },
            "focus": true,
        }),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

const LAUNCHING: &str = "splitting a pane and starting an agent…";

fn hand_to_agent() -> Result<(), String> {
    let config = crate::agent::Config::load()?;
    let dir = crate::agent::target_dir()
        .ok_or_else(|| "cannot tell which repository you are in".to_string())?;
    crate::agent::launch(&config, &dir).map(|_| ())
}

/// A popup is session-modal and owns the keyboard, so a panic that left the terminal in raw mode
/// would cost the user their keyboard until they called `popup.close` from elsewhere.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
        std::process::exit(1);
    }));
}

fn restore() {
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
    let _ = io::stdout().flush();
}

/// A sentence the footer has to make readable in full: a refusal, a manifest complaint, a daemon that
/// cannot be used. None of them shares the keys' line, where at any real popup width the longer ones
/// were cut off mid-sentence.
#[derive(Debug, Clone)]
struct Notice {
    text: String,
    colour: Color,
}

impl Notice {
    fn warn(text: impl Into<String>) -> Notice {
        Notice {
            text: text.into(),
            colour: Color::Yellow,
        }
    }

    fn bad(text: impl Into<String>) -> Notice {
        Notice {
            text: text.into(),
            colour: Color::Red,
        }
    }
}

/// The daemon as the heading shows it — its version and pid — plus, when it cannot be used, the whole
/// of why. Skew and an unreachable socket are sentences, so the heading keeps a tag and the sentence
/// goes where sentences are readable.
struct Daemon {
    tag: String,
    colour: Color,
    trouble: Option<Notice>,
}

fn daemon_report(link: &Result<Link, String>) -> Daemon {
    match link {
        Ok(link) if link.skewed() => Daemon {
            tag: DAEMON_SKEWED.to_string(),
            colour: Color::Yellow,
            trouble: Some(Notice::warn(link.footer())),
        },
        Ok(link) => Daemon {
            tag: link.footer(),
            colour: Color::DarkGray,
            trouble: None,
        },
        Err(complaint) => Daemon {
            tag: NO_DAEMON.to_string(),
            colour: Color::Red,
            trouble: Some(Notice::bad(complaint.clone())),
        },
    }
}

/// Everything with a claim on the footer, newest last: a manifest's own complaints stand for as long
/// as the manifest does, and the answer to the last keystroke is the line nearest the keys.
fn notices(
    view: Option<&View>,
    daemon: &Daemon,
    trouble: Option<&str>,
    notice: Option<&str>,
) -> Vec<Notice> {
    let mut said: Vec<Notice> = view
        .map(View::complaints)
        .unwrap_or_default()
        .into_iter()
        .map(Notice::warn)
        .collect();
    said.extend(daemon.trouble.clone());
    said.extend(trouble.map(Notice::bad));
    said.extend(notice.map(Notice::warn));
    // A skewed daemon refuses every verb with the same sentence the heading already stands under, and
    // reading it twice says nothing the once did not.
    let mut seen = BTreeSet::new();
    said.retain(|notice| seen.insert(notice.text.clone()));
    said
}

fn draw(
    frame: &mut Frame,
    view: Option<&View>,
    rows: &[Row],
    cursor: usize,
    said: &[Notice],
    daemon: &Daemon,
) {
    let dim = Style::default().fg(Color::DarkGray);
    let width = frame.area().width as usize;
    let notices = notice_lines(said, width, notice_room(frame.area().height));
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(notices.len() as u16 + 1),
    ])
    .split(frame.area());

    let (heading, body) = match view {
        Some(view) => (rows::title(view.focused()), unit_lines(rows, cursor, dim)),
        None => (String::new(), empty_state(dim)),
    };

    frame.render_widget(
        Paragraph::new(heading_line(&heading, daemon, width)),
        areas[0],
    );
    frame.render_widget(Paragraph::new(body), areas[1]);
    frame.render_widget(Paragraph::new(above(notices, keys(view, dim))), areas[2]);
}

/// The heading: where the project is, and the daemon right-aligned against it.
fn heading_line(title: &str, daemon: &Daemon, width: usize) -> Line<'static> {
    let tag = daemon.tag.chars().count();
    let title = elide(title, width.saturating_sub(tag + GAP));
    let filler = width.saturating_sub(title.chars().count() + tag);
    Line::from(vec![
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(filler)),
        Span::styled(daemon.tag.clone(), Style::default().fg(daemon.colour)),
    ])
}

fn keys(view: Option<&View>, dim: Style) -> Line<'static> {
    let mut hints: Vec<(&str, &str)> = vec![("q", "quit"), ("c", "agent")];
    match view {
        None => hints.push(("g", "write a manifest")),
        Some(view) => {
            hints.extend([
                ("s", "start"),
                ("x", "stop"),
                ("r", "restart"),
                ("L", "log"),
                ("O", "overlay"),
            ]);
            if view.has_terminals() {
                hints.push(("A", "attach"));
            }
            if view.has_repos() {
                hints.push(("\u{21b9}", "repo"));
            }
        }
    }
    Line::from(hint_spans(&hints, dim))
}

/// The key stands out and its word explains it, because a uniformly dim line reads as decoration.
fn hint_spans(hints: &[(&str, &str)], dim: Style) -> Vec<Span<'static>> {
    let key = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let word = Style::default().fg(Color::Gray);
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (index, (stroke, meaning)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", dim));
        }
        spans.push(Span::styled((*stroke).to_string(), key));
        spans.push(Span::styled(format!(" {meaning}"), word));
    }
    spans
}

/// The keys keep the bottom line whatever else is being said, so a notice appearing never moves them.
fn above(notices: Vec<Line<'static>>, keys: Line<'static>) -> Vec<Line<'static>> {
    notices.into_iter().chain([keys]).collect()
}

/// Every notice wrapped to the popup's width and marked, so no notice can be read as a row and none is
/// clipped. What will not fit in the room the row list can spare is counted at the top: the newest
/// notice is the one kept, because it is the one a keystroke just asked for.
fn notice_lines(said: &[Notice], width: usize, room: usize) -> Vec<Line<'static>> {
    let indent = " ".repeat(NOTICE_MARK.chars().count());
    let mut lines: Vec<Line<'static>> = Vec::new();
    for notice in said {
        let wrapped = wrap(&notice.text, width.saturating_sub(indent.chars().count()));
        for (nth, part) in wrapped.into_iter().enumerate() {
            let mark = match nth {
                0 => NOTICE_MARK,
                _ => indent.as_str(),
            };
            lines.push(Line::from(Span::styled(
                format!("{mark}{part}"),
                Style::default().fg(notice.colour),
            )));
        }
    }
    if lines.len() > room {
        let dropped = lines.len() + 1 - room;
        lines = lines.split_off(lines.len() - room.saturating_sub(1));
        if room > 0 {
            lines.insert(
                0,
                Line::from(Span::styled(
                    format!("{indent}…and {dropped} more"),
                    Style::default().fg(Color::DarkGray),
                )),
            );
        }
    }
    lines
}

/// How much of the popup the notices may take: half of it, and never the last row of the list. With
/// nothing to say they take none, which is what keeps the list its full height.
fn notice_room(height: u16) -> usize {
    let height = height as usize;
    (height / 2).min(height.saturating_sub(3))
}

/// Wraps on spaces. A word wider than the line is left whole and runs to the edge: what is that long is
/// a path or a command, and half of one is worse to read than one that reaches the edge.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// A heading too long for the room it has keeps its tail: which repo this is matters more than where
/// the tree it sits in begins.
fn elide(text: &str, room: usize) -> String {
    let length = text.chars().count();
    match room {
        0 => String::new(),
        room if length <= room => text.to_string(),
        room => format!(
            "…{}",
            text.chars().skip(length + 1 - room).collect::<String>()
        ),
    }
}

fn unit_lines(rows: &[Row], cursor: usize, dim: Style) -> Vec<Line<'static>> {
    let name_width = rows
        .iter()
        .map(|row| label(row).chars().count())
        .max()
        .unwrap_or(0)
        .max(NAME_MIN_WIDTH);
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let [glyph, name, kind, state, timing, note] = cells(row, name_width);
            let selected = index == cursor;
            let marker = if selected { CURSOR } else { " " };
            let marker_style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let state_style = state_style(&row.state, dim);
            Line::from(vec![
                Span::styled(marker.to_string(), marker_style),
                Span::styled(format!("{glyph} "), state_style),
                Span::raw(name),
                Span::styled(kind, dim),
                Span::styled(state, state_style),
                Span::styled(timing, dim),
                Span::styled(note, dim),
            ])
        })
        .collect()
}

fn empty_state(dim: Style) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            rows::EMPTY_HEADLINE,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(rows::EMPTY_BODY, dim)),
        Line::raw(""),
        Line::from(Span::styled(rows::EMPTY_HINT, dim)),
    ]
}

/// Each column padded to its own width bar the note, which takes what is left.
/// The state column is the one cell worth reading at a glance, so it carries the colour and the
/// glyph takes the same one. A row whose state is not a `unit::State` is holding docker's own health
/// wording, which is only ever something to look at; a repo row has no state at all.
/// A log line as the program painted it. Its colours name palette entries rather than paint, so a
/// log reads in the terminal's own theme, the same as it would in a shell.
fn painted(line: &readable::Line) -> Line<'static> {
    Line::from(
        line.segments()
            .iter()
            .map(|segment| Span::styled(segment.text.clone(), sgr_style(segment.sgr)))
            .collect::<Vec<_>>(),
    )
}

fn sgr_style(sgr: readable::Sgr) -> Style {
    let mut style = Style::default();
    if let Some(colour) = sgr.fg {
        style = style.fg(paint(colour));
    }
    if let Some(colour) = sgr.bg {
        style = style.bg(paint(colour));
    }
    for (set, modifier) in [
        (sgr.bold, Modifier::BOLD),
        (sgr.dim, Modifier::DIM),
        (sgr.italic, Modifier::ITALIC),
        (sgr.underline, Modifier::UNDERLINED),
        (sgr.reverse, Modifier::REVERSED),
    ] {
        if set {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn paint(colour: readable::Colour) -> Color {
    match colour {
        readable::Colour::Ansi(index) | readable::Colour::Indexed(index) => Color::Indexed(index),
        readable::Colour::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn state_style(state: &str, dim: Style) -> Style {
    let state = state.trim();
    if state.is_empty() {
        return dim;
    }

    match unit::State::read(state) {
        Some(unit::State::Up) => Style::default().fg(Color::Green),
        Some(unit::State::Starting) => Style::default().fg(Color::Yellow),
        Some(unit::State::Done) => Style::default().fg(Color::Blue),
        Some(unit::State::Dead) => Style::default().fg(Color::Red),
        Some(unit::State::Down) | Some(unit::State::Unknown) => dim,
        None => Style::default().fg(Color::Yellow),
    }
}

fn cells(row: &Row, name_width: usize) -> [String; 6] {
    [
        row.glyph.to_string(),
        pad(&label(row), name_width),
        pad(row.kind, KIND_WIDTH),
        pad(&row.state, STATE_WIDTH),
        pad(&row.timing, TIMING_WIDTH),
        row.note.clone(),
    ]
}

/// A unit of an unfolded repo is stepped in: its state columns still line up with everything else's,
/// because only the name moves.
fn label(row: &Row) -> String {
    match row.indent {
        true => format!("{INDENT}{}", row.name),
        false => row.name.clone(),
    }
}

fn pad(cell: &str, width: usize) -> String {
    let filler = (width + GAP).saturating_sub(cell.chars().count());
    format!("{cell}{}", " ".repeat(filler))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &'static str, note: &str) -> Row {
        Row {
            glyph: rows::UNKNOWN_GLYPH,
            name: name.to_string(),
            kind,
            state: String::new(),
            timing: String::new(),
            note: note.to_string(),
            owner: crate::view::Owner::Focused,
            indent: false,
            repo: false,
        }
    }

    /// The include here names a directory that does not exist, so the repo row carries the reason
    /// rather than a manifest — which is all these tests need of it: it is a row no verb reaches.
    fn view(text: &str) -> View {
        View::of(
            crate::manifest::Project::parse(
                text,
                std::path::Path::new("/repos/storefront/.herdr-dev.toml"),
            )
            .expect("manifest"),
        )
    }

    fn shown(view: &View) -> Vec<Row> {
        view.rows(&Statuses::new())
    }

    #[test]
    fn the_cursor_stops_at_both_ends_rather_than_wrapping() {
        assert_eq!(stepped(0, -1, 3), 0);
        assert_eq!(stepped(0, 1, 3), 1);
        assert_eq!(stepped(2, 1, 3), 2);
        assert_eq!(stepped(0, 1, 0), 0);
        assert_eq!(stepped(0, -1, 0), 0);
    }

    #[test]
    fn a_verb_finds_the_unit_under_the_cursor_of_either_kind_and_refuses_a_repo_row() {
        let view = view(
            "[local.vite]\ncmd = [\"bin/vite\"]\n[docker]\nnames = [\"db\"]\n\
             [includes.accounts]\npath = \"/repos/accounts\"\n",
        );
        let manifest = view.focused();
        let rows = shown(&view);
        assert_eq!(
            selected(&view, &rows, 0),
            Ok((manifest, Target::Docker(&manifest.docker[0])))
        );
        assert_eq!(
            selected(&view, &rows, 1),
            Ok((manifest, Target::Local(&manifest.local[0])))
        );
        assert_eq!(selected(&view, &rows, 2).unwrap_err(), NO_VERB);
        assert!(selected(&view, &rows, 9).is_err());
    }

    #[test]
    fn only_a_local_row_has_a_log_to_overlay() {
        let view = view(
            "[local.vite]\ncmd = [\"bin/vite\"]\n[docker]\nnames = [\"db\"]\n\
             [includes.accounts]\npath = \"/repos/accounts\"\n",
        );
        let rows = shown(&view);
        let store = Store::at("/state/herdr-dev");
        assert_eq!(
            log_path(&store, &view, &rows, 0).unwrap_err(),
            NO_DOCKER_LOG
        );
        assert_eq!(
            log_path(&store, &view, &rows, 2).unwrap_err(),
            peek::NO_REPO_LOG
        );
        // No such log exists under that root, so the local row refuses an empty overlay too.
        let complaint = log_path(&store, &view, &rows, 1).unwrap_err();
        assert!(complaint.contains("no log yet for vite"), "{complaint}");
    }

    #[test]
    fn a_local_row_overlays_the_log_the_store_keeps_for_it() {
        let view = view("[local.vite]\ncmd = [\"bin/vite\"]\n");
        let rows = shown(&view);
        let root = std::env::temp_dir().join("herdr-dev-tui-overlay");
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::at(&root);
        let slot = store
            .open(&Identity {
                path: view.focused().root.clone(),
                name: view.focused().name.clone(),
            })
            .expect("slot");
        let log = slot.log_path("local-vite");
        std::fs::write(&log, "").expect("log");
        let found = log_path(&store, &view, &rows, 0);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, Ok(log));
    }

    /// The rows a `View` builds carry `down` for a local unit the daemon has said nothing about, so
    /// running has to be put there by hand.
    fn running(rows: &mut [Row], name: &str) {
        for row in rows.iter_mut().filter(|row| row.name == name) {
            row.state = unit::State::Up.label().to_string();
        }
    }

    #[test]
    fn only_a_running_unit_that_asked_for_a_terminal_can_be_typed_at() {
        let view = view(
            "[local.console]\ncmd = [\"bin/rails\", \"c\"]\ntty = true\n\
             [local.vite]\ncmd = [\"bin/vite\"]\n[docker]\nnames = [\"db\"]\n\
             [includes.accounts]\npath = \"/repos/accounts\"\n",
        );
        let mut rows = shown(&view);
        assert_eq!(
            attachable(Some(&view), &rows, 0).unwrap_err(),
            NO_DOCKER_TERMINAL
        );
        assert_eq!(
            attachable(Some(&view), &rows, 3).unwrap_err(),
            NO_REPO_TERMINAL
        );
        // Declared with a terminal, but nothing is running it yet.
        let complaint = attachable(Some(&view), &rows, 1).unwrap_err();
        assert_eq!(complaint, "console is not running; start it first");
        // Running, but on a pipe: no terminal was ever asked for.
        running(&mut rows, "vite");
        let complaint = attachable(Some(&view), &rows, 2).unwrap_err();
        assert!(complaint.contains("vite runs on a pipe"), "{complaint}");
        assert!(complaint.contains("tty = true"), "{complaint}");

        running(&mut rows, "console");
        let (project, unit) = attachable(Some(&view), &rows, 1).expect("a unit to type at");
        assert_eq!(unit.name, "console");
        assert_eq!(project.root, view.focused().root);
    }

    #[test]
    fn a_unit_the_manifest_could_not_make_sense_of_refuses_with_its_own_complaint() {
        let view = view("[local.rails]\ncwd = \"/tmp\"\n");
        let rows = shown(&view);
        let complaint = selected(&view, &rows, 0).unwrap_err();
        assert!(complaint.contains("cmd"), "{complaint}");
    }

    #[test]
    fn columns_line_up_across_rows_and_the_note_comes_last() {
        let rows = [
            row("db", "docker", "unhealthy"),
            row("accounts", "", "from its own config"),
        ];
        let lines: Vec<String> = rows.iter().map(|r| cells(r, 13).join("")).collect();
        let note_at: Vec<Option<usize>> = lines
            .iter()
            .zip(["unhealthy", "from its own config"])
            .map(|(line, note)| line.find(note))
            .collect();
        assert_eq!(note_at[0], note_at[1]);
        assert!(note_at[0].is_some());
    }

    #[test]
    fn an_unfolded_row_is_stepped_in_without_taking_the_other_columns_with_it() {
        let mut inner = row("rails", "local", "");
        inner.indent = true;
        let outer = row("rails", "local", "");
        let [_, name, kind, ..] = cells(&inner, 8);
        assert_eq!(name, format!("{INDENT}rails  "));
        assert_eq!(name.chars().count(), cells(&outer, 8)[1].chars().count());
        assert_eq!(kind, cells(&outer, 8)[2]);
    }

    #[test]
    fn a_blank_state_still_holds_its_column_open() {
        let cells = cells(&row("db", "docker", ""), 8);
        assert_eq!(cells[3].trim(), "");
        assert_eq!(cells[3].len(), STATE_WIDTH + GAP);
        assert_eq!(cells[4].len(), TIMING_WIDTH + GAP);
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_notice_too_long_for_the_width_is_wrapped_whole_rather_than_clipped() {
        let long = peek::NO_REPO_LOG;
        let lines = texts(&notice_lines(&[Notice::warn(long)], 40, 8));
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|line| line.chars().count() <= 40));
        assert!(lines[0].starts_with(NOTICE_MARK));
        assert!(lines[1].starts_with("  "));
        let read_back = lines
            .iter()
            .map(|line| line.trim_start_matches(['!', ' ']))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(read_back, long);
    }

    #[test]
    fn the_footer_is_one_line_with_nothing_to_say_and_grows_by_what_there_is() {
        assert!(notice_lines(&[], 80, 8).is_empty());
        assert_eq!(
            notice_lines(&[Notice::warn("short enough")], 80, 8).len(),
            1
        );
    }

    /// The newest notice is the one a keystroke just asked for, so it is the one that survives.
    #[test]
    fn more_notices_than_the_list_can_spare_are_counted_and_the_newest_kept() {
        let said: Vec<Notice> = (0..6)
            .map(|nth| Notice::warn(format!("complaint {nth}")))
            .collect();
        let lines = texts(&notice_lines(&said, 80, 3));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("…and 4 more"), "{lines:?}");
        assert!(lines[2].ends_with("complaint 5"), "{lines:?}");
    }

    #[test]
    fn the_notices_never_take_the_last_row_of_the_list() {
        assert_eq!(notice_room(24), 12);
        assert_eq!(notice_room(4), 1);
        assert_eq!(notice_room(3), 0);
    }

    #[test]
    fn the_daemon_is_right_aligned_against_the_heading_rather_than_after_the_keys() {
        let daemon = Daemon {
            tag: "daemon 0.1.0  pid 4242".to_string(),
            colour: Color::DarkGray,
            trouble: None,
        };
        let line = texts(&[heading_line("~/projects/acme/storefront", &daemon, 80)]);
        assert_eq!(line[0].chars().count(), 80);
        assert!(line[0].starts_with("~/projects/acme/storefront"));
        assert!(line[0].ends_with("daemon 0.1.0  pid 4242"));
    }

    /// A heading long enough to collide with the daemon loses its head, not the daemon.
    #[test]
    fn a_heading_with_no_room_left_keeps_its_tail() {
        let daemon = Daemon {
            tag: "daemon 0.1.0  pid 4242".to_string(),
            colour: Color::DarkGray,
            trouble: None,
        };
        let long = "~/projects/acme/very/deeply/nested/checkouts/storefront-wt2";
        let line = texts(&[heading_line(long, &daemon, 60)]);
        assert!(line[0].ends_with("daemon 0.1.0  pid 4242"), "{line:?}");
        assert!(line[0].starts_with('…'), "{line:?}");
        assert!(line[0].contains("checkouts/storefront-wt2"), "{line:?}");
        assert_eq!(line[0].chars().count(), 60);
        assert_eq!(
            elide("~/projects/acme/storefront", 40),
            "~/projects/acme/storefront"
        );
    }

    #[test]
    fn a_skewed_daemon_says_the_whole_of_why_in_a_notice_and_tags_the_heading() {
        let daemon = Daemon {
            tag: DAEMON_SKEWED.to_string(),
            colour: Color::Yellow,
            trouble: Some(Notice::warn(
                "daemon 0.1.0 pid 4242 speaks protocol 3, this build speaks 2 — kill 4242 to clear it",
            )),
        };
        let lines = texts(&notice_lines(&notices(None, &daemon, None, None), 80, 12));
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|line| line.chars().count() <= 80));
        let read_back = lines
            .iter()
            .map(|line| line.trim_start_matches(['!', ' ']))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            read_back,
            "daemon 0.1.0 pid 4242 speaks protocol 3, this build speaks 2 — kill 4242 to clear it"
        );
    }

    #[test]
    fn a_name_longer_than_its_column_is_not_truncated() {
        let cells = cells(&row("a_very_long_unit_name", "local", ""), 8);
        assert!(cells[1].starts_with("a_very_long_unit_name"));
    }
}
