//! A sound, and a notification, when a session needs you.
//!
//! The trigger is a *transition*, not a state: a job arriving in Needs input,
//! or reaching Done. That is the one thing neither event loop keeps, so this
//! module is mostly the memory of the last snapshot, and the rules for when a
//! change in it is worth interrupting someone.
//!
//! Deciding is separate from announcing. [`Ping::observe`] is pure — snapshot
//! in, [`Notice`] out — so every rule below is tested without a terminal, a
//! speaker or a clock. [`Notice::announce`] is the part that makes noise.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::job::{Snapshot, Status};

/// After a ping, stay silent this long. Agents finish in bursts — five in a
/// second is normal — and a burst must be one ping, not a drum roll.
const QUIET: Duration = Duration::from_secs(20);

/// Which transitions are worth interrupting someone for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum When {
    /// Only when a session is asking you something. The default: it is the
    /// one transition that is actually blocking on you.
    #[default]
    Needs,
    /// Also when a session finishes.
    Done,
    Off,
}

impl When {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "needs" => Some(When::Needs),
            "done" => Some(When::Done),
            "off" | "none" => Some(When::Off),
            _ => None,
        }
    }
}

/// One ping's worth of news: what to say, and how many sessions it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

pub struct Ping {
    when: When,
    sound: bool,
    /// The status each job was in when we last looked. A job missing from here
    /// has never been seen, and a job's *first* sighting never pings: Savras
    /// starting up beside eight finished sessions is not eight interruptions.
    seen: HashMap<String, Status>,
    primed: bool,
    last: Option<Instant>,
}

impl Ping {
    pub fn new(when: When, sound: bool) -> Self {
        Self {
            when,
            sound,
            seen: HashMap::new(),
            primed: false,
            last: None,
        }
    }

    /// Note a new snapshot, and say what — if anything — deserves a ping.
    ///
    /// `open` is the session showing in the working pane; it never pings,
    /// because you are looking straight at it.
    pub fn observe(
        &mut self,
        snapshot: &Snapshot,
        open: Option<&str>,
        now: Instant,
    ) -> Option<Notice> {
        let mut fired = Vec::new();

        for job in &snapshot.jobs {
            let before = self.seen.insert(job.short.clone(), job.status);
            if self.when == When::Off || !self.primed {
                continue;
            }
            // Only a change of status is news. A job sitting in Needs input
            // for an hour has already been announced once.
            if before == Some(job.status) {
                continue;
            }
            if Some(job.short.as_str()) == open || Some(job.session_id.as_str()) == open {
                continue;
            }
            if self.worth_saying(job.status) {
                fired.push(job);
            }
        }

        // Jobs that vanished must not linger in the map, or a short id Claude
        // Code reuses would look like it never changed.
        self.seen
            .retain(|short, _| snapshot.jobs.iter().any(|j| &j.short == short));

        // The first snapshot only teaches us what is already on screen.
        if !self.primed {
            self.primed = true;
            return None;
        }
        if fired.is_empty() {
            return None;
        }
        if self.last.is_some_and(|t| now.duration_since(t) < QUIET) {
            return None;
        }
        self.last = Some(now);

        let notice = match fired.as_slice() {
            [job] => Notice {
                title: match job.status {
                    Status::NeedsInput => format!("{} needs you", job.name),
                    _ => format!("{} finished", job.name),
                },
                body: job.summary.clone(),
            },
            many => {
                let asking = many
                    .iter()
                    .filter(|j| j.status == Status::NeedsInput)
                    .count();
                Notice {
                    title: format!("{} sessions", many.len()),
                    body: if asking > 0 {
                        format!("{asking} needing you")
                    } else {
                        "finished".into()
                    },
                }
            }
        };
        Some(notice)
    }

    fn worth_saying(&self, status: Status) -> bool {
        match status {
            Status::NeedsInput => self.when != When::Off,
            Status::Done => self.when == When::Done,
            Status::Working => false,
        }
    }

    /// The whole point, in one call: look, and make noise if there is reason to.
    pub fn poll(&mut self, snapshot: &Snapshot, open: Option<&str>) {
        if let Some(notice) = self.observe(snapshot, open, Instant::now()) {
            notice.announce(self.sound);
        }
    }
}

