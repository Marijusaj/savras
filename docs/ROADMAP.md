# Roadmap

Shipped and planned work, in order. Each build is small enough to use before the
next one starts.

## Shipped

### M0 · Panel
The read-only session list, from `~/.claude/jobs/<id>/state.json`. Grouped into
Needs input / Working / Completed, with the pending question, the current
activity, or the final result on each row.

### M0.5 · Column
`svr panel` — the panel as a column beside your work, built with tmux.

### M1 · Host
`svr panel` hosts the working pane itself, so nothing needs installing.
`portable-pty` for the pseudo-terminal, `vt100` for the screen. Mouse mode is
mirrored to the real terminal so scrolling reaches the child. `--tmux` remains,
for the detach and reattach that hosting cannot give.

### M1.5 · Open
`ctrl-g` focuses the panel, `enter` opens the selected session in the working
pane — `claude --resume`, in that session's own repository. A session that
exits leaves its last screen on display rather than closing Savras.

### M1.6 · Default
`svr` on its own is the side panel; `svr solo` is the panel with nothing beside
it.

### M1.7 · Attach
Daemon-backed sessions — which is nearly all of them — open with `claude
attach <short-id>`, not `claude --resume`. Claude Code refuses to resume a
session it is already running, and says so; Savras now reads `backend` and
`daemonShort` from the job and picks the command that works.

### M2 · Ping
A session arriving in Needs input pings: an **OSC 9** desktop notification
written straight to the terminal, and a sound (`afplay`, `canberra-gtk-play`,
`[console]::beep`, with the terminal bell underneath). It fires on the
*transition*, so the first look is silent and a question already on screen is
never re-announced; a burst of sessions is one ping that counts them, and a
20-second quiet period follows. `--ping done` includes finished sessions,
`--ping off` and `--no-sound` turn the halves off separately.

The panel marks what pinged: a bright `●` in place of the status star, and a
count in the header. Without it the sound was useless — it says someone wants
you, and with four sessions in Needs input it cannot say which. The mark clears
when you go to the session, or when the session stops needing you.

The session in the working pane is spared a ping only while the terminal has
focus. Sparing it unconditionally was the first cut and it was wrong: with the
window behind a browser, the session you had open is exactly the one you would
never see ask. Savras enables focus reporting (DEC mode 1004), forwards the
events to the child only when the child asked for them itself — the rule the
mouse already follows — and treats unknown focus as away.

Two decisions worth keeping: **OSC 9 only, not OSC 9 and OSC 777** — WezTerm and
Ghostty understand both, so sending both notifies twice, and a missing
notification beats a doubled one. And Terminal.app, which understands neither,
is sent no sequence at all and gets the sound and the bell; the bell badges the
tab, which is what you look for anyway.

A banner through AppleScript was tried for Terminal.app and taken back out. It
works, but macOS attributes such a notification to Script Editor, and clicking
one launches Script Editor — a panel that opens another application when you
answer it is worse than a bell.

### M2.1 · Heard
The ping fired only sometimes. Three ways a real question could be consumed
without ever being said, all closed:

- **The quiet period swallowed instead of queueing.** A transition landing
  inside the twenty seconds was noted in the memory of the last snapshot and
  then dropped, so the second session to ask got no sound and — since the panel
  marks what pinged — no mark either, ever. News is now *held* as a list of
  session ids and said when the window closes, re-checked against the current
  snapshot first, so what is announced late is only what is still true.
- **Savras samples; it is not told.** A session that answers one question and
  asks the next between two readings never changes status. The question text is
  now part of what is remembered, and different words are a new question.
- **A `state.json` caught mid-rewrite parses as nothing**, and the job vanished
  from that reading — which erased what Savras knew about it, making its return
  a first sighting, and first sightings are silent. An unparseable file is now
  read once more before it is believed, and a job must be missing from three
  readings in a row before it is forgotten.

`--quiet <seconds>` came out of this: the window is a real preference, and a
short one lets the end-to-end test drive two pings in five seconds rather than
forty.

