<img width="1028" height="659" alt="image" src="https://github.com/user-attachments/assets/fd783b3a-a508-44d8-b868-5ce5b8200993" />

# harness-terminal

**A terminal-first client for working with AI coding agents across many computers and servers.**

One agent session per tab — wherever that session runs, on whichever machine, running whichever
agentic framework (Claude Code, Codex, OpenCode, …). Jump into any session in your fleet in a
keystroke and type into it as if it were local.

```
TAB = SESSION = PANE@HOST
```

Flat on purpose: each session is **one tmux pane on one host**, and a tab here connects to exactly
that pane. tmux owns layout on the host; this client owns your fleet of sessions.

Built on the same e2e-encrypted fabric as [`autonomous-harness`](https://github.com/autonomous-ai/autonomous-harness)
(`harness join`): `machineId` routing, Ed25519 + CPace PAKE pairing, X25519, ChaCha20-Poly1305.
We reuse that transport; we do not rebuild it.

## What it is

A standalone native window (winit + softbuffer + ab_glyph) drawing the alacritty terminal
emulator's grid directly — no host terminal needed. Every transport is supported: a local PTY, a
real local tmux pane, a pane on another machine over `ssh`, or a pane reached through the harness
e2ee tunnel.

## Running

```
cargo run                # standalone native window (default)
cargo run -- --tui       # legacy ratatui fallback (headless/SSH)
```

By default it reopens the tabs that were open last time, restores the font zoom, and reads
`~/.config/harness-terminal/config.toml` (see Config below).

## Keys

The prefix is `Ctrl+Space` (tmux-style), then a command. If macOS has a second input source
enabled (e.g. English + Vietnamese Telex), the OS itself owns `Ctrl+Space` (its *input-source
switcher* grabs the keystroke before any app sees it) — the app then falls back to **`Ctrl+\`**
and tells you so at launch. Either chord enters the same command mode:


| Keys | Action |
|------|--------|
| `Ctrl+Space` `/` | palette: fuzzy-jump to any session |
| `Ctrl+Space` `;` | command palette: run any action by name |
| `Ctrl+Space` `n` | new session (engine picker; type a working dir) |
| `Ctrl+Space` `r` | attach to remote `pane@host` (add `:port` for a non-default harness daemon) |
| `Ctrl+Space` `s` | fleet status (Up/Down + Enter to dive into a session) |
| `Ctrl+Space` `f` | search scrollback |
| `Ctrl+Space` `h` | search all sessions (fleet-wide) |
| `Ctrl+Space` `[` | copy mode (vim nav, block select, copy) |
| `Ctrl+Space` `,` | rename the active tab (persisted, shown in tab bar) |
| `Ctrl+Space` `a` | broadcast a line (per-session checkboxes target it) |
| `Ctrl+Space` `y` | peek the tail of every session, then jump |
| `Ctrl+Space` `e` | fleet grid: live tails of every session at once (war-room view; `Space` mark a tile, `b` broadcast to marked) |
| `Ctrl+Space` `d` | copy the whole scrollback to the clipboard |
| `Ctrl+Space` `j` | copy the session identity (`engine@host`) to the clipboard |
| `Ctrl+Space` `E` | copy a one-line summary of every open tab (the fleet, grep-friendly) |
| `Ctrl+Space` `w` | write the scrollback to a `.log` file |
| `Ctrl+Space` `u` | undo close (reopen the last closed tab) |
| `Ctrl+Space` `k` | duplicate the active tab (fork the same engine@host) |
| `Ctrl+Space` `m` | mute/unmute the active tab (no more busy nagging) |
| `Ctrl+Space` `M` | toggle do-not-disturb: mute ALL OS notifications fleet-wide (in-bar badges stay) |
| `Ctrl+Space` `A` | pin/unpin the active tab (won't close with `x` until unpinned) |
| `Ctrl+Space` `P` | jump to the next pinned tab |
| `Ctrl+Space` `R` | force-reconnect a dead tab now (bypasses the auto-retry backoff) |
| `Ctrl+Space` `T` | force-reconnect EVERY down remote pane at once |
| `Ctrl+Space` `D` | kill the active tab's pane (destroy the remote tmux session) |
| `Ctrl+Space` `!` | send Ctrl-C to the active tab (stop the run) |
| `Ctrl+Space` `?` | help (full keybinding reference) |
| `Ctrl+Space` `o` | jump to the next busy (produced-output) tab |
| `Ctrl+Space` `z` | jump to the next quiet (done/awaiting-input) tab |
| `Ctrl+Space` `H` | jump to the next host (page the fleet by machine) |
| `Ctrl+Space` `Q` | jump to the next down/reconnecting tab |
| `Ctrl+Space` `l` | flip back to the previous tab |
| `Ctrl+Space` `i` | show the active tab's info (kind, host, task, size, state, age) |
| `Ctrl+Space` `I` | mark all tabs read — clear every busy, bell, and recovery badge at once |
| `Ctrl+Space` `v` | focus mode: hide the tab bar + status line (distraction-free) |
| `1-9` / `0` / `Tab` / `Shift+Tab` | switch tab (`0` = last, Shift+Tab = backward) |
| `Ctrl+Space` `{` / `}` | move the active tab left / right |
| `x` / `C` | close tab / close all quiet (done) tabs at once |
| `c` | jump to tab 0 |
| `g` / `b` | scroll up a page / jump to bottom |
| `Ctrl+Enter` | toggle fullscreen |
| `Ctrl+=` / `Ctrl+-` | font zoom (`Ctrl+0` resets) |
| `PgUp` / `PgDn` | scrollback |
| `Cmd`/`Ctrl`+click | open the URL (web / `mailto:` / `tel:`), file path under the cursor |
| `Alt`+click | move the shell cursor (click-to-move) |
| `Cmd+C` | copy selection |
| `Ctrl+Space` `p` | paste clipboard (bracketed) |
| Middle-click | paste clipboard (raw) |

Backgrounded tabs that keep producing output are flagged with a magnitude badge in the tab bar
(e.g. `!43` — how many new lines since you last looked). Muted tabs show a dim `M` instead.
A tab that rings its terminal bell (a long agent run finishing) shows a short-lived `🔔` badge and
nudges once with a notification when it isn't focused. Pane-backed (non-local) tabs append `@host`
so a fleet diver reads where each session runs without hovering. A down pane with input you typed
while it was dead shows `⏳N` (bytes staged to flush on reconnect). The tab bar's right edge shows a
fleet-triage count (`↓2` panes down/reconnecting in red, `!3` busy, `⌛2` quiet/done agents, `⏳N`
queued) when any is non-zero, so a quiet fleet still advertises that something needs attention.

## Config

`~/.config/harness-terminal/config.toml` (same dir as session persistence).

Every option is optional; a missing key keeps its safe default, and a broken file just falls back to
defaults. See `config.example.toml` in the repo for the full option surface. The quickly-relevant
ones:

```toml
font_px = 14                # base font size the window opens at
default_engine = "claude"   # engine the new-session picker starts on
font_path = ""              # optional TTF/OTF monospace font
scrollback_cap = 262144     # cap on persisted per-tab scrollback (bytes)
start_cwd = ""              # dir new local tabs open in
quiet_after_secs = 120      # silent this long (backgrounded, live) -> counted quiet/"awaiting-you"

[theme]                     # optional: entries overrides, the rest keep defaults
foreground = [234, 234, 234]
background = [0, 0, 0]
cursor = [234, 234, 234]    # underline/beam cursor
selection = [38, 79, 140]   # text-selection highlight background
copy_cursor = [30, 255, 138] # copy-mode read cursor block

[theme.ansi]                # 0 black..7 white, 8-15 bright; only listed change
0 = [0, 0, 0]
1 = [205, 49, 49]

[theme.accents]             # per-engine inactive-tab tints; unlisted keep brand color
claude = [200, 120, 255]
codex = [0, 122, 204]
```

Set `HARNESS_CONFIG_DIR` to override the config/persistence directory (useful for portable or CI
setups).

## Status

Functional and in active development. Persistent tabs, fleet status, scrollback search, copy mode,
session restore, config, and font zoom all work. Architecture in `ARCHITECTURE.md`.

## License

MIT — see [LICENSE](LICENSE).

### Remapping prefix keys

The keys you press right after `Ctrl+Space` are configurable via a `[keybindings]` block in the
config. It maps an **action name** to the key that triggers it. Anything you don't list keeps
today's default, so an empty block is a no-op.

```toml
[keybindings]
new_session = "N"   # prefix+Shift+n now opens a new session
mute = "v"          # prefix+v toggles mute
```

Action names: `palette`, `new_session`, `remote_attach`, `local_shell`, `quit`, `fleet`,
`goto_tab0`, `next_busy`, `next_quiet`, `next_down`, `next_host`, `dnd`, `mute`, `last_window`, `paste`, `broadcast`, `close_tab`, `close_quiet`,
`copy_scrollback`, `export_scrollback`, `copy_identity`, `copy_fleet`, `peek`, `fleet_grid`, `undo_close`, `duplicate`, `page_up`, `scroll_bottom`,
`search`, `search_all`, `move_left`, `move_right`, `copy_mode`, `help`, `command_palette`,
`rename`, `session_info`, `toggle_focus`, `pin`, `next_pinned`, `reconnect`, `reconnect_all`, `destroy`, `interrupt`. The digit keys `1-9` / `0` (tab switching) and `Tab`
are not remappable.
