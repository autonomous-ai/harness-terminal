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

## 5. Open questions

- Native UI layer: how to render tab bar + palette around the terminal surface (TUI framework —
  e.g. ratatui-ish shell — vs desktop). TODO.
- Control plane: how `harness join`'s discovery/presence feeds the palette.
- Concurrency model for many simultaneous attached panes.
