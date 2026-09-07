# Savras

A side panel that sees every Claude Code session you have running.

Named for the god of divination, who sees all things as they are.

```
SAVRAS  ◦ live
1 needs input · 2 working · 4 done

Needs input
✳ WO        answer: All five items are tier A and the freeze is…   29m

Working
✳ PLAN      pipeline validated end-to-end; fixing grading…         1m
✳ SAVRAS    scaffolding the read-only panel                         8s

Completed
✳ SETTINGS  Triaged the five ASSISTANT_SETTINGS asks…             53s
✳ ROADMAP   58 tier-A items on origin/main @ ba75f62…              3d
────────────────────────────────────────────────────────────────────
~/Code/autodad-assistant  11,547 tokens
pr#357
claude --resume a876377e-dd14-4de9-9c67-de3aee690f5a
↑↓ move · r refresh · q quit
```

Keep it open in a narrow pane on the left. Glance at it to see which sessions
are waiting on you, which are still working, and what the finished ones
concluded.

## How it works

Claude Code already writes the state of every session to
`~/.claude/jobs/<id>/state.json`. Savras watches that directory and renders it.

Reading is all it does to the sessions it watches. It never writes to
`~/.claude/`, never touches the network, and cannot disturb a session that is
already running. If a Claude Code upgrade changes the format the panel degrades
rather than breaks — unparseable jobs are skipped, unknown fields ignored.

