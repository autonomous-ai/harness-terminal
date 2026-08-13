# Changelog

All notable changes to harness-terminal. This project is under rapid, pre-1.0 development;
entries record user-visible and architectural changes since the last tagged milestone.

## Unreleased / 0.1.0 (in progress)

### Fixed
- **A `[keybindings]` remap can no longer be silently shadowed by another action's default.** The
  dispatch table was built by reversing a sorted action-name map, so a remap like `broadcast → "x"`
  would quietly lose to `close_tab`'s default `x` (precedence was alphabetical, not "the user's
  explicit choice wins"). Resolution now inserts defaults first and explicit remaps last, so your
  `[keybindings]` override always wins a shared key; the digits/Tab stays fixed regardless.
  Covered by new `resolve_inverted` unit tests.
- **Mouse-drag state no longer sticks after the pointer leaves the window.** If you pressed inside
  a mouse-mode app (vim/tmux/htop), dragged out, and released *outside* the window, no release
  event arrived, so `mouse_left_down` stayed true and the app kept receiving drag-motion (code 32)
  instead of buttonless motion. Leaving the window or losing focus now clears the held state, so a
  mouse app returns to ordinary pointer motion — matching how a real terminal behaves.
- **A stray trailing `/` after a remote host no longer breaks the connection.** If you typed or
  pasted `build.example.com/` (or `10.0.0.4:18473/`) into the Remote Attach overlay, the slash was
  kept as part of the host, so the daemon was contacted at a dead name and the connect failed. A
  trailing slash with no session name is now stripped, so the fallback reaches the real host (and a
  true `host/session` attach is still detected as before). Covered by the `parse_remote_attach`
  unit tests, including the new slash/whitespace edge cases.
- **Tab order now persists across restarts.** Re-arranging the fleet with `Ctrl+H {`/`}` or by
  dragging a tab reordered the bar for the session but the new order was never saved, so on relaunch
  your arranged fleet (sorted by machine or priority) silently snapped back to the original order.
  Both reorder paths now persist `tabs.json`, so the order you set is the order you reopen.
- **A fleet-search jump now surfaces the matching native window.** In `native_tabs` mode, jumping
  to a hit from fleet search (`prefix+h` / `Cmd+Shift+F`) switched the in-app active session but
  never surfaced the matching `NSWindow`, so your focus stayed on the old tab while the content you
  asked for was in a hidden window. The jump now routes through `set_active` (the same path as
  `Cmd+1-9` / the jump palette), so the target tab's window comes to front and gains focus.
- **Every tab's `@host` suffix is now tinted by its machine's color, not just the active pill.**
  Previously the tab bar painted the whole inactive-tab label (including `@host`) in the engine's
  accent, so a session on `build05` and one on `edge1` of the *same engine* scanned identically.
  Now each pane-backed tab's `@host` is drawn in the stable per-host color (the same `host_color`
  the active pill already uses), so a multi-server fleet groups visually by machine across the whole
  bar — consistent with the peek list, which already tinted hosts. Local PTY tabs have no host and
  are unchanged, as is the active pill's look. Pure rendering change; idle CPU unaffected.
- **War-room and peek now show the same lingering unread count as the tab bar.** Previously the
  fleet grid (`Ctrl+H e`) and peek (`Ctrl+H y`) only surfaced an agent's `!N` while it was producing
  output *that very frame*, so a settled agent with output you hadn't seen vanished from the triage
  lists (its `!N` lived only in the tab bar). Both now use the cumulative `unread` count, so a war-
  room scan (and the peek list) expose every agent with something to read, matching the tab bar.
  The live "producing now" signal stays on the grid's amber border accent; quiet tiles that also
  have unread show `!N` first (the actionable "go look" signal), with the idle duration still in the
  peek row and hover tooltip. Idle CPU is unaffected.
- **The `!N` unread badge now actually means "new lines since you last looked."** Previously the
  count was a per-frame delta gated on the tab producing output *that very frame*, so a burst that
  settled (an agent that wrote 43 lines and paused) flashed `!43` for a split second and then
  vanished even if you never glanced at the tab. It's now a cumulative count in a dedicated `unread`
  slot that lingers until you look at the tab (or `prefix+I` mark-read) — the documented behavior.
  Muted tabs still show `M` and accumulate nothing, the live spinner is unchanged, and idle CPU is
  unaffected (unread only grows with actual output). The per-frame busy flag (`grew_delta`) that
  drives the peeks/fleet/palette is untouched.
- **A new native tab now takes focus.** Opening a session in `native_tabs` mode (Cmd+T/N, New
  Session, Remote Attach, duplicate, reopen) created a window spliced into the tab group but never
  surfaced it, so the new agent sat hidden behind the old tab until you clicked or tab-switched. Now
  the just-created tab becomes the visible, focused tab (iTerm2 behavior). Startup/restore still
  honors the tab you left active and never steals focus. Symmetrically, closing the active native
  tab (Cmd+W / traffic light) now hands keyboard focus to the next tab instead of leaving it on the
  closed window.
- **Typing while scrolled up now jumps to the live view and reaches the shell.** Previously, once
  you scrolled up into scrollback, any key that wasn't Page/Arrow/Escape was silently swallowed —
  so a command typed right after scrolling up vanished. Now any typing key (letters, Enter, Space,
  Left/Right arrows, Backspace) snaps back to the bottom and forwards to the session, matching
  Terminal.app / iTerm2; PageUp/PageDown/Up/Down still scroll and Esc still exits scroll.
- **Correct the help row for font zoom reset.** The keys overlay claimed `Cmd+0` resets zoom, but
  `Cmd+0` is the native "jump to last tab" binding (it's handled before the zoom path, so only
  `Ctrl+0` actually resets zoom). The row now reads `Ctrl+0` reset / `Cmd+0` = last tab to match
  reality; `Ctrl/Cmd+= / -` still zoom as before.
- **Fleet-grid / peek close no longer strands a native window.** Closing a session with the
  war-room's `x` (or peek's `x`) now also drops the matching native window host. Previously, in
  `native_tabs` mode, those close paths removed the tab but left its `NSWindow` lingering as a tab
  with nothing behind it — the prefix close (and Ctx/CloseTab) already handled hosts, but the grid
  and peek closures didn't.
- **README documents the native Cmd shortcuts.** Replaced the truncated native-shortcuts paragraph
  with a full table (Cmd+T/N/W/Q, Cmd+Shift+[/], Cmd+1-9, Cmd+Shift+P/F/T/D/R, Cmd+Shift+U/M, Ctrl+Tab).
- **Find `Shift+Tab` now goes to the previous match.** In the scrollback search (`prefix+f`),
  `Shift+Tab` previously advanced (same as Tab); it now steps backward, matching `Shift+Enter` and
  the other overlays' Shift+Tab-up convention, and the header hint stays truthful.
- **Fleet-grid marks are scoped to one session.** `Space` marks persisted after closing the grid
  (Esc/Enter), so a stale mark set could later seed an unintended `b`/`C`/`R` — and especially `X`,
  a bulk close — defeating the "marks required so a stray press is never destructive" guard. Marks
  now clear on grid close, matching the broadcast overlay's fresh-reset behavior.
### Added
- **Right-click fleet actions on any tab.** The context menu now offers **Interrupt (Ctrl-C)**,
  **Reconnect** (only when that pane is actually down), **Duplicate Session**, and **Mute / Unmute**
  alongside copy/paste — so a diver can stop, revive, or fork an agent without reaching for the
  prefix chord. Each routes through the same tested method as its prefix binding, and the menu
  stays non-destructive (interrupt/duplicate only ever target the tab you clicked).
- **`prefix+G` jumps straight to the top of the active tab's scrollback.** Until now only page-up
  (`g`) and jump-to-bottom (`b`) existed, so reading a long agent run from its start meant paging
  through every screenful. `prefix+G` (and the palette action) land you at the oldest line and pin
  the view in history — the exact complement to `prefix+b`, mirroring how you'd open a log from the
  top. Documented in the `?` help and README.
- **Fleet search rows now show the matched line centered on the hit.** A long agent line whose
  match sits mid-line used to display only the line's head, so the reason it matched was often
  off-screen. Each row now shows a `…`-clipped window around the match (short lines pass through
  whole), so scanning `Cmd+Shift+F` results reads like grep with context. The snippet geometry is
  a pure `focus_snippet` helper, unit-tested.
- **Find-in-session remembers your case / whole-word toggles across restarts** (iTerm2-style
  state memory, the same way query history is remembered). Toggle `c` / `w` in the find bar once
  and the `[Aa]` / `[whole-word]` flags come back on next launch; no re-toggling each session.
  Persisted to its own file with a round-trip unit test.
- **A scrollback scrollbar with a live thumb now sits on the grid's right edge.** Whenever a
  session has more history than the viewport, a translucent track and a thumb show your position
  in the log (at live bottom the thumb hugs the bottom; scrolled to the top it hugs the top) —
  the classic iTerm2/native-terminal cue, so a long agent log stays navigable without reading the
  `▾ N%` status number. The thumb's height tracks the visible fraction of the total history, never
  collapses away, and is painted translucently so text underneath stays legible. Pure rendering
  (geometry is unit-tested); idle CPU is unaffected.
