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

// ───────────────────────────────────────────────────────────────────────────
// Native macOS menu bar.
//
// winit 0.30 ships no menu API at all, so a real terminal keeps a bare, menu-less
// app — no Apple menu, no File/Tab/Window menus, no discoverability for Cmd+T/W/Q.
// We build a proper AppKit main menu whose key equivalents (Cmd+T, Cmd+W, Cmd+Q,
// Cmd+Shift+[ / ], Cmd+Shift+T, Cmd+Shift+P) dispatch into a tiny Rust-owned ObjC
// target. The target only pushes onto a static queue; the winit loop drains it once
// per frame and maps each command onto the exact handlers the in-app Cmd shortcuts
// route to.
//
// Key-equivalent ownership: a menu item with a key equivalent makes AppKit consume
// that keystroke during `performKeyEquivalent:` before it reaches the key window, so
// the winit `cmd_shortcut` fallback never double-fires — it only runs for chords that
// have no menu item. The two paths never overlap, and nothing that worked before
// stops working if the menu fails to take a chord.
// ───────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub mod menu {
    use std::sync::{Mutex, OnceLock};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{define_class, extern_methods, sel, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSEventModifierFlags as ModifierFlags, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    /// A menu-bar command the OS dispatched. Drained by the winit loop each frame and mapped onto
    /// the same handlers the Cmd shortcuts route to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MenuAction {
        NewTab,
        CloseTab,
        Quit,
        ReopenTab,
        NextTab,
        PrevTab,
        CommandPalette,
    }

    static QUEUE: OnceLock<Mutex<Vec<MenuAction>>> = OnceLock::new();
    fn queue() -> &'static Mutex<Vec<MenuAction>> {
        QUEUE.get_or_init(|| Mutex::new(Vec::new()))
    }
    fn push(a: MenuAction) {
        if let Ok(mut q) = queue().lock() {
            q.push(a);
        }
    }

    /// Drain any menu commands queued since the last frame (best-effort, never blocks the loop).
    pub fn drain_actions() -> Vec<MenuAction> {
        match queue().lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }

    // The ObjC target receiving NSMenu action messages. Each item aims at the one shared instance
    // with its own selector; the selector body only pushes the matching [`MenuAction`].
    define_class!(
        // SAFETY: NSObject imposes no subclassing requirements, and the class owns no Rust payload
        // that must be freed on dealloc — each selector just pushes into a static queue.
        #[unsafe(super(NSObject))]
        #[name = "HarnessMenuTarget"]
        pub struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(harnessNewTab:))]
            unsafe fn new_tab(&self, _: &NSObject) {
                push(MenuAction::NewTab);
            }
            #[unsafe(method(harnessCloseTab:))]
            unsafe fn close_tab(&self, _: &NSObject) {
                push(MenuAction::CloseTab);
            }
            #[unsafe(method(harnessQuit:))]
            unsafe fn quit(&self, _: &NSObject) {
                push(MenuAction::Quit);
            }
            #[unsafe(method(harnessReopenTab:))]
            unsafe fn reopen_tab(&self, _: &NSObject) {
                push(MenuAction::ReopenTab);
            }
            #[unsafe(method(harnessNextTab:))]
            unsafe fn next_tab(&self, _: &NSObject) {
                push(MenuAction::NextTab);
            }
            #[unsafe(method(harnessPrevTab:))]
            unsafe fn prev_tab(&self, _: &NSObject) {
                push(MenuAction::PrevTab);
            }
            #[unsafe(method(harnessPalette:))]
            unsafe fn palette(&self, _: &NSObject) {
                push(MenuAction::CommandPalette);
            }
        }
    );

    impl MenuTarget {
        extern_methods!(
            #[unsafe(method(new))]
            pub fn new(mtm: MainThreadMarker) -> Retained<Self>;
        );
    }

    static TARGET: OnceLock<Retained<MenuTarget>> = OnceLock::new();

    // NSCommandKeyMask=1<<20, NSShiftKeyMask=1<<17.
    const CMD: u64 = 1u64 << 20;
    const SHIFT: u64 = 1u64 << 17;

    /// Append a `title` item to `menu` with the given action/key equivalent, targeting our target.
    /// `key` None yields a plain (mouse-only) item; otherwise its modifier mask is applied.
    unsafe fn add(
        menu: &NSMenu,
        title: &str,
        action: objc2::runtime::Sel,
        target: &AnyObject,
        key: Option<(&str, u64)>,
        mtm: MainThreadMarker,
    ) {
        let t = NSString::from_str(title);
        let item = NSMenuItem::new(mtm);
        unsafe {
            item.setTitle(&t);
            item.setAction(Some(action));
            item.setTarget(Some(target));
            if let Some((k, mask)) = key {
                let ks = NSString::from_str(k);
                item.setKeyEquivalent(&ks);
                item.setKeyEquivalentModifierMask(ModifierFlags(mask as usize));
            }
        }
        menu.addItem(&item);
    }

    /// Install the native main menu on `NSApp`. Must run on the main thread (it is: at launch,
    /// before any window appears). Best-effort; the terminal keeps running without menus if a
    /// lookup fails, since only the menu tree is dropped, never app state.
    pub unsafe fn install_main_menu() {
        // SAFETY: callers invoke this once at launch before the event loop runs; if we are somehow
        // off the main thread (no marker) we bail rather than touch AppKit from the wrong thread.
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let target: Retained<MenuTarget> = TARGET.get_or_init(|| MenuTarget::new(mtm)).clone();
        let target_ref: &AnyObject = target.as_ref();
        let app = NSApplication::sharedApplication(mtm);

        let main = NSMenu::new(mtm);

        // App menu (first item, bare submenu titled with the app name).
        let app_item = NSMenuItem::new(mtm);
        let app_menu = NSMenu::new(mtm);
        add(
            &app_menu,
            "Quit Harness Terminal",
            sel!(harnessQuit:),
            target_ref,
            Some(("q", CMD)),
            mtm,
        );
        app_item.setSubmenu(Some(&app_menu));
        main.addItem(&app_item);

        // File.
        let file_item = NSMenuItem::new(mtm);
        let file_menu = NSMenu::new(mtm);
        add(
            &file_menu,
            "New Tab",
            sel!(harnessNewTab:),
            target_ref,
            Some(("t", CMD)),
            mtm,
        );
        add(
            &file_menu,
            "Reopen Last Tab",
            sel!(harnessReopenTab:),
            target_ref,
            Some(("t", CMD | SHIFT)),
            mtm,
        );
        add(
            &file_menu,
            "Close Tab",
            sel!(harnessCloseTab:),
            target_ref,
            Some(("w", CMD)),
            mtm,
        );
        file_item.setSubmenu(Some(&file_menu));
        main.addItem(&file_item);

        // Tab.
        let tab_item = NSMenuItem::new(mtm);
        let tab_menu = NSMenu::new(mtm);
        add(
            &tab_menu,
            "Previous Tab",
            sel!(harnessPrevTab:),
            target_ref,
            Some(("[", CMD | SHIFT)),
            mtm,
        );
        add(
            &tab_menu,
            "Next Tab",
            sel!(harnessNextTab:),
            target_ref,
            Some(("]", CMD | SHIFT)),
            mtm,
        );
        tab_item.setSubmenu(Some(&tab_menu));
        main.addItem(&tab_item);

        // Window.
        let win_item = NSMenuItem::new(mtm);
        let win_menu = NSMenu::new(mtm);
        add(
            &win_menu,
            "Command Palette",
            sel!(harnessPalette:),
            target_ref,
            Some(("p", CMD | SHIFT)),
            mtm,
        );
        win_item.setSubmenu(Some(&win_menu));
        main.addItem(&win_item);

        app.setMainMenu(Some(&main));
    }
}