### M2.2 · Tabs
Every opened session keeps its own pane, alive behind the one in front, so
flipping back shows the screen you left rather than a fresh attach.
`shift-option-←/→` or `↑/↓` flips one along, wrapping, from either side of the
divider — the point being that it takes no `ctrl-g` first, because switching is
the thing you do most often. `enter` on an already-open session brings its tab
forward instead of attaching twice; `x` closes a tab; the header counts the
tabs running behind you, since each is a live `claude attach`.

Command chords were the ask and are impossible: Command is not in the xterm
modifier encoding, so a terminal keeps every one of them and the pty never sees
it. Shift-Option reaches us — but only where the terminal encodes modifiers on
arrows, and **Terminal.app does not**: it sends shift-option-↑ as a plain
`ESC [ A`, byte-identical to the arrow the session in the pane wants, so the
first cut did nothing there and spilled `[A[B` across the shell prompt instead.

So both, at once. Savras reads raw bytes, so accepting several encodings of
one intent costs nothing: `ctrl-w`/`ctrl-s` reach it in any terminal, and
`ctrl-shift-arrows` (`;6`), shift-option (`;4`) and shift-meta (`;10`) reach it
wherever the terminal encodes them. No capability negotiation, no
configuration — an encoding a terminal cannot send simply never arrives.

Deliberately not accepted: `;2` (plain shift) and `;5` (plain ctrl) are
selection and word-movement in the program running in the pane.

The cycle is **the panel's own list**, with the shell at the head of it, and
landing on a session opens it if it has no pane yet.

Which forced a second correction, because a list you *navigate* has a
requirement a list you only read does not: it must hold still. Rows were sorted
freshest-first inside each status group, and a working session rewrites its
timestamp every few seconds, so rows traded places while you were looking at
them and the switch keys became a lottery — press twice, land somewhere else.
Sorting by name inside the group fixes it: the list now moves only when a
session changes status. The header also carries the name of the session in the
pane, since Claude Code draws its own name only sometimes and the panel always
knows. The first cut cycled the
panes that happened to be alive, which with one session opened meant flipping
between an empty shell and that one session while five more rows sat there
untouched — a hidden set instead of the rows on screen. The panel's cursor
moves with you, so the highlight says where you are.

W and S because they sit under the left hand where ctrl already is, and up/down
reads the way the panel's list runs. The cost is real and named rather than
hidden: ctrl-w is delete-previous-word. It is taken whenever the panel has a session to
flip to, which is nearly always — the price of every row being one press away.
`--switch` moves both keys — two letters for back and forward, one for a single key that wraps,
`off` to hand them back. It refuses letters that are already enter, tab,
backspace, the signals, or Savras's own ctrl-g and ctrl-l, and refuses binding
one letter to both directions.

Evaluated and rejected: the Kitty keyboard protocol (`CSI > 1 u`) and xterm's
`modifyOtherKeys` (`CSI > 4 ; 2 m`), which let a program ask its terminal for
richer key data. Terminal.app implements neither, and more fundamentally never
encodes arrow modifiers in any form — there is no flag to turn on, because the
information never enters the byte stream. Worth revisiting only if the panel
ever wants keys that the plain xterm encoding cannot express.

### M2.3 · Parallel agents
`a` in the panel starts a parallel agent under the selected session's lead: a
real background session (`claude --bg -n <LEAD>-<n>`), in the lead's own
repository, whose **opening prompt is the briefing**. That is what makes the
message genuinely sent rather than drafted — there is no supported way to put
words into a session that is already running, so instead of messaging an agent
after starting it, Savras starts it with what it needs to know. The lead is
told by the agent itself, in its first act, through Claude Code's own
messaging.

