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
pane — `claude --resume`, in that session's own repository.

---

## Next

### M2 · Ping  ← next build
**A sound, and a notification, when a session needs you.**

The trigger already exists and is exact: a job's `needs` field going from null
to non-null, or `state` reaching `done`. Savras sees both transitions today; it
just says nothing about them.

- Fire on the *transition*, never on a poll tick, or the panel becomes a car
  alarm.
- Desktop notification via the **OSC 9 / OSC 777** escape sequences, which
  iTerm2, WezTerm, Kitty, Ghostty and Windows Terminal understand. No
  dependency, works over SSH, and sidesteps macOS refusing notifications to
  unsigned CLI binaries.
- Sound: `afplay` on macOS, `paplay`/`canberra-gtk-play` on Linux,
  `[console]::beep` on Windows. Terminal bell (`\a`) as the floor.
- Configurable: sound on/off, notify on `needs` only or on `done` too, and a
  quiet period so a burst of finishing agents does not become a drum roll.
- Never ping for the session you are looking at.

Open question: Terminal.app supports neither OSC 9 nor OSC 777, so on the
current setup this is sound plus bell only. Worth deciding whether that is
enough or whether it justifies a small signed helper app.

### M3 · Repos
**Group and sort the panel by repository.**

Every job already carries its `cwd`, and the set of distinct `cwd`s across all
jobs *is* the set of repositories in play — no configuration to maintain.

- Group by repository, with the status groups nested inside, or sort flat by
  repository with the name as a dim prefix. Worth trying both before choosing.
- Repository name from the git remote where there is one, falling back to the
  directory name.
- Collapse a repository you are not working in today.
- A repository with a session needing input sorts to the top: the panel's job is
  to surface what is waiting, and grouping must not bury it.

### M4 · Vitals
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
- **Poller** — a background process for GitHub and GitLab: PR state, checks,
  reviews, deploys. Needed properly for M4's deploy column.
- **Packaging** — Homebrew tap, Scoop, winget, deb/rpm, install script, via
  `cargo-dist`.
- **Scrollback** — read a finished session's output without leaving the panel.