- **Find-in-session now has case and whole-word toggles (iTerm2-style).** In the find bar over an
  active pane, press **`c`** to toggle case-sensitive matching and **`w`** to toggle whole-word
  matching while the query is empty; both default off. Active flags are shown in the bar
  (`[Aa]`, `[whole-word]`) along with a `c case · w whole-word` hint, and the highlighted
  results update live. Covered by the new `all_matches_ex` render unit test.
- **Find remembers your recent searches.** The find bar now keeps an MRU of queries you actually
  ran (deduped, capped at 16, persisted across restarts like broadcast history). Press **Up** with
  an empty query to recall the most recent search — iTerm2-style search memory, so a regex or a
  phrase you used to hunt a bug earlier is one keystroke away next time.
- **Cmd+F now opens find-in-session (and Cmd+G / Cmd+Shift+G step the results).** Previously the
  universal iTerm/editor find key did nothing special here — search lived only behind the prefix
  chord. Cmd+F now opens the find bar over the active pane's scrollback (same fresh-state init as
  the prefix `search`), and the `Find`/`Next`/`Prev` routes are documented in the `?` help overlay.
- **The status line now shows a fleet-wide health summary at a glance.** When several agents are
  open, a glance at the bottom bar now reads `N ⚠ down` and `N ⚡ busy` (the urgent counts a diver
  needs without opening peek), and a healthy fleet shows `✓ fleet ok`. Derived purely from the
  already-computed per-tab live signals, so idle CPU is unaffected; single-session windows show the
  same line as before.
- **Cmd+G / Cmd+Shift+G jump between search matches from anywhere.** Once you've run a find
  (`Cmd+F` / `prefix+f`), Cmd+G steps forward and Cmd+Shift+G steps backward through the hits even
  after the find bar closes — the iTerm2 muscle memory for reviewing a search without reopening it.
  Closing find now keeps the matches highlighted and the last query alive (they clear on the next
  `Cmd+F`), so the "N of M" cursor and the scroll-back-to-match behavior keep working in the plain
  grid. `<key>` route via the same pure, unit-tested `cmd_shortcut` table as the other Cmd shortcuts.
- **Mouse reporting to terminals that request it.** TUIs like vim, tmux, and htop that enable
  mouse mode (`ESC [?1000h` / `1002` / `1003`) now receive proper SGR mouse reports
  (`ESC [ < ... M`) for clicks, wheel, drag, and motion — the same sequence real terminals send,
  so mouse-driven editors and pager UIs work like they do in iTerm2. Right-click stays reserved for
  the app's context menu (hold Ctrl+Right-click to hand it through); motion is throttled per grid
  cell so fast sweeps can't flood the PTY; and the whole path is strictly gated on the focused
  session having requested mouse mode, so normal selection/copy/scroll is byte-for-byte unchanged
  for non-mouse programs. The SGR encoders are unit-tested byte-for-byte against the xterm 1006
  spec.
- **Fleet overview auto-refreshes while open.** `Ctrl+H s` (the remote-fleet list) now re-polls
  the daemon every few seconds in the background, so a host that comes back shows up without
  manually pressing `s`. Still non-blocking (falls back to the cached snapshot) and stops the
  instant the overlay closes.
- **Native Cmd+Shift+Y opens peek.** Seeing every agent's tail at once (`peek`) is now one key
  away. Plain Cmd+Y stays free. Routes via the pure, unit-tested `cmd_shortcut`.
- **Native Cmd+Shift+K stops the whole fleet.** Sends Ctrl-C to every non-muted session — one
  reflex to halt every runaway agent at once (the fleet-wide cousin of `Cmd+.`). Non-destructive
  (a Ctrl-C only stops the current command) and respects muted tabs. Plain Cmd+K stays free. Routes
  via the pure, unit-tested `cmd_shortcut`.
- **Session-jump palette rows carry triage status.** `Ctrl+H /` now shows each agent's status next
  to its identity: `○` down, `!` busy (producing now), `⌛` quiet (done / waiting on you), plus the
  existing `🔒`/`M` pin/mute — so you can pick the agent that needs you straight from the jump list.
- **Native Cmd+Shift+J jumps to the next quiet (awaiting-you) agent.** When several agents run at
  once, the fastest thing to do is usually find the one waiting on your input — that's `next_quiet`,
  now one Cmd+Shift+J away. Plain Cmd+J stays free. Routes via the pure, unit-tested `cmd_shortcut`.
- **Native Cmd+. interrupts the active session.** The macOS universal "stop the running thing" key
  (the reflex that halts an infinite loop in Xcode/a Python script) now sends Ctrl-C to the active
  agent — so a runaway run is stopped with native muscle memory, no prefix chord. Routes via the
  pure, unit-tested `cmd_shortcut`.
- **Peek and the fleet grid gain a per-row `p` pin.** `Ctrl+H y` and `Ctrl+H e` now toggle pin
  (protect-from-close) on the selected pane/tile with `p`, so an important agent can be shielded
  from any bulk close (`X`, `prefix+close_quiet`) while triaging a fleet — the per-row sibling of
  prefix pin.
- **Fleet grid gains a per-tile `m` mute.** `Ctrl+H e` (the war-room grid) now toggles mute on the
  selected tile with `m` — the per-tile sibling of peek's `m` and prefix mute — so a noisy
  backgrounded agent is silenced straight from the grid without a drill-in. Consistent with the
  triage's `r` reconnect and `x` close.
- **Native Cmd+Shift+S exports the active tab's scrollback to a .log file.** The system shortcut for
  prefix+export_scrollback: hand an agent's session off to a file from anywhere. Plain Cmd+S stays
  with the shell. Routes via the pure, unit-tested `cmd_shortcut`.
- **Native Cmd+Shift+C copies the active tab's whole scrollback.** The system shortcut for
  prefix+copy_scrollback: grab an agent's full session log to the clipboard from anywhere. Plain
  Cmd+C stays the normal copy-selection key. Routes via the pure, unit-tested `cmd_shortcut`.
- **Native Cmd+Shift+I opens the active tab's info.** The system shortcut for prefix+i: shows the
  session's engine kind, host, and live task from anywhere without the prefix chord. Plain Cmd+I
  stays free for the shell. Routes via the pure, unit-tested `cmd_shortcut`.
- **Native Cmd+Shift+U / Cmd+Shift+M toggle pin / mute.** The system shortcut for prefix+pin and
  prefix+mute: `Cmd+Shift+U` shields the active tab from close, `Cmd+Shift+M` silences a noisy
  agent's badge + OS ping — both reachable from anywhere without the prefix chord. Plain Cmd+U/M
  stay free for the shell. Routes via the pure, unit-tested `cmd_shortcut`.
- **Broadcast target list navigates with Tab/Shift+Tab.** `prefix+b` walks the focused target with
  Tab (Shift+Tab up), so the row is selected without the arrows; Shift+arrows still recall history
  as before.
- **Session-jump palette accepts Tab/Shift+Tab.** `prefix+/` moves the selection down/up through the
  filtered sessions with Tab (Shift+Tab up), so keyboard-only divers can browse without the arrows —
  matching the peek/command-palette conventions.
- **Session-jump palette pages with PgUp/PgDn.** `prefix+/` (jump to any session) now moves the
  selection a full 12-row window per keypress, so a large fleet is browsed without arrowing one row
  at a time — mirroring the other list overlays.
- **Scrollback exports carry a self-describing header.** `prefix+w` (export) now writes a header line
  with `session: engine@host`, transport kind, the live task title, and a human-readable export time
  (falling back to the epoch if `date` is unavailable), so a handed-off agent log says what it is.
- **Peek triage pages with PgUp/PgDn.** The fleet triage (`prefix+s`) now jumps a full 10-row window
  per keypress, so a large fleet is browsed without arrowing one row at a time — mirroring the
  fleet/palette/broadcast list overlays. Header hint updated.
- **Peek rows tint the host by machine.** The fleet triage (`prefix+s`) now draws each row's host
  in its per-host color (local → `local`), so a multi-host fleet scans by machine at a glance while
  the rest of the row keeps its status color. The selected row stays uniform white for a clean sheet.
