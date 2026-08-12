//! Prefix keybinding resolution.
//!
//! After the prefix chord (`Ctrl+Space`, or `Ctrl+\` when macOS owns `Ctrl+Space` — see
//! `crate::macos`), a single key triggers an action (tmux-style). These binds are configurable
//! via the `[keybindings]` block so a user can remap them without recompiling. An absent or empty
//! `[keybindings]` block keeps today's exact defaults — see `DEFAULT_KEYS`.

use std::collections::BTreeMap;

use winit::keyboard::{Key, ModifiersState, NamedKey};

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
    ("next_host", "H"),
    ("dnd", "M"),
    ("reconnect_all", "T"),
    ("close_quiet", "C"),
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
    ("interrupt", "!"),
    ("session_info", "i"),
    ("mark_all_read", "I"),
    ("toggle_focus", "v"),
    ("pin", "A"),
    ("next_pinned", "P"),
    ("reconnect", "R"),
    ("destroy", "D"),
];

/// Normalize the spacebar to the `Character(" ")` the rest of the codebase matches. macOS's
/// winit reports the space key as `Named(Space)` (it routes through `code_to_key`), while every
/// other path in the app matches `Character(" ")` — so without this, a plain space is silently
/// swallowed by the shell and every text field. Other keys pass through untouched.
pub fn normalize_space(key: &Key) -> Key {
    match key {
        Key::Named(NamedKey::Space) => Key::Character(" ".into()),
        other => other.clone(),
    }
}

/// Prefix *chord* detection: the key(s) that enter command mode, as opposed to the single key
/// pressed after the prefix. Ctrl+Space is the primary, tmux-style chord; Ctrl+\\ is the
/// fallback because on macOS the system's input-source switcher owns Ctrl+Space when a second
/// layout is enabled (see `crate::macos`), so that keystroke never reaches the app. A plain
/// space always types normally — only the control chord enters command mode.
pub fn is_prefix_press(key: &Key, mods: &ModifiersState) -> bool {
    let is_space = matches!(key, Key::Character(c) if c == " ")
        || matches!(key, Key::Named(NamedKey::Space));
    // `|` is Shift+Ctrl+Backslash on US layouts (winit lets SHIFT through to the logical key),
    // so accept it too — the user reaching for the fallback chord with shift held still works.
    let is_backslash = matches!(key, Key::Character(c) if c == "\\" || c == "|");
    mods.control_key() && (is_space || is_backslash)
}

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

    #[test]
    fn ctrl_space_is_a_prefix_press_but_plain_space_is_not() {
        let mut ctrl = ModifiersState::default();
        ctrl.insert(ModifiersState::CONTROL);
        assert!(is_prefix_press(&Key::Character(" ".into()), &ctrl));
        assert!(is_prefix_press(&Key::Named(NamedKey::Space), &ctrl));
        assert!(!is_prefix_press(&Key::Character(" ".into()), &ModifiersState::default()));
        assert!(!is_prefix_press(&Key::Named(NamedKey::Space), &ModifiersState::default()));
        // Control alone, without a space, is never a prefix press.
        assert!(!is_prefix_press(&Key::Character("n".into()), &ctrl));
    }

    #[test]
    fn ctrl_backslash_is_the_fallback_chord() {
        let mut ctrl = ModifiersState::default();
        ctrl.insert(ModifiersState::CONTROL);
        assert!(is_prefix_press(&Key::Character("\\".into()), &ctrl));
        // Without control it's just a backslash, not a prefix.
        assert!(!is_prefix_press(&Key::Character("\\".into()), &ModifiersState::default()));
        // Shift+Ctrl+Backslash is the pipe on most layouts, and as a coincidental fallback should
        // still be a valid prefix press (the physical key is what the user is reaching for).
        let mut ctrl_shift = ctrl;
        ctrl_shift.insert(ModifiersState::SHIFT);
        assert!(is_prefix_press(&Key::Character("|".into()), &ctrl_shift));
    }

    #[test]
    fn normalize_space_rewrites_named_space_to_a_character_space() {
        // The (macOS) spacebar arrives as Named(Space); it must become Character(" ").
        assert_eq!(normalize_space(&Key::Named(NamedKey::Space)), Key::Character(" ".into()));
        // Everything else is untouched.
        assert_eq!(normalize_space(&Key::Character("a".into())), Key::Character("a".into()));
        assert_eq!(normalize_space(&Key::Named(NamedKey::Enter)), Key::Named(NamedKey::Enter));
        assert_eq!(normalize_space(&Key::Named(NamedKey::Escape)), Key::Named(NamedKey::Escape));
        assert_eq!(normalize_space(&Key::Named(NamedKey::ArrowUp)), Key::Named(NamedKey::ArrowUp));
    }
}
