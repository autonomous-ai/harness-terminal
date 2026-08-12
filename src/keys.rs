//! Prefix keybinding resolution.
//!
//! After `Ctrl+Space`, a single key triggers an action (tmux-style). These binds are configurable
//! via the `[keybindings]` block so a user can remap them without recompiling. An absent or empty
//! `[keybindings]` block keeps today's exact defaults — see `DEFAULT_KEYS`.

use std::collections::BTreeMap;

/// The canonical, ordered set of every prefix action. The order here is also the order the defaults
/// table is checked, so later entries never shadow earlier ones (each action maps to one key).
pub const ACTIONS: &[(&str, &str)] = &[
    ("palette", "/"),
    ("fleet_grid", "e"),
    ("new_session", "n"),
    ("remote_attach", "r"),
    ("local_shell", "t"),
    ("quit", "q"),
    ("fleet", "s"),
    ("goto_tab0", "c"),
    ("next_busy", "o"),
    ("next_quiet", "z"),
    ("next_down", "Q"),
    ("mute", "m"),
    ("last_window", "l"),
    ("paste", "p"),
    ("broadcast", "a"),
    ("close_tab", "x"),
    ("copy_scrollback", "d"),
    ("export_scrollback", "w"),
    ("copy_identity", "j"),
    ("copy_fleet", "E"),
    ("peek", "y"),
    ("undo_close", "u"),
    ("duplicate", "k"),
    ("page_up", "g"),
    ("scroll_bottom", "b"),
    ("search", "f"),
    ("search_all", "h"),
    ("move_left", "{"),
    ("move_right", "}"),
    ("copy_mode", "["),
    ("help", "?"),
    ("command_palette", ";"),
    ("rename", ","),
    ("session_info", "i"),
    ("toggle_focus", "v"),
    ("pin", "A"),
    ("next_pinned", "P"),
    ("reconnect", "R"),
    ("destroy", "D"),
];

/// The built-in full keybinding table. `(action, key)` in ACTIONS order.
fn defaults() -> BTreeMap<&'static str, &'static str> {
    ACTIONS.iter().map(|&(a, k)| (a, k)).collect()
}

/// Resolve the full keybinding table: an action name optionally overridden by the user config,
/// otherwise the built-in default. Unknown action names in the config are ignored gracefully; the
/// config can only remap actions it recognizes, never the digits/tab handling (which stays fixed).
pub fn resolve(cfg: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (action, key) in ACTIONS {
        let key = cfg
            .get(*action)
            .filter(|k| !k.is_empty())
            .map(String::as_str)
            .unwrap_or(key);
        out.insert((*action).to_string(), key.to_string());
    }
    out
}

/// The configured key that triggers `action`, or today's default if the config did not remap it.
/// Returns the default even when the action is unknown (so a lookup can never fail). This lets a
/// caller answer "which key runs `action`?" without building the whole resolved table.
pub fn binding_for(cfg: &BTreeMap<String, String>, action: &str) -> String {
    if let Some(k) = cfg.get(action) {
        if !k.is_empty() {
            return k.clone();
        }
    }
    defaults()
        .get(action)
        .map(|k| k.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve_every_action_to_a_unique_key() {
        let base = BTreeMap::new();
        let resolved = resolve(&base);
        assert_eq!(resolved.len(), ACTIONS.len());
        let mut seen = std::collections::HashSet::new();
        for (action, key) in &resolved {
            assert!(!key.is_empty(), "action {action} must bind to a key");
            assert!(
                seen.insert(key.clone()),
                "action {action} collides on key {key}"
            );
            assert_eq!(binding_for(&base, action), *key);
        }
        // Spot-check a few defaults.
        assert_eq!(resolved["new_session"], "n");
        assert_eq!(resolved["command_palette"], ";");
        assert_eq!(resolved["copy_mode"], "[");
        assert_eq!(resolved["rename"], ",");
        assert_eq!(resolved["quit"], "q");
    }

    #[test]
    fn config_override_remaps_one_action() {
        let mut cfg = BTreeMap::new();
        cfg.insert("new_session".to_string(), "N".to_string());
        let resolved = resolve(&cfg);
        assert_eq!(resolved["new_session"], "N");
        // Everything else stays on its default.
        assert_eq!(resolved["fleet"], "s");
        assert_eq!(resolved["quit"], "q");
        assert_eq!(binding_for(&cfg, "new_session"), "N");
        let mut seen = std::collections::HashSet::new();
        for (_, key) in &resolved {
            assert!(seen.insert(key.clone()));
        }
    }

    #[test]
    fn unknown_action_names_are_ignored() {
        let mut cfg = BTreeMap::new();
        cfg.insert("new_session".to_string(), "N".to_string());
        cfg.insert("does_not_exist".to_string(), "z".to_string());
        let resolved = resolve(&cfg);
        assert!(!resolved.contains_key("does_not_exist"));
        assert_eq!(resolved["new_session"], "N");
        assert_eq!(resolved["fleet"], "s");
        assert_eq!(resolved.len(), ACTIONS.len());
    }

    #[test]
    fn remapping_does_not_break_the_default_path() {
        let mut cfg = BTreeMap::new();
        cfg.insert("search".to_string(), "F".to_string());
        let resolved = resolve(&cfg);
        // The remapped action resolves to the new key…
        assert_eq!(resolved["search"], "F");
        // …and binding_for still returns the built-in default for untouched/empty configs.
        let empty = BTreeMap::new();
        assert_eq!(binding_for(&empty, "search"), "f");
        assert_eq!(binding_for(&empty, "new_session"), "n");
    }
}
