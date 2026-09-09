# Savras

A side panel that sees every Claude Code session you have running.

Named for the god of divination, who sees all things as they are.

```
SAVRAS  ◦ live
1 needs input · 2 working · 4 done

Needs input
✳ WO        WAITING  answer: All five items are tier…    31%  29m

Working
✳ PLAN      WORKING  pipeline validated end-to-end…      12%   4h
✳ SAVRAS    WORKING  scaffolding the read-only panel      2%   8m

Completed
✳ SETTINGS  DONE     Triaged the five ASSISTANT_SET…     44%  53m
✳ ROADMAP   DONE                       MERGED #357      88%   3d
────────────────────────────────────────────────────────────────────
~/Code/autodad-assistant  11,547 tokens
pr#357
claude --resume a876377e-dd14-4de9-9c67-de3aee690f5a
↑↓ move · r refresh · q quit
```

Keep it open in a narrow pane on the left. Glance at it to see which sessions
are waiting on you, which are still working, and what the finished ones
concluded.

### What a row says

```
▶ BOOKS-LEG3   WORKING    READY #411   38%   2h
  └ name       └ what     └ its PR     └ ctx └ open for
```

- **The name** — in white, with no colour badge, for the session in the pane
  beside the panel. That is the one your keystrokes are going to.
- **One word.** `WAITING` wants you, `WORKING` does not, `DONE` is finished,
  `FAILED` broke. It sits where the summary used to and answers the question you
  actually scan ten rows for.
- **The pull request**, if the session produced one: `READY` when its checks are
  green, `CHECKS` while they run, `FAILED` when one has not, then `MERGED` or
  `CLOSED`. Read from the cache Claude Code keeps itself, so it costs no network
  and can be a minute stale.
- **Context spent.** Measured against the model's real window — a `[1m]` model
  is a million tokens, everything else 200k. It turns red past 85%, where
  compaction is coming and it is worth wrapping up rather than being surprised.
- **How long it has been open**, counted from when the session started and not
  from when it last spoke. A session in its fourth hour goes amber and its
  eighth red, because that is usually a session that has lost the plot.

The summary takes whatever width is left over, so a wide panel shows it and a
narrow one clips or drops it. That is the trade: what a session *is* survives at
every width, and the sentence about what it is doing is the thing that gives
way — it is still in the detail footer under the cursor.

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
svr                # panel on the right, the top session beside it
svr -- claude      # panel on the right, Claude Code beside it
svr --side left    # the other way round
svr --width 52
svr --open shell   # start on a prompt instead of a session
svr solo           # just the panel, no working pane, for its own tab
```

Both of those are also keys, so you can settle it by looking rather than by
guessing at a number: with the panel focused, **`<` and `>` move the divider**
four columns at a time and **`[` and `]` put the panel on the left or the
right**. The pane beside it is a real pseudo-terminal, so it is told about the
new size and the program inside repaints itself against it. The panel will not
narrow past 12 columns or grow until the pane stops being usable.

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
● AGENT-2     WAITING  approve Bash: M…  #382  9%  5m
▶ PLAN        WORKING  pipeline valid…        12%  4h   ← in the pane now
▷ SAVRAS      WORKING  scaffolding th…         2%  8m   ← open, running, behind
✳ ROADMAP     DONE       MERGED #357         88%  3d   ← not open
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
followed `--` otherwise — **in the directory of the session you are on**: a
shell opened beside `PLAN` starts in `PLAN`'s repository, because that is what
you were about to `cd` to. The session in the pane decides it, or the row under
the cursor when there is none, and Savras's own directory when there is no
session in play at all. Your terminal
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
unnamed pane has always meant here.

The cycle is the list **as you are looking at it**, so `s` regroups the flip
keys along with the screen. Grouped by repository, flipping goes down one
repository and on to the next, in the order the headings are drawn — not down
some other order that only agrees with the screen while you are grouped by
status.

**`exit` closes a tab you opened**, the way it closes a tab in any terminal —
there is nothing on a finished shell's screen worth keeping you there. `x` in
the panel closes the one under the cursor without waiting for that. A
*session's* pane is the opposite case and keeps its last screen, because that
screen is usually the reason it stopped. And the shell you *arrived* in is
neither: leaving that one is leaving Savras, which asks first.

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

### Repositories

The panel groups by repository, because with several in play at once the status
groups interleave them all and neither "who needs me" nor "what is happening in
this codebase" reads off the screen:

```
SAVRAS  ◦ live  ● 1
1 needs input · 3 working · 2 done

