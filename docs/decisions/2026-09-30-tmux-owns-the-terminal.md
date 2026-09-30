# tmux owns the terminal; savras is the panel

2026-09-30. Decided by the owner, after svr's copy, links and image paste all
broke in the same way.

## What went wrong

Plain `svr` hosts the working pane itself: it reads each child through `vt100`
and redraws the cells with ratatui. Everything a terminal carries *besides*
cells has to be passed on by hand, one feature at a time:

```
child ──▶ svr (vt100 → cells) ──▶ Terminal.app / Ghostty
  mouse modes        passed on
  bracketed paste    passed on (#25)
  focus reports      passed on
  OSC 52 copy        passed on in 0.1.11
  OSC 8 links        dropped
  selection          the terminal's own, across both panes
```

Each row is a bug until someone notices it. Copying a wrapped sign-in URL
picked up the divider (`│`), and a copy made in Claude Code never reached the
clipboard before 0.1.11.

## Decision

tmux owns the terminal. svr is one pane in it: the panel. Every tab is a real
tmux pane, and svr drives the layout through tmux commands.

```
BEFORE                                  AFTER
 Terminal ◀── svr (renders all) ──┐      Terminal ◀── tmux -L savras-<pid>
                panel │ work pty  │                    ├─ pane: svr panel
                        tab ptys ─┘                    ├─ pane: work slot (a tab)
                                                       └─ stash session: other tabs
```

Selection, links, clipboard, paste, mouse and focus become tmux's business.
They are handled once, by a program that has done them for years.

## How each piece moves

- **Tabs** are windows in a hidden stash session. Showing one is `swap-pane`
  into the work slot beside the panel. The process never restarts.
- **Keys** that were svr's (ctrl-g, ctrl-t, ctrl-w/s, the chords, enter/q on a
  dead pane) are `bind -n` lines in svr's private tmux config. A binding tells
  the panel what happened by sending a private key sequence to the panel pane,
  so the panel reads its orders on its own stdin. There is no socket.
- **A tab's facts** (session short id, relay repo, remote host, whether svr
  opened it) are pane user options (`@svr-short`, `@svr-relay`, `@svr-remote`,
  `@svr-opened`). What the tab *is* lives on the pane, not in a parallel list
  that could drift from it.
- **Exit**: `remain-on-exit on` for session and relay panes, and svr draws the
  banner from `#{pane_dead_status}`. Enter is `respawn-pane`.
- **Pings** read client focus (`#{client_flags}`), not pane focus, because tmux
  also sends a pane focus events when you move between panes.
- **Clipboard**: `set-clipboard on`, plus `copy-command pbcopy` on a Mac,
  because Terminal.app ignores OSC 52.

## Kept for now

The hosted mode stays behind `svr --host` until the tmux layout has done
everything it does for a week of real use. Then it is deleted, along with
`focus.rs` and the mouse, paste and OSC 52 relaying in `host.rs`.

## Costs we accept

- tmux becomes a dependency. Homebrew already installs it with the formula.
- The child sees `TERM=tmux-256color`, not `xterm-256color`.
- A key svr binds is gone from the child unless svr hands it back. ctrl-w is
  passed through when there is nowhere to flip, as before.
