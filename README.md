# harness-terminal

**A terminal-first client for working with AI coding agents across many computers and servers.**

*One agent session per tab* — wherever that session runs, on whichever machine, running whichever
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

## Status

Early bootstrap. Architecture in `ARCHITECTURE.md`. Nothing usable yet.

## License

MIT — see [LICENSE](LICENSE).