The group needs no configuration because the names already carry it: Claude
Code names repeats `NAME-2`, `NAME-3`, so the bare name leads and the numbered
ones follow. Two sessions sharing a base name are a group; one is not, which is
what keeps a lone `PR-357` from being read as somebody's agent. The lead falls
back to the lowest-numbered agent when the bare name is not running — a lead
has to be a session that can actually be messaged, or the address handed to
every new agent goes nowhere. Numbering follows the lead rather than the base
name: the next agent takes the lowest free number *above* the lead's own. A
group led by `SAVRAS-4` leaves the free `2` alone and adds `SAVRAS-6`, because
`SAVRAS-2` would have been briefed to report to `SAVRAS-4` and then, being the
lowest number, been shown as the lead of the session that briefed it.

The panel shows the relationship in the detail footer rather than the rows: a
44-column row has no width for it, and "who commands whom" is a question you
ask about one session at a time.

This is where the read-only rule bends, and the shape of the bend is
deliberate. Savras still never writes to `~/.claude/`, and still cannot disturb
a session that is running. It can now *start* one. The alternatives were worse:
typing into the pane only reaches the session in front of you, and reaching a
running session at all would mean speaking the private socket protocol under
`/tmp/cc-socks/`, which is undocumented and would break on any Claude Code
release.

### M3.5 · Machines
`--machine <ssh-host>` watches another box's sessions and lists them beside
your own, with the machine in the repository heading so two checkouts of the
same repo on two machines do not read as one place.

The far side has no jobs directory — nothing runs the daemon on a box you ssh
into and drive by hand — so the source is `~/.claude/sessions/<pid>.json`, a
different file with a different shape: `status` where the jobs file says
`state`, no summary, no token count, and one field the jobs file has never
had, `tmux`. That field is what makes opening one possible at all.

`enter` therefore joins the tmux window rather than attaching to a daemon, and
it joins it as a *grouped* session: the user is already attached from their own
terminal, and a second client would force both to the smaller size. Grouped
gives the pane its own size and its own selected window, and
`destroy-unattached` removes it when the tab closes.

`idle` on the far side is read as "needs input" here — an interactive session
that has stopped thinking is one waiting for you, and that transition is
exactly what the ping exists to announce. Liveness is decided over there, with
`kill -0`, because nothing removes a session's json when it exits.

### M3.6 · Remembering which machines
The hosts to watch now live in `<config>/savras/machines`, so plain `svr`
keeps them. A flag that has to be retyped is a flag that gets forgotten, and a
forgotten one looked exactly like a box with nothing on it — which is how the
machines went missing from the panel with nothing appearing to be wrong. ssh's
stderr is kept and shown for the same reason: a misspelt host, a key the agent
forgot and a switched-off box all used to look like silence.

And a pane you start a session in is now that session's tab, found through
`sessions/<pid>.json` and its `jobId`. It used to be an anonymous `shell 2`
sitting beside a row for the same session, with `enter` on the row attaching a
second time to what was already in front of you.

### M3.7 · Stop repainting the world
The flip keys no longer erase the screen. Every tab change called
`terminal.clear()`, which blanks the terminal *now* and leaves it blank until
the next draw lands — a black flash on every ctrl-shift-arrow, which in
Ghostty reads as the whole window reloading. Nothing needed it: ratatui resets
its buffer each frame and the pane writes every cell of its area, so a stale
cell cannot survive a draw. `ctrl-l` still clears, because that one is asked
for.

What is left after that is honest waiting: `claude attach` takes a second or
two to say anything, and the pane is genuinely empty until it does. It now
says `opening NAME…` in the middle of the pane rather than showing a black
rectangle that looks like a crash.

### M3.8 · Watching taken back out, and a floor under the frame rate
`v` and `c` are gone, four days after they shipped. Read-only watching answers
"someone else is at that keyboard", and on a box you ssh into alone there is
nobody else — so the whole mechanism (a second ssh, a read-only tmux client the
far server has to render for, a `client-attached` hook to tidy it, a key to
undo it, a footer state, and an `enter` that meant something different inside
one) served a case that does not arise. `ctrl-t` and typing `ssh <host>` is the
thing it was standing in for, and it was already there. `enter` on a remote row
still joins the tmux window, which is the part typing cannot do quickly.

