# Changelog

All notable changes to harness-terminal. This project is under rapid, pre-1.0 development;
entries record user-visible and architectural changes since the last tagged milestone.

## Unreleased / 0.1.0 (in progress)

### Added
- **Close-quiet-tabs (`prefix+C`)** — close every live, backgrounded, unprotected tab that's been
  sitting silent past the quiet threshold (the ones the triage marks `⌛` and `prefix+z` jumps to)
  in one go, skipping the active tab and pinned ones. A fleet-cleanup gesture for when a batch of
  agent runs has finished and parked: instead of stepping each one and hitting close, sweep them
  all. Mirrors how each tab is restored on resume; flashes how many were closed.
- **Interrupt (prefix+!)** — send Ctrl-C to the active session so a diver can stop a runaway agent run
  without dropping into its raw terminal. Works over every transport; bytes ride the same
  `Session::write` path (buffered type-ahead if the pane is momentarily down). Also in the command
  palette. Complements `destroy` (kill the whole pane) as the lighter, non-destructive stop.
- **Fleet grid (prefix+e)** — a war-room view of the whole fleet: every session's live tail drawn
  in tiled panes that update each frame, so a diver sees what all agents are doing at once instead
  of hopping tab to tab. Up/Down/1-9 focus a tile, Enter dives into it; per-tile `@host` headers
  and the active tile's white border keep orientation. Also reachable from the command palette.
- **Reconnect-all-down (`prefix+T`)** — force a connect attempt on EVERY down remote pane at once,
  bypassing each transport's backoff. When several hosts drop together (a blip, a reboot), one key
  re-runs the connect path fleet-wide instead of tab-hopping and pressing `R` per pane; flashes how
  many were reached vs. still down.
- **Pane-recovery signal (`↻`)** — the moment a down (disconnected) remote pane comes back alive,
  its tab shows a short-lived `↻` badge (and the fleet-grid tile too) and — when backgrounded and
  unmuted — one "N sessions reconnected" notification fires. A host that silently reappears is now
  announced instead of blinked past. Honors per-tab mute and global Do-Not-Disturb.
- **Engine install hints in the new-session picker** — each engine row now marks whether its CLI
  is actually on PATH (`✗` in red for absent), so a diver doesn't pick a framework that isn't
  installed and fail on spawn. A pure `which`-style PATH scan (`engines::is_installed`), no subprocess.
- **Do-Not-Disturb (`prefix+M`)** — flip a fleet-wide switch that swallows every OS notification
  (backgrounded-busy nag and terminal-bell popup) at the source. In-bar `!N`/🔔 badges still render,
  so a diver who mutes popups can see later that something rang; a `🔕` chip shows in the triage while
  on. Complements per-tab `mute`, which silences one pane.
- **Next-host jump (`prefix+H`)** — page the fleet by machine: jump to the first tab of the next
  distinct host after the active one, wrapping, instead of stepping through every pane. For a fleet
  spread across several boxes this turns N panes into N hosts of stops. Also in the command palette.
- **Quiet ("awaiting-you") signal** — tracks when each tab last produced output and counts a live,
  backgrounded, unprotected tab sitting silent past a threshold as quiet (likely done / parked
  waiting on your input). Shown as a `⌛N` fleet-triage count and a `prefix+z` jump to the next
  quiet tab, with a `silence` age row in `prefix+i` info. Threshold via `quiet_after_secs`
  config (default 120s). Complements busy: `!M` = producing, `⌛N` = finished/stalled.
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
