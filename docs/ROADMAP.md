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
- **Context spent, as a percentage.** The denominator is the model in
  `respawnFlags`: `[1m]` in the name is a million, everything else 200k. It
  turns red past 85%, where compaction is coming. A session that reports no
  tokens — anything on another machine — shows nothing rather than `0%`.
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

---

## Next

### M4.1 · Geometry
**Move and resize the panel from the keyboard, while it is running.**

`--side left|right` and `--width <cols>` are start-up flags today; the layout
they feed is one `Layout::horizontal` in the host, recomputed every draw. So
both are already variables — they are simply set once. Wanted: a key that
widens, a key that narrows, and a key that flips the panel to the other side,
each taking effect on the next frame.

What makes it more than a variable: the working pane is a real pty, and a pty
that changes width must be told (`SIGWINCH` and a resize on the `portable-pty`
handle) or the program inside keeps drawing to the old size. Flipping sides is
the same resize with the columns swapped.

Open: which keys. The chord budget is spent — `ctrl-g`, `ctrl-l`, `ctrl-w`/`s`
— and M2.2 established that Command never reaches us. Likely a mode: `ctrl-g`
to the panel, then plain `<`/`>` and `[`/`]` while the panel has focus, where
single letters are free because the pane is not listening.

### M4.2 · Under the lead
**Agents sit beneath the session that started them, and look subordinate.**

M2.3 made the group knowable from the names alone — `BOOKS` leads, `BOOKS-2`,
`BOOKS-3`, `BOOKS-4` follow — and then showed it only in the detail footer, one
session at a time. In the list they are five peers sorted alphabetically, so the
lead is not visibly the lead and an agent can sort above it.

Wanted, inside each repository group: the lead's row, then its agents directly
under it in number order, indented and drawn smaller — the mark and the name
dimmed, or a `└` gutter — so the shape is read before the names are. An agent
should never appear away from its lead, and a session with no group keeps
today's row exactly.

Two things this must not break. Sorting is by name inside a group *because* the
list has to hold still under the flip keys (M2.2), and grouping by lead has to
inherit that: the order of a group must not change when an agent starts
working. And the group is only a group when the lead is running — a lone
`PR-357` is still one row, not an orphan indented under nothing.

The repository headings are **not** part of this pass. `claude-box:` repeats on
every heading of a machine with more than one repo, and that repetition was
raised as waste to trim — but the prefix is the thing that says where the work
is, and it reads well. It stays as it is. Revisit only if a real box with four
repos on it makes the column genuinely unreadable, and then by shortening the
host, never by dropping it.

### M4.3 · A tab on the box
**`n` can open a shell on another machine, and a session started in it is one
row, not two.**

M3.5 lists another machine's sessions and `enter` joins them; what is missing is
starting something there. The ssh invocation already exists — `Remote { host,
tmux: None }.open_command()` builds it, ControlMaster and all, and falls through
to `exec ${SHELL:-sh} -l` — so this is one argument, not a subsystem: `new_tab`
takes a *place*, and the only thing that differs is which command is spawned. A
parallel `new_remote_tab` would duplicate the spawn, the push, the reopen and
the redraw, and would have to be edited again for every future field on a tab.

`n` opens a chooser when there are machines to choose between, and behaves
exactly as it does today when there are none — an unconfigured `machines` file
is the common case, and it must not pay for the rare one. A second key was the
other sketch and does not scale past one machine.

**The tab is a named tmux window on the box, not a bare login shell**, and that
is the whole decision. Adoption (M3.6) makes a pane you start a session in that
session's tab by matching the pane's process group leader against a local
`sessions/<pid>.json` — a pid, which is local by nature. An ssh pane's leader is
the local ssh client, which owns no session, so a `claude` started in a bare
remote shell would sit there as an unadopted `ssh` row while the far-side tick
adds the session's own row beside it: the double row that M3.6 just removed,
back again, for remote sessions only. The far side already publishes an
identifier that crosses the hop — `tmux`, `session:@window.%pane`, which
`open_command` already steers by — so the window is remembered on the pane and
adoption joins on it. Same rule, keyed on the identifier that is valid on both
machines.

It stays a fact throughout: **savras must not remember that it started something
remotely.** Where a session runs is owned by the machine running it and arrives
on that host's ssh stream; a creation record would drift on the first reboot,
the first hand-started session, and every savras restart. The price of that is
visible and correct — a session started on the box appears when the far side has
written its json and the 2s tick has carried it, not instantly.

Deliberately not in v1: picking a repository and launching `claude` there. A
shell on the box guesses nothing, and the owner's own path (`cb <repo>`, then
`claude`) is two words once the shell is open.

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
