# Architecture — harness-terminal

Terminal-first dive into a fleet of agent sessions. This document is the design source of truth;
it changes as the design does.

## 0. Model

```
TAB = SESSION = PANE@HOST    (strictly 1:1)
```

- **A session** is one running agent (Claude Code, Codex, OpenCode, …) in **one tmux pane on one host**.
- **A tab** in harness-terminal connects to exactly that pane.
- **Local and remote are the same gesture.** Every pane has an address (`pane@host`); where it runs
  is metadata on the tab, not a mode. A locally-spawned session is just `pane@this-machine`.

**Deliberately flat.** There is no client-side window/pane tree. tmux owns layout on the host; this
client owns the fleet of sessions. The tab bar IS the session palette.

## 1. Component split — what's ours vs borrowed

| Layer | Owner | Notes |
|---|---|---|
| Transport / tunnel | **reuse harness** | `machineId` routing, e2ee (Ed25519 + CPace PAKE, X25519, ChaCha20-Poly1305). We do NOT rebuild this. |
| Session index | **ours** | Which panes exist across the fleet; fuzzy palette. Built on harness `tmuxAgentDiscovery`. |
| Pane wire protocol | **tmux control mode** | Bytes in / keystrokes out / titles, per host. No reimplemented multiplexer. |
| Raw terminal engine | **borrow** | `termwiz` (WezTerm) or `alacritty_terminal` — escape parsing, rendering, scrollback, local echo. |
| Client shell / UX | **ours** | Tab bar, palette, keys, per-tab control. |

## 2. Client shell UX

- **Tab bar = session palette.** Every pane@host on every joined machine, flat, fuzzy-findable,
  ordered by recency. One keystroke to open any session.
- **New session** = spawn a pane on a chosen host (`tmux new-session`/split), run the engine, it
  becomes a tab.
- **Status chrome per pane:** `host · engine · session-title · status` (title from tmux pane-title,
  same logic as harness `tmuxAgentDiscovery`). Remote-ness is a small glyph.