impl Notice {
    /// Say it: a desktop notification through the terminal, and a sound.
    ///
    /// Both are best-effort and neither can fail loudly — a panel that panics
    /// because a sound file moved is worse than a silent one.
    pub fn announce(&self, sound: bool) {
        let mut out = std::io::stdout();
        let _ = out.write_all(wrap(&osc9(&self.title, &self.body)).as_bytes());
        let _ = out.flush();
        if sound {
            play();
        }
    }
}

/// OSC 9, understood by iTerm2, WezTerm, Ghostty, kitty and Windows Terminal.
///
/// Only OSC 9, deliberately: several of those understand OSC 777 as well, and
/// sending both gets you two notifications for one event. Terminal.app
/// understands neither, and there the bell and the sound are the whole story.
fn osc9(title: &str, body: &str) -> String {
    // A stray BEL or ESC inside the text would end the sequence early and
    // spill the rest onto the screen.
    let text = sanitize(&format!("{title} — {body}"));
    format!("\x1b]9;{text}\x07")
}

/// Inside tmux, an escape sequence meant for the real terminal has to be
/// handed through explicitly, or tmux eats it.
fn wrap(seq: &str) -> String {
    if std::env::var_os("TMUX").is_none() {
        return seq.to_string();
    }
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect::<String>()
        .trim()
        .to_string()
}

