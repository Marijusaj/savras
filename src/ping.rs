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
///
/// Silence, not deafness: news arriving inside the window is held and said
/// when it closes. `--quiet` moves it.
pub const QUIET: Duration = Duration::from_secs(20);

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

/// One ping's worth of news: what to say, and which sessions it was about.
///
/// The panel needs `shorts` as much as the notification needs the words: a
/// sound tells you *someone* wants you, and the panel is where you find out
/// which — see `App::alert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub shorts: Vec<String>,
}

/// How many consecutive snapshots a job may be missing from before we forget
/// it.
///
/// Claude Code rewrites `state.json` in place, so a read can land mid-write and
/// yield no job at all. Forgetting one on the strength of a single bad read
/// makes its next appearance a *first* sighting — and a first sighting is
/// exactly what this module stays quiet about.
const GRACE: u8 = 3;

/// What a job looked like the last time we saw it.
#[derive(Debug, Clone)]
struct Seen {
    status: Status,
    /// What it was saying. A session that answers one question and asks the
    /// next between two polls never changes status, and the words are the only
    /// evidence that the question is a new one.
    saying: String,
    /// Consecutive snapshots it has been missing from — see [`GRACE`].
    missing: u8,
}

pub struct Ping {
    when: When,
    sound: bool,
    quiet: Duration,
    /// What each job looked like when we last looked. A job missing from here
    /// has never been seen, and a job's *first* sighting never pings: Savras
    /// starting up beside eight finished sessions is not eight interruptions.
    seen: HashMap<String, Seen>,
    /// Sessions whose news has been noticed but not yet said out loud, because
    /// the quiet period was still running. Held as ids rather than as
    /// sentences, so that what is finally announced is each session as it is
    /// *then*: a question withdrawn while we waited is dropped, not announced
    /// late.
    pending: Vec<String>,
    primed: bool,
    last: Option<Instant>,
}

impl Ping {
    pub fn new(when: When, sound: bool, quiet: Duration) -> Self {
        Self {
            when,
            sound,
            quiet,
            seen: HashMap::new(),
            pending: Vec::new(),
            primed: false,
            last: None,
        }
    }

    /// Note a new snapshot, and say what — if anything — deserves a ping.
    ///
    /// `watching` is the session you are actually looking at: showing in the
    /// working pane *and* the terminal has focus. That one never pings,
    /// because it is asking you in person. A session merely open in a pane
    /// behind another application is not being watched, and pings like any
    /// other — see `focus`.
    pub fn observe(
        &mut self,
        snapshot: &Snapshot,
        watching: Option<&str>,
        now: Instant,
    ) -> Option<Notice> {
        for job in &snapshot.jobs {
            let before = self.seen.insert(
                job.short.clone(),
                Seen {
                    status: job.status,
                    saying: job.summary.clone(),
                    missing: 0,
                },
            );
            if self.when == When::Off || !self.primed {
                continue;
            }
            if !self.news(before.as_ref(), job) {
                continue;
            }
            if is(job, watching) {
                continue;
            }
            if self.worth_saying(job.status) && !self.pending.contains(&job.short) {
                self.pending.push(job.short.clone());
            }
        }

        self.forget_the_gone(snapshot);

        // The first snapshot only teaches us what is already on screen.
        if !self.primed {
            self.primed = true;
            self.pending.clear();
            return None;
        }
        if self.pending.is_empty() {
            return None;
        }
        // Still inside the quiet period: hold the news rather than drop it.
        // Dropping is how a second session asking five seconds after the first
        // went unheard — and, since the panel marks what pinged, unseen too.
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < self.quiet)
        {
            return None;
        }