savras
▶ SAVRAS      WORKING  grouping by rep…        2%  2h
● └ SAVRAS-4  WAITING  approve Bash: c…  #382  9%  5m

autodad-assistant
✳ BOOKS       WORKING  Four of the 24…        61% 10m
✳ NEXT        DONE       MERGED #411          88% 19h

processore
✳ PLAN        DONE        READY #28          25%  1d
```

**A repository with a session waiting on you sorts to the top**, and inside a
repository the old order holds: waiting, then working, then done, and by name
within each. A question does not stop being a question because of where it was
asked, and grouping must not bury it.

**Parallel agents sit under their lead.** `SAVRAS-4` is indented beneath
`SAVRAS`, in number order, because the names already say they belong together —
Claude Code numbers repeats, so the bare name leads and the numbered ones
follow. The indent comes out of the name column rather than shifting the row,
so everything to the right of it still reads as a column down the list.

A *family* is placed by the most demanding status anyone in it has, then by its
lead's name — so a group with a question in it rises to the top, and an agent
finishing a task never reshuffles the group under your cursor. A session with
no siblings is an ordinary row: the tail has to parse as a number, so `PR-357`
leads nothing and `BOOKS-LEG3` is not an agent of `BOOKS`. Grouped by status
there is no indent at all, since a lead and its agent can be under different
headings and an arrow pointing off-screen would be a lie.

The name comes from the repository root — the nearest directory above the
session's `cwd` with a `.git` in it. **Worktrees come home**: a worktree's
`.git` is a file saying `gitdir: <repo>/.git/worktrees/<name>`, so three
parallel agents each in their own worktree are three rows under one heading
rather than three headings named after branches. The detail footer says
`worktree` when the session you are on is in one. A session outside any
repository is filed under its own directory, and one in your home directory
under `~`.

`s` flips between grouping by repository and by status, and `--group status`
starts that way for good.

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

Which is why numbering starts above the lead, not at the lowest free number
anywhere. With `SAVRAS-4` leading `SAVRAS-5` and no bare `SAVRAS` running, the
free `2` is left alone and the next agent is `SAVRAS-6`: an agent called
`SAVRAS-2` would have been briefed to report to `SAVRAS-4` and then, being the
lowest number, been read as the lead of the session that briefed it. Numbers
are still reused above the lead — close `AGENT-2` under a bare `AGENT` and the
next agent is `AGENT-2` again, rather than the numbers climbing forever.

The panel says where each session sits in its group, in the detail footer:
`leads AGENT-2, AGENT-3` under the lead, `parallel agent under AGENT` under a
member.

This is the one thing Savras does that is not looking. It still never writes to
`~/.claude/`, and it still cannot disturb a session that is running — but it
can now *start* one, with `claude --bg`, in that group's own repository.

### Other machines

`--machine <ssh-host>` puts the sessions running on another box in the same
list as yours. Repeat it for more than one.

```
svr --machine claude-box
```

With no `--machine` at all, Savras watches the hosts written in
`<config>/savras/machines` — one per line, `#` for comments:

```
# ~/Library/Application Support/savras/machines, or ~/.config/savras/machines
claude-box
```

This is the one thing Savras has to be *told*. Everything else it reads off
the disk, but no file anywhere says which of the hosts in your ssh config you
want watched — and a flag you retype every time is a flag you forget, which
looks exactly like a box with nothing running on it. `--machine off` watches
none of them for a run.

```
claude-box:autodad-assistant          ← the machine is part of the heading
● autodad-assistant-9c  waiting at the prompt   2m
▷ AGENT                 working                 7m
```

The far side is read from `~/.claude/sessions/<pid>.json`, not from
`~/.claude/jobs/`. That is not a detail: **a machine you ssh into and work in
by hand has no jobs directory at all**, because nothing there runs Claude
Code's daemon. What the sessions file has instead is the one thing the jobs
directory does not — `tmux`, naming the window the session is running in.

So `enter` on one of those rows does not run `claude attach`; there is nothing
to attach to. It opens the tmux window, in a **grouped** session:

