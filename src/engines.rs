//! The 12 agent frameworks this terminal can run, matching `autonomous-harness`.
//!
//! Each engine is a bundle: human label, the CLI command that launches it, and a status color.
//! Adding a framework = adding a row here (and, for spawn support, its CLI invocation).

/// Canonical engine identifiers — must match `automous-harness` `ENGINES`.
pub const ENGINE_IDS: &[&str] = &[
    "claude", "codex", "cursor", "opencode", "pi", "hermes",
    "commandcode", "devin", "muse", "amp", "kilo", "grok",
];

/// A single agent framework definition.
#[derive(Clone, Copy, Debug)]
pub struct Engine {
    pub id: &'static str,
    pub label: &'static str,
    /// CLI command that launches the framework.
    pub cmd: &'static str,
    /// ARGB accent color used for the tab badge.
    pub color: u32,
}

pub const ENGINES: &[Engine] = &[
    Engine { id: "claude", label: "Claude Code", cmd: "claude", color: 0xff_9a4dff },
    Engine { id: "codex", label: "Codex", cmd: "codex", color: 0xff_22c55e },
    Engine { id: "cursor", label: "Cursor", cmd: "agent", color: 0xff_38bdf8 },
    Engine { id: "opencode", label: "OpenCode", cmd: "opencode", color: 0xff_0ea5e9 },
    Engine { id: "pi", label: "PI", cmd: "pi", color: 0xff_a3e635 },
    Engine { id: "hermes", label: "Hermes", cmd: "hermes", color: 0xff_a78bfa },
    Engine { id: "commandcode", label: "Command Code", cmd: "cmd", color: 0xff_f472b6 },
    Engine { id: "devin", label: "Devin", cmd: "devin", color: 0xff_818cf8 },
    Engine { id: "muse", label: "Muse", cmd: "muse", color: 0xff_fbbf24 },
    Engine { id: "amp", label: "AMP", cmd: "amp", color: 0xff_34d399 },
    Engine { id: "kilo", label: "Kilo", cmd: "kilo", color: 0xff_2dd4bf },
    Engine { id: "grok", label: "Grok", cmd: "grok", color: 0xff_ef4444 },
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
            assert!(by_id(e.id).is_some());
        }
    }
}
