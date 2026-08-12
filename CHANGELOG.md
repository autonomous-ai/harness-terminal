# Changelog

All notable changes to harness-terminal. This project is under rapid, pre-1.0 development;
entries record user-visible and architectural changes since the last tagged milestone.

## Unreleased / 0.1.0 (in progress)

### Added
- **Configurable color theme** — `[theme]` / `[theme.ansi]` in `config.toml` override the
  foreground, background, cursor, selection, copy-cursor, and the 16 ANSI colors. Absent theme =
  exact built-in defaults; sparse `[theme.ansi]` maps override only the listed entries.
- **Configurable per-tab working directory** — `prefix+n` now prompts for the directory a new
  local tab should open in; blank falls back to the config `start_cwd` / the binary's cwd.
- **CI** — `.github/workflows/ci.yml` runs `cargo fmt --check`, clippy, a release build, and the
  full test suite on push to main and every PR.
- **`--version` / `-V` / `-v` flags** — print the crate version and exit instead of opening a window.
- **Fleet summary shows live count** — the status line's fleet summary now reports
  `N agents · M live · tunnel up/down`, distinguishing recently-updated (working) agents from idle ones.
- **Orphan-state purge at startup** — scrollback files and muted-tab entries for tabs that no
  longer exist (closed or renamed) are removed, keeping the state dir from accumulating stale data.

- **Last-broadcast persistence** — `prefix+a` pre-fills the last line sent fleet-wide so a repeat
  command (e.g. `git pull` on every host) is one keypress.
- **Type-ahead buffering** — typing into a dead remote pane buffers the keystrokes (visible as
  `⏳N` in the tab bar, the fleet triage, and `prefix+i`) and flushes them into the pane on the
  next successful reconnect, so a command you queue for a host that's coming back actually lands.
- **Broadcast history** — `Shift+Up` / `Shift+Down` in the broadcast overlay recalls previously sent
  lines (MRU, persisted), so alternating commands across machines doesn't need retyping.
- **MRU working directories** — `prefix+n` pre-fills `dir:` from the last repo a local tab spawned
  in, so respawning in the same repo is one Enter.
- **Remote attach `host:port`** — `prefix+r` accepts a `:port` to reach a non-default harness daemon.

### Changed
- Codebase reformatted with `cargo fmt` (drift from many hand-edits); CI now enforces it.
- `ARCHITECTURE.md` updated to describe the winit + softbuffer native layer (the ratatui TUI is
  demoted to the `--tui` headless fallback).

## Before 0.1.0 (unreleased but shipped incrementally)

- Standalone native window (winit + softbuffer + ab_glyph) drawing the alacritty grid.
- Tab = session = pane@host; local PTY, tmux, ssh, and harness e2ee tunnel transports.
- Full SGR (bold/dim/underline/strikethrough/inverse), OSC 52 clipboard, bracketed paste, mouse
  text selection, click-to-move shell cursor, Cmd/Ctrl+click URL/path open.
- Scrollback + search + copy mode, word/line selection, DECSCUSR cursor shapes, live OSC title sync.
- Command palette, session palette, fleet status + fleet-wide search, peek, broadcast (targeted),
  copy/export scrollback, undo close, mute tab, rename tab, next-busy-tab, move tab.
- Session/tab/scrollback/geometry persistence across restarts, config + theme, font zoom.
