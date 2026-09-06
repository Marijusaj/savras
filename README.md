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

It is **read-only**. It never writes to `~/.claude/`, never talks to Claude Code
over any interface, and never touches the network. It cannot disturb the
sessions it reports on, and if a Claude Code upgrade changes the format the
panel degrades rather than breaks — unparseable jobs are skipped, unknown
fields ignored.

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
keyboard back. Claude Code runs most sessions in its daemon, and those are
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
and twenty quiet seconds follow.

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
svr --once             print the current sessions as plain text and exit
svr --jobs-dir <path>  read jobs from somewhere other than ~/.claude/jobs
```

`--once` is for status lines, scripts, and anywhere without a TTY.

| key | with the panel focused | in `svr solo` |
|---|---|---|
| `↑` `↓` / `k` `j` | move | move |
| `g` / `G` | first / last | first / last |
| `r` | refresh now | refresh now |
| `enter` | open the selected session | — |
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