One exception, and it is deliberate: [parallel agents](#parallel-agents) lets
you *start* a session, with `claude --bg`. Starting is not disturbing — the
sessions already on the panel are untouched by it — but it is the one place
Savras does something rather than looking, and it is worth knowing about.

## Install

Not packaged yet — that is milestone M3. For now:

```sh
git clone https://github.com/Marijusaj/savras && cd savras
./scripts/install.sh            # builds, then installs to ~/.local/bin
./scripts/install.sh /usr/local/bin
```

Use the script rather than copying the binary yourself: replacing it in place
while a copy is running rewrites the same inode, and macOS then kills the
running one outright. The script renames a new file over the old, which is
atomic.

## The side panel

The point is a narrow column down the side of the terminal you are already
working in, with your session beside it:

```sh
svr                # panel on the right, a shell beside it
svr -- claude      # panel on the right, Claude Code beside it
svr --side left    # the other way round
svr --width 52
svr --open top     # start on the session at the top of the list
svr solo           # just the panel, no working pane, for its own tab
```

No tmux, no configuration, nothing else to install. Savras hosts the working
pane itself: it opens a pseudo-terminal, runs your command in it, and draws its
screen beside the panel. Keystrokes are forwarded to the child as raw bytes
rather than decoded and re-encoded, so arrow keys, Ctrl chords, paste and
full-screen TUIs behave exactly as they would in a normal terminal.

Savras does not implement a terminal emulator. [`portable-pty`][pty] provides
the pseudo-terminal (ConPTY on Windows) and [`vt100`][vt100] interprets the
output; Savras is the layout and the glue.

`ctrl-g` moves the keyboard to the panel and back; with the panel focused,
`enter` opens the selected session in the working pane and `esc` hands the
keyboard back. **`shift-option-←/→`, or `↑/↓`, flips between the sessions you
have open** without going through the panel at all. Claude Code runs most sessions in its daemon, and those are
*attached* (`claude attach`), not resumed — asking to resume a running session
is refused. Savras reads which kind a session is and uses the right command;
attaching leaves the session running either way.

If an opened session exits — resuming one that is already open elsewhere will do
that — its last screen stays on display so you can read why, with `enter` to try
again and `q` to quit. Only leaving the shell you *started* with closes Savras. Everything else goes
straight to your work, including the mouse: Savras follows the child in and out
of mouse reporting and mirrors it to the real terminal, so scrolling reaches
Claude Code instead of dragging the terminal's own scrollback across both panes.
With the panel on the left, mouse coordinates are shifted into the working
pane's own frame.

[pty]: https://crates.io/crates/portable-pty
[vt100]: https://crates.io/crates/vt100

### Tabs

Every session you open gets its own pane, and the pane **keeps running when you
flip away from it**. Come back and you are looking at the screen you left:
scrollback, the tool call halfway through, the message you had typed but not
sent. That is the whole feature; everything else is bookkeeping around it.

```
SAVRAS  ◦ live  ● 1  ▷ 2
1 needs input · 3 working · 2 done

Needs input
● AGENT-2     approve Bash: M=/User…    #382 5m
▶ PLAN        pipeline validated end…         1m   ← in the pane now
▷ SAVRAS      scaffolding the panel            8s   ← open, running, behind
✳ ROADMAP     58 tier-A items…                 3d   ← not open
```

**`ctrl-w` and `ctrl-s` walk the panel's list**, back and forward, from either
side of the divider. **`ctrl-shift-←/→`** and `ctrl-shift-↑/↓` do the same
thing, and so does `shift-option` with an arrow.

They step through **the rows you can see**, in the order you see them, with
your shell at the head of the list. Landing on a session opens it if it has no
pane yet, so every row is one press away rather than three, and the panel's
cursor moves with you so the highlight always says where you are.

**`ctrl-t` opens a tab of your own**, and `n` in the panel does the same. It
runs what Savras was started with — your shell for plain `svr`, whatever
followed `--` otherwise — in the directory Savras was started in. Your terminal
keeps `cmd-t` for itself and always will, so the new tab it gives you is the
wrong one: a window *beside* Savras, without the panel and without the sessions
you have open. This is the same gesture, one level in.

Those panes are rows, at the head of the list, carrying the same markers as the
sessions below them:

```
SAVRAS  ◦ live  ▶ shell 2  ▷ 3
1 needs input · 3 working · 2 done

▷ shell
▶ shell 2                                        ← the one you are typing in

Needs input
● AGENT-2     approve Bash: M=/User…    #382 5m
```

They are rows because they are stops: the flip keys walk them like anything
else, and a stop you cannot see is one you go to without knowing where you
went. One shell is drawn unnumbered and unnamed in the header, which is what an
unnamed pane has always meant here. `x` closes the one under the cursor, and
leaving it — `exit`, `ctrl-d` — leaves a dead tab rather than closing Savras;
only the shell you *arrived* in still takes the panel with it.

**The order holds still.** Rows are grouped by status and then sorted by name,
never by how recently a session did something. Freshest-first was the first cut
and it made the panel unnavigable: a working session rewrites its timestamp
every few seconds, so rows swapped places under your fingers and pressing the
key twice landed somewhere different each time. A name is the one thing about a
session that stands still, so the list now only moves when a session changes
*status* — which is a change you want to see.

**The header says which session you are in**, by name, next to the title. The
pane itself does not reliably say — Claude Code draws its own name only
sometimes — and the panel always knows.

Both, on purpose. Savras reads raw bytes, so it can accept every encoding of
the same intent at once: the control keys reach it in *any* terminal, the
chords reach it in the ones that encode modifiers on arrows. There is nothing
to detect and nothing to configure — a chord your terminal cannot send simply
never arrives, and the control keys are still there.

Ctrl-W costs you something and it is worth knowing: it is delete-previous-word
in a shell and in Claude Code's input, and Savras takes it whenever there is a
session to flip to — which, in practice, is always. `--switch <back><forward>`
moves both keys (`svr --switch ou` for ctrl-o and ctrl-u), `--switch <letter>`
binds one key that wraps forward, and `--switch off` hands them back for good.

Ctrl-S is free despite its reputation: the flow control that freezes a terminal
is turned off by raw mode, which Savras is already in. Left/right is the muscle memory your terminal
already trained; up/down matches the panel's own list. `enter` on a session you
already have open brings that tab forward rather than attaching to it twice.
`x` in the panel closes the selected session's tab, and `q` closes a tab whose
session has exited.

**`x` closes a tab; `d` deletes the session.** They are different things and
the difference is the whole point of tabs: closing a tab ends the `attach`, not
the session, which keeps running and stays in `claude agents`. That is what
leaves finished sessions piling up there with nothing to clear them. `d` on a
row asks — naming the session, in yellow, because it cannot be undone — and a
second `d` runs Claude Code's own `claude stop` and then `claude rm`, which
takes the session out of the list and its worktree with it where that is safe.
Its tab closes with it. Savras still never writes to `~/.claude/` itself: the
daemon knows what a session is, and a directory deleted behind its back leaves
it believing otherwise.

**Terminal.app is why the control keys exist.** It does not encode modifiers on
arrow keys at all: `ctrl-shift-←` arrives there as a bare `ESC [ D`, identical
to the plain left-arrow your session wants, so no program running inside it can
tell the two apart — this is a limit of that terminal, not of Savras, and no
protocol fixes it because the modifier never enters the byte stream. Ctrl-W and
Ctrl-S work there and everywhere. If you would rather have the arrows in
Terminal.app, go to **Settings → Profiles → Keyboard**, press **+** four times
and add:

| Key | Modifier | Action | Send text |
|-----|----------|--------|-----------|
| ↑ | Control, Shift | Send Text | `\033[1;6A` |
| ↓ | Control, Shift | Send Text | `\033[1;6B` |
| → | Control, Shift | Send Text | `\033[1;6C` |
| ← | Control, Shift | Send Text | `\033[1;6D` |

Type the `\033` by pressing the **esc** key in that field; it shows as `\033`.

Command chords cannot be used for this, however much `cmd-shift-←/→` is what
the fingers want: Command is not part of the terminal's modifier encoding at
all, so the terminal keeps every Command chord for its own tabs and the program
inside never sees one. If you want the Cmd feel, map `cmd-shift-←/→` the same
way, sending `\033[1;4D` and `\033[1;4C` — iTerm2 calls this "Send Escape
Sequence", and Ghostty and WezTerm have the same thing in their config.

**A pane opened onto a running session repaints itself a moment after it
appears**, and again whenever you come back to the terminal from another
application. `claude attach` replays the session as it was *drawn* — wrapped
for whatever width the terminal had when the lines were written. Replayed into
a pane of a different width, the old wrapping and the new land on top of each
other and the screen comes up interleaved with itself. Nothing in the byte
stream says so and the pane is already the right size, so there is no resize to
notice; Savras jogs the pty one column narrow and straight back instead, and
Claude Code answers the SIGWINCH by drawing the whole screen again at the size
it is actually being shown at. Coming back to the window is the other moment it
shows, because that is when the program repaints unprompted.

**Savras will not run inside itself.** A panel in a pane of another panel is
two lists of the same sessions, two sets of the keys, and an `attach` opened
twice over one session if you use both — so typing `svr` in a pane says so and
tells you to press `ctrl-t` instead. Every pane carries `SAVRAS_PANE` in its
environment, which is how the second one knows; `svr --once` is exempt, being
plain text a status line inside a pane may well want.

The cost is worth knowing: an open tab is a live `claude attach` and a
2000-line scrollback buffer, so the header carries a `▷` count of the tabs
running behind the one you are looking at. Sessions you have never opened cost
nothing. Leaving the shell you started with still closes Savras — but only
while you are looking at it; exiting it in a background tab leaves a dead tab
rather than taking your other sessions down from somewhere you cannot see.

### Parallel agents

A **parallel agent** is a session of its own that takes its work from another
session. Not a subagent: a subagent lives inside one model's turn and dies with
it, while a parallel agent has its own context, its own row in the panel, and
keeps running between assignments. You can open it, talk to it directly, and it
can message its peers.

`a` on any session in the panel starts one:

```
                you press  a  on AGENT
                           │
                           ▼
   claude --bg -n AGENT-3 "You are AGENT-3, a parallel agent working
                           under AGENT. … message AGENT to say you are
                           up and ask what it needs …"
                           │
   AGENT-3 starts ──SendMessage──▶ AGENT   "AGENT-3 here, ready."
```

The briefing is the new session's **opening prompt**, so it is genuinely sent
rather than drafted, and it arrives before the agent has done anything. The
lead is told by the agent itself, in its first act — Savras never interrupts a
session that is already running, and there is no supported way to put words
into one that has already started.

**Nothing is configured.** The group is read out of the names, the way
everything else here is read off the disk: Claude Code names a second session
with the same name `NAME-2` and a third `NAME-3`, so the bare name leads and
the numbered ones follow. Two sessions sharing a base name are a group; one is
just a session, which is also what stops a lone `PR-357` being read as somebody
else's agent. If the session a group was named after exits, the lowest-numbered
agent leads — a lead has to be a session you can actually message, or every new
agent is handed an address that goes nowhere.

The panel says where each session sits in its group, in the detail footer:
`leads AGENT-2, AGENT-3` under the lead, `parallel agent under AGENT` under a
member.

This is the one thing Savras does that is not looking. It still never writes to
`~/.claude/`, and it still cannot disturb a session that is running — but it
can now *start* one, with `claude --bg`, in that group's own repository.

### Ping

When a session arrives in **Needs input**, Savras says so: a desktop
notification, written to the terminal as an OSC 9 escape sequence, and a sound.
No dependency, no permissions dialog, and it works over SSH — the notification
comes from your terminal, not from the binary.

The panel says which session it was. A session that has pinged carries a bright
`●` where its status star normally sits, and the header counts them:

```
SAVRAS  ◦ live  ● 1
2 needs input · 2 working · 5 done

Needs input
● AGENT-2     approve Bash: M=/User…    #382 5m     ← this one just asked
✳ AGENT       1) approve AGENT-2 bu…    #369 5m
```

A sound is over in a second and you may be in another application when it
happens, so the mark stays until you go to the session: opening it, or moving
the cursor onto it, clears it, and so does the session no longer needing you —
answered in its own tab, say.

It fires on the *transition*, not on a poll tick. Savras opening beside four
sessions that already need you is silent; a question already on screen is never
re-announced. A burst of sessions asking at once is one ping that counts them,
and twenty quiet seconds follow — `--quiet <seconds>` moves that.

Those seconds are silence, not deafness. A session that starts asking while the
window is still running is *held*, and announced the moment it closes; it is
never dropped, and it is marked in the panel when it is finally said. That was
the one way a real question could go unheard, and it was the difference between
a ping that works and a ping that works most of the time. What is held is also
re-checked before it is said: a session that went back to working, or that you
have since opened, is dropped rather than announced twenty seconds stale.

Two smaller ways a question used to slip past, both closed. Savras *samples*
the jobs directory — it is not told about changes — so a session that answers
one question and asks another between two samples never appears to change
status; the question text is now compared as well, and different words are a
new question. And a `state.json` caught mid-rewrite parses as nothing at all,
which briefly made the session vanish; a job now has to be missing from three
readings in a row before Savras forgets what it was doing, and an unparseable
file is read a second time before it is believed.

The session showing in the working pane is spared a ping — but only while the
terminal has focus, because only then is it asking you in person. Switch to
your browser and that pane is as invisible as any other session, so it pings
like one. Savras knows the difference by asking the terminal to report focus
(DEC mode 1004); until the terminal says otherwise it assumes you are away,
since a ping you did not need is a smaller failure than the question you never
saw.

```sh
svr --ping done       ping when a session finishes, too
svr --ping off        no notification, no sound
svr --no-sound        notify silently
svr --quiet 5         a shorter silence after each ping
```

The sound is `afplay` on macOS, `canberra-gtk-play` or `paplay` on Linux,
`[console]::beep` on Windows, with the terminal bell underneath all of them.
iTerm2, WezTerm, Ghostty, kitty and Windows Terminal understand OSC 9.
Terminal.app understands no notification sequence at all and is sent none — a
terminal that cannot parse a sequence may print it across the panel instead of
swallowing it — so there the sound and the bell are the whole story. They are
enough: the bell puts a badge on the tab, which is what you look for anyway.

### tmux, if you want it

```sh
svr --tmux
```

Hosting the pane means the session dies with Savras. `--tmux` builds the same
layout in tmux instead, so the session survives a crash or a dropped SSH
connection, and you can detach and reattach. The tmux is kept invisible: no
status bar, no prefix keys to learn, just a divider. Savras writes its own tmux
config and applies it only to the server it starts, so an existing tmux setup is
left alone. Run it from inside tmux and it adds the column to the window in
front of you. `--dry-run` prints the tmux commands instead of running them.

## Usage

```
svr                    the side panel, with your shell beside it
svr -- claude          ... with Claude Code beside it
svr --side left        put the panel on the left instead of the right
svr --tmux             use tmux, so the session survives a crash
svr solo               just the panel, with no working pane
svr --ping <when>      ping on needs (the default), done, or off
svr --no-sound         notify without a sound
svr --quiet <seconds>  silence after a ping; news is held, not lost (default 20)
svr --switch <keys>    ctrl-<back><forward> flips tabs (default ws), or off
svr --open <what>      what the pane starts on: shell, top, or a session name
svr --once             print the current sessions as plain text and exit
svr --keys             print what this terminal sends for each key
svr --jobs-dir <path>  read jobs from somewhere other than ~/.claude/jobs
```

`--once` is for status lines, scripts, and anywhere without a TTY.

**An empty shell is not always what you came for.** `svr --open top` puts the
session at the top of the panel in the pane at startup — Needs input before
Working before Completed, so it is the one most likely to be the reason you
opened Savras. `--open <NAME>` opens that session by name. The shell is still
the first tab either way, so ctrl-w takes you back to a prompt.

**When a chord seems to do nothing**, `svr --keys` says what your terminal
actually sent and what Savras would make of it. There are only two possible
answers and no way to tell them apart by staring: the terminal never sent the
chord — which is most of them, since Command chords and unmodified arrows are
indistinguishable from plain arrows in the byte stream — or it sent something
Savras does not read. This says which:

```
$ svr --keys
Press keys to see what this terminal sends. Ctrl-C to stop.

\e[1;6A                  the chord — flip back a tab
\x17                     the switch key — flip back a tab
\e[A                     passed to the program in the pane
```

| key | with the panel focused | in `svr solo` |
|---|---|---|
| `↑` `↓` / `k` `j` | move | move |
| `g` / `G` | first / last | first / last |
| `r` | refresh now | refresh now |
| `enter` | open the selected session | — |
| `n` / `ctrl-t` | a tab of your own | — |
| `x` | close the selected tab | — |
| `d` | delete the selected session, after asking | — |
| `a` | start a parallel agent under its lead | — |
| `q` / `Esc` | back to your work | quit |
| `Q` | quit Savras, after asking | quit |
| `ctrl-l` | paint the screen again | paint the screen again |

Quitting the side panel closes the working pane with it, so `Q` asks before it
does — the footer says so, and any other key answers no.

`ctrl-l` is there for one specific annoyance: Terminal.app lets you scroll the
view of a full-screen application, and a scrollbar drag sends no bytes at all,
so Savras cannot see it happen and cannot repaint on its own. Scrolling back to
the bottom is the real cure; `ctrl-l` redraws everything if the screen is left
looking wrong.

The panel is responsive. Summaries are truncated to fit rather than dropped, so
a 44-column sidebar still tells you what each session is doing; only when fewer
than 8 columns are left for it does the summary go. Below 16 rows the detail
footer goes, to keep sessions visible.

## Roadmap

Shipped: the panel (M0), the column (M0.5), hosting the working pane (M1),
opening a session from the panel (M1.5), and the ping (M2).

Next up is **M3 · Repos** — group and sort the panel by repository. Then
**M4 · Vitals**. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Development

```sh
cargo test        # 94 tests: parsing, grouping, navigation, ping
                  # transitions and marks, focus reporting, argument
                  # handling, tmux layout, and render buffers
cargo build --release
```

The render tests draw into a real terminal buffer and assert on the resulting
characters, so column alignment and the responsive breakpoints are covered
rather than eyeballed. The ping has an end-to-end test as well: it runs the
built binary in a pseudo-terminal, changes a job on disk, and reads the
notification back off the terminal stream.

## License

MIT