```
ssh -t <host> tmux new-session -t <session> \; set destroy-unattached on \; \
                               select-window -t <window>
```

Grouped rather than attached, because you are already attached to that session
from your own terminal, and a second client forces both terminals to the
smaller of the two sizes — the panel would silently shrink the window you are
working in. A grouped session shares the windows but keeps its own size and its
own selected window, and `destroy-unattached` takes it away the moment you
close the tab, so nothing is left behind on the machine.

**Watching read-only was built and removed.** `v` opened the same window with
tmux's `-r` so the pane could not type into it, and `c` turned that pane back
into one you could work in. Both are gone. The case they served — somebody
*else* at the keyboard of that session — is not one that happens on a box you
ssh into alone, and the cost was not free: a read-only view is still a live
tmux client, which means a second ssh, a client the far tmux server has to
render for, and a screen streamed across the internet for a pane you were only
looking at. To open a shell on another machine, `ctrl-t` and type `ssh` — the
thing you would have typed anyway.

Two more things worth knowing:

- **`idle` over there is "needs input" over here.** A session you drive by hand
  has no question flag to read; it is either thinking or it is not, and "not"
  means it is your turn. So the ping fires when a remote session stops working,
  which is exactly the moment you wanted to know about.
- **One ssh per machine, held open.** A round trip is most of a second and
  sshd allows ten sessions, so a poll per refresh would be both slow and
  wasteful. One connection runs a loop on the far side and streams a batch
  every two seconds, multiplexed so that opening a session costs no second
  handshake. The loop asks `kill -0` before sending a row, because nothing
  cleans those files up when a session exits — without it the panel would show
  ghosts for as long as the box stayed up.

### A tab that is a session

Start `claude` yourself in a tab and the panel used to show it twice: an
anonymous `shell 2` you were looking at, and the session's own row, with
nothing to say they were the same thing — so `enter` on the row attached a
*second* time to a session already in front of you.

Claude Code writes `~/.claude/sessions/<pid>.json` for every live session, and
it carries the job id. Savras asks the pane's own foreground process what it
is, so a pane running a session *is* that session's tab: one row, marked as
the one you are in, and `enter` on it goes there rather than opening it again.
It is asked on every pass rather than once, because it goes both ways — leave
the session and the pane is a shell again.

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
svr                    the side panel, with the top session beside it
svr -- claude          ... with Claude Code beside it
svr --side left        put the panel on the left instead of the right
svr --tmux             use tmux, so the session survives a crash
svr solo               just the panel, with no working pane
svr --ping <when>      ping on needs (the default), done, or off
svr --no-sound         notify without a sound
svr --quiet <seconds>  silence after a ping; news is held, not lost (default 20)
svr --switch <keys>    ctrl-<back><forward> flips tabs (default ws), or off
svr --open <what>      what the pane starts on: top (default), a name, or shell
svr --once             print the current sessions as plain text and exit
svr --keys             print what this terminal sends for each key
svr --jobs-dir <path>  read jobs from somewhere other than ~/.claude/jobs
```

`--once` is for status lines, scripts, and anywhere without a TTY.

**An empty shell is not what you came for.** Savras starts on the session at
the top of the panel — Needs input before Working before Completed, so it is
the one most likely to be the reason you opened Savras at all. `--open <NAME>`
opens a particular session instead, and `--open shell` gives you the prompt
that used to be the default. A command after `--` overrides all of it: naming
one is saying what the pane is for. Your shell is the first tab either way, so
ctrl-w is always a prompt away, and with no sessions to open it is what you
get.

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
| `s` | group by repository, or by status | group by repository, or by status |
| `enter` | open the selected session | — |
| `n` / `ctrl-t` | a tab of your own | — |
| `x` | close the selected tab | — |
| `d` | delete the selected session, after asking | — |
| `a` | start a parallel agent under its lead | start a parallel agent under its lead |
| `<` / `>` | narrow / widen the panel | narrow / widen the panel |
| `[` / `]` | put the panel left / right | put the panel left / right |
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
opening a session from the panel (M1.5), the ping (M2), and repositories (M3).

Next up is **M4 · Vitals** — make the session line say more in the same width.
See [docs/ROADMAP.md](docs/ROADMAP.md).

## Development

```sh
cargo test        # 155 tests: parsing, grouping, navigation, ping
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