- **Local echo** so remote typing feels local.
- **Keyboard-first**, tmux-style prefix keys — primary `Ctrl+H` (configurable via `prefix_key`
  in `config.toml`), with `Ctrl+Space`/`Ctrl+\` as fixed fallback chords. `src/macos.rs` detects
  when macOS owns `Ctrl+Space` for its input-source switcher (the OS swallows the keystroke before
  it reaches the app once a second layout is enabled) so the hints and the one-time notice match
  reality.

## 3. Wire protocol sketch (to be finalized)

Frames ride the harness e2ee tunnel (existing envelopes + `machineId` routing). Reuse the
CommanderMirror / tmux-discovery concepts already in `autonomous-harness`:

- `pane_index` — enumerate sessions (pane list + metadata) across hosts, using the same per-host
  tmux pane discovery as harness `tmuxAgentDiscovery`.
- `pane_spawn` — create a session (pane) on a host, with an engine + workdir.
- `pane_attach` / `pane_detach` — open/close the raw byte channel to one pane.
- Raw tmux-control-mode byte flow for the attached pane (bytes upstream, input downstream).

Cryptography: reuse `cli/src/lib/e2ee/core.ts` scheme. Terminal bytes are opaque; AEAD already
fits byte streams.

Server/host side already has session→agent addressing and a `commander_event` stream
(`cli/src/lib/commander.ts`) that this client can piggyback for pane status (busy/idle/summary)
without its own parse.

## 4. Terminal engine decision (DECIDED)

**Use `alacritty_terminal` (MIT/Apache, Rust) as the raw emulator engine; we own the slim client
shell.** Rationale: our differentiator is the fleet/session layer, not terminal intrinsics. The
clean engine gives full control of the shell while keeping dependency surface small, and avoids
inheriting WezTerm's whole opinionated mux architecture. Local echo for remote tabs is a small
addition (optimistic render + echo suppression on returned bytes).

Rust toolchain: **1.97 stable** (was pinned 1.57 — upgraded; modern terminal crates require it).

## 5. Product scope (DECIDED) — 12 agent frameworks

A tab = one agent session = one tmux pane@host. The client spawns any of 12 engines on any host,
or attaches to panes already discovered by `autonomous-harness`. Engines and their CLI commands
(from `autonomous-harness/cli/src/lib/engineBin.ts`):

| engine | cli | | engine | cli |
|---|---|---|---|---|
| claude | `claude` | | commandcode | `cmd` |
| codex | `codex` | | devin | `devin` |
| cursor | `agent` | | muse | `muse` |
| opencode | `opencode` | | amp | `amp` |
| pi | `pi` | | kilo | `kilo` |
| hermes | `hermes` | | grok | `grok` |

## 6. UX (DECIDED) — keyboard-first TUI

- **Tab bar** across the top: one per session (pane@host), engine + host + status badge.
- **Tab = session = pane@host**, flat 1:1. No window/panel tree client-side.
- **Session palette** (`prefix + /`): fuzzy-find any session across the fleet; jump in a keystroke.
- **Engine picker** (`prefix + n`): choose engine + host to spawn a new session tab.
- **Status chrome** per tab: `host · engine · pane-title · status`.
- **Local echo** for remote tabs so typing feels local.
- Open question (later): pull the commander bus (busy/idle/summary) into status badges.

## 7. Native UI layer (DECIDED) — winit + softbuffer

The shell is a **standalone native window**: winit owns the window + input event loop, softbuffer
provides a CPU-side framebuffer, and ab_glyph rasterizes glyphs onto it. `draw_grid` in `render.rs`
walks the active `alacritty_terminal` `Term` grid and paints (cell) → (char, fg, bg) ARGB pixels.
The chrome (tab bar, status line, palette/overlays, find bar) is drawn in the same buffer by the
same rasterizer — no host terminal is involved. A legacy **ratatui** TUI survives only as the
`--tui` fallback for headless/SSH use.

**Theme.** Grid colors (ANSI 16, foreground, background, cursor, selection) come from a config
`[theme]` block in config.toml, resolved into a `Theme`/`Colors` struct at startup and passed into
the render path. Your personal palette overrides the built-in defaults per-tab, exactly as alacritty
does.

## 8. Concurrency (DECIDED — initial)

One thread per attached session runs its event loop (as alacritty does). The app thread owns the
`Vec<Tab>` + active index and renders synchronously. Background tabs keep ingesting via their own
transport threads.

## 9. The multilayer build (bottom → top)

1. `engines` — 12 engine definitions (name, CLI command, colors).
2. `config` — TOML config (font_px, default_engine, font_path, scrollback_cap, start_cwd, theme).
3. `restore` — persistence gates: session tabs/names, muted tabs, scrollback, window geometry.
4. `session` — one `Term` + transport + its input/output channel.
5. `transport` — `LocalPtyTransport` (alacritty PTY), `TmuxTransport` (real tmux pane,
   control-mode, proven end-to-end), and `TunnelTransport` (harness pane-relay). `Box<dyn
   Transport>` per session; remote = the same trait with a different byte source.
6. `render` — draws the `Term` grid + chrome into the framebuffer (theme-aware).
7. `app` — `Vec<Tab>`, active index, overlay state, key dispatch.
8. `native` — winit window/event loop wiring the above; `tui` is the `--tui` fallback.
9. remote — the tunnel-backed transport (harness e2ee, `machineId`, tmux pane@host).

## 10. Remote pane@host attach — current state

`TAB = SESSION = PANE@HOST` is literal for local panes today: a `Session`'s transport is either
alacritty's PTY or a real tmux pane driven via control mode (`tmux -C`). tmux control mode is the
raw byte-stream primitive: the pane's `%output` notifications are decoded (tmux octal escapes) and
replayed into the shared `Term` grid via `vte::ansi::Processor::advance`; keystrokes become
`send-keys -l` / `send-keys Enter` commands; resize is `resize-window`.

The harness fabric (`harness join` → `machine-ws`) is a *structured* agent-event channel, not a raw
pane byte relay. So true remote attach (a tab sourcing bytes from a pane on another joined machine)
is the next transport behind the same trait — decode `%output` → `advance` stays identical; only the
byte source changes. That remote impl needs one thing harness doesn't yet expose from Rust: a raw
per-pane byte stream over the tunnel. Two buildable paths, in preference order:

1. **Tunnel a tmux control-mode stream**: on the remote host run `tmux -C` (or `harness attach`),
   and relay its `%output`/`send-keys` byte pairs over the existing e2ee `machine-ws` channel as a
   new frame type. This reuses control-mode semantics proven here and adds just a relay hop.
2. **`machine-ws` raw relay**: extend the harness backend/adapter with a raw bytes frame, mirroring
   how the web UI streams launcher frames today.

Either slots into `Transport` behind `kind() == "remote"` with no session/tui changes.

## 11. Cross-machine transport — built: ssh + harness pane-relay tunnel

Remote attach is live two ways, both streaming `%output` → `advance` behind `Transport`:

- **ssh** (`prefix + r` → `App::spawn_remote` → `Session::remote` → `RemoteTransport`, kind
  `"ssh"`): spawns `ssh -tt <host> tmux -C` and streams that remote pane through the identical
  `ControlPipe` decode → `advance` path. No harness change. Host discovery
  (`transport::discover_hosts`) reads `~/.ssh/config` `Host` entries.
- **harness tunnel** (ARCHITECTURE §10 path 1 — now real; kind `"tunnel"`): a tab backs onto a
  pane on another joined machine via the machine's `harness` daemon (`prefix + r` defaults the host
  to `127.0.0.1` and reaches `/api/pane-ws` on `HARNESS_PORT_DEFAULT`, 18473).
  - Harness side (`autonomous-harness/cli`): `hookServer.ts` serves `/api/pane-ws`; on attach it
    spawns `tmux -C`, relays the pane's `%output` byte stream to the client verbatim, and forwards
    client messages (control-mode commands) into the tmux client's stdin. `cli.ts` routes
    `onPaneRelay`.
  - Client side (`transport.rs`): `TunnelTransport::spawn` opens one nonblocking WebSocket to
    `/api/pane-ws`, sends `new-session` on attach, and a single thread reads `%output` →
    `parse_output` → `advance` while draining keystrokes encoded via the shared
    `encode_keys(bytes)` helper (the same `send-keys -l` / `send-keys Enter` encoding the local
    control-mode pipe uses — the relay expects commands, not raw bytes). Locked in by
    `tests/tunnel_live.rs`, a live E2E that types a marker over the tunnel and asserts it echoes
    back into the grid.

- **Auto-reconnect** (tmux/ssh/tunnel): each drop-prone transport tracks liveness
  (`Transport::alive` — the tmux/ssh reader sets it false when the control client's stdout closes;
  the tunnel thread when the WebSocket closes) and can `reconnect` against its saved identity +
  grid. A main-loop watchdog (`App::reconnect_sweep`) re-attaches any dead tab on a 5s throttle,
  killing a stale same-name session first so it can't trip tmux's "duplicate session". The harness
  relay teardown is what makes this prompt: on `%exit` it closes the connection so the client sees
  the drop. Locked in by `tests/tunnel_reconnect.rs`, a live E2E that kills the pane and asserts a
  reconnect round-trips a fresh marker. Only local PTYs (which can't drop) skip all of this.

- **Latency smoothing** (ssh/tunnel): each remote session owns an `EchoCanceller`
  (`Session::write` optimistically renders keystrokes for instant typing and records them; the
  transport reader thread cancels the identical copy that returns ~RTT later, so nothing
  double-renders). It is byte-oriented + windowed (pending echo expires after 1.5s so a pane that
  never echoes — password prompt, fullscreen app — can't poison later genuine output) and
  conservative: only bytes matching the front of the pending queue are dropped; real program output
  passes untouched, split across chunks or not. Locked in by unit tests in `session.rs`.

No remaining product gaps flagged.
What's live today is geometric: bytes flow as fast as ssh will carry them; no latency
smoothing, no local echo, no reconnect — those are refinements, not missing pieces.
Host discovery (`transport::discover_hosts`) reads `~/.ssh/config` `Host` entries; the
harness `machine-ws` tunnel can back the same attach later without touching this code.

## 12. Harness fleet status (commander bus) + remote local echo

- **Status badges from the real harness daemon** (`src/harness.rs`): every joined machine runs
  `harness` on a fixed loopback control port (`HARNESS_PORT_DEFAULT`, 18473). `GET /api/status`
  is pulled read-only and non-blocking (`prefix + s` prints a fleet summary into the pane, e.g.
  "4 agents · tunnel up"); it's the same live data `autonomous-harness` uses, so we reuse the
  commander-bus concept without re-implementing its parse. Serde maps exactly the wire fields the
  daemon sends (`machineId`, `connected`, `deviceTransportConnected`, `sessions[].updatedAt`),
  pinned by a unit test against the real payload.
- **Local echo for remote tabs**: a `remote`-kind transport mirrors keystrokes into the grid
  optimistically while they cross the network, so typing feels instant; the pane's real echo
  overwrites it. Purely a render optimization — never mutates the transport bytes.

The raw pane *byte* relay across machines now rides the harness fabric itself: the daemon's
`/api/pane-ws` relay (see §11) carries the control-mode stream over the existing loopback control
port, so a joined machine is reached without raw ssh. The emulator client and the daemon relay
share the control-mode protocol end to end.