- **Peek rows carry the full shield/activity badges.** The fleet triage (`prefix+s`) now shows each
  row's busy `!N`, muted `M`, pinned `🔒`, just-reconnected `↻`, and staged-input `⏳N` badges — the
  same Chrome the grid and tab bar use — so a diver sees a row's mute/pin/protection state at a
  glance instead of drilling in. The selected row stays un-flagged busy (you're reading it).
- **Help + fleet-grid hint document the `n` triage hop.** The `prefix+?` help and the fleet grid's
  header now advertise `n` (next/prev trouble tile), so the recently-added walk matches the docs.
- **Broadcast list pages with PgUp/PgDn.** The broadcast (`prefix+b`) target list now jumps a full
  20-row window per keypress, so fanning a line out to a large fleet doesn't require arrowing past
  every session — matching the fleet/palette list overlays. Header hint updated.
- **Fleet grid `n` hops to the next trouble tile.** The war-room sibling of peek's `n`: `n` in the
  grid jumps the selection to the next DOWN pane (else the next busy one), wrapping around, so a
  large fleet's trouble spots are triaged pane-to-pane instead of one tile at a time. A healthy
  fleet flashes a legible no-op. Precedence/wrap logic is unit-tested.
- **Fleet grid `N` hops backward to the previous trouble tile.** Capital `N` walks the same
  down-then-busy priority in the opposite direction, so a war-room diver can sweep both ways through
  a fleet's trouble spots without arrowing tile-by-tile. Backward wrap/precedence is unit-tested.
- **Fleet grid is color-coded by session status.** Each tile's border and header are tinted so the
  war-room is scannable at a glance: down = red, busy/output = amber, quiet/awaiting-you = blue,
  just-reconnected = green, idle local = neutral. The precedence (down > busy > quiet) is unit-tested.
- **Info panel shows fleet context for the active machine.** `session_info` now reports how many
  sessions are open on the active session's host and how many are down, turning the per-tab details
  into a one-host micro-overview (for `build02` a diver sees the whole machine at a glance).
- **Reconnect one tile straight from the fleet grid (`r`).** The per-tile sibling of bulk `R`
  (which heals every marked / all-down tile), mirrors peek's `r`: select a down tile and press `r`
  to heal just that pane without touching the rest of the fleet.
- **Mute a pane straight from peek (`m`).** The peek triage (`prefix+s`) now toggles mute on the
  selected pane, so a noisy backgrounded agent is silenced from the list without a drill-in — the
  per-row sibling of prefix mute.
- **Close a pane straight from peek (`x`).** The peek triage (`prefix+s`) now closes the selected
  pane in place — the per-row sibling of the fleet grid's `x` — honouring the pin guard and stashing
  an undo spec, so pruning a dead/finished agent is one key without a drill-in.
- **Peek `/` filter composes multiple keywords (AND).** Space-separated tokens must all match —
  `build05 down`, `claude quiet` — so a diver can ask "which agents are down on this host?" in one
  query, alongside the existing `up`/`down`/`busy`/`quiet` keywords and substring matching.
- **Peek `/` filter understands `busy` and `quiet`.** On top of `up`/`down` and substring matching,
  the peek triage (`prefix+s`) now narrows to agents that just produced output (`busy`) or have been
  parked past the quiet threshold (`quiet`) — so a diver can go straight to "who needs attention".
- **Help overlay now lists the in-overlay shortcuts** (peek `r`/`n`, broadcast `Space`/`⇧Space`,
  fleet-grid marking) so the keybindings you can use inside the triage overlays are discoverable.
- **Broadcast `Shift+Space` selects/clears every target at once.** In the broadcast overlay,
  `Shift+Space` toggles the whole selection between all-on and all-off — a one-key reset after
  hand-curating a set, or a fast fan-out to every session (the common open state).
- **Reconnect straight from peek triage (`r`).** In the peek overlay (`prefix+s`), `r` reconnects
  the selected pane immediately — no drill-in needed — and the row stays selected so a down `○` flips
  to live when the transport comes back. Aimed at the common "one of my hosts dropped" flow.
### Fixed
- **Native mode persists which window you last focused.** Clicking/focusing a different native tab
  window updates the active session but wasn't saved, so a relaunch could reopen on a stale earlier
  tab. `focus_host` now saves the focused session like `set_active` does.
- **No double notification when a remote host reconnects.** A just-recovered pane (down→alive) was
  getting both a `recover` toast and, in the same frame, a `busy` toast the moment its reconnect
  produced output. `activity_flags` now skips the redundant `busy` nudge while a tab is still in its
  recovery window — the `recover` toast already covers that reconnect, so a diver gets exactly one.
- **Docs: README reflects the fleet features.** The key table now names peek `/` filtering, the
  fleet grid's full action set (`C`/`x`/`X`), per-host broadcast in the hosts drill, and the
  down/reconnected OS notifications and `↓` tab marker, so onboarding matches the implementation.
- **Native-mode Cmd+W clamps its focused window index.** The close path indexes
  `hosts[active_host]` to honor the pin guard; if a window was dropped without the index being
  re-derived, an unclamped index could panic. It now clamps `active_host` into range before use,
  matching the other guards scattered across native mode.
- **Fleet-grid bulk actions share one tested target rule.** `R`/`C` now resolve "marked, else
  fallback" through a single pure `grid_targets` helper (marks win; unmuted/down mask as fallback;
  close requires marks), unit-tested once instead of copy-pasted. Triage jump no-op messages are
  now phrased `no other busy/quiet/down tab` for accuracy.
- **Bulk-close marked panes from the fleet grid.** `X` prunes every marked tile at once (high→low,
  honoring the pin guard + per-tab undo) — the bulk sibling of the single-tile `x`. Unlike
  `b`/`C`/`R` it requires explicit marks (no "close everything" fallback), so a stray press can
  never nuke the fleet; a miss flashes a reminder to Space-mark first.