        // Announce each held session as it is *now*. One that has since gone
        // back to working, been answered, or been opened in front of you is no
        // longer news, and saying so late is worse than not saying it.
        let fired: Vec<_> = std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|short| snapshot.jobs.iter().find(|j| j.short == short))
            .filter(|job| self.worth_saying(job.status) && !is(job, watching))
            .collect();
        if fired.is_empty() {
            return None;
        }
        self.last = Some(now);

        let shorts = fired.iter().map(|j| j.short.clone()).collect();
        let (title, body) = match fired.as_slice() {
            [job] => (
                match job.status {
                    Status::NeedsInput => format!("{} needs you", job.name),
                    _ => format!("{} finished", job.name),
                },
                job.summary.clone(),
            ),
            many => {
                let asking = many
                    .iter()
                    .filter(|j| j.status == Status::NeedsInput)
                    .count();
                (
                    format!("{} sessions", many.len()),
                    if asking > 0 {
                        format!("{asking} needing you")
                    } else {
                        "finished".into()
                    },
                )
            }
        };
        Some(Notice {
            title,
            body,
            shorts,
        })
    }

    /// Whether this job has done something since we last looked.
    ///
    /// A change of status, plainly. But also a session that is *still* asking
    /// a different question than it was: Savras samples the jobs directory, it
    /// is not told about changes, so a round trip that begins and ends between
    /// two samples — asked, answered, asked again — leaves the status looking
    /// untouched. The question itself is the only thing that moved.
    fn news(&self, before: Option<&Seen>, job: &crate::job::Job) -> bool {
        match before {
            None => true,
            Some(seen) => {
                seen.status != job.status
                    || (job.status == Status::NeedsInput && seen.saying != job.summary)
            }
        }
    }

    /// Drop jobs that have stayed gone. Lingering entries would make a short
    /// id Claude Code reuses look like it never changed; dropping them the
    /// instant one snapshot misses them would make a half-written `state.json`
    /// look like a session that had never existed — so it takes [`GRACE`]
    /// snapshots in a row.
    fn forget_the_gone(&mut self, snapshot: &Snapshot) {
        self.seen.retain(|short, seen| {
            if snapshot.jobs.iter().any(|j| &j.short == short) {
                return true;
            }
            seen.missing += 1;
            seen.missing < GRACE
        });
        // News about a session that is no longer there cannot be said, and
        // must not sit in the queue waiting for a session that never returns.
        // Same grace: a job we have not truly forgotten yet may still come back.
        let seen = &self.seen;
        self.pending.retain(|short| seen.contains_key(short));
    }

    fn worth_saying(&self, status: Status) -> bool {
        match status {
            Status::NeedsInput => self.when != When::Off,
            Status::Done => self.when == When::Done,
            Status::Working => false,
        }
    }

    /// The whole point, in one call: look, make noise if there is reason to,
    /// and hand back the sessions it was about so the panel can point at them.
    pub fn poll(&mut self, snapshot: &Snapshot, watching: Option<&str>) -> Vec<String> {
        match self.observe(snapshot, watching, Instant::now()) {
            Some(notice) => {
                notice.announce(self.sound);
                notice.shorts
            }
            None => Vec::new(),
        }
    }
}

/// Whether this job is the one you are watching. The pane was opened with a
/// session id, and the panel knows jobs by short id; either name identifies it.
fn is(job: &crate::job::Job, watching: Option<&str>) -> bool {
    Some(job.short.as_str()) == watching || Some(job.session_id.as_str()) == watching
}

impl Notice {
    /// Say it: a desktop notification through the terminal, and a sound.
    ///
    /// Both are best-effort and neither can fail loudly — a panel that panics
    /// because a sound file moved is worse than a silent one.
    pub fn announce(&self, sound: bool) {
        if understands_osc9(std::env::var("TERM_PROGRAM").ok().as_deref()) {
            let mut out = std::io::stdout();
            let _ = out.write_all(wrap(&osc9(&self.title, &self.body)).as_bytes());
            let _ = out.flush();
        }
        if sound {
            play();
        }
    }
}