The cost was not only conceptual. A tmux client is a *push* stream with no
rate limit, and Savras coupled it straight to the screen: bytes arrived, the
frame was marked dirty, and the loop drew — at the 16ms tick, indefinitely, for
a pane that could not be typed into. **Frames now have a floor of 33ms**, about
thirty a second. `dirty` is not cleared by a frame that comes too soon, so
nothing is dropped, only coalesced; a session printing a spinner no longer
pins the terminal to sixty full repaints a second.

The same loop was asking an expensive question on every frame. `adopt_sessions`
— which notices that a pane of yours has become a session's tab — costs a
`process_group_leader()` and a read of `sessions/<pid>.json` per pane, and it
ran inside `terminal.draw`, up to sixty times a second, to learn something that
changes when you start a session. It now runs on the two-second refresh, beside
the job scan, which is the cadence the session's own row appears at anyway.

Worth naming for whoever meets this next: the defect was never the remote
feature. It was that **"output arrived" was wired directly to "repaint
everything"**, and a remote view was simply the loudest thing ever plugged into
it. A local session that prints fast did the same, more quietly.

### M4 · Vitals
**The session line says what a session *is*, not what it is doing.**

```
✳ BOOKS        DONE      MERGED #423  22% 1d
✳ PLAN         DONE        READY #28  25% 1h
▶ SAVRAS-6     WORKING  M4 vitals: …  13% 1d
  └ name       └ what    └ its PR     └ ctx └ open for
```

Five changes, all reading from files that were already on disk:

- **The session in the pane is named in white**, plain, where every other row
  wears its colour badge. The `▶` said this already, but it is one glyph in a
  column carrying four meanings, and the name is what the eye lands on.
- **One word: `WORKING`, `WAITING`, `DONE`, `FAILED`.** `FAILED` has no source
  in Claude Code today; it is wired to a `state` of `failed` or `error` so that
  one it grows shows up rather than reading as `done`.
- **The pull request says what it is doing** — `READY`, `CHECKS`, `FAILED`,
  `MERGED`, `CLOSED`, or a bare number when nothing is known yet. `children[]`
  on the job carries the link and `~/.claude/gh-pr-status-cache.json` carries
  its state, keyed by the same href, so the join needs nothing invented at
  either end and costs one small file read per scan. A failing check outranks
  every other true thing about an open pull request, because it is the only one
  asking for something.
- **Context spent, as a percentage.** Both halves of this were wrong on the
  first cut and are corrected in M4.4 below. It turns red past 85%, where
  compaction is coming.
- **The age is counted from `createdAt`, not `updatedAt`.** Freshness was
  useless: a working session rewrites its timestamp every few seconds, so it
  read `8s` for as long as it ran. How long a session has been *open* is the
  number that changes what you do, and it warms to amber at four hours and red
  at eight.

The width question turned out to answer itself. The four columns are fixed and
the summary takes what is left, so nothing is dropped by a rule — the sentence
is simply the thing that gives way, and a 44-column panel shows every column
plus as much of the summary as fits. Below that the ladder is: keep the name and
the age, then the word, then the pull request, then the percentage. The sentence
is the right one to lose because it is the only one still available elsewhere,
in the detail footer under the cursor.

`Job::machine_tag()` landed with it, so "which machine" is spelled in one place
and read by three — the repository heading, the row, and the footer deciding
whether a key would do anything.

### M4.1 · Geometry, and two things the panel knew but was not using
**`<` `>` move the divider, `[` `]` move the panel.** Four columns a press,
with the panel focused — single letters are free there because the pane is not
listening. `--side` and `--width` fed a layout recomputed every frame, so they
were already variables; what made it more than a variable is that the working
pane is a real pty, and one whose width changes must be *told* or the program
inside keeps drawing to the old size. Every open pane is resized, which is the
`SIGWINCH` that makes each one repaint against the truth. The floor is the same
12 columns `--width` enforces, and the ceiling leaves the pane 20 — a panel
that can eat the terminal is a way to lose your session behind a list of
sessions. Deciding is separate from doing, so the arithmetic is tested without
a pty.

