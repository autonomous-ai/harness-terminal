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

The prefix is `Ctrl+Space` (tmux-style), then a command:

| Keys | Action |
|------|--------|
| `Ctrl+Space` `/` | palette: fuzzy-jump to any session |
| `Ctrl+Space` `n` | new session (engine picker) |
| `Ctrl+Space` `r` | attach to a remote `pane@host` |
| `Ctrl+Space` `s` | fleet status (read-only) |
| `Ctrl+Space` `f` | search scrollback |
| `Ctrl+Space` `[` | copy mode (vim nav, block select, copy) |
| `Ctrl+Space` `?` | help (full keybinding reference) |
| `1-9` / `Tab` | switch tab |
| `x` / `c` | close tab / jump to tab 0 |
| `g` / `b` | scroll up a page / jump to bottom |
| `Ctrl+=` / `Ctrl+-` | font zoom (`Ctrl+0` resets) |
| `PgUp` / `PgDn` | scrollback |
| `Cmd`/`Ctrl`+click | open the URL / file path under the cursor |
| `Alt`+click | move the shell cursor (click-to-move) |
| `Cmd+C` | copy selection |

Backgrounded tabs that keep producing output are flagged with a `!` in the tab bar.

## Config

`~/.config/harness-terminal/config.toml` (same dir as session persistence):

```toml
font_px = 14                # base font size the window opens at
default_engine = "claude"   # engine the new-session picker starts on
```

Set `HARNESS_CONFIG_DIR` to override the config/persistence directory (useful for portable or CI
setups).

## Status

Functional and in active development. Persistent tabs, fleet status, scrollback search, copy mode,
session restore, config, and font zoom all work. Architecture in `ARCHITECTURE.md`.

## License

MIT — see [LICENSE](LICENSE).