/// Whether to send the sequence at all.
///
/// Terminal.app understands no notification sequence, and a terminal that does
/// not understand one may print it rather than swallow it — across the panel,
/// which is worse than the notification is worth. It gets the sound and the
/// bell instead, which is what it is good at.
fn understands_osc9(term_program: Option<&str>) -> bool {
    term_program != Some("Apple_Terminal")
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
        let mut ping = Ping::new(When::Needs, false, QUIET);
        assert_eq!(ping.observe(&snap(&f), None, Instant::now()), None);
    }

    #[test]
    fn a_session_starting_to_ask_pings() {
        let f = Fixture::new("ping-asks").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        ping.observe(&snap(&f), None, Instant::now());

        write(&f, "aaa", ASKING);
        let notice = ping.observe(&snap(&f), None, Instant::now()).unwrap();
        assert_eq!(notice.title, "RUN needs you");
        assert_eq!(notice.body, "answer: which one?");
        // The panel marks these, so the sound has something to point at.
        assert_eq!(notice.shorts, ["aaa"]);
    }

    #[test]
    fn a_session_that_keeps_asking_pings_once() {
        let f = Fixture::new("ping-once").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
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
        let mut ping = Ping::new(When::Needs, false, QUIET);
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
        let mut needs_only = Ping::new(When::Needs, false, QUIET);
        let mut also_done = Ping::new(When::Done, false, QUIET);
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
        let mut ping = Ping::new(When::Off, false, QUIET);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);
        write(&f, "aaa", ASKING);
        assert_eq!(ping.observe(&snap(&f), None, now), None);
    }

    #[test]
    fn the_session_you_are_looking_at_never_pings() {
        // It is on the other half of your screen, asking you in person. Only
        // while you are actually there, though — see `watching` in host.rs.
        let f = Fixture::new("ping-open").job("aaa", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
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
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let now = Instant::now();
        ping.observe(&snap(&f), None, now);

        for short in ["aaa", "bbb", "ccc"] {
            write(&f, short, ASKING);
        }
        let notice = ping.observe(&snap(&f), None, now).unwrap();
        assert_eq!(notice.title, "3 sessions");
        assert_eq!(notice.body, "3 needing you");
        // One sound, but all three get marked — the count in the title is no
        // help finding them in the list.
        let mut shorts = notice.shorts.clone();
        shorts.sort();
        assert_eq!(shorts, ["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn a_second_ping_waits_for_the_quiet_period() {
        let f = Fixture::new("ping-quiet")
            .job("aaa", WORKING)
            .job("bbb", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
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
    fn a_job_that_stays_gone_is_forgotten() {
        let f = Fixture::new("ping-gone").job("aaa", ASKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);
        assert_eq!(ping.seen.len(), 1);

        std::fs::remove_dir_all(f.0.join("aaa")).unwrap();
        for _ in 0..GRACE {
            ping.observe(&snap(&f), None, start);
        }
        assert!(ping.seen.is_empty(), "a gone job must not be remembered");
    }

    #[test]
    fn a_job_that_blinks_out_for_one_read_is_still_remembered() {
        // A `state.json` caught mid-rewrite parses as nothing, and the job is
        // absent from that one snapshot. Forgetting it there would make its
        // return a first sighting — and first sightings are silent, so the
        // question it is asking would never be announced at all.
        let f = Fixture::new("ping-blink").job("aaa", ASKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        std::fs::write(f.0.join("aaa").join("state.json"), "{\"state\":").unwrap();
        assert!(snap(&f).jobs.is_empty(), "the torn read yields no job");
        assert_eq!(ping.observe(&snap(&f), None, start), None);

        write(&f, "aaa", ASKING);
        assert_eq!(
            ping.observe(&snap(&f), None, start),
            None,
            "it is the same question it was asking before the blink"
        );
    }

    #[test]
    fn news_held_through_the_quiet_period_is_said_afterwards() {
        // The bug this replaces: a transition landing inside the quiet window
        // was consumed and dropped, so a second session asking moments after
        // the first got no sound and — since the panel marks what pinged — no
        // mark either, ever.
        let f = Fixture::new("ping-held")
            .job("aaa", WORKING)
            .job("bbb", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, start).is_some());

        write(&f, "bbb", ASKING);
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET / 2), None);
        // Nothing changes on disk; the news is simply no longer being held.
        let notice = ping.observe(&snap(&f), None, start + QUIET).unwrap();
        assert_eq!(notice.shorts, ["bbb"]);
    }

    #[test]
    fn news_that_stops_being_true_while_held_is_dropped() {
        // Announcing "bbb needs you" twenty seconds after bbb went back to
        // working is worse than staying quiet: you go and find nothing.
        let f = Fixture::new("ping-stale")
            .job("aaa", WORKING)
            .job("bbb", WORKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(&f, "aaa", ASKING);
        assert!(ping.observe(&snap(&f), None, start).is_some());
        write(&f, "bbb", ASKING);
        ping.observe(&snap(&f), None, start + QUIET / 2);
        write(&f, "bbb", WORKING);
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET), None);
    }

    #[test]
    fn a_fresh_question_pings_even_when_the_status_never_moved() {
        // Savras samples; it is not told. Answered-and-asked-again between two
        // samples looks exactly like a session that never stopped waiting, and
        // the only thing that moved is the question itself.
        let f = Fixture::new("ping-requestion").job("aaa", ASKING);
        let mut ping = Ping::new(When::Needs, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(
            &f,
            "aaa",
            r#"{"state":"working","name":"RUN","needs":"answer: and now which?","sessionId":"s-1"}"#,
        );
        let notice = ping.observe(&snap(&f), None, start + QUIET).unwrap();
        assert_eq!(notice.title, "RUN needs you");
        assert_eq!(notice.body, "answer: and now which?");
        // The same question again is still not news.
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET * 2), None);
    }

    #[test]
    fn a_finished_session_saying_more_does_not_ping_twice() {
        // Only a pending question is re-announced on new words; a result line
        // that grows as the summary is written must not ping again.
        let f = Fixture::new("ping-done-twice").job("aaa", WORKING);
        let mut ping = Ping::new(When::Done, false, QUIET);
        let start = Instant::now();
        ping.observe(&snap(&f), None, start);

        write(&f, "aaa", FINISHED);
        assert!(ping.observe(&snap(&f), None, start).is_some());
        write(
            &f,
            "aaa",
            r#"{"state":"done","name":"RUN","output":{"result":"shipped, and pushed"},"sessionId":"s-1"}"#,
        );
        assert_eq!(ping.observe(&snap(&f), None, start + QUIET * 2), None);
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
    fn the_one_terminal_that_cannot_read_it_is_not_sent_it() {
        assert!(!understands_osc9(Some("Apple_Terminal")));
        assert!(understands_osc9(Some("iTerm.app")));
        assert!(understands_osc9(Some("WezTerm")));
        assert!(understands_osc9(Some("ghostty")));
        // Unknown terminals get it: standard OSC parsing swallows a sequence
        // it does not recognise, so the risk is small and the gain is real.
        assert!(understands_osc9(None));
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
