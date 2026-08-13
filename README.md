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

The prefix is **`Ctrl+H`** (tmux-style — "Ctrl **H**arness", tmux's `Ctrl+B` equivalent), then a
command. `Ctrl+Space` and `Ctrl+\` are always accepted as fallback chords too, so macOS claiming
`Ctrl+Space` for its input-source switcher (when a second layout such as Vietnamese Telex is
enabled) can never break the prefix. To move the primary chord, set `prefix_key` in
`~/.config/harness-terminal/config.toml` (e.g. `"space"`, `"\"`, `"b"`). Every chord enters the
same command mode:


| Keys | Action |
|------|--------|
| `Ctrl+H` `/` | palette: fuzzy-jump to any session |
| `Ctrl+H` `;` | command palette: run any action by name |
| `Ctrl+H` `n` | new session (engine picker; type a working dir) |
| `Ctrl+H` `r` | attach to remote `pane@host` (add `:port` for a non-default harness daemon; pre-fills the last host) |
| `Ctrl+H` `s` | fleet status (Up/Down + Enter to dive into a session) |
| `Ctrl+H` `f` | search scrollback |
| `Ctrl+H` `h` | search all sessions (fleet-wide) |
| `Ctrl+H` `[` | copy mode (vim nav, block select, copy) |
| `Ctrl+H` `,` | rename the active tab (persisted, shown in tab bar) |
| `Ctrl+H` `a` | broadcast a line (per-session checkboxes target it) |
| `Ctrl+H` `y` | peek the tail of every session, then jump (`/` filters the list by host/engine/name/`down`) |
| `Ctrl+H` `e` | fleet grid: live tails of every session at once (war-room view; `Space` mark a tile, `b` broadcast to marked, `C` Ctrl-C marked, `m` mute selected, `x`/`X` close sel/all marked, `R` reconnect marked) |
| `Ctrl+H` `d` | copy the whole scrollback to the clipboard |
| `Ctrl+H` `j` | copy the session identity (`engine@host`) to the clipboard |
| `Ctrl+H` `E` | copy a one-line summary of every open tab (the fleet, grep-friendly) |
| `Ctrl+H` `w` | write the scrollback to a `.log` file |
| `Ctrl+H` `u` | undo close (reopen the last closed tab) |
| `Ctrl+H` `k` | duplicate the active tab (fork the same engine@host) |
| `Ctrl+H` `m` | mute/unmute the active tab (no more busy nagging) |
| `Ctrl+H` `M` | toggle do-not-disturb: mute ALL OS notifications fleet-wide (in-bar badges stay) |
| `Ctrl+H` `A` | pin/unpin the active tab (won't close with `x` until unpinned) |
| `Ctrl+H` `P` | jump to the next pinned tab |
| `Ctrl+H` `R` | force-reconnect a dead tab now (bypasses the auto-retry backoff) |
| `Ctrl+H` `T` | force-reconnect EVERY down remote pane at once |
| `Ctrl+H` `D` | kill the active tab's pane (destroy the remote tmux session) |
| `Ctrl+H` `!` | send Ctrl-C to the active tab (stop the run) |
| `Ctrl+H` `?` | help (full keybinding reference) |
| `Ctrl+H` `o` | jump to the next busy (produced-output) tab |
| `Ctrl+H` `z` | jump to the next quiet (done/awaiting-input) tab |
| `Ctrl+H` `H` | jump to the next host (page the fleet by machine) |
| `Ctrl+H` `Q` | jump to the next down/reconnecting tab |
| `Ctrl+H` `l` | flip back to the previous tab |
| `Ctrl+H` `i` | show the active tab's info (kind, host, task, size, state, age) |
| `Ctrl+H` `I` | mark all tabs read — clear every busy, bell, and recovery badge at once |
| `Ctrl+H` `v` | focus mode: hide the tab bar + status line (distraction-free) |
| `1-9` / `0` / `Tab` / `Shift+Tab` | switch tab (`0` = last, Shift+Tab = backward) |
| `Ctrl+H` `{` / `}` | move the active tab left / right |
| `x` / `C` | close tab / close all quiet (done) tabs at once |
| `c` | jump to tab 0 |
| `g` / `b` | scroll up a page / jump to bottom |
| `Ctrl+Enter` | toggle fullscreen |
| `Ctrl+=` / `Ctrl+-` | font zoom (`Ctrl+0` resets) |
| `PgUp` / `PgDn` | scrollback |
| `Cmd`/`Ctrl`+click | open the URL (web / `mailto:` / `tel:`), file path under the cursor |
| `Alt`+click | move the shell cursor (click-to-move) |
| `Cmd+C` | copy selection |
| `Ctrl+H` `p` | paste clipboard (bracketed) |
| Middle-click | paste clipboard (raw) |

Native macOS shortcuts work without an AppKit menu installed (routed Rust-side, so they fire
whenever the window is focused):
| `Cmd+T` / `Cmd+N` | new session (new native tab) |
| `Cmd+W` | close active tab / window |
| `Cmd+Q` | quit |
| `Cmd+Shift+[` / `]` | previous / next tab |
| `Cmd+1-9` / `0` | jump straight to that tab (`0` = last) |
| `Cmd+Shift+P` | command palette |
| `Cmd+Shift+F` | search all sessions (fleet) |
| `Cmd+Shift+T` | reopen the last-closed tab |
| `Cmd+Shift+D` | duplicate the active session |
| `Cmd+Shift+R` | force-reconnect ALL down panes |
| `Cmd+Shift+U` / `Cmd+Shift+M` | pin the active tab / mute the active tab |
| `Cmd+Shift+I` | show this tab's info (kind/host/task) |
| `Cmd+Shift+C` | copy the active tab's whole scrollback |
| `Cmd+Shift+S` | write the active tab's scrollback to a `.log` file |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | previous / next tab (browser muscle memory) |

Backgrounded tabs that keep producing output are flagged with a magnitude badge in the tab bar
(e.g. `!43` — how many new lines since you last looked). Muted tabs show a dim `M` instead.
A tab that rings its terminal bell (a long agent run finishing) shows a short-lived `🔔` badge and
nudges once with a notification when it isn't focused. Pane-backed (non-local) tabs append `@host`
so a fleet diver reads where each session runs without hovering (a down pane's tab carries a `↓`).
A down pane with input you typed while it was dead shows `⏳N` (bytes staged to flush on reconnect).
The tab bar's right edge shows a fleet-triage count (`↓2` panes down/reconnecting in red, `!3` busy,
`⌛2` quiet/done agents, `↻` just-reconnected, `⏳N` queued) when any is non-zero, so a quiet fleet
still advertises that something needs attention. Fleet OS notifications name the machine
(`claude@build02 · went down` on a drop, `build05 · reconnected` when it comes back), so a multi-host
fleet pings are actionable at a glance; the hosts overview (`Ctrl+H .` → `→`) drills into a machine,
where `r` reconnects its panes and `b` broadcasts to all of them at once.
The tab strip is drawn as a raised native-style bar (Safari/Chrome silhouette on the active tab,
hairline divider above the grid), and a `+` at its right edge opens the New-Session picker like any
idle-native terminal. When nothing is visible-changing the app idles at ~0% CPU, only pumping
full-rate while a pane is producing, a badge is fading, or an overlay/tooltip is up.

**Native macOS window-level tabs (opt-in).** Set `native_tabs = true` in `config.toml` and relaunch:
every session becomes a real `NSWindow`, and AppKit's system title-bar tab bar (OS traffic lights,
Cmd+Tab tab picker, drag-a-tab-out, `Ctrl+H n`/`+` opening a new tab in the same group) replaces the
in-app strip. The OS tab bar owns switching; the in-app strip stays when the switch is off.

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
preset = "tokyo-night"      # base palette: tokyo-night | gruvbox-dark | solarized-dark | nord | dracula | github-dark
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

The keys you press right after `Ctrl+H` are configurable via a `[keybindings]` block in the
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