- **Close a pane straight from the fleet grid.** `x` prunes the selected tile (honoring the pin
  guard + undo, exactly like the tab bar's ×), so a war-room can clear dead/finished panes without
  leaving the overview — the counterpart to `b`/`C`/`R` marked bulk actions.
- **Fleet interrupt: stop the whole batch with one key.** In the command palette,
  `send Ctrl-C to every session` fans an interrupt fleet-wide; in the fleet grid, `C` sends Ctrl-C
  to every marked tile (falling back to all non-muted when nothing is marked) — the "stop the
  looping agents" sibling of `b` broadcast and `R` reconnect. Works with the buf into a down pane
  on reconnect; muted tabs are skipped by the fallback so a deliberately-quiet pane is never hit.
- **The peek overlay filters the fleet.** Press `/` in the peek, then type (host, engine, name, or
  `down`/`up`) to narrow the triage to matching sessions; `n`, arrows and Enter all operate on the
  filtered list, `Esc` clears back to all. Escaping a huge multi-machine fleet to "just the dead
  panes on build05" is now a few keys.
- **Broadcast to one whole host from the hosts drill-in.** In the hosts overview (`prefix+.` →
  `→`), `b` opens the broadcast overlay pre-scoped to every session on that machine — the fanned-out
  sibling of `r` (reconnect this host). A "redeploy / git pull / restart every agent on build05"
  command now needs no hand-marking of tiles.
- **The fleet tab bar marks panes that are currently down.** Each down (non-PTY) pane's tab now
  carries a persistent `↓` glyph — the in-bar counterpart to the transient `↻` recovery badge — so
  a dead pane reads at a glance instead of only surfacing in the status count or peek.
- **Mute/pin toggles are panic-proofed against freshly-spawned tabs.** `prefix+m` / `prefix+a` now
  read and write the per-tab mute/pin state through the same guarded `.get()` pattern as every
  other tab-parallel vector, so toggling right after a `duplicate_session` (which can leave the
  mute list a slot short) can never index out of bounds and crash the app. The Info-overlay idle
  read is guarded the same way.
- **Fleet notifications now fire when a pane goes down.** The one missing edge — the most important
  one — is covered: when a backgrounded (unmuted) remote pane that was alive dies, a single
  coalesced OS notification reads `name@host · went down / … disappeared.` New tabs and panes
  restored already-dead are exempt, so a fresh app launch never nags; PTYs and the pane you're
  watching never fire. The counterpart to the existing `reconnected` alert.
- **The peek preview no longer re-walks the whole scrollback every frame.** It now reads only the
  newest handful of history rows (bounded, cheap) instead of capturing the full scrollback each
  render — a real "fast" win when the peek is open over a long-running agent log.
- **Page the fleet grid by a full row.** `PgDn`/`PgUp` jump the tile selection by an entire row of
  sessions at a time (column count matches the actual layout), so covering a large multi-host fleet
  no longer takes one keypress per tile.
- **Reconnect marked tiles straight from the fleet grid.** Mark tiles with Space then hit `R` to
  force-reconnect every marked session at once (falling back to all-down when nothing is marked) —
  the healing counterpart to `b` broadcast, so you can bulk-recover several machines in the war-room.
- **The peek list shows how long a quiet tab has been waiting.** A quiet (awaiting-you) row now
  reads `⌛3m` (or `5s`/`2h`/`1d`), matching the fleet grid — so triaging "which agents are parked
  longest" works from the peek too, not just the war-room grid.
- **The fleet grid shows how long a quiet tab has been waiting.** A `⌛` tile now reads `⌛3m`
  (or `5s`/`2h`/`1d`), so the war-room tells you at a glance which agents have been parked the
  longest — not just that one is idle.
- **Copy-mode search says when there's no match.** `n`/`N` (and Enter after `/`) with a query that
  hits nothing now flash `copy: no match /…` instead of silently doing nothing, so a skipped jump is
  explained rather than leaving the diver wondering.
- **The always-on status line reports panes that just reconnected.** Alongside `↓N down`, `!M busy`,
  `⏳N queued` and `⌛N quiet`, the triage now shows `↻N` for panes that came back online in the last
  few seconds — matching the peek header, so a fleet-wide burst of recovery is visible on the status
  line without opening the peek.
- **The broadcast overlay shows where you are in history while recalling.** Shift+Up/Shift+Down
  now annotate the staged line with a `⇧↑ i/n` position (0 = newest), so before you hit Enter you
  can see which prior fan-out you recalled instead of only a bare echoed line.
- **The peek header counts panes that just reconnected too.** Alongside the down count it now reads
  `N reconnected` while the transient `↻` badge is showing, so the header tracks the fleet's recent
  recovery, not just what's still down.
- **Reconnect a whole host from its drill-in.** In the hosts drill-in (`prefix+.` → `→`), `r`
  force-reconnects every down pane on that host at once (the per-machine cousin of reconnect-all),
  and the header names the shortcut.
- **Remote-Attach lets you Tab through remembered hosts.** The overlay now cycles the host field
  through your recent hosts (MRU) with `Tab` / `Shift+Tab` (forward/back), and the hint names the
  last one — so reconnecting to a machine you used before no longer requires retyping the address.
- **Fleet-search results label renamed tabs with their engine.** Matches now read
  `name (engine)@host` (and drop the trailing `@` for local tabs), matching the peek list, so a
  cross-session search stays attributable after renaming.
- **Fixed: `b` in the fleet grid actually broadcasts the marked tiles.** It was dead code — a stray
  branch toggled a tile's mark on any letter, so `b` never fired. Now, after marking tiles with
  Space, `b` opens the broadcast overlay scoped to exactly those marked sessions.
- **The broadcast overlay shows which targets are down.** Each down row is flagged `○ down (reason)`
  and the header counts `⏳N queued` among the selected targets, so a fan-out that will only land on
  reconnect isn't mistaken for one every host received instantly.
- **The hosts drill-in names why a pane is down.** Drilling into a host from the hosts overview
  (`prefix+.` → `→`) now shows each down session's reconnect reason beside it, matching the overview.
- **The native status bar names which host is down.** When only one or two panes are down, the
  fleet triage reads `↓1 build02` (or `↓2 build02, build05`) instead of a bare count, so a diver
  knows where to look; with more down it falls back to the count.
- **The peek header shows the whole fleet's health.** It now reads `N down` (red) when panes are
  down or `fleet healthy` otherwise, so the peek triage reflects all sessions, not just the row
  you're looking at.
- **Fleet notifications name the machine.** Bell, busy, and reconnected OS notifications now read
  `name@host` (e.g. `claude@build02 · reconnected`, `codex@build05 produced new output`), so in a
  multi-host fleet every ping is actionable instead of ambiguous.
- **The palette gains scrollback-review actions.** `page up` and `scroll to bottom` are now
  palette actions, so skimming a long agent log starts without the prefix chord.
- **The command palette gains the triage jumps.** `next busy`, `next quiet`, `mute`/`unmute`, and
  `rename` are now first-class palette actions, so a diver who forgot the prefix chords can find
  them by typing in `Ctrl+H /`.
- **`n` inside peek jumps to the next down pane.** With the peek list open, `n` wraps the
  selection to the next still-down remote pane and scrolls it into view — triage without leaving
  the picker.
- **The hosts overview names why a host is down.** A fully-down machine (`prefix+.`) now shows its
  first pane's reconnect reason (`○ build02 · down (refused)`) instead of a bare "down".
- **Peek rows show the engine for renamed tabs.** A custom-named session now reads `name (engine)`
  in the peek list, so a multi-engine fleet stays legible after renaming.
- **The native status bar is optional.** A new `native_status_bar` config key (default `true`)
  turns the bottom status strip on/off in native mode; `false` makes the grid full-bleed. Outside
  native mode the key is ignored and the in-app status line is unchanged.
- **Native mode gains an iTerm2-style status bar.** Each native tab window now keeps a slim bottom
  strip — this window's session identity + ✓/○ health (with the reconnect reason when down) on the
  left, and the whole-fleet triage (`↓N` down, `!M` busy, `⏳N` queued, `⌛N` quiet, `🔕` DND) on the
  right — so a multi-machine diver still sees machines going dark without leaving the OS tab bar.
  One terminal row is reserved uniformly across windows (no resize churn when switching tabs).
- **`prefix+Q` (next down) names the failure.** The jump toast now appends the reconnect reason
  (`down — claude@build02 (refused)`) so a diver knows whether a pane is mid-reconnect or hard-down
  without tab-hopping.
- **Down panes are tagged in the peek list.** Rows for down remote panes now render red with a `○`
  and their reconnect reason, so the diver sees the nature of every outage at a glance alongside the
  new "land on the first down pane" behavior.
- **The peek list opens on the first pane that's down.** Previously `prefix+y` (peek) always landed
  on the top of the fleet, so a diver had to page back through live hosts to find the one that
  dropped. It now selects and scrolls to the first still-down remote pane (if any), so trouble is
  front and center the moment the list opens.
- **Copying the whole scrollback now confirms it worked.** `prefix+d` copied silently — no flash,
  unlike every other copy action. It now flashes `copied N chars of scrollback` so a diver dumping a
  long agent log knows the paste landed without second-guessing the clipboard.
- **`reconnect-all` says which hosts still won't come back.** `prefix+T` / the palette's "force
  reconnect ALL down panes" used to report only a count (`2 reached, 3 still down`). It now names
  the still-down hosts and their failure reason (`build02: refused`), clipped so a large fleet's
  toast stays short — so bringing a whole fleet back tells you which machines to go investigate.
- **Exported agent logs are tagged with the host.** `prefix+w` already wrote a timestamped
  `<name>-<stamp>.log`; for a pane-backed session it now writes `<name>-<host>-<stamp>.log`, so a
  dump pulled from a multi-machine fleet is identifiable at a glance (local pty exports stay bare).
- **A copied fleet summary now says why machines are down.** `prefix+E` / the fleet copy has always
  marked each down tab with `○`; it now appends the reconnect reason (`⚠ tunnel connect 10.0.0.4:
  refused`), so a pasted health handoff reads as "which host is dark and why" instead of a bare
  unreachable glyph.
- **The session palette ranks the best match on top.** Typing a query now floats the strongest hit
  to the top of a many-tab fleet: a prefix match in the engine/name/host outranks a loose fuzzy
  subsequence, and equally-strong hits break ties by engine recency then tab order. Previously
  results came back in raw open order, so "clau" might not surface the Claude tab first.
- **The fleet-grid war-room shows *why* a machine is down.** A down tile now carries the reconnect
  reason (`tunnel connect 10.0.0.4: refused`) right after its `○`, clipped to the tile's own width
  so it never spills onto a neighbor — so the at-a-glance view of every agent tells you which host
  is dark AND what knocked it over without hovering each one.
- **Hovering a down tab says how it's recovering and why.** The tab tooltip now shows the live
  retry state (`reconnect 2 · retry in 5s`) and, when known, the last failure reason (`tunnel
  connect 10.0.0.4: refused`) beneath `○ down` — the roomier hover panel carries the reason the
  concise status line keeps out, so you see at a glance which remote agent is down and why.
- **Broadcast confirms the fan-out.** After `prefix+a`/`b`, a one-line flash says exactly how many
  targets got the command now (`broadcast to N`) and — when any target is down — how many are staged
  to flush on reconnect (`broadcast to N · M queued on reconnect`), so a command to a dead host is
  never assumed lost or silently dropped.
- **Down panes say *why* they're down.** `prefix+i` info now shows the last reconnect failure's
  reason (e.g. `tunnel connect 10.0.0.4: refused`), so a dropped agent reads as host-unreachable /
  auth-rejected / timeout instead of just "reconnect 3 · retry in 10s". The tab-bar/status line
  stays concise; the reason appears in the roomier info panel and clears on the next successful
  reconnect.

### Fixed
- **Pin now protects a native-mode tab from Cmd+W.** `Cmd+W` / the menu's Close Tab closed a pinned
  session in native window-tabs, even though the shield is a documented promise ("won't close until
  unpinned") honored everywhere else. Our close now refuses with the same 🔒 flash as the in-app `x`;
  closing via the OS traffic-light remains a deliberate, unblocked on-window gesture.
- **A configured `default_engine` now actually pins the new-session picker.** Previously any engine
  usage at all made recency override the config, so setting `default_engine = "codex"` but having once
  used `claude` kept preselected `claude`. An explicitly-set default (anything other than the built-in
  `claude`) now wins over recency; the un-set default keeps the usual "most recently used floats" and
  unknown ids still fall back safely to the first engine.
- **A `~` in the new-session working directory now actually expands.** The `dir:` field passed the
  path straight through, so typing `~/projects` spawned the session (and saved the working dir) with
  a literal `~/projects` that couldn't be resolved. `~` / `~/…` now expand to your home directory
  in both the new-session picker and a config `start_cwd`; absolute, relative, and `~user` paths are
  left untouched.
- **`prefix+l` (back to previous tab) no longer jumps to the wrong session after a close or
  reorder.** The "previous tab" index was only re-recorded on focus switches and never invalidated
  when tabs were closed or dragged into a new order, so after removing/reordering a tab the stored
  pointer silently named a *different* session and `prefix+l` landed you somewhere unexpected. The
  pointer is now cleared on every close and reorder (and re-recorded on the next focus switch), so
  flip-back always returns to the tab you were actually on.
- **Cmd+click opens URLs without a trailing sentence period.** A scheme URL that ended in
  punctuation (``Read https://…/foo.``) included the trailing period and opened a 404; the docs
  claimed it was stripped but the code kept it. A single trailing period is now trimmed, while an
  internal dot (`pkg.v2`) and a relative parent `..` are preserved.

- **Ctrl/Option-arrow now send word/paragraph-move sequences.** Ctrl+Left/Right in readline/bash
  previously fell back to a plain character move because the modifier was dropped from the arrow
  escape sequence. Arrows now honor the xterm modifier encoding (`ESC [ 1;5C/D` = Ctrl, `1;3` =
  Option, `1;7` = Ctrl+Option), so Ctrl+Left/Right jump by word and Ctrl+Up/Down by paragraph in
  readline as expected; bare arrows and scrollback-paging arrows are unchanged.

- **Home, End, forward-Delete, Insert, and F1-F12 now actually work in the shell.** These keys fell
  through normal-mode key forwarding into a generic `_ => {}` and were silently dropped, so Home/End
  and forward-delete did nothing in bash/readline/TUIs. They now forward the standard terminal escape
  sequences (`ESC [ H/F`, `ESC [ 3~`, `ESC [ 2~`, and the F-key sequences). Keys the app owns (arrows,
  Tab, PageUp/PageDown scrollback, Escape) are untouched.

- **Each native-tab window renders and sizes at its own display density.** A background tab on a
  different-scale screen (Retina laptop next to an external monitor) previously rendered with the
  focused window's cell metrics, giving it the wrong grid that re-flowed when you focused it. Every
  session window now derives its own font/grid size from its window's backing scale factor, so all
  windows look correct at once; single-display setups are unaffected.
- **Native-tab windows now rescale when you focus a tab on a different display.** Each session
  window can sit on a different-density screen (e.g. a Retina laptop next to an external monitor);
  switching focus to a tab on another display keeps terminal text at a sensible size by re-deriving
  the font/grid metrics from the focused window's backing scale factor instead of the first
  window's. Single-window mode is unaffected (all windows share one scale there).

- **Distinct remote agents on one host no longer share persisted state.** Scrollback, mute, and
  pin state were all keyed only on kind+host+engine, so attaching two different tmux sessions of
  the same engine on one host (`host/session-a` vs `host/session-b`) collapsed them into one
  identity: they shared a last-writer-wins scrollback file (both replayed the same pane on
  relaunch), and muting or pinning one silently applied to the other. The remote session name is
  now part of the identity for attach tabs across all three, so each `host/session` agent keeps
  its own history, mute, and pin; spawned panes (no attach) still map to their single
  `auton-<engine>` identity as before.

- **Moving or drag-reordering a tab no longer leaves fleet broadcast marks or the content-change
  detector on the wrong session.** The tab-move fix covered pin/mute/busy/badge state, but three
  more tab-parallel vectors were left behind: `broadcast_targets` (a `b` broadcast mark could fire
  on the wrong session after a move) and the per-session change-detection snapshots
  `detect_len`/`content_sig` (a spurious idle-wake / missed redraw). They now follow the session
  like the rest; closing a tab also drops all three consistently. A test proves every element type
  used by these vectors (bool/usize/u64/Instant/Option) stays aligned through both the swap and
  drag-reorder paths.

### Added
- **Next-* jumps say when there's nothing left to find.** `prefix+z` (next quiet), `prefix+o`
  (next busy), `prefix+Q` (next down), `prefix+P` (next pinned) and `prefix+H` (next host) now flash
  `no quiet tabs` / `no busy tabs` / `no down tabs` / `no other pinned tab` / `no other host`
  instead of silently doing nothing — so a no-op jump is explained, like the copy-mode no-match cue.
- **The hosts drill-in shows how long a quiet session has been waiting.** Drilling into a host
  (`Ctrl+H .` → `→`) now tags a quiet (awaiting-you) session `⌛3m` — matching the fleet grid and
  peek — so per-host triage tells you which agents on that machine are parked the longest.
- **Page the fleet list and host overview like the grid.** `PgUp`/`PgDn` step the selection by a
  10-row slice in the fleet (`prefix+s`) and hosts (`prefix+.`) overlays — including a host's
  drill-in session list — so long lists don't need one keypress per row.
- **The fleet grid is mouse-friendly now.** A left click selects the tile under the cursor (arrows
  can still refine), and a double-click dives straight into that session — the war-room finally
  responds to the pointer, not just keys. Hit-testing mirrors the renderer's exact tile layout and
  is unit-tested.
- **Host overview shows each machine's agent mix.** `prefix+.` now lists not just how many
  sessions are up per host but which agents are running there (`● build02 · live · 2 sessions ·
  claude×2, codex`), so a diver sees the whole fleet's engine spread at a glance. Backed by a pure,
  unit-tested `host_engine_breakdown` helper.

- **Drill-in shows what each agent is doing right now.** In the host-overview session list
  (`prefix+.` → `→`), the selected session's newest output line is previewed underneath it
  (`↳ …`), width-truncated, so you can pick a run by what it's streaming rather than by name alone
  — the same live tail the hover tooltip shows, now in the overlay.

- **Host overview drills into one machine's sessions.** On a host with several agent runs, `→`
  (or Enter on a single-session host) jumps past the host row into a sub-list of that machine's
  sessions — each labeled with its engine, tab number, name, and live title — so you can land on a
  specific agent on a remote box instead of always the first tab. `↑/↓` navigate, `Enter` opens,
  `←`/`Esc` back to the host list. Backed by a pure, unit-tested `session_indices_for_host` helper.

- **Copied fleet summary now carries each machine's agent mix too.** `prefix+c` (copy fleet
  summary) renders the same per-host status + agent list as the host overview, so a pasted report
  reads `● build02 · live · claude×2, codex` — not just a live/total tally. A shared pure
  `fleet_host_line` formatter keeps the on-screen and copied views identical.
- **Native macOS menu bar.** winit 0.30 ships no menu API, so the app previously had a bare
  menu-less bar. A real AppKit main menu is now installed at launch with the File, Tab, and Window
  menus: `Cmd+T` new tab, `Cmd+W` close tab, `Cmd+Q` quit, `Cmd+Shift+[` / `Cmd+Shift+]` previous /
  next tab, `Cmd+Shift+T` reopen last-closed tab, and `Cmd+Shift+P` command palette. Each item
  dispatches through a Rust-owned ObjC target into a queue the winit loop drains each frame, wired
  to the exact same handlers the keyboard shortcuts use; menu key equivalents consume their chords
  first, so nothing that worked before can double-fire or regress.

### Fixed
- **Moving a tab no longer strands its pin/mute/busy/badge state on the wrong session.** Both the
  prefix move (`Ctrl+H` move left/right) and drag-to-reorder swapped/relocated the tab entry alone,
  leaving the ten tab-parallel state vectors (`muted`, `pinned`, `seen_history`, `grew_delta`,
  `last_output`, `notified`, `grid_marks`, `bell_until`, `was_down`, `recover_until`) at their old
  indices — so after a move the wrong session showed the moved tab's pin/mute/busy/quiet indicators.
  They now ride along with the session (guarded against transient post-close staleness), and a
  unit test pins the alignment for both the swap and remove/insert reorder paths.

- **Scrollback export never fails silently.** `prefix+export_scrollback` (`w`) silently did nothing
  when the working directory wasn't writable. It now falls back to the temp directory and always
  flashes the outcome (written path, or the write error), so an export is never a silent no-op.

- **Queued type-ahead for a down host is now capped (1 MiB, newest-kept).** If a machine is offline
  for a long stretch the reconnect watchdog retries for hours, and the old code buffered every key
  typed into a dead pane without bound — a real memory leak for a far-flung fleet. Input now drops
  the OLDEST bytes past a generous ceiling and keeps the newest command, so a queued command is
  preserved but RAM stays bounded.

- **Peek overlay clamps its selection if tabs close underneath it.** `prefix+peek` could leave
  `peek_sel` pointing past the end after a `Cmd+W` closed a tab while the overlay was open, and the
  Enter handler then set the active tab out of range (a latent panic). It now re-clamps selection
  and scroll each frame and on Enter, matching the fleet grid's already-safe behavior.

- **Fleet search no longer panics if its targets change mid-search.** `prefix+h` / `Cmd+Shift+F`
  cached per-session match positions while the overlay was open; if a session closed (`Cmd+W`) or
  its terminal resized/streamed while searching, `render_fleet_search` could index a stale tab or
  grid line and crash the app. Matches now skip closed sessions and out-of-range rows instead.

- **Palette, broadcast, and fleet lists all scroll past the visible window.** The same
  invisible-selection bug as the fleet list (`prefix+s`) also affected the palette (`prefix+/`,
  capped at 12 rows), the broadcast target list (`prefix+a`, capped at 20 rows), and the fleet
  search hit list (`prefix+h`, capped at 8 rows): with enough tabs/entries, Up/Down could select a
  row that was never rendered. They now share a `scroll_top` viewport that keeps the highlighted row
  on screen and rides the bottom edge.

- **Fleet overlay scrolls past the first 20 sessions.** `prefix+s` now uses a scrolling viewport
  that keeps the highlighted row on screen, so a fleet bigger than a screenful no longer hides every
  session after the first 20 behind an invisible selection (Up/Down could select a row you couldn't
  see). The header shows `▲N above` / `▼N below` when rows are scrolled out of view.

### Added
- **Host overview overlay (`prefix+.`).** A one-screen "which machines are up" view: every distinct
  host across your open tabs, each with a live/total tally (`● build02 · 2/2 live`, `○ edge1 ·
  down`). Up/Down select, Enter jumps to that host's first tab. Built on the pure `host_tally`
  grouping (unit-tested) and the shared scrolling viewport; also reachable from the command palette.

- **Copied fleet summary opens with a per-host health block.** `copy_fleet` now prepends a
  machine-by-machine snapshot (``● build02 · 2/2 live`` / ``○ edge1 · down``) before the per-session
  lines, so pasting fleet status answers "which machines are up" at a glance across a multi-computer
  farm. Grouping is a pure `host_tally` helper (unit-tested).

- **`Cmd+Shift+R` force-reconnects every down pane at once** (the browser "reload" muscle memory),
  mirroring the `reconnect_all` prefix action — one key to bring a whole fleet back instead of
  hopping host by host. Plain `Cmd+R` stays with the shell. Listed in the help overlay.

- **`Cmd+Shift+F` opens fleet search** (search every session's scrollback at once) — the
  browser/editor "find in all" muscle memory, next to `Cmd+Shift+P` for the palette. Plain `Cmd+F`
  is left alone (it stays the in-session find). Listed in the help overlay.

### Added
- **Fleet list shows each agent by name.** The `prefix+s` fleet overlay now surfaces the daemon's
  per-session agent name/task (not just engine glyph + truncated id), so a diver can tell which
  agent each row is at a glance — and the fleet filter now matches by name too, so you can type an
  agent's label to jump straight to it.

### Added
- **Jump flashes say where you landed.** `prefix+z` (next quiet), `prefix+Q` (next down), and
  `prefix+P` (next pinned) now flash the target tab's identity (`quiet — claude@build02`) like
  `prefix+H` already did for host jumps, so a fleet triage hop is legible at a glance instead of a
  silent tab switch.

### Added
- **Configurable remote-attach connect timeout (`config.connect_timeout_secs`).** A tunnel
  spawn/attach waits up to 4s by default on the main thread, so a wedged host freezes the UI that
  long. The timeout is now tunable (clamped 1..=30): lower it to cap the freeze, raise it for a slow
  but reachable link. This is the same "no main-thread freeze" concern as the fleet-status work,
  giving the diver control over the exact tradeoff.


### Added
- **`Cmd+Shift+N` also opens a new session** (matching `Cmd+T`/`Cmd+N` and the browser/iTerm
  "new window" muscle memory). `Cmd+Shift+T` still reopens the last-closed tab. Covered by the pure
  `cmd_shortcut` unit test.


### Added
- **`Cmd+Shift+D` duplicates the active session** (VS Code / iTerm "Duplicate" muscle memory), reusing
  the same fork path as `prefix+k` so it preserves the engine@host identity and pin state. Plain
  `Cmd+D` is untouched (stays with the shell). Covered by the pure `cmd_shortcut` unit test.


### Fixed
- **Native-tab windows are now distinguishable in the system tab bar even before an agent announces
  a title.** `render_host_window` only set each window's title when the session reported a live OSC
  title, so otherwise every native tab just showed the generic "harness-terminal" and you couldn't
  tell which agent was in which tab. Each window now falls back to the session's identity (custom
  name, else `engine@host`) so separate agent tabs are readable at a glance. The single-window title
  got the same identity fallback (and no longer reads the redundant "harness-terminal — harness-
  terminal").


### Fixed
- **Mouse-wheel scroll to the bottom now returns the pane to live-follow.** The keyboard PgDn path
  cleared the "scrolled into history" pin once the view reached the live bottom, but the wheel path
  never did — so after wheel-scrolling up into history and back down to the latest line, the tab
  stayed pinned: it kept the "scrolled into history" label and new output no longer auto-followed.
  Wheel-scrolling down now un-pins the moment the display offset hits the bottom (mirrors PgDn).


### Added
- **Browser-style `Ctrl+Tab` / `Ctrl+Shift+Tab` now cycle tabs** (in addition to `Cmd+Shift+[/]`), so
  the Chrome/Firefox/VS-Code muscle memory for jumping between many agent sessions works too. A plain
  `Tab` is untouched (still goes to the shell/app), and the binding is covered by the pure
  `cmd_shortcut` unit test.


### Added
- **Configurable in-memory scrollback (``config.scrollback_lines``).** Each session's terminal grid
  previously kept the alacritty hardcoded default of 10000 history lines. You can now set how much
  history stays in RAM for scrollback search/find/copy/export — raise it (e.g. 50000+) to carry more
  context from long multi-agent runs, or lower it (even 0) to cap memory. Persisted-to-disk history is
  still bounded separately by ``scrollback_cap``.

### Performance
- **The per-frame render path no longer heap-allocates a whole new framebuffer every frame.** Both the
  single-window redraw and native-tab mode (every session window) previously built a fresh
  `Framebuffer` (≈7MB at a 1909×955 window, ×N windows in native mode) on every redraw. The scratch
  buffer is now kept across frames and reused via a capacity-preserving `resize`, so streaming output
  across several agent sessions allocates once instead of continuously — a real win under the 60fps
  pump with many tabs open.

### Performance
- **The fleet's quiet detector no longer reads the config file on every frame.** `quiet_flags`
  (the per-frame triage count and status line) plus the fleet-grid and `prefix+z` quiet checks each
  called `Config::load()` — a disk read + TOML parse — up to twice per rendered frame. The
  `quiet_after_secs` threshold is now resolved once at startup and cached, eliminating all
  hot-path config I/O.

- **Font-fallback logic pinned with tests.** The new `read_valid_font` validation helper now has
  coverage proving a real mono font validates, and a corrupt/absent path fails validation (so a bad
  config degrades rather than panics) — 114→116 unit tests.

### Fixed
- **`config.scrollback_lines` is now clamped to a safe ceiling (1M lines).** A mis-typed value (e.g.
  `1000000000`) previously would have made the grid pre-allocate history up to that many lines,
  risking an out-of-memory blank on relaunch. Oversized values (and `usize::MAX`) are pinned to 1M,
  `0` still disables history, and normal values pass through unchanged.

### Fixed
- **Opening the fleet overlay (prefix+e) and the fleet overlay's `s` refresh no longer freeze the UI
  on a wedged daemon.** Both called `HarnessClient::local().status()` synchronously on the main
  event-loop thread, stalling the whole terminal for up to 800ms when the local daemon accepted the
  connection but stopped responding. They now route through the same cached/background path as the
  periodic sweep (`refresh_fleet_nonblocking`): show the latest cached fleet snapshot and kick a
  fresh fetch on a background thread, so the fleet view stays responsive even when a remote/flaky
  host makes the daemon slow.

### Fixed
- **The fleet-status poll can no longer freeze the UI.** `reconnect_sweep_refresh` called
  `HarnessClient::local().status()` synchronously on the main event-loop thread every 5s, so a wedged
  daemon that accepts the connection but never responds would stall the whole terminal for the full
  800ms HTTP timeout on every sweep — the same freeze the Remote-Attach path already guards against.
  The periodic status refresh now runs on a background thread that lands its snapshot in a shared
  cache; the main loop only takes the latest value (non-blocking), and a failed fetch leaves the
  previous snapshot intact.

### Fixed
- **Native-tab mode now runs the per-frame activity pass (notifications + 60fps pump).** With
  `native_tabs = true`, the in-app chrome is hidden, so the busy/bell/recover activity pass only ran
  on demand (prefix+o) — native mode never fired the coalesced "agent went busy / your run finished
  (🔔 bell) / a host came back" OS notifications, and `live_busy` was never set, so streaming agent
  output was pumped at only ~8fps instead of the smooth 60fps the single-window path uses. The
  per-frame activity pass now runs in `redraw_hosts`, restoring notifications and the fast pump in
  the native-tab mode the app actually runs.

### Fixed
- **In-place terminal redraws no longer freeze the display.** The idle loop only woke when a pane's
  *scrollback* grew (`history_len`), so any output that redraws the screen without scrolling — a vim
  cursor move, an htop/top refresh, a TUI pane, a spinner/progress line, an agent updating in place —
  left the terminal frozen on stale content until a key was pressed. The wake detector now also
  tracks a rolling `visible_signature` of each session's grid (position + char + SGR flags + fg/bg +
  cursor), so any visible change wakes the idle loop and the frame repaints. A stable screen still
  hashes once per 120ms idle tick (0% CPU preserved), and a regression test rewrites a line in place
  with zero scrollback growth and asserts the change is now seen.

### Fixed
- **A bad/missing configured font can no longer crash the app at launch.** `GlyphCache::load`
  panicked (`expect`) the moment `font_path()` pointed at an unreadable or corrupt file (a typo in
  `font_path`, a stale `HARNESS_FONT`) — the whole app died on open instead of rendering. The font is
  now validated and, when the configured face is unusable, the known macOS mono faces are tried in
  order (SF Mono → Monaco), so the terminal still opens with a working font. It only fails loudly
  when no monospace at all can be loaded.

### Fixed
- **Native-tab mode no longer discards the persisted active tab on relaunch.** `sync_hosts` forced
  the focused session/window to tab 0 when it built the initial tab set, clobbering the active-tab
  that `main.rs` restores from the saved state — so a native-tab relaunch always reopened on the
  first session regardless of which one you left focused. It now points the active host (and the
  window that gets focus) at the restored tab, matching the single-window path.

### Fixed
- **Fleet-grid mark toggling can no longer panic on a desync.** The fleet-grid `Space` toggle read
  `grid_marks[grid_sel]` with an unwrap guarded only by the *tab* count, so a vector desync (the
  same class as the earlier `last_output` / tab-close fixes) would `unwrap()` past `grid_marks` and
  panic. The toggle now reads through `get_mut`, so a stale index can no longer panic — the mark is
  just left alone until the vectors re-sync.

### Fixed
- **`prefix+H` next-host paging pinned with tests.** `next_host_index` (jumping the fleet by
  machine) had no coverage for interleaved/repeated hosts or the wrap-around case; added a test
  exercising distinct-host ordering, skipping ahead across repeats, and wrap-to-front (113→114).

- **Pinned URL/path click expansion with regression tests.** `expand_click_word` (what `Cmd`-click
  URL-opening and `Alt`-click-cursor-move use to find the token under the cursor) had no tests and
  tricky boundary logic. Added an exhaustive test covering URL/path tokens, boundary columns,
  clamped start/past-end columns, and single-character tokens (112→113 unit tests).

- **Adding a session no longer risks an out-of-bounds panic.** `last_output` (the per-tab "quiet
  since" timestamp) was the only tab-indexed vector never grown when a new tab spawned — every
  other parallel vector is resized to the tab count each frame. With two or more *remote* (non-pty)
  tabs, a backgrounded pane producing output, or the info overlay on a newly-added remote pane,
  indexed `last_output[i]` past its length would panic. It's now resized alongside the other
  per-frame vectors (new tabs stamped "now" so they don't instantly read quiet).

- **Closing tabs no longer desyncs the internal per-tab bookkeeping.** The palette / context-menu /
  `prefix+D` close path (and "close all quiet tabs") removed a session from the tab list without
  re-syncing the per-tab vectors (`last_output`, `was_down`, `bell_until`, `recover_until`,
  `broadcast_targets`, …), so after closing a tab every subsequent tab's mute/pin/quiet/status
  state silently shifted. All close paths now drain the tab through `forget_tab`, and the batch
  "close all quiet" path also drops the matching native window (no orphaned windows) and correctly
  re-anchors focus when a quiet tab below the active one is peeled off. Covered by an exhaustive
  `reanchor_active_after_batch` regression test (111→112 unit tests).

### Fixed
- **`retry_backoff_ladder_caps_at_60` silently wasn't running.** A stray `#[test]` attribute had
  been misplaced onto the next function, so the unit test guarding the reconnect backoff ladder
  (5s→10s→20s→40s→60s cap) was never executed and the function was flagged dead code. The
  attribute is restored; the test now runs (and passes) as part of the suite (105→106 unit tests).
- **Flaky test harness (suite is now deterministic).** The file-backed persistence tests isolated
  their dir by mutating the process-global `HARNESS_CONFIG_DIR` via `std::env::set_var` — unsound
  under parallel test threads (concurrent `config_dir()` reads while env mutates is UB in Rust
  2021), which intermittently panicked ~11 restore tests. The override is now per-thread (thread-
  local), race-free by construction; the suite passes deterministically across repeated runs.
### Fixed
- **No more frozen UI on a dead remote host.** The tunnel connect ran synchronously on the main
  thread with no timeout, so a Remote-Attach to a remote address that silently drops packets (not
  refusing) could block for the OS's default multi-minute connect timeout and freeze the whole
  terminal. `TcpStream::connect_timeout` now caps that at 4s and still returns the error, so the
  in-app `⚠ host:port: …` toast fires promptly instead of the app hanging.
### Added
- **Native-tab window title updates are rate-limited.** The per-session `set_title`
  (a platform round-trip) was called on every frame even when the OSC title hadn't changed;
  native mode now caches each window's last title and only calls `set_title` on change, like
  the single-window path already did.
- **Offline sessions are no longer silently dropped on launch.** If a persisted session's
  host is unreachable when the app starts, the first frame now flashes how many didn't reopen
  and how to reconnect, so an agent on a down server isn't quietly missing from your fleet
  with no explanation.
- **Right-click "New Session" now pre-selects the default engine + last dir**, matching
  `Cmd+T` / the palette, so the picker is predictable no matter how it's opened.
- **Closing the find overlay clears the search highlight.** Dismissing `find` with Esc left
  `find_hit`/`find_all` set, so the last match stayed frozen and highlighted in the grid
  after find closed. Esc now clears it (opening find already did).
- **New sessions persist immediately.** A tab opened via `Ctrl+H n`, `Cmd+T`, remote
  spawn, local shell, or fleet attach was only written to disk later (on close/quit), so a
  crash or force-quit right after spawning silently lost it on relaunch. Native spawn sites
  now write the tab list to disk on success, matching the persistence guarantees of the
  duplicate/attach/undo paths.
- **Tunnel tabs keep their remote port across restart / duplicate / undo-close.** A tunnel
  session opened on a non-default harness port (`host:20000`) was persisted with `port: None`,
  so a relaunch, a duplicate, or an undo-close silently reconnected to the default 18473 and
  missed your agent. The transport now exposes its port and it rides through the tab spec in
  every path (save, duplicate, undo-close in both native and in-app close).
- **`Cmd+Shift+T` reopens the last-closed tab.** The browser/iTerm recovery muscle memory now
  restores the most recently closed session (same as `prefix+u` / the palette's Undo Close),
  in both native-tab and in-app-tab modes. `Cmd+T` alone still opens a new tab.
- **First-run empty state mentions Cmd+T.** The "no sessions" hint (shown on a fresh
  launch with zero tabs, in both in-app and native-tab modes) now leads with `Cmd+T` for a
  quick new tab, then the prefix new/attach/palette shortcuts, so a new user sees the
  fastest path immediately.
- **Remote connect success is now visible.** Starting a remote spawn showed no feedback (only
  failures did), so connecting to a faraway host was a silent wait. A successful Remote-Attach
  now flashes `attached <session> @ host:port ✓` (or `connecting <engine> @ host:port …`),
  while the existing keep-your-input-on-error behavior is unchanged.
- **`Cmd+1..9` / `Cmd+0` jump straight to a tab.** The universal iTerm/browser muscle memory
  now pages directly to any session (1-based; `0` = last) in both native-tab and in-app tab
  modes, so a diver managing many agents can hop to a specific one in one keystroke instead
  of cycling with `Cmd+Shift+[ / ]`. Pure `Cmd+number` never reaches the shell.
- **`Cmd+Shift+[ / ]` tab-switch matched the real US-layout glyphs.** The shortcut only
  checked the un-shifted `[`/`]`, but a real US keyboard produces `{`/`}` when Shift is held,
  so on actual hardware `Cmd+Shift+[/]` (next/prev tab) could silently no-op while the unit
  test passed on synthetic input. It now matches both glyphs.
- **Failed New-Session / Remote-Attach keeps your input.** A spawn or tunnel/attach error
  used to dismiss the overlay, wiping the typed working-directory or `host[:port][/session]`
  (and skipping the recent-host history, so a typo meant retyping everything). Now the picker
  stays open with your text and the `⚠ …` reason, so you fix and re-submit in one go.
- **Help overlay documents the Cmd shortcuts.** The `Ctrl+H ?` / `Ctrl+H h` help list now
  shows `Cmd+T/N` (new session / new native tab), `Cmd+W` (close active), `Cmd+Q` (quit),
  `Cmd+Shift+[ / ]` (prev/next tab), and `Cmd+Shift+P` (palette), so the iTerm-style
  shortcuts are discoverable from inside the app instead of only in the changelog.
- **Cmd+Shift+P opens the command palette.** The conventional macOS/VSCode/iTerm shortcut now
  opens the run-any-action palette (same as `Ctrl+H ;`). Pure `Cmd+P` is left alone.
- **Recent-remote-host memory (`Ctrl+H r`).** The Remote-Attach overlay now pre-fills the last
  server/session you attached to (`host[:port][/session]`, most-recent first, capped at 8 and
  deduped), so re-connecting to the same computer is a single Enter instead of retyping the
  address every time. Persisted across restarts.
- **Cmd+= / Cmd+- / Cmd+0 font zoom.** The font-zoom shortcuts now also respond to the
  Cmd (macOS ⌘) variants, matching how iTerm2 lets you zoom with either Ctrl or Cmd.
- **Cmd+Shift+[ / Cmd+Shift+] prev/next tab.** The standard macOS terminal convention (iTerm2
  uses it) to page through open sessions, working in both the in-app strip and native-tab mode.
  Plain `Cmd+[` is left to the shell; only the Shift variant is intercepted.
- **Remote attach/spawn failures are now visible.** A failed tunnel connect or session attach
  (host down, wrong port, no such session) used to be silent — `push_ok`/`spawn_tunnel_attach`
  only wrote to stderr, so a diver who typed an unreachable `host[:port][/session]` got no tab and
  no explanation. The spawn/attach methods now return the error, and the New-Session and Remote
  Attach overlays flash it in-status (`⚠ host:port: …`) instead of disappearing.
- **Native Cmd+T/N/W/Q shortcuts.** With no AppKit menu installed, macOS delivered `Cmd+T`/`Cmd+Q`
  to the app as ordinary key events, which the forwarding path then wrote to the session as plain
  characters (Cmd+T typed `t`, Cmd+Q typed `q`). These now route to the native-terminal actions:
  `Cmd+T`/`Cmd+N` open the New-Session picker (same as `Ctrl+H n`), `Cmd+W` closes the active
  window (in native-tab mode it closes the focused session window correctly), and `Cmd+Q` quits
  with the same save-then-exit dance as `prefix+q`. `Cmd+C`/`Cmd+V` copy/paste are untouched.
- **Shift+Tab forwarding.** A plain modifier-less `Tab` was being sent to the session with the Shift
  state dropped, so `Shift+Tab` reached Claude Code / shells as an ordinary Tab and their back-cycling
  shortcuts broke. `Shift+Tab` is now written to the PTY as the standard reverse-tab sequence
  (`ESC [ Z`), and plain `Tab` stays `\t`.
- **True macOS window-level tabs.** When `native_tabs = true`, every session becomes a real
  `NSWindow`, and AppKit's system title-bar tab bar (native traffic lights, Cmd+Tab picker, drag-a-
  tab-out, `Ctrl+H n`/`+` spawning a new grouped window) replaces the in-app strip. Added an AppKit
  FFI shim (`src/macos.rs`, via the `objc2`/`objc2-app-kit` stack winit already pins) that sets
  `NSWindow.tabbingMode` and splices real `NSWindow`s into one native tab set
  (`addTabbedWindow:ordered:`). A `Host` per session owns its own window + softbuffer surface; the
  frame renderer draws each session's grid full-bleed into its own window (overlays stay on the
  focused one), and focus/resize/close events are routed by `WindowId`. Switching tabs in-app
  (last-window, number keys, palette) surfaces the matching real window. Closing a tab's window
  closes that session; closing the last one quits. Opt-in (default off) so the in-app strip is
  untouched until you flip it on.
- **Native-style tab strip.** The in-window tab bar is now a two-row chrome strip with a raised,
  rounded-top active-tab sheet (Safari/Chrome silhouette), a 1px hairline dividing it from the
  grid, and a soft hover chip on inactive tabs — the current session reads as a lifted tab rather
  than a flat pill. The bottom status strip gains a matching top hairline so the two bars bookend
  the grid with the same native panel edge.
- **New-tab (+) affordance.** A `+` button at the strip's right edge (hover-raised, native tab-strip
  muscle memory) opens the New-Session picker — the same path `Ctrl+H n` and the context menu use.
- **Idle CPU near 0%.** The render loop now pumps at ~60fps only while something is visibly live
  (a tab pouring output, a fading bell/`↻` badge, an open overlay/tooltip, copy mode, hover);
  otherwise it drops to a slow idle tick and skips the full-framebuffer re-present, so a quiet
  terminal idles at ~0% instead of pegging a whole core re-uploading pixels to QuartzCore. Output is
  still caught within the idle tick and flips the loop back to full speed the moment a pane produces.
- **Attach an existing tmux session on a server (`Ctrl+H r`).** The remote-attach prompt now accepts
  `host[:port]/session` to attach to a specific already-running remote tmux session (attach-or-
  create, no kill/recreate) instead of spawning a fresh engine; the session identity persists in
  the tab spec so a relaunch re-attaches to the same named session. `host[:port]` still spawns a new
  `auton-<engine>` pane as before.
- **Tab-bar close × buttons.** Each tab grows a right-edge `×` on hover (iTerm2 / Chrome-style);
  clicking it closes that tab, honoring the pin guard (a pinned tab flashes the unpin hint instead).
  Close-by-index keeps every tab-parallel bookkeeping vector in lockstep so pins/mutes/badges don't
  shift after a middle-bar close.
- **Right-click context menu.** Hover-tab-aware popover with Copy / Paste / Open Link / Select All /
  Search for Selection / New Session / Close Tab, keyboard-navigable (`j`/`k`/arrows/Enter/Escape),
  dismissed on any click outside.
- **Configurable prefix — now `Ctrl+H` ("Ctrl Harness", tmux's `Ctrl+B` analog).** The prefix's
  leading chord was hardcoded to `Ctrl+Space`, which macOS silently owns for its input-source
  switcher (English + Vietnamese Telex = the canonical case), so the prefix just wouldn't answer.
  The primary is now `Ctrl+H` by default, with `Ctrl+Space` and `Ctrl+\` kept as always-on
  fallback chords, and `prefix_key` in `config.toml` rebinds it (`"h"`, `"b"`, `"space"`, `"\"`,
  …) without recompiling — so a diver who wants tmux muscle memory is one config line away. Case-
  insensitive matching, `keys::prefix_label` drives the in-app hints, and the macOS-claim notice
  now names the actual prefix.
- **Space actually types again.** macOS's winit reports the spacebar as `Named(Space)` while the
  rest of the app matches `Character(" ")`, so a plain space was silently swallowed *everywhere* —
  the shell (commands like `ls -la`, `git commit -m "…"` had untypeable spaces), every text field
  (palette, find, rename, broadcast, remote host, new-session cwd, copy-search), and the broadcast
  overlay's "Space toggles target" was dead code on macOS. Keys are now normalized once at the top
  of the key handler (`Named(Space)` → `Character(" ")`), copy mode keeps its "Space = copy" arm,
  and the fleet-grid mark toggle + broadcast target toggle work on every platform. Unit-tested via
  a new `keys::normalize_space`.
- **Prefix dislikes being dead: `Ctrl+\\` is now a working fallback chord.** macOS silently eats
  `Ctrl+Space` for its *input-source switcher* whenever a second layout is enabled (English + a
  Vietnamese Telex IM is the classic case) — the keystroke never reaches the app, so a tmux-style
  prefix just stops answering. A new `src/macos.rs` detects that exact condition at launch
  (`com.apple.symbolichotkeys` hotkey 60 enabled **and** ≥2 selectable input sources) and the app
  then: (1) answers the prefix on `Ctrl+\\` too (the OS never grabs it), (2) flashes a one-line
  "macOS owns Ctrl+Space" notice at launch and again once on the first `Ctrl+\\` press, and
  (3) rewrites every in-app hint (empty-state, keymap, palette row) to advertise the chord that
  actually works. Both chords stay live, so reclaiming `Ctrl+Space` in System Settings ▸ Keyboard ▸
  Keyboard Shortcuts ▸ Input Sources just turns the primary back on. Unit tests for the plist parse
  (fail-open on any unreadable/invalid input) and for the chord predicate (`Space` vs `\\` add
  `|` for Shift+Ctrl+Backslash).
- **Mark-all-read (`prefix+I`)** — clear every tab's busy `!N`, bell 🔔, and recovery ↻ badge in one
  key. Re-baselines each backgrounded tab's seen-history so the next output is the only thing that
  nags again; a baseline reset, not a mute (fresh output still refills a badge normally). In the
  command palette too.
- **Session age in the info panel** — `prefix+i` now shows how long a session has been alive
  (`age` row, via a new `Session::age`), so a diver can tell a long-running agent from a just-spawned
  one at a glance alongside the existing idle `silence` row.
- **Targeted broadcast from the fleet grid** — the war-room grid (`prefix+e`) is now more than a
  viewer: `Space` toggles a mark (●) on the focused tile and `b` opens the broadcast overlay
  pre-scoped to exactly the marked sessions, so commanding a subset of hosts is two keys instead of
  a checkbox walk. Marks are shown on the tile header, consumed on broadcast, and fall back to
  all-on if nothing is marked (you can never broadcast to zero by accident).
- **Busy-tab tooltip shows settled scrollback** — hovering a tab that's actively streaming now shows
  the freshly-printed rows that have frozen into history (a stable read of what the agent decided to
  print) instead of the live screen tail, which reflows every frame into an unreadable blur. Idle
  tabs keep showing their live tail as before.
- **Streaming spinner** — a tab that is producing output right now (this frame) shows a small,
  cycling spinner next to its busy badge, so live tabs visibly turn while settled ones sit still.
  Distinct from the cumulative `!N` badge, which lingers after an agent goes quiet.
- **Clickable hover tooltip** — the hover-preview popover (the live tail shown for a backgrounded
  tab) is now click-through: clicking inside it switches to that session instead of having to find
  the tab chip. "Hover to preview, go" is one gesture.
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
- **Named color-theme presets.** `[theme] preset = "gruvbox-dark" | "solarized-dark" | "nord" |
  "dracula" | "github-dark" | "tokyo-night"` selects a full palette as the base; any individual
  `[theme]` fields layer on top. The built-in default is now the `tokyo-night`-inspired palette.
- **Modal overlays dim the terminal behind them**, so the engine picker / remote-attach / palette /
  fleet / find / broadcast text reads clearly against bright agent output instead of washing out.
- **List overlays highlight the selected row** with the chrome's active-tab pill, so the current
  entry reads at a glance (consistent with the context menu).
- **Command-mode chip.** While the prefix is armed (you typed `Ctrl+H`, next key is a command), a
  highlighted `Ctrl+H` chip appears in the status line, so it's clear the app is waiting for a
  command and why the next key is consumed rather than typed.
- **Fuzzy filtering for the session + command palettes.** Typing partial characters now matches in
  subsequence order (case-insensitive), so e.g. `crd` matches "cursor codex" and `fle` finds the
  fleet actions — a faster jump than exact substring.
- **Visible spawn-failure toasts.** When a new-session or remote-attach fails (e.g. a host you can't
  reach), a `⚠ couldn't start …` toast appears in the status line instead of failing silently to
  stderr.
- **Mouse-wheel over the tab bar cycles tabs** (iTerm2/Chrome-style): wheel up steps left, down
  steps right, wrapping at the edges — while the wheel over the grid still scrolls scrollback.
- **Idle CPU dropped to ~0%.** The continuous-render loop is capped at ~60fps via `WaitUntil`
  instead of redrawing uncapped (previously pegged a core at ~99% when idle), while remaining fully
  responsive to input and output.
- **New default theme: a modern Tokyo Night-inspired dark palette.** Deeper, better-contrast ANSI
  colors (rose/teal/lavender family) and a non-pure-black canvas; the whole framebuffer starts from
  the theme background so grid margins read as one surface instead of a hard black frame. Tab bar,
  status line, hover tooltip, and context menu share one coherent chrome surface that recedes below
  the grid. A `[theme]` block still overrides any entry.
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
