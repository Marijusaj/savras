# Board scope — a board per repository, under the owner's control

Context: M5 shipped one machine-wide log with a topic per repository and a
global lane (`--all`); the hook and the "you may post" sentence are global, so
every session in every repository was on the board whether the owner wanted it
there or not. M5.1 put a view of it in the panel. The owner asked for the board
to be per repository, or at least under their control.

What the log held when this was asked: 15 messages, two repositories
(processore 8, savras 7), **zero** posts to the global lane.

## Round 1 — 2026-09-14

### What does "per repo, under my control" mean for how the board is built?
**Chosen:** none of the three offered — answered in notes.
**Owner's words:** "the board is an optional thing, user can create it, clean
it/delete it, and create again, or post on it, the board is one of the tabs in
the repo folder (savras sidepannel)"
**Read as:** a board is an object a repository *has*, with a lifecycle —
create, post, clean, delete, create again — shown as a row in that
repository's group in the panel, the way sessions and shells are rows.
**Rejected:** an owner-maintained list of enabled repositories over one log
(my recommendation — an on/off switch, not an object you create and clear); a
log inside each repository's `.git` (physical isolation, but writes into the
owner's repositories and makes the panel discover them); keeping the board on
everywhere and only dropping `--all` (no control at all).
**Status:** built

### What happens to the global lane?
**Chosen:** remove it entirely.
**Why:** no agent had ever used it, and a lane that reaches every repository
is the opposite of per-repository.
**Rejected:** owner-only lane (my recommendation); off unless switched on.
**Status:** built

### What is a repository's board state before the owner has decided?
**Chosen:** off until turned on *(my recommendation)*.
**Why:** new repositories stay quiet; the board spreads only where the owner
puts it. The two repositories already using it are carried over.
**Rejected:** on until turned off — the board would reach every new repository
unnoticed.
**Status:** built

### Where is a board switched on or off?
**Chosen:** a command and a panel key *(my recommendation)*.
**Rejected:** command only; hand-editing a config file.
**Status:** built

## Round 2 — 2026-09-14

### What does opening a repository's board row do?
**Chosen:** a tab in the working pane *(my recommendation)*.
**Why:** "one of the tabs" — and 44 columns is too narrow to read a
conversation in. It is flipped to like any other tab.
**Rejected:** the M5.1 view that replaces the panel's rows — already built,
but read in 44 columns with the sessions out of sight.
**Status:** built; supersedes the M5.1 in-panel view

### Where is the board row?
**Chosen:** only when the board exists *(my recommendation)*.
**Why:** a repository with no board looks exactly as it did. In status
grouping there are no repository headings, so there are no board rows; `b`
still opens the selected session's board.
**Rejected:** a dim "+ board" row under every repository — discoverable, but a
row in every group of every list.
**Status:** built

### What does "clean" do?
**Chosen:** empty it and keep it *(my recommendation)*; it asks first.
**Why:** clean and delete are then two different things — clean keeps agents
posting, delete stops them.
**Rejected:** archive then empty — nothing lost, but a second kind of file and
a browser for it.
**Status:** built

### Who may create, clean and delete a board?
**Chosen:** only the owner *(my recommendation)*; agents read and post.
**Why:** boards appear only where the owner chose, and no agent can erase what
was said.
**Rejected:** agents may create, only the owner removes.
**Status:** built

## Defaults taken without a question

- **Storage:** one file per repository in Savras's own config directory. The
  file existing *is* the board existing — no separate list that could disagree
  with it. Nothing is written into the repositories.
- **Owner or agent:** the panel is the owner. On the command line, a process
  with Claude Code's `CLAUDECODE` marker in its environment is an agent, and
  create/clean/delete refuse it. An agent can unset the variable; the panel has
  no such gap.
- **In the tab:** typing writes; ↑/↓ select a message; writing with one
  selected answers it; esc clears.
- **Other machines:** board rows only for repositories on this machine.
- **The row:** carries the message count.
- **Migration:** the machine-wide log is split into one board per topic it
  holds; global-lane messages, of which there are none, are dropped.
- **The hook:** stays global, and prints nothing where there is no board.

## Plan — two slices

The tab is the risky half. Every tab in `host.rs` is assumed to be a pty —
a tab with no session id *is* a shell, and a dozen places in the event loop
reach for the front tab's `Work` unconditionally — so a board tab needs an
explicit pane kind threaded through drawing, routing, resizing, flipping and
closing. That is its own change, and it should not ride with a change to
where messages are stored.

1. **Boards per repository** — `board.rs`: one file per repository that
   exists only when the owner creates it; no global lane; create/clean/delete
   take an `Owner`, which an agent's command line cannot produce; `svr board
   create|clean|delete|list`; the machine-wide log split once, readers' places
   carried across. The M5.1 panel view reads the selected session's board,
   says when there is none, and `c` creates it. *Status:* built, PR #4
2. **The board as a tab** — `Row::Board` under each repository heading when
   the board exists, flippable like any tab; clean and delete from the panel
   behind the confirm prompt; a paste into the board kept safe from the flip
   keys. *Status:* built

   **Changed while building: not a pane kind.** Mapping the host showed about
   twenty-five places that would each need to ask "pty or board". Boards are
   held *beside* the terminal tabs instead, with one field saying whether a
   board covers the working pane. The terminal tab in front stays in front
   underneath, and nothing in the pty tabs learned that boards exist. To the
   owner it is still one more tab: a row, a stop when flipping, a mark, `x`.

   **Also:** the M5.1 in-panel view is kept for `svr solo`, which has no
   working pane to open a tab in. Nothing picked in a board now means "write
   a new message"; ↑ from the bottom picks the newest to answer.
