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
cargo build --release
cp target/release/svr ~/.local/bin/    # or anywhere on PATH
```

## The side panel

The point is a narrow column down the side of the terminal you are already
working in, with your session beside it:

```sh
svr panel                # panel on the right, a shell beside it
svr panel -- claude      # panel on the right, Claude Code beside it
svr panel --side left    # the other way round
svr panel --width 52
```

No tmux, no configuration, nothing else to install. Savras hosts the working
pane itself: it opens a pseudo-terminal, runs your command in it, and draws its
screen beside the panel. Keystrokes are forwarded to the child as raw bytes
rather than decoded and re-encoded, so arrow keys, Ctrl chords, paste and
full-screen TUIs behave exactly as they would in a normal terminal.

Savras does not implement a terminal emulator. [`portable-pty`][pty] provides
the pseudo-terminal (ConPTY on Windows) and [`vt100`][vt100] interprets the
output; Savras is the layout and the glue.

`ctrl-g` moves the keyboard to the panel and back. Everything else goes
straight to your work, including the mouse: Savras follows the child in and out
of mouse reporting and mirrors it to the real terminal, so scrolling reaches
Claude Code instead of dragging the terminal's own scrollback across both panes.
With the panel on the left, mouse coordinates are shifted into the working
pane's own frame.

[pty]: https://crates.io/crates/portable-pty
[vt100]: https://crates.io/crates/vt100

### tmux, if you want it

```sh
svr panel --tmux
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
svr                    open the panel on its own
svr panel              open the panel as a column beside your work
svr panel --side left  put it on the left instead of the right
svr panel --tmux       ... using tmux, so the session survives a crash
svr --once             print the current sessions as plain text and exit
svr --jobs-dir <path>  read jobs from somewhere other than ~/.claude/jobs
```

`--once` is for status lines, scripts, and anywhere without a TTY.

| key | |
|---|---|
| `↑` `↓` / `k` `j` | move |
| `g` / `G` | first / last |
| `r` | refresh now |
| `q` / `Esc` | quit |

The panel is responsive. Summaries are truncated to fit rather than dropped, so
a 44-column sidebar still tells you what each session is doing; only when fewer
than 8 columns are left for it does the summary go. Below 16 rows the detail
footer goes, to keep sessions visible.

## Roadmap

- **M0 — the panel.** ✔
- **M0.5 — the side panel.** ✔ `svr panel`, hosted directly or via tmux.
- **M1** — ping on attention (OSC 9 desktop notification), `enter` to resume via
  tmux, `d` to kill a session.
- **M2** — a background poller for GitHub/GitLab: PR state, checks, reviews.
- **M3** — packaging: Homebrew tap, Scoop, winget, deb/rpm, install script.
- **M4** — GitLab, deploy status, per-repo grouping.

## Development

```sh
cargo test        # 52 tests: parsing, grouping, navigation,
                  # argument handling, tmux layout, and render buffers
cargo build --release
```

The render tests draw into a real terminal buffer and assert on the resulting
characters, so column alignment and the responsive breakpoints are covered
rather than eyeballed.

## License

MIT