**`ctrl-t` starts in the directory of the session you are on.** It ran in
Savras's own start directory, so a shell opened beside `PLAN` needed a `cd` to
`PLAN`'s repository every time — the tell being that the panel knew the answer
and was not using it. The session in the pane decides it, the row under the
cursor when there is none, and Savras's directory when there is no session in
play. A session on another machine is skipped rather than used: its directory
is on that machine.

**`d` reaches a session on another machine.** It ran `claude stop` and
`claude rm` against an id nothing local has ever heard of, so it silently did
nothing at all and the row could not be got rid of. There is no daemon on a box
you ssh into — a session there is an ordinary process — so `d` now signals it,
with the `TERM` that closing its terminal would send, and the row goes when the
process stops answering `kill -0`. Nothing is deleted over there: the far side
leaves `sessions/<pid>.json` behind whatever happens, and the watcher already
refuses to show a pid that is gone. The question says which act it is —
*stop X on claude-box* rather than *delete X for good* — because they are not
the same promise.

This is the read-only rule bending a second time, in the same shape as the
first: Savras still writes nothing and still speaks no private protocol. It
signals a process, which is what `d` already did here.

### M4.2 · Under the lead
**Agents sit beneath the session that started them, and look it.**

```
✳ BOOKS     WORKING  auditing leg 3    70% 2h
✳ └ BOOKS-2 WAITING  answer: which s…  21% 2h
✳ └ BOOKS-4 DONE     merged      #411  45% 1h
✳ PR-357    WORKING  a lone hyphen      5% 8m   ← not an agent
```

M2.3 made the group knowable from the names alone and then showed it only in
the detail footer, one session at a time. In the list they were peers sorted
alphabetically, so the lead was not visibly the lead and an agent could sort
above it.

The indent is paid for **out of the name column**, not out of the row: the
columns to its right are read down a list, so they stay where they are, and the
column is widened by the two the gutter takes so no name is squeezed by having
a sibling. Grouped by repository only — grouped by status a lead and its agent
can be under different headings, and an indent pointing at a row that is not on
screen would be a lie.

The ordering rule is the interesting part, because a list you *navigate* has to
hold still (M2.2). A **family** is placed by the most demanding status anyone in
it has, then by its lead's name — both things that do not change while it runs,
so an agent finishing a task never reshuffles the group. A family with a
question in it rises, which is the rule the repository headings already follow:
a question does not stop being a question because an agent asked it. Inside the
family it is the lead, then its agents by number.

A session with no siblings *in that heading* is a family of one and comes out
exactly as it went in. `PR-357` is still one row — the tail has to parse as a
number for the name to be read as a group, and `BOOKS-LEG3` splits to itself,
not to `BOOKS`, which is what stops every hyphenated name in a repository being
filed under the first one alphabetically.

The repository headings are **not** part of this pass. `claude-box:` repeats on
every heading of a machine with more than one repo, and that was raised as waste
to trim — but the prefix is the thing that says where the work is, and it reads
well. It stays. Revisit only if a real box with four repos on it makes the
column unreadable, and then by shortening the host, never by dropping it.

### M4.3 · A tab on the box
**`ctrl-t` can open a shell on another machine, and a session started in it is
one row, not two.**

```
ctrl-t ─▶ new tab: 1 here · 2 claude-box · any key cancels
              │
              └─ 2 ─▶ ssh claude-box tmux new-session -A -s savras-1
                        └─ start `claude` there and the row adopts the pane
```

The chooser is only offered when there is more than one answer: with no
machines written down `ctrl-t` asks nothing and opens here, because the common
case must not pay for the rare one. It is one argument to `new_tab` rather than
a second function beside it — the only thing that differs is which command is
spawned.

