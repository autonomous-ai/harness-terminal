//! macOS-specific runtime checks (no-op on other platforms).
//!
//! The one OS negotiation a native terminal has to win on macOS is the system's "Select the
//! previous input source" shortcut, which is `Ctrl+Space` by default. When a second input source
//! is enabled (English ABC + Vietnamese Telex being the common case), macOS *consumes* `Ctrl+Space`
//! to switch layouts and the keystroke never reaches the app — so a tmux-style `Ctrl+Space` prefix
//! is silently dead even though every other key works. These helpers detect that exact condition
//! so the app can fall back (`Ctrl+\`, which the OS never grabs) and tell the diver why instead of
//! leaving them guessing why their prefix stopped answering.
//!
//! Reading the preferences: we shell out to `plutil -extract <key> json -o - <file>`. Extracting
//! just the subtree we need (instead of converting the whole file to JSON) matters — some plists
//! (e.g. `com.apple.HIToolbox`) carry blobs and non-string keys that make whole-file JSON
//! conversion fail with "Invalid object in plist for JSON format", while the extracted subtree is
//! always JSON-safe.

use std::process::Command;

/// True when macOS currently owns `Ctrl+Space` (the input-source switcher) so the keystroke can't
/// reach the app. False on non-macOS and whenever the preference plists are unreadable (fail open:
/// assume the key works and let the user's real config decide).
///
/// Detects BOTH halves of the condition: the `Ctrl+Space` system shortcut has to be registered
/// (`com.apple.symbolichotkeys` → hotkey `"60"` enabled) AND a second selectable input source has
/// to exist so that shortcut actually fires (`com.apple.HIToolbox` → at least two Keyboard Layout /
/// Input Mode sources). With only one source the shortcut is a registered no-op and the keystroke
/// still flows through to the app.
#[cfg(target_os = "macos")]
pub fn ctrl_space_claimed() -> bool {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| ".".into());
    let symbolic = extract_json(
        &format!("{home}/Library/Preferences/com.apple.symbolichotkeys.plist"),
        "AppleSymbolicHotKeys.60",
    );
    let hitoolbox = extract_json(
        &format!("{home}/Library/Preferences/com.apple.HIToolbox.plist"),
        "AppleEnabledInputSources",
    );
    match (symbolic, hitoolbox) {
        (Some(hotkey), Some(sources)) => {
            hotkey60_enabled(&hotkey) && selectable_sources(&sources) >= 2
        }
        _ => false,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ctrl_space_claimed() -> bool {
    false
}

/// `plutil -extract <key> json -o - <file>` for a single key subtree. Returns the JSON text, or
/// None when the file is absent or unreadable (fails open, never panics).
fn extract_json(path: &str, key: &str) -> Option<String> {
    if !std::path::Path::new(path).exists() {
        return None;
    }
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "json", "-o", "-", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Is the macOS "previous input source" hotkey (system shortcut 60 = Ctrl+Space) enabled? Accepts
/// `plutil -extract` output: `{"enabled": true | "1" | "true" | 1, ...}`. Anything else → not active.
fn hotkey60_enabled(hotkey_json: &str) -> bool {
    let v: serde_json::Value = match serde_json::from_str(hotkey_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    match v.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// Count selectable keyboard input sources in `plutil -extract AppleEnabledInputSources` output
/// (an array of dicts). "Non Keyboard Input Method" sources (CharacterPalette, PressAndHold…) never
/// make the input-source switcher active, so they don't count.
fn selectable_sources(sources_json: &str) -> usize {
    let v: serde_json::Value = match serde_json::from_str(sources_json) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return 0,
    };
    arr.iter()
        .filter_map(|s| s.get("InputSourceKind").and_then(|k| k.as_str()))
        .filter(|k| *k == "Keyboard Layout" || *k == "Input Mode")
        .count()
}

/// The one-line explanation flashed when the backslash fallback chord is used on a mac that
/// claims `Ctrl+Space` (its input-source switcher, active when a second layout is enabled). The
/// wording adapts to the configured prefix: if the user actually chose `space` as the prefix it
/// tells them the OS is eating their prefix; otherwise it notes the Space chord is kept but
/// claimed, so the configured primary is the one to use.
pub fn ctrl_space_notice(primary: &str) -> String {
    let label = crate::keys::prefix_label(primary);
    if primary == "space" {
        format!(
            "macOS owns Ctrl+Space (input-source switcher) — disable it in System Settings ▸ Keyboard ▸ Keyboard Shortcuts ▸ Input Sources so the prefix ({label}) can hear the key"
        )
    } else {
        format!(
            "{label} is the prefix · Ctrl+Space stays claimed by macOS's input-source switcher — disable that in System Settings ▸ Keyboard ▸ Keyboard Shortcuts ▸ Input Sources to also use Ctrl+Space"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkey(enabled: &str) -> String {
        format!(
            r#"{{"enabled":{enabled},"value":{{"type":"standard","parameters":[32,49,262144]}}}}"#
        )
    }

    fn sources(json: &str) -> String {
        json.to_string()
    }

    #[test]
    fn disabled_hotkey_is_not_claimed() {
        assert!(!hotkey60_enabled(&hotkey("false")));
        assert!(!hotkey60_enabled(&hotkey("\"0\"")));
    }

    #[test]
    fn enabled_hotkey_parses_across_macos_variants() {
        // plutil emits a real JSON boolean on current macOS…
        assert!(hotkey60_enabled(&hotkey("true")));
        // …but the underlying pref is sometimes a "1"/"true" string on older macOS.
        assert!(hotkey60_enabled(&hotkey("\"1\"")));
        assert!(hotkey60_enabled(&hotkey("\"true\"")));
    }

    #[test]
    fn selectable_count_is_only_keyboard_layouts_and_input_modes() {
        assert_eq!(
            selectable_sources(&sources(
                r#"[{"InputSourceKind":"Keyboard Layout"},{"InputSourceKind":"Input Mode"},{"InputSourceKind":"Non Keyboard Input Method"},{"InputSourceKind":"Non Keyboard Input Method"}]"#
            )),
            2
        );
        assert_eq!(
            selectable_sources(&sources(r#"[{"InputSourceKind":"Keyboard Layout"}]"#)),
            1
        );
        assert_eq!(selectable_sources(&sources(r#"[]"#)), 0);
    }

    #[test]
    fn claim_requires_both_the_hotkey_and_a_second_source() {
        let two =
            sources(r#"[{"InputSourceKind":"Keyboard Layout"},{"InputSourceKind":"Input Mode"}]"#);
        let one = sources(r#"[{"InputSourceKind":"Keyboard Layout"}]"#);
        // The real machine shape: hotkey on + two sources = claimed.
        let claimed = hotkey60_enabled(&hotkey("true")) && selectable_sources(&two) >= 2;
        assert!(claimed);
        // Hotkey on but only one source = the shortcut never fires; keystroke passes through.
        let pass = hotkey60_enabled(&hotkey("true")) && selectable_sources(&one) >= 2;
        assert!(!pass);
        // Hotkey off entirely = never claimed.
        let off = hotkey60_enabled(&hotkey("false")) && selectable_sources(&two) >= 2;
        assert!(!off);
    }

    #[test]
    fn malformed_input_fails_open() {
        assert!(!hotkey60_enabled("{ not json"));
        assert_eq!(selectable_sources("{ not json"), 0);
        assert_eq!(selectable_sources("{}"), 0);
    }
}

// True macOS window-level tabs (system title-bar tabbing).
//
// winit has no API for AppKit's native tabbed windows, but every winit window is backed by a real
// `NSWindow`. We grab that pointer via winit's raw window handle and drive it with objc2 (the same
// stack winit itself pins). This lets us group multiple real `NSWindow`s into one native tabbed set
// so the system draws the title-bar tab bar with the OS traffic-light chrome. We only reliably get
// the `NSView*` from winit, so we ask it for its owning window (`[view window]`).
// ───────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub mod tabs {
    use objc2::runtime::AnyObject;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    /// Pull the live `NSWindow*` for a winit window (via its NSView's `window` property). None on
    /// non-Apple platforms / when there's no window.
    pub(crate) fn ns_window(w: &Window) -> Option<*mut AnyObject> {
        let raw = w.window_handle().ok()?.as_raw();
        let view = match raw {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr().cast::<AnyObject>(),
            _ => return None,
        };
        let wnd: *mut AnyObject = unsafe { objc2::msg_send![view, window] };
        if wnd.is_null() {
            None
        } else {
            Some(wnd)
        }
    }

    /// Advertise native tabbing (`NSTabbingModePreferred` = 1) so macOS offers the system tab bar.
    pub(crate) fn enable_tabbing(w: &Window) {
        let Some(obj) = ns_window(w) else { return };
        unsafe {
            let _: () = objc2::msg_send![obj, setTabbingMode: 1isize];
        }
    }

    /// Splice `sibling` into `primary`'s window group as a tab (`addTabbedWindow:ordered:` with
    /// `NSWindowAbove` = 1), so macOS draws them as one tabbed window set.
    pub(crate) fn join_tab_group(primary: &Window, sibling: &Window) {
        let Some(a) = ns_window(primary) else { return };
        let Some(b) = ns_window(sibling) else { return };
        unsafe {
            let _: () = objc2::msg_send![a, addTabbedWindow: b, ordered: 1isize];
        }
    }
}
