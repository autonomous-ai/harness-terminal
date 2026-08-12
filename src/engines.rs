//! The 12 agent frameworks this terminal can run, matching `autonomous-harness`.
//!
//! Each engine is a bundle: human label, the CLI command that launches it, and a status color.
//! Adding a framework = adding a row here (and, for spawn support, its CLI invocation).

/// Canonical engine identifiers — must match `automous-harness` `ENGINES`.
pub const ENGINE_IDS: &[&str] = &[
    "claude",
    "codex",
    "cursor",
    "opencode",
    "pi",
    "hermes",
    "commandcode",
    "devin",
    "muse",
    "amp",
    "kilo",
    "grok",
];

/// A single agent framework definition.
#[derive(Clone, Copy, Debug)]
pub struct Engine {
    pub id: &'static str,
    pub label: &'static str,
    /// One-line human description, shown in the picker/remote-attach/fleet so a diver can tell
    /// frameworks apart before spawning one.
    pub desc: &'static str,
    /// CLI command that launches the framework.
    pub cmd: &'static str,
    /// ARGB accent color used for the tab badge.
    pub color: u32,
}

pub const ENGINES: &[Engine] = &[
    Engine {
        id: "claude",
        label: "Claude Code",
        desc: "Anthropic CLI coding agent (this one) - fast, terminal-first",
        cmd: "claude",
        color: 0xff_9a4dff,
    },
    Engine {
        id: "codex",
        label: "Codex",
        desc: "OpenAI terminal agent - GPT code gen + shell workflows",
        cmd: "codex",
        color: 0xff_22c55e,
    },
    Engine {
        id: "cursor",
        label: "Cursor",
        desc: "Cursor Agent - the popular VS Code AI, shipped as a CLI",
        cmd: "agent",
        color: 0xff_38bdf8,
    },
    Engine {
        id: "opencode",
        label: "OpenCode",
        desc: "OpenCode - open-source agentic CLI (solver, MCP-ready)",
        cmd: "opencode",
        color: 0xff_0ea5e9,
    },
    Engine {
        id: "pi",
        label: "PI",
        desc: "Perplexity PI - reasoning agent with web browsing",
        cmd: "pi",
        color: 0xff_a3e635,
    },
    Engine {
        id: "hermes",
        label: "Hermes",
        desc: "Hermes - fast local-first agent (Nous Research)",
        cmd: "hermes",
        color: 0xff_a78bfa,
    },
    Engine {
        id: "commandcode",
        label: "Command Code",
        desc: "Command Code - JetBrains agent CLI (AI Assistant)",
        cmd: "cmd",
        color: 0xff_f472b6,
    },
    Engine {
        id: "devin",
        label: "Devin",
        desc: "Devin - Cognition autonomous software engineer",
        cmd: "devin",
        color: 0xff_818cf8,
    },
    Engine {
        id: "muse",
        label: "Muse",
        desc: "Muse - DeepMind-style generalist coding agent",
        cmd: "muse",
        color: 0xff_fbbf24,
    },
    Engine {
        id: "amp",
        label: "AMP",
        desc: "AMP - agent marketplace CLI (open agent discovery)",
        cmd: "amp",
        color: 0xff_34d399,
    },
    Engine {
        id: "kilo",
        label: "Kilo",
        desc: "Kilo - open-source Codex Workflow reimplementation",
        cmd: "kilo",
        color: 0xff_2dd4bf,
    },
    Engine {
        id: "grok",
        label: "Grok",
        desc: "Grok Code Fast - xAI coding agent (fast, low-latency)",
        cmd: "grok",
        color: 0xff_ef4444,
    },
];

impl Engine {
    pub fn by_id(id: &str) -> Option<&'static Engine> {
        ENGINES.iter().find(|e| e.id == id)
    }
    /// Resolve by CLI command too (so a session title can name the engine).
    pub fn by_cmd(cmd: &str) -> Option<&'static Engine> {
        ENGINES.iter().find(|e| e.cmd == cmd)
    }
}

/// Whether `cmd` resolves on this machine's PATH. Pure filesystem scan (no subprocess): walks the
/// `$PATH`-split dirs for an executable file named `cmd` (or `cmd.exe`). Used to dim engines in the
/// new-session picker that a diver doesn't actually have installed, so picking one doesn't surprise.
/// A command may still fail at spawn (a bare stub, a broken install) — this is a hint, not a promise.
pub fn is_installed(cmd: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if is_executable(&candidate) {
            return true;
        }
        // Windows shims come as `cmd.exe`; harmless no-op elsewhere.
        #[cfg(windows)]
        {
            if is_executable(&candidate.with_extension("exe")) {
                return true;
            }
        }
    }
    false
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_match_harness() {
        // Order/length sanity: 12 frameworks.
        assert_eq!(ENGINE_IDS.len(), 12);
        assert_eq!(ENGINES.len(), 12);
        // Every id has a unique, non-empty command.
        for e in ENGINES {
            assert!(!e.cmd.is_empty());
            assert!(Engine::by_id(e.id).is_some());
        }
    }

    /// A present executable on PATH is found; a nonsense name is not.
    #[test]
    fn is_installed_finds_present_executable() {
        if cfg!(unix) {
            assert!(is_installed("sh")); // guaranteed present
        }
        assert!(!is_installed("unlikely-nonexistent-binary-harness"));
    }
}