**A named tmux session, not a bare login shell**, and that was the whole
decision. Adoption joins a pane to a session through the pane's process group
leader, which is a pid and local by nature; an ssh pane's leader is the local
ssh client, which owns no session. So a `claude` started in a plain remote
shell would have sat here as an unadopted `ssh` row while the far side's own
row appeared beside it — the double row M3.6 removed, back for remote sessions
only. tmux's `session:@window.%pane` crosses the hop, so Savras names the
window and adoption matches on that name. Verified against the real box: a
session in `savras-probe` reports `savras-probe:@19.%19`.

`-A` attaches if the session is already there, so opening the box's first tab a
second time is opening the work you left in it. Closing the tab detaches rather
than kills — which is the whole reason for tmux, and what makes a session
started over there safe.

Savras still does not record that it started anything remotely: it names a
window and then *asks the far side* what is running in it. Where a session runs
stays the machine's own fact, and the price is named — a session started on the
box appears when the 2s tick carries it, not instantly.

**And a bug this uncovered, which was costing the local case too.** `new_tab`
marked its panes as "opened onto a session", which excluded every one of them
from adoption — so M3.6's fix only ever worked for the shell Savras started
with, and a `claude` started in a `ctrl-t` tab still showed twice. A tab you
opened yourself is a pane of your own, wherever it runs.

Deliberately not in v1: picking a repository and launching `claude` there. A
shell on the box guesses nothing, and `cb <repo>` then `claude` is two words
once the shell is open.

### M4.4 · The percentage was wrong, twice
**Both halves of it — the number and what it was divided by.**

```
                       savras said   the session said
  BOOKS-LEG3               15%            39%
  X                        38%            18%
```

Wrong in *both directions*, which is the useful clue: no correction factor
would have saved it, so the inputs were wrong rather than the arithmetic.

**The numerator.** `tokens` in `state.json` is not what a session is holding —
it reported 154k for a session holding 387k, and 78k for one holding 185k. What
Claude Code puts in its own status line is the last assistant message's
`usage`, and that is now what Savras reads: input, output and *both* halves of
the cache, which together are the whole of what the model was sent. The
transcript runs to megabytes, so it is read from the end — one bounded 256KB
seek, not a walk — and sub-agent turns are skipped, since a subagent has a
context of its own and the row is about the session. The detail footer reads
the same number, so the two cannot disagree.

**The denominator.** `respawnFlags` carries `--model` only when the session was
*started* with one. Without it the session runs on the configured default, so
`settings.json` is where the answer is. Missing that is what made `X` — a
million-token session at 18% — read 39% against a 200k window it was never on.

Rounded rather than truncated, so 386,839 of a million is 39% here and 39%
there. Two numbers for one thing is worse than either.

**And `0%` is a fact.** A percentage was hidden whenever the count was zero,
which was meant to spare a remote session from claiming it had spent nothing.
But a session of your own always has a count, so a blank there says "nobody
counted" about a session that had merely just started. The rule is now the
honest one: nothing to show only when there is nothing that counts — which is
only ever a session on another machine.

Found while testing: the pane and ping suites fail outright when run from
inside a Savras pane, because the child inherits `SAVRAS_PANE` and refuses to
nest. The marker is now taken off the test's child. The panel is where this
work happens, so "run the tests from anywhere but here" was never a real
option.

---

## Next

Nothing. The M4 series is done — the row says what a session is, the panel
moves and resizes, agents sit under their lead, a tab can open on another
machine, and the percentage is the one the session shows itself. What is left
is below, and none of it is in anyone's way yet.

---

## Later

- **Kill** — end a session from the panel, with a confirmation.
- **Notification protocols** — OSC 99 for kitty, and a signed helper if
  Terminal.app ever needs a real banner rather than a bell.
- **Poller** — a background process for GitHub and GitLab: PR state, checks,
  reviews, deploys. Needed properly for M4's deploy column.
- **Packaging** — Homebrew tap, Scoop, winget, deb/rpm, install script, via
  `cargo-dist`.
- **Scrollback** — read a finished session's output without leaving the panel.