/// A sound, without blocking the panel and without caring whether it worked.
/// The terminal bell is the floor: it costs nothing and works everywhere.
fn play() {
    let _ = std::io::stdout().write_all(b"\x07");
    let _ = std::io::stdout().flush();

    let candidates: [(&str, &[&str]); 3] = if cfg!(target_os = "macos") {
        [
            ("afplay", &["/System/Library/Sounds/Ping.aiff"]),
            ("afplay", &["/System/Library/Sounds/Glass.aiff"]),
            ("true", &[]),
        ]
    } else if cfg!(target_os = "windows") {
        [
            (
                "powershell",
                &["-NoProfile", "-Command", "[console]::beep(880,120)"],
            ),
            ("true", &[]),
            ("true", &[]),
        ]
    } else {
        [
            ("canberra-gtk-play", &["-i", "message"]),
            (
                "paplay",
                &["/usr/share/sounds/freedesktop/stereo/message.oga"],
            ),
            ("true", &[]),
        ]
    };

    for (program, args) in candidates {
        if program == "true" {
            continue;
        }
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            // Nobody waits on it; reaping is the shell's problem at exit.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job;
    use crate::testing::Fixture;

    fn snap(f: &Fixture) -> Snapshot {
        job::load(&f.0).unwrap()
    }

    fn write(f: &Fixture, short: &str, json: &str) {
        std::fs::write(f.0.join(short).join("state.json"), json).unwrap();
    }

    const WORKING: &str =
        r#"{"state":"working","name":"RUN","detail":"compiling","sessionId":"s-1"}"#;
    const ASKING: &str =
        r#"{"state":"working","name":"RUN","needs":"answer: which one?","sessionId":"s-1"}"#;
    const FINISHED: &str =
        r#"{"state":"done","name":"RUN","output":{"result":"shipped"},"sessionId":"s-1"}"#;

    #[test]
    fn the_first_look_is_silent() {
        // Savras opening beside four sessions that already need you is not
        // four interruptions; it is the state of the world.
        let f = Fixture::new("ping-first").job("aaa", ASKING);
        let mut ping = Ping::new(When::Needs, false);
        assert_eq!(ping.observe(&snap(&f), None, Instant::now()), None);
    }

    #[test]
    fn a_session_starting_to_ask_pings() {
        let f = Fixture::new("ping-asks").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        ping.observe(&snap(&f), None, Instant::now());

        write(&f, "aaa", ASKING);
        let notice = ping.observe(&snap(&f), None, Instant::now()).unwrap();
        assert_eq!(notice.title, "RUN needs you");
        assert_eq!(notice.body, "answer: which one?");
    }

    #[test]
    fn a_session_that_keeps_asking_pings_once() {
        let f = Fixture::new("ping-once").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);

        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, now).is_some());
        // Same question, later poll: already announced.
        assert_eq!(ping.observe(&snap(&f), None, now + QUIET * 2), None);
    }

    #[test]
    fn asking_again_after_working_pings_again() {
        let f = Fixture::new("ping-again").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, start).is_some());
        write(&f, "aaa", WORKING);
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET * 2), None);
        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, start + QUIET * 4).is_some());
    }

    #[test]
    fn finishing_pings_only_when_asked_for() {
        let f = Fixture::new("ping-done").job("aaa", WORKING);
        let mut needs_only = Ping::new(When::Needs, false);
        let mut also_done = Ping::new(When::Done, false);
        let now = Instant::now();
        needs_only.observe(&snap(&f), None, now);
        also_done.observe(&snap(&f), None, now);

        write(&f, "aaa", FINISHED);
        let after = snap(&f);
        assert_eq!(needs_only.observe(&after, None, now), None);
        let notice = also_done.observe(&after, None, now).unwrap();
        assert_eq!(notice.title, "RUN finished");
        assert_eq!(notice.body, "shipped");
    }

    #[test]
    fn off_says_nothing_at_all() {
        let f = Fixture::new("ping-off").job("aaa", WORKING);
        let mut ping = Ping::new(When::Off, false);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);
        write(&f, "aaa", ASKING);
        assert_eq!(ping.observe(&snap(&f), None, now), None);
    }

    #[test]
    fn the_session_you_are_looking_at_never_pings() {
        // It is on the other half of your screen, asking you in person.
        let f = Fixture::new("ping-open").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);

        write(&f, "aaa", ASKING);
        assert_eq!(ping.observe(&snap(&f), Some("aaa"), now), None);
        // ...and it is identified by session id too, which is what the pane
        // was actually opened with.
        write(&f, "aaa", WORKING);
        ping.observe(&snap(&f), Some("aaa"), now);
        write(&f, "aaa", ASKING);
        assert_eq!(ping.observe(&snap(&f), Some("s-1"), now), None);
    }

    #[test]
    fn a_burst_is_one_ping_that_counts_them() {
        let f = Fixture::new("ping-burst")
            .job("aaa", WORKING)
            .job("bbb", WORKING)
            .job("ccc", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);

        for short in ["aaa", "bbb", "ccc"] {
            write(&f, short, ASKING);
        }
        let notice = ping.observe(&snap(&f), None, now).unwrap();
        assert_eq!(notice.title, "3 sessions");
        assert_eq!(notice.body, "3 needing you");
    }

    #[test]
    fn a_second_ping_waits_for_the_quiet_period() {
        let f = Fixture::new("ping-quiet")
            .job("aaa", WORKING)
            .job("bbb", WORKING);
        let mut ping = Ping::new(When::Needs, false);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, start).is_some());
        // A second session asks a moment later: swallowed, we just spoke.
        write(&f, "bbb", ASKING);
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET / 2), None);
        // Once it is quiet again, the next transition is heard.
        write(&f, "bbb", WORKING);
        ping.observe(&snap(&f), None, start + QUIET);
        write(&f, "bbb", ASKING);
        assert!(ping.observe(&snap(&f), None, start + QUIET * 2).is_some());
    }

    #[test]
    fn a_job_that_disappears_is_forgotten() {
        let f = Fixture::new("ping-gone").job("aaa", ASKING);
        let mut ping = Ping::new(When::Needs, false);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);
        assert_eq!(ping.seen.len(), 1);

        std::fs::remove_dir_all(f.0.join("aaa")).unwrap();
        ping.observe(&snap(&f), None, start);
        assert!(ping.seen.is_empty(), "a gone job must not be remembered");
    }

    #[test]
    fn the_notification_cannot_spill_onto_the_screen() {
        // A summary carrying an ESC or a BEL would end the sequence early and
        // print the remainder over the panel.
        let seq = osc9("A\x07B", "line\nnext\x1b[31m");
        assert!(seq.starts_with("\x1b]9;"));
        assert!(seq.ends_with('\x07'));
        assert_eq!(seq.matches('\x07').count(), 1);
        assert!(!seq[4..].contains('\x1b'));
    }

    #[test]
    fn when_reads_the_words_people_would_type() {
        assert_eq!(When::parse("needs"), Some(When::Needs));
        assert_eq!(When::parse("done"), Some(When::Done));
        assert_eq!(When::parse("off"), Some(When::Off));
        assert_eq!(When::parse("loud"), None);
        assert_eq!(When::default(), When::Needs);
    }
}
