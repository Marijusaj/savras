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

---

## Next

### M4 · Vitals  ← next build
**Make the session line say more in the same width.**

Today: name, summary, PR number, age. Wanted: name, a one-word status, how much
context is spent, and where the change actually is.

- **One-word status** — `asking`, `working`, `done`, `failed`. Derived from
  `state`, `needs` and `tempo` (`blocked` is already in the file).
- **Token percentage** — `tokens` is in `state.json`. The denominator comes from
  the model in `respawnFlags` (`opus[1m]` is a 1M context), so `21k/1M` renders
  as `2%`. A session at 85% is about to compact, which is worth seeing coming.
- **Deploy status** — `PR`, `MERGED`, `CHECKS`, `ERROR`. Two sources already on
  disk: `children[]` on the job carries the pull request link, and
  `~/.claude/gh-pr-status-cache.json` carries `{state, checks:{passed, failed,
  pending}, review}` for it, refreshed by Claude Code itself. Anything beyond
  that — a real deployment state from Vercel or GitHub Actions — needs the
  background poller from M2 of the original plan, and should wait for it.

The constraint is width. A 44-column sidebar cannot hold all four columns and a
summary, so this build is as much about what to *drop* at each width as what to
add.

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
