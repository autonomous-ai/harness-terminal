# Architecture — autonomous-term

Terminal-first dive into a fleet of agent sessions. This document is the design source of truth;
it changes as the design does.

## 0. Model

```
TAB = SESSION = PANE@HOST    (strictly 1:1)
```

- **A session** is one running agent (Claude Code, Codex, OpenCode, …) in **one tmux pane on one host**.
- **A tab** in autonomous-term connects to exactly that pane.
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
- **Keyboard-first**, tmux-style prefix keys.

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

## 7. Native UI layer (DECIDED) — ratatui TUI

We use **ratatui** (Rust, MIT) for the shell: tab bar, palette, engine picker drawn in a TUI around
the `alacritty_terminal` grid. The active tab's `Term` grid is rendered as the main surface; the
chrome (tabs/palette/status) is ratatui widgets. Keyboard-first, tmux-style prefix keys.

## 8. Concurrency (DECIDED — initial)

One thread per attached session runs its event loop (as alacritty does). The app thread owns the
`Vec<Tab>` + active index and renders synchronously. Background tabs keep ingesting via their own
transport threads.

## 9. The multilayer build (bottom → top)

1. `engines` — 12 engine definitions (name, CLI command, colors).
2. `session` — one `Term` + transport + its input/output channel.
3. `transport` — `LocalPtyTransport` (alacritty PTY) and `TmuxTransport` (real tmux pane,
   control-mode, proven end-to-end). `Box<dyn Transport>` per session; a remote tunnel-backed
   impl slots in behind the same trait. (Same `Term` surface.)
4. `app` — `Vec<Tab>`, active index, palette index, key dispatch.
5. `tui` — ratatui shell drawing tabs/palette/status + the active `Term` grid.
6. remote — the tunnel-backed transport (harness e2ee, `machineId`, tmux pane@host).

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
