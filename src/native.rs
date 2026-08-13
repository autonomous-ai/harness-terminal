//! Standalone native window (our terminal — no host emulator).
//!
//! winit provides the window + event loop; softbuffer provides a CPU framebuffer we draw the
//! alacritty grid into; ab_glyph rasterizes glyphs. This replaces the ratatui/crossterm TUI as the
//! default shell — the fleet/tunnel/reconnect machinery in `session.rs`/`transport.rs` is untouched
//! and shared. Chrome (tab bar, palette, status) is drawn natively with `draw_text`.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize, Size};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Window, WindowId};

use softbuffer::{Context, Surface};

use crate::app::{App, Overlay};
use crate::engines::ENGINES;
use crate::render::{
    argb, draw_grid, draw_text, fill_rect, filled_round_top, text_width, Framebuffer, GlyphCache,
};
use crate::session::TermSize;

/// Chromeless text colors (macOS style).
const CHROME_FG: (u8, u8, u8) = (0xcc, 0xcc, 0xcc);
const CHROME_DIM: (u8, u8, u8) = (0x66, 0x66, 0x66);
/// Muted red for the fleet-triage "N panes down" count — a host went dark, not a busy signal.
const CHROME_ERR: (u8, u8, u8) = (0xf0, 0x6a, 0x6a);
/// Status accent for a session that is actively producing output (busy) — warm amber so it reads
/// as "in motion" from across the war-room, distinct from the attention blue and outage red.
const CHROME_BUSY: (u8, u8, u8) = (0xf2, 0xb0, 0x4f);
/// Status accent for a session that has gone quiet and is awaiting you (⌛) — attention blue.
const CHROME_QUIET: (u8, u8, u8) = (0x6a, 0x9f, 0xf2);
/// Status accent for a session that just reconnected (↻) — recovery green, all clear.
const CHROME_RECOVER: (u8, u8, u8) = (0x4f, 0xc0, 0x7a);
const WHITE: (u8, u8, u8) = (0xff, 0xff, 0xff);
/// Elevated chrome surfaces (tab bar / status line panels) so the shell chrome reads as designed
/// panels rather than text floating on the terminal background. Slightly darker than the theme's
/// grid so the bars recede and the content reads forward; the active-tab pill sits a step lighter.
const CHROME_BG: (u8, u8, u8) = (0x12, 0x13, 0x1c);
/// Pre-computed opaque panel pixel (alpha already applied) for fast row-clears.
const CHROME_BG_PX: u32 = argb(255, 0x12, 0x13, 0x1c);
/// Active-tab background pill — a distinct, slightly-lit slab so the current session reads at a
/// glance from across the bar (iTerm2/macOS-style).
const CHROME_ACTIVE_BG: (u8, u8, u8) = (0x27, 0x29, 0x36);
/// One-pixel hairline separating the chrome strip from the terminal grid below it, so the tab/status
/// bars read as a designed surface rather than text bleeding into the grid.
const CHROME_HAIR: (u8, u8, u8) = (0x22, 0x24, 0x31);
/// Top bevel on the raised active tab sheet — a lighter hairline at the sheet's top edge.
const CHROME_SHEET_HI: (u8, u8, u8) = (0x34, 0x37, 0x45);
/// Hover chip behind an inactive tab under the pointer, so the target reads before you commit.
const CHROME_HOVER: (u8, u8, u8) = (0x1c, 0x1e, 0x28);

/// One fleet-search hit: which tab (index into `app.tabs`), the grid line in that session's
/// scrollback, and the column where the match text begins. Sorted by (tab, line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FleetMatch {
    tab: usize,
    line: i32,
    col: usize,
}

/// A named action the command palette can run (the prefix+; palette). Each variant mirrors the
/// effect of one existing prefix command, so the palette is one discoverable home for the growing
/// set of prefix bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteAction {
    NewSession,
    RemoteAttach,
    SessionPalette,
    FindInTab,
    SearchAll,
    CopyScrollback,
    ExportLog,
    CopyIdentity,
    CopyFleet,
    Broadcast,
    Peek,
    FleetGrid,
    UndoClose,
    Duplicate,
    SessionInfo,
    MarkAllRead,
    ToggleFocus,
    Pin,
    NextPinned,
    NextBusy,
    NextQuiet,
    NextDown,
    MuteActive,
    Rename,
    PageUp,
    ScrollTop,
    ScrollBottom,
    NextHost,
    Hosts,
    Dnd,
    Reconnect,
    ReconnectAll,
    Destroy,
    Interrupt,
    InterruptAll,
    CloseQuiet,
    Help,
    Quit,
}

impl PaletteAction {
    /// The full, ordered list of palette rows, built once at startup.
    fn all_rows() -> Vec<(&'static str, PaletteAction)> {
        use PaletteAction::*;
        vec![
            ("new session (engine picker)", NewSession),
            ("attach to a remote pane@host", RemoteAttach),
            ("jump to a session (palette)", SessionPalette),
            ("find in current tab", FindInTab),
            ("search all sessions", SearchAll),
            ("copy whole scrollback", CopyScrollback),
            ("export scrollback to .log file", ExportLog),
            ("copy session identity (engine@host)", CopyIdentity),
            ("copy whole fleet summary (all tabs)", CopyFleet),
            ("broadcast a line to all sessions", Broadcast),
            ("peek at all session tails", Peek),
            ("fleet grid: live tails of every session", FleetGrid),
            ("undo close (reopen last)", UndoClose),
            ("duplicate active tab (fork same engine@host)", Duplicate),
            ("show session info (kind/host/task)", SessionInfo),
            (
                "mark all tabs as read (clear busy + bell badges)",
                MarkAllRead,
            ),
            ("toggle focus mode (hide tab bar + status)", ToggleFocus),
            ("pin/unpin active tab (protect from close)", Pin),
            ("jump to next pinned tab", NextPinned),
            ("jump to next busy tab (just produced output)", NextBusy),
            ("jump to next quiet (awaiting-you) tab", NextQuiet),
            ("jump to next down/reconnecting tab", NextDown),
            (
                "mute/unmute active tab (stop busy badge + OS ping)",
                MuteActive,
            ),
            ("rename active session", Rename),
            ("jump to next host (page fleet by machine)", NextHost),
            ("host overview (which machines are up)", Hosts),
            ("toggle do-not-disturb (mute all OS notifications)", Dnd),
            ("force reconnect active tab (bypass backoff)", Reconnect),
            ("force reconnect ALL down panes", ReconnectAll),
            ("kill active tab's pane (destroy remote session)", Destroy),
            ("send Ctrl-C to active tab (stop the run)", Interrupt),
            (
                "send Ctrl-C to every session (stop the fleet)",
                InterruptAll,
            ),
            ("close all quiet (done) tabs", CloseQuiet),
            ("page up (review this tab's scrollback)", PageUp),
            ("scroll to top (start of the log)", ScrollTop),
            ("scroll to bottom (back to live)", ScrollBottom),
            ("show this help", Help),
            ("quit", Quit),
        ]
    }
}

/// The native application: window + framebuffer surface + app state + glyph cache.
struct Application {
    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    size: PhysicalSize<u32>,
    font_px: u32,
    cell_w: u32,
    cell_h: u32,
    app: App,
    cache: GlyphCache,
    /// Resolved render palette (built-in colors unless a `[theme]` block is configured).
    colors: crate::render::Colors,
    prefix_down: bool,
    mods: ModifiersState,
    /// Set at startup when macOS owns Ctrl+Space (its input-source switcher grabs the
    /// keystroke when a second layout is enabled, e.g. ABC + Vietnamese). When true the
    /// prefix answers Ctrl+\ instead and hints advertise that; without this the "prefix is
    /// dead" failure mode is a silent mystery. See `crate::macos`.
    prefix_claimed: bool,
    /// The prefix's leading key (`prefix_key` config, default "h" → Ctrl+H / "Ctrl Harness", the
    /// tmux-Ctrl+B equivalent). Ctrl+Space and Ctrl+\ stay accepted as fixed fallback chords, so
    /// this only picks the advertised primary, not the safety nets.
    prefix_key: String,
    /// One-shot: once the Ctrl+Space-claimed explanation has flashed (on the first Ctrl+\
    /// prefix press), don't repeat it every single time the fallback chord is used.
    prefix_alt_notice: bool,
    /// Resolved `quiet_after_secs` threshold (default 120), cached once at startup. The quiet
    /// detector (triage count, fleet grid, `prefix+z`) runs every frame; re-reading the config file
    /// from disk each time would be wasteful, so the threshold is resolved here instead.
    quiet_secs: u64,
    /// The tab that was active before the current one, so prefix+l can flip back to it (tmux
    /// last-window muscle memory). Cleared to None when tabs get rearranged out from under it.
    last_active: Option<usize>,
    /// Active search query ("" when the Find overlay is closed).
    find_query: String,
    /// MRU of find queries actually run, most-recent first (cap 16), persisted across restarts.
    /// Up in the find bar with an empty query recalls the most recent — iTerm2-style search memory.
    find_history: Vec<String>,
    /// Find toggles: `c` case-sensitive, `w` whole-word, both default off (iTerm2-style options).
    find_opts: crate::render::FindOptions,
    /// In-progress rename for the active tab ("" when the Rename overlay is closed).
    rename_query: String,
    /// In-progress broadcast line ("" when the Broadcast overlay is closed). Enter sends it to every
    /// session; backspace/escape edit/cancel it.
    broadcast_query: String,
    /// The last broadcast line that was actually sent, pre-filled on the next open so a repeat
    /// command (e.g. `git pull` on every host) survives without retyping. Kept across restarts via
    /// `restore::save_last_broadcast`.
    last_broadcast: String,
    /// MRU of broadcast lines actually sent, most-recent first (cap 16). Shift+Up/Down in the
    /// broadcast overlay recalls old commands to repeat, e.g. alternating `git pull` / `make build`
    /// across machines. Persisted via `restore::save_broadcast_history`.
    broadcast_hist: Vec<String>,
    /// Index into `broadcast_hist` that a Shift+Up/Down recall currently points at; None = editing
    /// a fresh line.
    hist_sel: Option<usize>,
    /// Per-tab broadcast target selection. Grows/shrinks to match `tabs`; open targets are is_static, closed toggled off. Defaults all-on.
    broadcast_targets: Vec<bool>,
    /// Index of the row the broadcast overlay's focus is on (all-on by default).
    broadcast_sel: usize,
    /// In-progress working-directory for the NewSession picker ("" when blank → config start_cwd).
    new_cwd: String,
    /// Per-tab mute state (prefix+m). A muted tab's busy nudge + badge are suppressed so a noisy
    /// pane a diver doesn't care about stops nagging. Grows/shrinks with the tab set.
    muted: Vec<bool>,
    /// Per-tab pin state (prefix+a, default key `A`). A pinned tab can't be closed with `x` /
    /// prefix+`close_tab` (or via the palette), so a long-running agent a diver cares about can't
    /// be fat-fingered away — you must unpin first. Grows/shrinks with the tab set.
    pinned: Vec<bool>,
    /// Per-tab count of new scrollback lines produced since we last looked (output delta). Populated
    /// by `activity_flags`; the tab bar shows it as the badge magnitude so a diver reads how hot a
    /// pane is, not just *that* it moved.
    grew_delta: Vec<usize>,
    /// Cumulative unread scrollback lines per tab since the user last looked (or marked read), used
    /// solely for the persistent `!N` badge. Unlike `grew_delta` (a per-frame delta that feeds the
    /// busy detector and live-pump), this accumulates across frames and only resets when the tab is
    /// focused or `mark_all_read` runs, so a settled agent's badge lingers per the docs.
    unread: Vec<usize>,
    /// Whether any tab produced output the last frame we rendered (the live "busy" signal used to
    /// decide if the render loop should keep pumping at full rate vs. drop to its idle tick). Set
    /// during `render` from the same `activity_flags` pass that draws the badges, so it reflects
    /// exactly what the current frame showed — not the cumulative, still-unread `grew_delta`.
    live_busy: bool,
    /// Per-tab scrollback length last seen by the idle loop, so a quiet (not-rendering) loop can
    /// cheaply detect that NEW output arrived and request a repaint without re-uploading the whole
    /// frame every tick. Kept in sync during every render so the idle detector only flags genuinely
    /// fresh content.
    detect_len: Vec<usize>,
    /// Per-tab rolling signature of the VISIBLE grid (see `render::visible_signature`), used with
    /// `detect_len` so the idle-wake detector also notices output that redraws the screen in place
    /// without growing scrollback (vim/htop/TUI redraws, spinner lines). Resized to the tab count on
    /// each detect, mirroring `detect_len`.
    content_sig: Vec<u64>,
    /// Pending OS notifications not yet delivered this frame, (kind "busy"|"bell", session index).
    /// Queued by `poll_bells`/`activity_flags` and drained together at the end of the frame so
    /// simultaneous busy/bell events across the fleet coalesce into ONE notification instead of one
    /// osascript popup per tab (a `broadcast` to every host would otherwise fan out N launches).
    /// Muted tabs are never queued. Emptied each frame by `flush_notifications`.
    pending_notify: Vec<(String, usize)>,
    /// Per-tab: whether the session was down (disconnected) on the previous frame. Used to detect the
    /// down→alive recovery edge and nudge the diver once (a `↻` badge + one notification), so a box
    /// that comes back is announced rather than silently reappearing.
    was_down: Vec<bool>,
    /// Per-tab: whether the session was alive (connected) on the previous frame. Used to detect the
    /// alive→down edge and nudge the diver once — the fleet event alert that was previously missing
    /// (only bell/busy/recover notified). New tabs start `false` so a pane restored already-dead
    /// doesn't nag on its first frame.
    was_alive: Vec<bool>,
    /// Monotonic instant until which each tab shows a `↻` recovery badge after its pane reconnected.
    /// Mirrors `bell_until`'s self-fading timeline.
    recover_until: Vec<Option<std::time::Instant>>,
    /// Global Do-Not-Disturb (prefix+M): while on, NO OS notifications are fired fleet-wide — neither
    /// the backgrounded-busy nag nor the terminal-bell popup. In-bar busy badges stay (they're not
    /// interruptions), just no popups. A single bool so nothing else in the fleet state shifts.
    dnd: bool,
    /// The currently-focused search match (absolute line, col, width); recomputed on each query
    /// change / Enter and passed to draw_grid for highlighting.
    find_hit: Option<crate::render::Find>,
    /// Every match of the active query (line, col, width), so draw_grid can highlight all of them
    /// in yellow while the focused one shows orange.
    find_all: Vec<crate::render::Find>,
    /// Index into `find_all` of the currently-focused match (the "N of M" cursor).
    find_index: usize,
    /// Live fleet-search query (the FleetSearch overlay). Same anatomy as `find_query` but the
    /// recompute sweeps every tab once instead of only the active one.
    fleet_q: String,
    /// Every matching (tab, line, col) across all open sessions, sorted by tab then line.
    fleet_matches: Vec<FleetMatch>,
    /// Index into `fleet_matches` of the currently-selected hit (the highlighted list row).
    fleet_sel: usize,
    /// Whether we're in tmux-style copy mode (prefix+[). While active, keystrokes navigate a read
    /// cursor instead of reaching the shell, and `v` starts/extends a selection to copy.
    copy_mode: bool,
    /// Copy-mode read cursor: (line, col) grid coordinates in the scrollback.
    copy_pos: (i32, usize),
    /// Copy-mode anchor: where the block selection started (Some while selecting), in grid coords.
    copy_anchor: Option<(i32, usize)>,
    /// Live copy-mode search query (the `/` prompt). While non-empty, `n`/`N` move cursor between
    /// matches of this text; Enter/`g` jump the cursor to the next match then drop back to nav.
    copy_query: String,
    /// Whether the copy-mode `/` prompt is currently active (accepting typing).
    copy_searching: bool,
    /// Fleet-overlay filter: typing narrows the session list (by session id / tmux pane / engine).
    /// An empty string shows every session; the highlighted row indexes `fleet_filtered`.
    fleet_query: String,
    /// Indices into `app.fleet.fleet` matching the current fleet query, for the overlay.
    fleet_filtered: Vec<usize>,
    /// Peek-overlay selection: which session row the highlight is on (index into `app.tabs`).
    peek_sel: usize,
    /// Peek list scroll offset — how many (capped) rows to skip at the top so ALL tabs are
    /// reachable, not just the first ~10. Bumped when the selection moves below the visible window.
    peek_scroll: usize,
    /// Peek-overlay filter text: typing after `/` narrows the peek list to matching sessions (by
    /// host / engine / name / kind / down+up state) so a big fleet's triage can be focused on one
    /// machine or one agent. Empty shows every session.
    peek_q: String,
    /// Whether the peek filter prompt is open (`/` toggled). While open, character keys build
    /// `peek_q`; Esc/Backspace-empty close it. Filtering applies to nav (`n`, arrows) and Enter.
    peek_filtering: bool,
    /// Indices into `app.tabs` matching `peek_q`, recomputed each frame. `peek_sel`/`peek_scroll`
    /// index this list; when `peek_q` is empty it is the identity mapping.
    peek_filtered: Vec<usize>,
    /// Selected row in the host-overview overlay (`Overlay::Hosts`), an index into `host_tally`.
    hosts_sel: usize,
    /// When `Some`, the host-overview is drilled into that host, listing its sessions (this is the
    /// sub-list you navigate to land on a specific agent run instead of the first tab of the host).
    hosts_host: Option<String>,
    /// Command-palette filter text (the prefix+; palette). Typing narrows `palette_filtered`.
    palette_q: String,
    /// The full, static list of (label, action) rows the palette filters over.
    palette_rows: Vec<(&'static str, PaletteAction)>,
    /// Indices into `palette_rows` matching `palette_q` (recomputed each render).
    palette_filtered: Vec<usize>,
    /// Command-palette selection: index into `palette_filtered`.
    palette_sel: usize,
    /// Set when any quit path fires (prefix+q, or the palette's Quit action); the event loop honors
    /// it at the next `about_to_wait`, applying the same save-then-exit dance as CloseRequested.
    quit_requested: bool,
    /// Set by Cmd+W (close the active tab/window); honored in `about_to_wait` because closing a
    /// native host window needs the `ActiveEventLoop` we only have there.
    close_active_requested: bool,
    /// Mouse state: the cell anchor where a drag-selection started (Some while left button held).
    /// With winit 0.30 we track presses/releases ourselves; dragging updates the selection end.
    mouse_anchor: Option<Point>,
    /// Latest cursor position in framebuffer px (winit's MouseInput has no position; we read this).
    cursor: (f64, f64),
    /// Whether the left mouse button is currently held (for SGR drag-motion reporting to a
    /// mouse-mode PTY). Set on every left press/release.
    mouse_left_down: bool,
    /// Last grid cell we forwarded a motion report to, so fast movement over a mouse-mode TUI
    /// doesn't flood the pipe with a redundant sequence every pixel.
    last_motion_cell: Option<(usize, usize)>,
    /// True while the pointer sits over a clickable URL so we show a hand instead of the arrow.
    /// Updated on CursorMoved so the affordance tracks the pointer without a full redraw.
    over_link: bool,
    /// Last (press-time, press-position, accumulated-click-count) to detect double/triple clicks.
    /// winit 0.30 doesn't hand us a click count, so we time consecutive presses ourselves.
    last_press: Option<(std::time::Instant, (f64, f64), u32)>,
    /// Last OS window title we set, so we only call set_title when it changes (each call is a
    /// platform round-trip).
    window_title: String,
    /// Base font scale (1.0 = 14px). Zoom multiplies font/margins; persisted in restore.
    zoom: f32,
    /// Base font size in px from config (the size a fresh window opens at, before display-scale
    /// and zoom). `zoom` still scales on top; Ctrl+0 resets zoom but not this.
    base_font: f32,
    /// Last-seen scrollback line count per tab index, used to flag tabs that produced output while
    /// not focused. `None` until a tab has been sampled once (so a freshly-spawned tab doesn't
    /// immediately badge).
    seen_history: Vec<usize>,
    /// Monotonic instant each tab last produced output (as observed by `activity_flags` — a grown
    /// scrollback while backgrounded). The focused tab is never stamped, so the tab you're actively
    /// reading never counts as "quiet". Feeds the quiet/waiting triage count and `prefix+z`.
    last_output: Vec<std::time::Instant>,
    /// A transient status-line toast (text, shown-at) for one-shot confirmations like an export
    /// path. Displayed for a couple seconds, then fades on its own. None = no toast.
    flash: Option<(String, std::time::Instant)>,
    /// Resolved prefix keybindings, reversed: key (the char pressed after Ctrl+Space) -> action
    /// name. Built from `crate::keys::resolve`, so it mirrors the hardcoded defaults unless the user
    /// overrides them in `[keybindings]`.
    key_action: std::collections::BTreeMap<String, String>,
    /// Per-tab: whether a backgrounded-output notification has ALREADY fired since the tab last went
    /// quiet. Reset when the tab is focused (looked at) or stops growing, so each silent→busy
    /// transition nudges the user exactly once.
    notified: Vec<bool>,
    /// Focus mode (prefix+v): hide the tab bar + status line so the grid gets the whole window.
    /// A distraction-free dive into one session. Toggle again to bring the chrome back.
    focus: bool,
    /// Opt into macOS native window-level tabs (the system title-bar tab bar). When on, each
    /// session maps to a real `NSWindow` and they're grouped into AppKit's native tab set; the
    /// framebuffer tab strip is hidden in favor of the OS chrome.
    native_tabs: bool,
    /// Show the native-mode bottom status strip (session identity + fleet triage). Mirrors the
    /// `native_status_bar` config; when false the native grid is full-bleed.
    native_status_bar: bool,
    /// Monotonic instant until which a terminal-bell badge is shown for each tab (index). A bell
    /// (a long agent run finishing) shows a 🔔 badge for a few seconds, then fades on its own.
    bell_until: Vec<Option<std::time::Instant>>,
    /// The tab index currently under the pointer (for a hover-preview tooltip of its tail), or
    /// None when the cursor isn't over a tab. Recompute on every CursorMoved against the tab bar.
    hover_tab: Option<usize>,
    /// Last time the remote-fleet overview re-polled the daemon while it was open, so a host that
    /// comes back shows up without pressing `s` (see `about_to_wait`).
    fleet_last_poll: std::time::Instant,
    /// The last-rendered hover tooltip's rect `(px0, py0, panel_w, panel_h)`, so a click inside
    /// the popover can switch to the hovered tab. `None` when no tooltip is showing.
    tooltip_box: Option<(usize, usize, usize, usize)>,
    /// The tab index being drag-reordered (left button pressed on a tab), or None when idle. While
    /// this is Some, drags reorder the tab bar instead of growing a text selection, and release
    /// lands the dragged tab. Clicking a tab (press→release without moving) still switches to it.
    drag_tab: Option<usize>,
    /// The focused tile in the fleet-grid overlay (prefix+e); Enter dives into this session.
    grid_sel: usize,
    /// Tiles marked (Space toggles) in the fleet grid for a targeted-`b` broadcast. Grows/shrinks
    /// with the tab set like the other aux vectors; `b` in the grid pre-scopes `broadcast_targets`
    /// to exactly these sessions.
    grid_marks: Vec<bool>,
    /// The open right-click context menu, or None. A framebuffer-drawn popover of contextual
    /// actions (Copy/Paste/Open Link/Select All/Search selection/New/Close), keyboard-drivable
    /// like the other overlays. Dismissed on Escape or any click outside it.
    ctx: Option<CtxMenu>,
    /// Pixel rects for each rendered tab `(x_start, x_end)` within the top chrome, recomputed
    /// every frame so hit-testing (hover preview + the close ×) tracks the painted bar exactly.
    /// Truncated tabs past the window edge are simply not present past the clip.
    tab_rects: Vec<(usize, usize)>,
    /// Hit rect `(x0, y0, w, h)` of the "new tab" (+) affordance at the strip's right edge, or
    /// None in focus mode. Clicking it opens the New-Session picker (native tab-strip behavior).
    newtab_btn: Option<(usize, usize, usize, usize)>,
    /// Monotonic frame counter, bumped each redraw, so frame-animated affordances (the per-tab
    /// streaming spinner) can cycle without a wall-clock dependency.
    frame: u64,
    /// Native-tab mode (`native_tabs = true`): one real window per session, grouped into AppKit's
    /// system title-bar tab bar. `self.window`/`self.surface`/`self.size` always alias the ACTIVE
    /// host so all the single-window rendering/input code keeps working unchanged; this list drives
    /// grouping and per-session repaint, and `active_host` selects which one the user is looking at.
    hosts: Vec<Host>,
    /// Index into `hosts` of the window the user is currently looking at (the focused one). Its
    /// session is `app.active`.
    active_host: usize,
    /// Reusable render scratch buffer, kept across frames so the hot per-frame path (single-window
    /// or per-host in native mode) doesn't heap-allocate a whole new framebuffer every frame under
    /// streaming. Resized (capacity-preserving) to whatever window is being rendered at the time.
    scratch_fb: crate::render::Framebuffer,
}

/// One real macOS window backing a session in native-tab mode. Each session gets its own `NSWindow`;
/// AppKit groups them into the system title-bar tab bar. `self.window`/`self.surface`/`self.size`
/// alias the active host so the shared single-window code paths still work untouched.
struct Host {
    window: Rc<Window>,
    /// Held solely so the softbuffer `context` outlives our `surface` (the surface borrows it);
    /// nothing reads it directly.
    _context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    size: PhysicalSize<u32>,
    /// The session (index into `app.tabs`) this window displays.
    tab: usize,
    /// True once this window has been spliced into the native tab group (AppKit `addTabbedWindow:`).
    grouped: bool,
    /// Last OS window title we set for this host, so we only call `set_title` (a platform
    /// round-trip) when the session's live title actually changes instead of every frame.
    title: String,
}

/// A right-click context menu row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CtxAction {
    Copy,
    Paste,
    OpenLink,
    SelectAll,
    SearchSelection,
    /// A non-selectable divider row.
    Separator,
    Interrupt,
    Reconnect,
    Duplicate,
    MuteToggle,
    NewSession,
    CloseTab,
}

/// The open right-click popover: its top-left pixel origin, panel geometry, and rows.
struct CtxMenu {
    /// Top-left pixel origin (clamped into the window).
    px: usize,
    py: usize,
    /// Panel width in px (the widest label + padding).
    w: usize,
    /// Rows in display order. `Separator` draws a divider and is skipped by navigation.
    items: Vec<CtxAction>,
    /// Keyboard selection: index into `items` (never a `Separator`).
    sel: usize,
    /// Pointer pixel position at open, for "Open Link" cell resolution.
    mx: f64,
    my: f64,
}

fn ctx_label(a: CtxAction) -> &'static str {
    match a {
        CtxAction::Copy => "Copy",
        CtxAction::Paste => "Paste",
        CtxAction::OpenLink => "Open Link",
        CtxAction::SelectAll => "Select All",
        CtxAction::SearchSelection => "Search for Selection",
        CtxAction::Separator => "",
        CtxAction::Interrupt => "Interrupt (Ctrl-C)",
        CtxAction::Reconnect => "Reconnect",
        CtxAction::Duplicate => "Duplicate Session",
        CtxAction::MuteToggle => "Mute / Unmute",
        CtxAction::NewSession => "New Session",
        CtxAction::CloseTab => "Close Tab",
    }
}

impl Application {
    fn new(app: App) -> Self {
        let cfg = crate::config::Config::load();
        let base_font = cfg.font_px as f32;
        // Resolve prefix bindings once: action name -> key. Reverse it to key -> action so the
        // command handler can look up a pressed key directly. Unknown actions in config are dropped
        // by `resolve`, so this always covers every action with a valid key.
        // Resolve bindings into the key→action table used at dispatch. `resolve_inverted`
        // inserts defaults first and explicit `[keybindings]` remaps last, so a remap always wins
        // over another action's default sharing that key (a `broadcast → x` remap won't be
        // shadowed by `close_tab`'s default `x`). The digits/Tab stay fixed outside this table.
        let key_action = crate::keys::resolve_inverted(&cfg.keybindings.unwrap_or_default());
        let colors = match &cfg.theme {
            Some(t) => crate::render::Colors::from(t),
            None => crate::render::Colors::default(),
        };
        // macOS borrows Ctrl+Space when a second input source is enabled; detect once at launch
        // so a fallback-chord notice can explain it. The prefix primary is configurable and
        // defaults to Ctrl+H ("Ctrl Harness", the tmux-Ctrl+B analog) so macOS's claim on
        // Ctrl+Space can never break the prefix.
        let prefix_claimed = crate::macos::ctrl_space_claimed();
        let prefix_key = cfg.prefix_key.clone().unwrap_or_else(|| "h".to_string());
        let chord = crate::keys::prefix_label(&prefix_key);
        let quiet_secs = cfg.quiet_after_secs.unwrap_or(120);
        let tab_count = app.tabs.len();
        let offline = app.startup_offline;
        let seen_history = vec![usize::MAX; tab_count];
        // Quiet threshold from config: how long a live, backgrounded, unprotected tab can sit silent
        // before it counts as waiting-on-input. Fresh tabs start stamped "now" so they never read
        // instantly quiet on open.
        let now = std::time::Instant::now();
        let last_output = vec![now; tab_count];
        // Bring back any tabs the user muted last session (prefix+m) so they stay muted across a
        // restart instead of nagging again the moment the window reopens.
        let mut muted = vec![false; tab_count];
        {
            let saved = crate::restore::load_muted();
            if !saved.is_empty() {
                for (i, s) in app.tabs.iter().enumerate() {
                    let key = crate::restore::mute_key(
                        &s.kind(),
                        &s.meta.host,
                        &s.meta.engine,
                        s.attach_session.as_deref(),
                    );
                    if saved.contains(&key) {
                        muted[i] = true;
                    }
                }
            }
        }
        // Bring back pinned tabs (prefix+a) from the last session — a pinned agent run that was
        // still going the previous session stays just as protected across a restart.
        let mut pinned = vec![false; tab_count];
        {
            let saved = crate::restore::load_pinned();
            if !saved.is_empty() {
                for (i, s) in app.tabs.iter().enumerate() {
                    let key = crate::restore::mute_key(
                        &s.kind(),
                        &s.meta.host,
                        &s.meta.engine,
                        s.attach_session.as_deref(),
                    );
                    if saved.contains(&key) {
                        pinned[i] = true;
                    }
                }
            }
        }
        Application {
            window: None,
            context: None,
            surface: None,
            size: PhysicalSize::new(800, 600),
            font_px: 14,
            cell_w: 8,
            cell_h: 18,
            app,
            cache: GlyphCache::load(),
            colors,
            prefix_down: false,
            mods: ModifiersState::default(),
            prefix_claimed,
            prefix_key,
            prefix_alt_notice: false,
            quiet_secs,
            last_active: None,
            find_query: String::new(),
            find_history: crate::restore::load_find_history(),
            find_opts: {
                let (cs, ww) = crate::restore::load_find_opts();
                let mut o = crate::render::FindOptions::default();
                o.case_sensitive = cs;
                o.whole_word = ww;
                o
            },
            rename_query: String::new(),
            broadcast_query: String::new(),
            last_broadcast: crate::restore::load_last_broadcast(),
            broadcast_hist: crate::restore::load_broadcast_history(),
            hist_sel: None,
            broadcast_targets: vec![true; tab_count],
            broadcast_sel: 0,
            new_cwd: String::new(),
            muted,
            pinned,
            grew_delta: vec![0; tab_count],
            unread: vec![0; tab_count],
            live_busy: false,
            detect_len: Vec::new(),
            content_sig: Vec::new(),
            pending_notify: Vec::new(),
            find_hit: None,
            find_all: Vec::new(),
            find_index: 0,
            fleet_q: String::new(),
            fleet_matches: Vec::new(),
            fleet_sel: 0,
            copy_mode: false,
            copy_pos: (0, 0),
            copy_anchor: None,
            copy_query: String::new(),
            copy_searching: false,
            fleet_query: String::new(),
            fleet_filtered: Vec::new(),
            mouse_anchor: None,
            cursor: (0.0, 0.0),
            mouse_left_down: false,
            last_motion_cell: None,
            over_link: false,
            last_press: None,
            window_title: String::new(),
            zoom: crate::restore::load_zoom(),
            base_font,
            seen_history,
            last_output,
            notified: vec![false; tab_count],
            flash: if offline > 0 {
                Some((
                    format!(
                        "{offline} session{} not reached — host down · reconnect via {}+r or Ctrl+H r",
                        if offline == 1 { "" } else { "s" },
                        chord,
                    ),
                    std::time::Instant::now(),
                ))
            } else {
                None
            },
            peek_sel: 0,
            peek_scroll: 0,
            peek_q: String::new(),
            peek_filtering: false,
            peek_filtered: Vec::new(),
            hosts_sel: 0,
            hosts_host: None,
            palette_q: String::new(),
            palette_rows: PaletteAction::all_rows(),
            palette_filtered: Vec::new(),
            palette_sel: 0,
            quit_requested: false,
            close_active_requested: false,
            key_action,
            focus: false,
            native_tabs: cfg.native_tabs.unwrap_or(false),
            native_status_bar: cfg.native_status_bar.unwrap_or(true),
            dnd: false,
            bell_until: vec![None; tab_count],
            was_down: vec![false; tab_count],
            was_alive: vec![false; tab_count],
            recover_until: vec![None; tab_count],
            hover_tab: None,
            drag_tab: None,
            tooltip_box: None,
            fleet_last_poll: std::time::Instant::now(),
            frame: 0,
            hosts: Vec::new(),
            active_host: 0,
            scratch_fb: crate::render::Framebuffer::new(1, 1),
            grid_sel: 0,
            grid_marks: vec![false; tab_count],
            ctx: None,
            tab_rects: Vec::new(),
            newtab_btn: None,
        }
    }

    fn metrics_from_scale(&mut self) {
        if let Some(w) = &self.window {
            let s = w.scale_factor() as f32 * self.zoom;
            self.font_px = (self.base_font * s).round().clamp(8.0, 40.0) as u32;
            self.cell_w = (8.0 * s).round().max(2.0) as u32;
            self.cell_h = (18.0 * s).round() as u32;
        }
    }

    /// Zoom the terminal font by a multiplicative factor, applied on top of the display scale.
    /// Clamps so the grid never becomes unusable, then re-derives cell metrics.
    fn zoom_font(&mut self, delta: f32) {
        self.zoom = (self.zoom * delta).clamp(0.5, 3.0);
        crate::restore::save_zoom(self.zoom);
        self.metrics_from_scale();
        if let Some(active) = self.app.active_session() {
            let lines = (self.size.height as usize - (self.chrome_top() + self.chrome_bottom()))
                / self.cell_h as usize;
            let cols = self.size.width as usize / self.cell_w as usize;
            active.resize(crate::session::TermSize { lines, cols });
        }
    }

    /// Return which tab indices have produced output since we last looked at them (are
    /// backgrounded AND have grown scrollback since our last sample). The focused tab is never
    /// flagged. Unknown/gone tabs (never sampled) are not flagged.
    /// Check every tab's bell flag and turn a fresh ring into a short-lived 🔔 badge. Runs once per
    /// frame (via `activity_flags`); `take_bell` is reset-on-read so each ring badges exactly once,
    /// and the badge fades after a few seconds without being re-armed.
    fn poll_bells(&mut self) {
        let n = self.app.tabs.len();
        self.bell_until.resize(n, None);
        self.was_down.resize(n, false);
        self.was_alive.resize(n, false);
        self.recover_until.resize(n, None);
        let now = std::time::Instant::now();
        // Events drained together at the end of this frame so same-burst events coalesce.
        let mut bells: Vec<usize> = Vec::new();
        let mut recovered: Vec<usize> = Vec::new();
        let mut went_down: Vec<usize> = Vec::new();
        for (i, s) in self.app.tabs.iter().enumerate() {
            if s.take_bell() {
                // A bell while focused is a "your run finished" cue in view — just a badge. A bell in
                // a backgrounded (unmuted) tab queues an OS notification (coalesced below).
                if i != self.app.active && !self.muted.get(i).copied().unwrap_or(false) {
                    bells.push(i);
                }
                self.bell_until[i] = Some(now + std::time::Duration::from_secs(5));
            }
            // Fade stale badges on their own; bell_until goes None once expired.
            if let Some(until) = self.bell_until[i] {
                if until < now {
                    self.bell_until[i] = None;
                }
            }
            // Recovery edge: a remote (non-PTY) pane was down last frame and is alive again — a box
            // came back. Show a short `↻` badge and, when backgrounded and unmuted, queue ONE
            // notification so a diver notices (a host that silently reappears is easy to miss). PTYs
            // never flap, and reconnects that never complete (still down) don't fire.
            let alive_now = s.alive();
            if self.was_down[i] && alive_now && s.kind() != "pty" {
                self.recover_until[i] = Some(now + std::time::Duration::from_secs(8));
                if i != self.app.active && !self.muted.get(i).copied().unwrap_or(false) {
                    recovered.push(i);
                }
            }
            self.was_down[i] = !alive_now;
            // Down edge: a remote (non-PTY) pane was alive last frame and died. This is arguably
            // the most important fleet event, so notify before the pane is forgotten to a diver —
            // but only for backgrounded, unmuted panes (a pane you're watching or muted isn't a
            // surprising drop). New tabs start `was_alive=false`, so a pane restored already-dead
            // never trips its own first-frame nag.
            if self.was_alive[i]
                && !alive_now
                && s.kind() != "pty"
                && i != self.app.active
                && !self.muted.get(i).copied().unwrap_or(false)
            {
                went_down.push(i);
            }
            self.was_alive[i] = alive_now;
            if let Some(until) = self.recover_until[i] {
                if until < now {
                    self.recover_until[i] = None;
                }
            }
        }
        // Queue each bell for (coalesced) delivery rather than notifying per-tab here.
        for i in bells {
            self.queue_notify("bell", i);
        }
        for i in recovered {
            self.queue_notify("recover", i);
        }
        for i in went_down {
            self.queue_notify("down", i);
        }
    }

    /// Queue an OS notification for later this frame. Events of the same kind in the same frame are
    /// merged by `flush_notifications` into ONE popup listing every tab, so a `broadcast` to every
    /// host (which produces output in N tabs at once) fans out as a single notification instead of N
    /// osascript launches.
    fn queue_notify(&mut self, kind: &str, tab: usize) {
        // Global Do-Not-Disturb swallows every fleet notification (busy nag + bell popup) at the
        // source, so nothing even reaches the merge/launch path. In-bar badges are unaffected — a
        // diver who mutes popups still sees the `!N`/🔔 in the bar to act on later.
        if self.dnd {
            return;
        }
        self.pending_notify.push((kind.to_string(), tab));
    }

    /// Drain queued notifications, merging same-kind events that arrived in this frame into one
    /// notification listing every affected tab. Runs once at the end of each `activity_flags` pass;
    /// a frame with nothing queued drains nothing.
    fn flush_notifications(&mut self) {
        if self.pending_notify.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_notify);
        for (kind, tabs) in group_notifications(&pending) {
            self.fire(&kind, &tabs);
        }
    }

    /// Imperatively fire one coalesced notification of `kind` for the given (deduped) tab indices.
    /// Notification label for a session: `name@host`, falling back to just `name` for local
    /// (hostless) panes. In a multi-machine fleet a bell/busy/recover is a MACHINE event, so the
    /// host is what makes "back online" or "produced output" actionable.
    fn notify_label(s: &crate::session::Session) -> String {
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        if s.meta.host.is_empty() {
            head
        } else {
            format!("{head}@{}", s.meta.host)
        }
    }

    fn fire(&mut self, kind: &str, tabs: &[usize]) {
        let labels: Vec<String> = tabs
            .iter()
            .filter_map(|&i| self.app.tabs.get(i))
            .map(Self::notify_label)
            .collect();
        let n = labels.len();
        if n == 0 {
            return;
        }
        let list = join_labels(&labels);
        match kind {
            "bell" => {
                let title = if n == 1 {
                    format!("{list} — bell")
                } else {
                    format!("{n} sessions — bell")
                };
                let body = if n == 1 {
                    format!("Session {list} rang the terminal bell.")
                } else {
                    format!("Fleet bells: {list}.")
                };
                notify_simple(&title, &body);
            }
            "busy" => {
                let title = if n == 1 {
                    format!("{list} · busy")
                } else {
                    format!("{n} sessions busy")
                };
                let body = if n == 1 {
                    format!("{list} produced new output.")
                } else {
                    format!("New output from {list}.")
                };
                notify_simple(&title, &body);
            }
            "recover" => {
                // A pane coming back is a MACHINE event in a multi-host fleet — `list` already
                // carries `name@host`, so "back online" is actionable at a glance.
                let title = if n == 1 {
                    format!("{list} · reconnected")
                } else {
                    format!("{n} sessions reconnected")
                };
                let body = if n == 1 {
                    format!("{list} is back online.")
                } else {
                    format!("Back online: {list}.")
                };
                notify_simple(&title, &body);
            }
            "down" => {
                // A pane dying is the fleet event a diver must not miss — the counterpart to
                // `recover`. `list` carries `name@host`, so "went down" is actionable at a glance.
                let title = if n == 1 {
                    format!("{list} · went down")
                } else {
                    format!("{n} sessions went down")
                };
                let body = if n == 1 {
                    format!("{list} disappeared.")
                } else {
                    format!("Connections lost: {list}.")
                };
                notify_simple(&title, &body);
            }
            _ => {}
        }
    }

    fn activity_flags(&mut self) -> Vec<bool> {
        self.poll_bells();
        let n = self.app.tabs.len();
        self.seen_history.resize(n, usize::MAX);
        self.notified.resize(n, false);
        self.muted.resize(n, false);
        self.pinned.resize(n, false);
        self.grew_delta.resize(n, 0);
        self.unread.resize(n, 0);
        self.grid_marks.resize(n, false);
        // `last_output` is otherwise only shaped by forget_tab (shrink) — every other tab-parallel
        // vector is resized here. New tabs start stamped "now" so they don't instantly read quiet.
        self.last_output.resize(n, std::time::Instant::now());
        let mut flags = vec![false; n];
        // Collect the indices that first went busy so we can queue after the immutable tab-borrow
        // loop ends (`queue_notify` needs `&mut self`).
        let mut fresh_busy: Vec<usize> = Vec::new();
        for (i, s) in self.app.tabs.iter().enumerate() {
            if i == self.app.active {
                // We're looking at it now: re-baseline, don't flag, and reset any pending nudge.
                self.seen_history[i] = s.history_len();
                self.notified[i] = false;
                self.grew_delta[i] = 0;
                // Looking at it clears the cumulative unread badge (the same re-baseline a
                // `mark_all_read` applies). The count starts fresh next time it's backgrounded.
                self.unread[i] = 0;
                continue;
            }
            let len = s.history_len();
            let grew = self.seen_history[i] != usize::MAX && len > self.seen_history[i];
            let delta = len.saturating_sub(self.seen_history[i]);
            self.grew_delta[i] = delta;
            // Accumulate the fresh output into the cumulative unread count so the `!N` badge
            // lingers after the agent settles (it only resets when this tab is looked at / read).
            if !self.muted[i] {
                self.unread[i] = self.unread[i].saturating_add(delta);
            }
            self.seen_history[i] = len;
            if grew {
                // This is genuine fresh output from a session we're not looking at — a timestamp of
                // "last did something." The focused tab is never stamped (it can't reach this arm),
                // so a tab you're actively reading stays busy-forever, never quiet.
                self.last_output[i] = std::time::Instant::now();
            }
            flags[i] = grew;
            // A muted tab is intentionally ignored: no busy badge, no OS notification. It still
            // feeds its seen-history so it isn't flagged the moment it's unmuted.
            if self.muted[i] {
                continue;
            }
            // Fire a one-shot OS notification the first time a backgrounded agent goes busy, so a
            // diver is nudged without having to watch the badge. Once the tab is looked at (or it
            // settles), `notified` is reset and the next transition nags again. A tab still inside
            // its recovery window (down→alive within the last few seconds) is skipped: it just got a
            // `recover` toast for the same reconnect, so a second `busy` toast would be a double-nag.
            // Backgrounded-unmuted is the only state that reaches here with a set recovery badge
            // (active/muted tabs bail earlier), so this exactly cancels the duplicate.
            if should_busy_nudge(
                grew,
                self.notified[i],
                self.recover_until.get(i).copied().flatten().is_some(),
            ) {
                self.notified[i] = true;
                fresh_busy.push(i);
            }
        }
        for i in fresh_busy {
            self.queue_notify("busy", i);
        }
        // Every caller of this pass (render badge + next_busy + tab-bar) expects the queued busy and
        // bell events to have been delivered; flush the merged batch into one notification per kind.
        self.flush_notifications();
        flags
    }

    /// Focus tab `i` and remember it (so a relaunch opens on the same tab).
    fn set_active(&mut self, i: usize) {
        if i != self.app.active && self.app.active < self.app.tabs.len() {
            self.last_active = Some(self.app.active);
        }
        self.app.active = i.min(self.app.tabs.len().saturating_sub(1));
        crate::restore::save_active(self.app.active);
        // Native-tab mode: switching tabs in-app (prefix+l last, prefix+number, palette) must also
        // surface the matching real window and make it key, so the OS title-bar tab set follows.
        if self.native_tabs {
            if let Some(hi) = self.hosts.iter().position(|h| h.tab == self.app.active) {
                self.active_host = hi;
                self.alias_active();
                self.hosts[hi].window.focus_window();
                self.request_redraw();
            }
        }
    }

    /// Move the active tab left/right (prefix-move), keeping every tab-parallel state (pin,
    /// mute, busy/quiet sampling, unread/bell/recover badges, notification flags) aligned with the
    /// session that moved — not with its old slot. Mirrors what `App::move_tab` does to `tabs`.
    fn move_tab_parallel(&mut self, delta: isize) {
        let a = self.app.active;
        self.app.move_tab(delta);
        if self.app.active != a {
            // A structural move shifts every index, so the tmux-style "previous" pointer would
            // silently point at a different session; forget it (see `forget_tab`).
            self.last_active = None;
            self.swap_parallel(a, self.app.active);
            // Persist the new tab order so a `{`/`}` re-arrangement survives a relaunch — tab order
            // is part of a diver's arranged fleet (by machine/priority), not a transient layout.
            self.persist_tabs();
        }
    }

    /// Swap two indices across every tab-parallel vector so a session's pin/mute/busy/quiet/badge
    /// state rides along with it when tabs are swapped. Every access is length-guarded because a
    /// parallel vector can be transiently stale (a tab just closed, `forget_tab` not yet run).
    fn swap_parallel(&mut self, a: usize, b: usize) {
        swap_slot(&mut self.muted, a, b);
        swap_slot(&mut self.pinned, a, b);
        swap_slot(&mut self.seen_history, a, b);
        swap_slot(&mut self.grew_delta, a, b);
        swap_slot(&mut self.unread, a, b);
        swap_slot(&mut self.last_output, a, b);
        swap_slot(&mut self.notified, a, b);
        swap_slot(&mut self.grid_marks, a, b);
        swap_slot(&mut self.bell_until, a, b);
        swap_slot(&mut self.was_down, a, b);
        swap_slot(&mut self.was_alive, a, b);
        swap_slot(&mut self.recover_until, a, b);
        swap_slot(&mut self.broadcast_targets, a, b);
        swap_slot(&mut self.detect_len, a, b);
        swap_slot(&mut self.content_sig, a, b);
    }

    /// Apply the same remove/insert relocation as `App::move_tab_from_to` to every tab-parallel
    /// vector, so each session's pin/mute/busy/quiet/badge state follows it to its new slot on a
    /// drag-to-reorder.
    fn reorder_parallel(&mut self, from: usize, to: usize) {
        // Drag-reordering shifts every index, so the "previous tab" pointer no longer names the
        // session it was recorded for; drop it rather than jump to the wrong tab on `prefix+l`.
        self.last_active = None;
        move_slot(&mut self.muted, from, to);
        move_slot(&mut self.pinned, from, to);
        move_slot(&mut self.seen_history, from, to);
        move_slot(&mut self.grew_delta, from, to);
        move_slot(&mut self.unread, from, to);
        move_slot(&mut self.last_output, from, to);
        move_slot(&mut self.notified, from, to);
        move_slot(&mut self.grid_marks, from, to);
        move_slot(&mut self.bell_until, from, to);
        move_slot(&mut self.was_down, from, to);
        move_slot(&mut self.was_alive, from, to);
        move_slot(&mut self.recover_until, from, to);
        move_slot(&mut self.broadcast_targets, from, to);
        move_slot(&mut self.detect_len, from, to);
        move_slot(&mut self.content_sig, from, to);
    }

    /// Persist the current tab list to disk. Called right after a successful spawn so a freshly
    /// opened session survives a crash/force-quit (otherwise a new tab only gets saved later, on
    /// close/quit). Cheap idempotent file write; safe to call from the UI handlers only.
    fn persist_tabs(&self) {
        crate::restore::save(&self.app.tab_specs());
    }

    /// Open the broadcast overlay pre-scoped to exactly the tiles marked in the fleet grid (`b`).
    /// If nothing is marked, fall back to opening broadcast on all sessions (safety — you can't
    /// silently broadcast to zero). Marks are consumed (cleared) once applied.
    /// The column count the fleet grid lays tiles into — the same the renderer uses, so the
    /// PgUp/PgDn selection page matches what's actually on screen. Derived from `fleet_grid_geom`
    /// so the PgUp/Dn page, the mouse hit-test, and the renderer can never drift apart.
    fn fleet_grid_cols(&self) -> usize {
        self.fleet_grid_geom().4
    }

    /// The fleet grid's tile geometry, mirroring the renderer's layout exactly: `(x0, y0, tw, th,
    /// cols, n, height)`. `x0`/`y0` are the first tile's top-left, `tw`/`th` its cell size, `cols`
    /// the column count, `n` the session count (at least 1), `height` the window height (so a click
    /// in an un-drawn clipped row maps to nothing). Shared by the PgUp/PgDn page and the mouse.
    fn fleet_grid_geom(&self) -> (usize, usize, usize, usize, usize, usize, usize) {
        let n = self.app.tabs.len().max(1);
        let gcol = self.cell_w as usize;
        let x0 = 8usize;
        let inner_w = (self.size.width as usize).saturating_sub(x0 + 8);
        let cols = (inner_w / (gcol.max(1) * 12)).max(1).min(n);
        let tw = inner_w / cols;
        let th = self.cell_h as usize * 4;
        let line_px = self.font_px as usize + 6;
        let y0 = self.chrome_top() + 2 + line_px;
        (x0, y0, tw, th, cols, n, self.size.height as usize)
    }

    /// `R` (fleet grid): force-reconnect every marked tile at once — the war-room cousin of the
    /// broadcast-`b` action. Mark with Space then `R` to heal several down machines in one sweep;
    /// if nothing is marked, falls back to every down pane (so a bare `R` is never a silent no-op).
    fn grid_reconnect_marked(&mut self) {
        let n = self.app.tabs.len();
        let down: Vec<bool> = (0..n)
            .map(|i| {
                let s = &self.app.tabs[i];
                s.kind() != "pty" && !s.alive()
            })
            .collect();
        let mut targets = grid_targets(&self.grid_marks, Some(&down));
        self.grid_marks = vec![false; n];
        if targets.is_empty() {
            self.flash = Some((
                "no marked or down panes to reconnect".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let mut ok = 0usize;
        let mut still: Vec<(String, String)> = Vec::new();
        for i in &targets {
            match self.app.tabs[*i].reconnect_now() {
                Ok(()) => ok += 1,
                Err(e) => {
                    let host = {
                        let h = self.app.tabs[*i].meta.host.clone();
                        if h.is_empty() {
                            "local".to_string()
                        } else {
                            h
                        }
                    };
                    let reason = if e.to_string().is_empty() {
                        e.kind().to_string()
                    } else {
                        e.to_string()
                    };
                    still.push((host, reason));
                }
            }
        }
        self.flash = Some((fmt_reconnect_summary(ok, &still), std::time::Instant::now()));
    }

    fn grid_broadcast_marked(&mut self) {
        let n = self.app.tabs.len();
        let marked: Vec<usize> = (0..n)
            .filter(|&i| self.grid_marks.get(i).copied().unwrap_or(false))
            .collect();
        self.broadcast_targets = vec![false; n];
        for &i in &marked {
            self.broadcast_targets[i] = true;
        }
        self.grid_marks = vec![false; n];
        // If the diver marked nothing, don't open an overlay that would send to zero — reset all-on
        // (the broadcast overlay's default) and let them narrow from there.
        if marked.is_empty() {
            self.broadcast_targets.iter_mut().for_each(|t| *t = true);
        }
        self.broadcast_query = self.last_broadcast.clone();
        self.broadcast_sel = 0;
        self.app.overlay = Overlay::Broadcast;
    }

    /// `b` (hosts drill-in): fan a line out to every session on the selected host — the per-machine
    /// cousin of `grid_broadcast_marked`. Opens the broadcast overlay pre-scoped to exactly this
    /// host's panes, so a command to "all of build05" (git pull, restart the agent, redeploy)
    /// doesn't require hand-marking tiles. The diver can still toggle targets off / Space before
    /// Enter, exactly like the fleet-grid broadcast.
    fn host_broadcast(&mut self, host: &str) {
        let n = self.app.tabs.len();
        self.broadcast_targets = vec![false; n];
        for i in self.host_session_indices(host) {
            if i < n {
                self.broadcast_targets[i] = true;
            }
        }
        self.broadcast_query = self.last_broadcast.clone();
        self.broadcast_sel = 0;
        self.app.overlay = Overlay::Broadcast;
    }

    /// `C` (fleet grid): send Ctrl-C to every marked tile (falling back to all non-muted sessions
    /// when nothing is marked) — the "stop the batch job" sibling of `b` broadcast and `R`
    /// reconnect. Explicitly-marked sessions are always interrupted (the diver asked for them);
    /// muted tabs are skipped by the fallback so a deliberately-quiet pane is never round-housed.
    fn grid_interrupt_marked(&mut self) {
        let n = self.app.tabs.len();
        let unmuted: Vec<bool> = (0..n)
            .map(|i| !self.muted.get(i).copied().unwrap_or(false))
            .collect();
        let mut targets = grid_targets(&self.grid_marks, Some(&unmuted));
        self.grid_marks = vec![false; n];
        if targets.is_empty() {
            self.flash = Some((
                "no sessions to interrupt".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let mut sent = 0usize;
        for i in &targets {
            if let Some(s) = self.app.tabs.get(*i) {
                s.write(b"\x03");
                sent += 1;
            }
        }
        self.flash = Some((
            format!(
                "sent Ctrl-C to {sent} session{}",
                if sent == 1 { "" } else { "s" }
            ),
            std::time::Instant::now(),
        ));
    }

    /// `X` (fleet grid): close every marked tile at once (high→low so indices stay valid), honoring
    /// the pin guard + per-tab undo via `close_tab_at`. The bulk-prune cousin of the single-tile
    /// `x`; unlike `b`/`C`/`R` it does NOT fall back to "all" — closing everything with a miss is
    /// too destructive, so a missing selection is a no-op with a reminder to mark first.
    fn grid_close_marked(&mut self) {
        let n = self.app.tabs.len();
        let marked: Vec<usize> = (0..n)
            .filter(|&i| self.grid_marks.get(i).copied().unwrap_or(false))
            .collect();
        self.grid_marks = vec![false; n];
        if marked.is_empty() {
            self.flash = Some((
                "no marked tiles to close — Space to mark".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let mut closed = 0usize;
        for &i in marked.iter().rev() {
            if self.close_tab_at(i) {
                closed += 1;
            }
        }
        self.flash = Some((
            if closed == 0 {
                "nothing closed (all marked were pinned)".to_string()
            } else {
                format!(
                    "closed {closed} session{}",
                    if closed == 1 { "" } else { "s" }
                )
            },
            std::time::Instant::now(),
        ));
        if self.app.tabs.is_empty() {
            self.app.overlay = Overlay::None;
        }
    }

    /// `prefix+l`: flip to the tab that was active just before this one (tmux last-window).
    /// Repeated presses ping-pong, since swapping focus swaps `last_active`.
    fn last_window(&mut self) {
        if let Some(prev) = self.last_active.take() {
            if prev < self.app.tabs.len() {
                self.set_active(prev);
            }
        }
    }

    /// `prefix+p`: paste the system clipboard into the active session via the bracketed-paste
    /// sequence. Works uniformly for local and remote/tunnel sessions, so it's the reliable paste
    /// when Ctrl+V is taken by a window manager.
    fn paste_clipboard(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                let seq = format!("\x1b[200~{}\x1b[201~", text);
                s.write(seq.as_bytes());
            }
        }
    }

    /// Middle-click: paste the system clipboard into the active session WITHOUT the bracketed-paste
    /// wrapper. Terminal middle-click paste has always been raw bytes; bracketing assumes the app
    /// requested it, and most CLIs ignore the delimiter sequence but echo it, corrupting the line.
    /// Raw write is what every X11 terminal does.
    fn paste_raw(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                s.write(text.as_bytes());
            }
        }
    }

    /// `Ctrl+D`: copy the active session's entire scrollback (history + screen) to the system
    /// clipboard as plain text. Handy for dumping an agent's whole log for pasting into an issue or
    /// a summary. Builds the text with the same capture path used for restart persistence.
    fn copy_whole_scrollback(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let text = s.capture_scrollback();
        if text.trim().is_empty() {
            return;
        }
        let n = text.len();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
            self.flash = Some((
                format!("copied {n} chars of scrollback"),
                std::time::Instant::now(),
            ));
        }
    }

    /// `prefix+j`: copy the active session's identity to the system clipboard — `<engine|name>@host`
    /// for a local PTY, `pane@host` for a pane-backed tab (the exact target `prefix+r` attach
    /// accepts). Quick way to share or reference a session without hand-typing `user@host:session`.
    fn copy_identity(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let id = format!("{}@{}", head, s.meta.host);
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(id);
        }
        self.flash = Some((
            format!("copied {}@{}", head, s.meta.host),
            std::time::Instant::now(),
        ));
    }

    /// `prefix+E`: copy a one-line summary of every open tab to the clipboard — the "what's my
    /// fleet doing right now" gesture. Each row is `●/○ state · engine@host · name · live title`,
    /// so the clipboard reads as a grep-friendly status dump (ready to paste into a chat or notes).
    fn copy_fleet(&mut self) {
        if self.app.tabs.is_empty() {
            self.flash = Some((
                "no sessions to summarize".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let text = self.fleet_summary_text();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
        self.flash = Some((
            format!("copied {} sessions to clipboard", self.app.tabs.len()),
            std::time::Instant::now(),
        ));
    }

    /// Fleet-summary copy: a per-host health block (`● build02 2/2 live`) followed by one line per
    /// open tab (`●/○ engine (N)@host · name · live · ⏳`). The host block is the at-a-glance "which
    /// machines are up" snapshot for a fleet spread across many computers.
    fn fleet_summary_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let hosts = host_engine_breakdown(
            self.app
                .tabs
                .iter()
                .map(|s| (s.meta.host.as_str(), s.alive(), s.meta.engine.as_str())),
        );
        for (h, alive, total, mix) in hosts {
            lines.push(fleet_host_line(&h, alive, total, &mix));
        }
        lines.push(String::new());
        for (i, s) in self.app.tabs.iter().enumerate() {
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let state = if s.alive() { "●" } else { "○" };
            let live = s
                .live_title()
                .map(|t| format!(" · {t}"))
                .unwrap_or_default();
            let where_s = if s.kind() == "pty" {
                String::new()
            } else {
                format!("@{}", s.meta.host)
            };
            let queued = s.pending_bytes();
            let queued_s = if queued > 0 {
                format!(" · ⏳{}B queued", queued)
            } else {
                String::new()
            };
            // A down tab carries its reconnect reason so a copied fleet report reads as a health
            // handoff ("which host is dark and why"), not a bare ○. Local PTYs have no transport
            // to diagnose and stay bare.
            let down_s = if !s.alive() && s.kind() != "pty" {
                match s.down_reason() {
                    Some(r) if !r.trim().is_empty() => {
                        format!(" · ⚠ {}", clip_dots(r.trim(), 60))
                    }
                    _ => format!(" · ⚠ {}", clip_dots("reconnecting…", 60)),
                }
            } else {
                String::new()
            };
            lines.push(format!(
                "{state} {} ({}){where_s} · {head}{live}{queued_s}{down_s}",
                s.meta.engine,
                i + 1
            ));
        }
        lines.join("\n")
    }

    /// `prefix+w`: write the active session's whole scrollback to a timestamped text file in the
    /// current directory (or HOME if that fails). Bigger than the clipboard and leaves a lasting
    /// artifact — a diver can dump a long agent log to disk to grep, diff, or share. Shows the path
    /// in the OSC title slot so the user sees where it landed.
    fn export_scrollback(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let text = s.capture_scrollback();
        if text.trim().is_empty() {
            return;
        }
        let slug = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let slug: String = slug
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // Tag the file with the host for pane-backed tabs so a dump from a multi-machine fleet is
        // identifiable at a glance (`claude-fix42-build02-…log`); a local pty has no remote host to
        // disambiguate, so it stays bare.
        let host_tag = if s.kind() == "pty" {
            String::new()
        } else {
            let h: String = s
                .meta
                .host
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if h.is_empty() {
                String::new()
            } else {
                format!("-{h}")
            }
        };
        let base = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        // The timestamp needs to be readable but collision-safe; epoch-ms keeps it unique.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let fname = format!("{}{}-{}.log", slug, host_tag, stamp);
        let now = std::time::Instant::now();
        // Prefix a self-describing header so a handed-off log file says what it is without opening
        // it: engine@host, transport kind, the live task title if the shell set one, and a
        // human-readable export time. Epoch-ms is used for the collision-safe filename; the header
        // uses `date` (macOS) for a readable timestamp, falling back to the epoch on any failure so
        // the header always renders. Export is a rare explicit action, so one process spawn is fine.
        let engine_host = format!(
            "{}@{}",
            s.meta.engine,
            if s.meta.host.is_empty() {
                "local"
            } else {
                &s.meta.host
            }
        );
        let task = s.live_title().unwrap_or_default();
        let task = task.replace('\n', " ");
        let task = if task.trim().is_empty() {
            "-".to_string()
        } else {
            task
        };
        let human = std::process::Command::new("date")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("epoch-ms {stamp}"));
        let header = format!(
            "# harness-terminal scrollback export\n# session: {engine_host}  ({})\n# task: {task}\n# exported: {human}\n\n",
            s.kind()
        );
        let out = format!("{header}{text}");
        // Try the current directory first; if that isn't writable (e.g. an unwritable cwd) fall back
        // to the guaranteed-writable temp dir so the export never silently does nothing.
        let mut path = base.join(&fname);
        let mut res = std::fs::write(&path, &out);
        if res.is_err() {
            let alt = std::env::temp_dir().join(&fname);
            res = std::fs::write(&alt, &out);
            path = alt;
        }
        match res {
            Ok(_) => {
                self.flash = Some((
                    format!("wrote {} bytes → {}", out.len(), path.to_string_lossy()),
                    now,
                ));
            }
            Err(e) => {
                self.flash = Some((format!("⚠ couldn't write export: {e}"), now));
            }
        }
    }

    /// Fleet-overlay Enter: jump to an already-open tab running the selected session's engine if one
    /// exists (closest thing to 'diving into' that pane), else open a fresh local tmux pane for it.
    /// Refresh `fleet_filtered` (indices into `app.fleet.fleet` matching `fleet_query`) and clamp the
    /// selection. Mirrors the palette filter, but over fleet sessions (matched by id / pane / engine).
    fn fleet_refresh_filter(&mut self) {
        let q = self.fleet_query.to_lowercase();
        self.fleet_filtered = self
            .app
            .fleet
            .fleet
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if q.is_empty() {
                    return true;
                }
                s.session_id.to_lowercase().contains(&q)
                    || s.tmux_pane.to_lowercase().contains(&q)
                    || s.engine.to_lowercase().contains(&q)
                    || s.name.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.app.selected >= self.fleet_filtered.len() {
            self.app.selected = self.fleet_filtered.len().saturating_sub(1);
        }
    }

    fn fleet_attach_selected(&mut self) {
        // `selected` is an index into `fleet_filtered`; resolve it back to the real session.
        let real = self
            .fleet_filtered
            .get(self.app.selected)
            .copied()
            .unwrap_or(self.app.selected);
        let Some(fs) = self.app.fleet.fleet.get(real) else {
            self.app.overlay = Overlay::None;
            return;
        };
        let eng = fs.engine.clone();
        // Prefer an open tab with the same engine id, so re-running the same session isn't duplicated.
        if let Some(i) = self.app.tabs.iter().position(|t| t.meta.engine == eng) {
            self.set_active(i);
        } else if !eng.is_empty() && self.app.spawn_tmux("this-host", &eng).is_none() {
            self.persist_tabs();
        }
        self.app.overlay = Overlay::None;
    }

    /// `prefix+o`: jump to the next backgrounded tab that produced output since we last looked,
    /// wrapping around. Makes the activity badge actionable — a key to reach 'the one that just
    /// did something'. If none is flagged, does nothing.
    fn next_busy(&mut self) {
        let flags = self.activity_flags();
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            if flags[i] {
                self.set_active(i);
                return;
            }
        }

        // Accurate phrasing: the active tab itself may be busy; we only report no OTHER tab is.
        self.flash = Some(("no other busy tab".to_string(), std::time::Instant::now()));
    }

    /// Which live, backgrounded, unprotected tabs have sat silent past the quiet threshold — the
    /// inverse of `activity_flags`'s busy. A session that's been quiet this long is almost certainly
    /// done (or paused at an input prompt) and waiting on you, so it's worth the triage count's
    /// `⌛N`. The focused tab, dead tabs, and pinned/muted tabs (deliberately shielded) are excluded;
    /// a tab is only counted once its history has been sampled AND it has actually sat idle.
    fn quiet_flags(&self) -> (bool, usize, std::time::Duration) {
        let present = self.seen_history.len() == self.app.tabs.len();
        let threshold = std::time::Duration::from_secs(self.quiet_secs);
        let now = std::time::Instant::now();
        let mut any = false;
        let mut count = 0;
        let mut max_idle = std::time::Duration::ZERO;
        for (i, s) in self.app.tabs.iter().enumerate() {
            let live = s.alive() && s.kind() != "pty";
            let watched = i == self.app.active;
            let shielded = self.pinned.get(i).copied().unwrap_or(false)
                || self.muted.get(i).copied().unwrap_or(false);
            let sampled = present && self.seen_history[i] != usize::MAX;
            if !live || watched || shielded || !sampled {
                continue;
            }
            let idle = now - self.last_output[i];
            if idle >= threshold {
                any = true;
                count += 1;
                if idle > max_idle {
                    max_idle = idle;
                }
            }
        }
        (any, count, max_idle)
    }

    /// Whether a single tab currently reads as "quiet" (live, backgrounded, unprotected, sampled,
    /// and sat silent past the threshold). Shares the exact condition `next_quiet` uses so the fleet
    /// grid's per-tile `⌛` matches the `prefix+z` jump and the triage count.
    fn quiet_for(&self, i: usize) -> bool {
        let present = self.seen_history.len() == self.app.tabs.len();
        let Some(s) = self.app.tabs.get(i) else {
            return false;
        };
        let live = s.alive() && s.kind() != "pty";
        let watched = i == self.app.active;
        let shielded = self.pinned.get(i).copied().unwrap_or(false)
            || self.muted.get(i).copied().unwrap_or(false);
        let sampled = present && self.seen_history[i] != usize::MAX;
        if !live || watched || shielded || !sampled {
            return false;
        }
        let threshold = std::time::Duration::from_secs(self.quiet_secs);
        let idle = std::time::Instant::now() - self.last_output[i];
        idle >= threshold
    }

    /// `prefix+z`: jump to the next tab whose live session has gone quiet (sat silent past the
    /// quiet threshold — likely done, or parked at an input prompt waiting on you), wrapping.
    /// Complements `next_busy`/`next_down`: busy means "just produced output", quiet means
    /// "finished/stalled — needs a look". No-op when no tab is quiet.
    fn next_quiet(&mut self) {
        let present = self.seen_history.len() == self.app.tabs.len();
        let threshold = std::time::Duration::from_secs(self.quiet_secs);
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            let s = &self.app.tabs[i];
            let live = s.alive() && s.kind() != "pty";
            let watched = i == self.app.active;
            let shielded = self.pinned.get(i).copied().unwrap_or(false)
                || self.muted.get(i).copied().unwrap_or(false);
            let sampled = present && self.seen_history[i] != usize::MAX;
            if !live || watched || shielded || !sampled {
                continue;
            }
            let idle = std::time::Instant::now() - self.last_output[i];
            if idle >= threshold {
                let id = self.tab_identity(i);
                self.flash = Some((format!("quiet — {id}"), std::time::Instant::now()));
                self.set_active(i);
                return;
            }
        }

        self.flash = Some(("no other quiet tab".to_string(), std::time::Instant::now()));
    }

    /// `prefix+Q`: jump to the next tab whose session is down (disconnected / in reconnect backoff),
    /// wrapping. Complements `next_busy` (which finds tabs that produced output): when a host dies
    /// and several panes go dark, this cycles just the dead ones so a diver sees what's reconnecting
    /// instead of typing into a blank pane. No-op when every tab is alive.
    fn next_down(&mut self) {
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            let down = !self.app.tabs[i].alive() && self.app.tabs[i].kind() != "pty"; // live local PTY is never "down"
            if down {
                let id = self.tab_identity(i);
                // Say *why* it's down alongside where it landed, so a diver knows at a glance
                // whether it's a reconnect in flight or a hard failure (auth/refused/timeout).
                let reason = self
                    .app
                    .tabs
                    .get(i)
                    .and_then(|s| s.down_reason())
                    .unwrap_or_else(|| "reconnecting…".to_string());
                let reason = clip_dots(&reason.trim().to_string(), 26);
                let msg = if reason.is_empty() {
                    format!("down — {id}")
                } else {
                    format!("down — {id} ({reason})")
                };
                self.flash = Some((msg, std::time::Instant::now()));
                self.set_active(i);
                return;
            }
        }

        self.flash = Some(("no other down tab".to_string(), std::time::Instant::now()));
    }

    /// `prefix+P`: jump to the next pinned tab (wrapping). Pinned tabs are the long-running agents
    /// a diver deliberately protects, so this is a fast way to cycle just the protected ones
    /// without scrolling the whole bar. No-op when there are no other pinned tabs.
    fn next_pinned(&mut self) {
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            if self.pinned.get(i).copied().unwrap_or(false) {
                let id = self.tab_identity(i);
                self.flash = Some((format!("pinned — {id}"), std::time::Instant::now()));
                self.set_active(i);
                return;
            }
        }

        self.flash = Some(("no other pinned tab".to_string(), std::time::Instant::now()));
    }

    /// `prefix+H`: jump to the next unique host in the fleet, wrapping. For a fleet spread across
    /// machines this pages by machine instead of by tab — a three-host farm becomes three stops
    /// rather than a trip through every pane. It lands on the first tab of the next distinct host
    /// after the active one; ties in the linear tab order are resolved by cycling order. No-op when
    /// every tab is on one host (or there are no tabs).
    fn next_host(&mut self) {
        let hosts: Vec<&str> = self.app.tabs.iter().map(|s| s.meta.host.as_str()).collect();
        if let Some(i) = next_host_index(&hosts, self.app.active) {
            let target = self.app.tabs[i].meta.host.clone();
            self.flash = Some((format!("host {target}"), std::time::Instant::now()));
            self.set_active(i);
        } else {
            self.flash = Some(("no other host".to_string(), std::time::Instant::now()));
        }
    }

    /// Short legible identity for a tab, e.g. `claude@build02` (name falls back to engine).
    /// Used by `next_quiet`/`next_down`/`next_pinned` so a jump flash says *where* it landed.
    fn tab_identity(&self, i: usize) -> String {
        match self.app.tabs.get(i) {
            Some(s) => {
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                format!("{head}@{}", s.meta.host)
            }
            None => "?".to_string(),
        }
    }

    /// `prefix+m`: toggle mute on the active tab. A muted tab stops firing the busy OS notification
    /// and its `!` badge (see `activity_flags`), so a noisy pane a diver doesn't care about stops
    /// nagging — while still showing its own live tail in the tab bar. Toggle again to unmute.
    fn toggle_mute_active(&mut self) {
        if self.app.active < self.app.tabs.len() {
            // Mirror the `.get()`-guarded pattern used across every other tab-parallel vector: a
            // fresh tab (e.g. right after duplicate_active, which only resizes `pinned`) can leave
            // this vector a slot short, and an unguarded index would panic on the very first toggle.
            let on = !self.muted.get(self.app.active).copied().unwrap_or(false);
            self.muted.resize(self.app.active + 1, false);
            self.muted[self.app.active] = on;
            let state = if on { "MUTED" } else { "unmuted" };
            let head = self
                .app
                .active_session()
                .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
                .unwrap_or_default();
            self.flash = Some((format!("{head} {state}"), std::time::Instant::now()));
        }
    }

    /// `prefix+C`: close every tab whose live session is "quiet" — done or parked waiting on you
    /// (the exact `quiet_for` predicate behind prefix+z and the `⌛N` triage). A fleet-cleanup
    /// gesture: after a long night of agent runs, one key sweeps the finished ones off the bar.
    /// Pinned tabs and the active tab are never closed (unprotect via `A`, or hop away first);
    /// mutes do NOT shield, since a muted-but-done agent is exactly the cruft you'd sweep.
    /// Reopenable via prefix+u for the last one removed.
    fn close_quiet_tabs(&mut self) {
        // Collect indices (high→low) of quiet, unpinned, non-active tabs.
        let mut doomed: Vec<usize> = Vec::new();
        for i in 0..self.app.tabs.len() {
            if i == self.app.active {
                continue;
            }
            if self.pinned.get(i).copied().unwrap_or(false) {
                continue; // protected — never batch-closed
            }
            if self.quiet_for(i) {
                doomed.push(i);
            }
        }
        if doomed.is_empty() {
            self.flash = Some((
                "no quiet tabs to close".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        // Remove high→low so indices stay valid; remember the first (lowest) for undo.
        doomed.sort_unstable();
        let first = doomed[0];
        if let Some(s) = self.app.tabs.get(first) {
            self.app.last_closed = Some(crate::restore::TabSpec {
                kind: s.kind().to_string(),
                host: s.meta.host.clone(),
                engine: s.meta.engine.clone(),
                port: s.port(),
                session: s.attach_session.clone(),
                name: s.meta.name.clone(),
            });
        }
        // Peel off each quiet tab high→low, keeping every tab-parallel bookkeeping vector in sync
        // (`forget_tab`) and—in native mode—dropping the matching window host. Native host removal
        // also re-derives focus from the active window, so the active index is only re-anchored
        // manually in the single-window path.
        let active = self.app.active;
        for &i in doomed.iter().rev() {
            self.forget_tab(i);
            self.app.tabs.remove(i);
            if self.native_tabs {
                self.native_remove_host(i);
            }
        }
        if !self.native_tabs {
            self.app.active = reanchor_active_after_batch(active, &doomed);
        }
        crate::restore::save(&self.app.tab_specs());
        self.save_pin_state();
        self.flash = Some((
            format!(
                "closed {} quiet tab{}",
                doomed.len(),
                if doomed.len() == 1 { "" } else { "s" }
            ),
            std::time::Instant::now(),
        ));
    }

    /// `prefix+M`: toggle global Do-Not-Disturb. When on, no OS notifications fire fleet-wide — the
    /// backgrounded-busy nag and the terminal-bell popup are both swallowed at `queue_notify`. In-bar
    /// busy/bell badges still render, so a diver who mutes popups can still see, later, that a tab
    /// rang. Toggle again to restore notifications. The status line shows `🔕` while on.
    fn toggle_dnd(&mut self) {
        self.dnd = !self.dnd;
        let state = if self.dnd {
            "do-not-disturb ON"
        } else {
            "notifications restored"
        };
        self.flash = Some((state.to_string(), std::time::Instant::now()));
    }

    /// `prefix+!`: send an interrupt (Ctrl-C) to the active session so a diver can stop a runaway agent
    /// run without dropping into its raw terminal. `Session::write` handles the typing — buffered
    /// into `pending` if the transport is momentarily down, flushed on reconnect.
    fn interrupt_active(&mut self) {
        let Some(head) = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
        else {
            return;
        };
        if let Some(s) = self.app.active_session() {
            s.write(b"\x03");
            self.flash = Some((
                format!("{head} interrupted (Ctrl-C)"),
                std::time::Instant::now(),
            ));
        }
    }

    /// Send an interrupt (Ctrl-C) to every session — the "stop the whole fleet" hammer a diver
    /// reaches for when several agents are looping at once. Muted tabs are off-limits (their
    /// silence means "leave me alone"), matching how fleet notices skip them. Buffered into
    /// `pending` for down transports just like a normal write, so it still lands on reconnect.
    fn interrupt_fleet(&mut self) {
        let mut sent = 0usize;
        for (i, s) in self.app.tabs.iter().enumerate() {
            if self.muted.get(i).copied().unwrap_or(false) {
                continue;
            }
            s.write(b"\x03");
            sent += 1;
        }
        if sent == 0 {
            self.flash = Some((
                "no sessions to interrupt (all muted)".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        self.flash = Some((
            format!(
                "sent Ctrl-C to {sent} session{}",
                if sent == 1 { "" } else { "s" }
            ),
            std::time::Instant::now(),
        ));
    }

    /// Force a live-again attempt on the active tab, ignoring its auto-reconnect backoff. A no-op for
    /// local PTYs (nothing to re-attach) and alive tabs, so it only nudges dead remote panes.
    fn reconnect_active(&mut self) {
        let Some(s) = self.app.active_session_mut() else {
            return;
        };
        if s.kind() == "pty" || s.alive() {
            self.flash = Some((
                format!("{} is fine", s.meta.engine),
                std::time::Instant::now(),
            ));
            return;
        }
        match s.reconnect_now() {
            Ok(()) => {
                self.flash = Some((
                    format!("{} reconnected", s.meta.engine),
                    std::time::Instant::now(),
                ))
            }
            Err(_) => {
                self.flash = Some((
                    format!("{} still unreachable — will keep retrying", s.meta.engine),
                    std::time::Instant::now(),
                ))
            }
        }
    }

    /// `prefix+T`: force a live-again attempt on EVERY down remote pane at once, ignoring each
    /// transport's auto-reconnect backoff. The fleet-shaped cousin of `reconnect_active`: when
    /// several hosts dropped together (a network blip, a box reboot), one key re-runs the connect
    /// path for all of them instead of tab-hopping and pressing `R` per pane. PTYs are skipped
    /// (nothing to re-attach); the focused tab's reconnect, when down, also fires so the dive
    /// session isn't left behind. Flashes how many were attempted vs. actually reached.
    fn reconnect_all_down(&mut self) {
        let down: Vec<usize> = (0..self.app.tabs.len())
            .filter(|&i| {
                let s = &self.app.tabs[i];
                s.kind() != "pty" && !s.alive()
            })
            .collect();
        if down.is_empty() {
            self.flash = Some((
                "no down panes — fleet is healthy".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let mut ok = 0usize;
        let mut still: Vec<(String, String)> = Vec::new();
        for i in &down {
            match self.app.tabs[*i].reconnect_now() {
                Ok(()) => ok += 1,
                Err(e) => {
                    let host = {
                        let h = self.app.tabs[*i].meta.host.clone();
                        if h.is_empty() {
                            "local".to_string()
                        } else {
                            h
                        }
                    };
                    let reason = if e.to_string().is_empty() {
                        e.kind().to_string()
                    } else {
                        e.to_string()
                    };
                    still.push((host, reason));
                }
            }
        }
        self.flash = Some((fmt_reconnect_summary(ok, &still), std::time::Instant::now()));
    }

    /// Kill the active tab's pane (remote tmux/ssh/tunnel), then close the tab so the watchdog
    /// doesn't reconnect it. The one way a fleet diver reclaims a runaway agent's resources on a
    /// remote host from here. Local PTYs have no separate session, so it's just a close.
    fn destroy_active(&mut self) {
        let Some(head) = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
        else {
            return;
        };
        // Destroy first, while the session object still owns the transport. A pinned tab is killed
        // just like an unpinned one — destroy is explicit and removing resources is the point.
        let kind = self.app.active_session().map(|s| s.kind().to_string());
        if let Some(s) = self.app.active_session() {
            s.destroy();
        }
        let note = match kind.as_deref() {
            Some("pty") => format!("closed {head}"),
            Some(k) => format!("killed {k} pane {head}"),
            None => return,
        };
        self.flash = Some((note, std::time::Instant::now()));
        let closed = self.app.active;
        if crate::native::close_tab(&mut self.app, false) {
            self.forget_tab(closed);
            if self.native_tabs {
                self.native_remove_host(closed);
            }
        }
    }

    /// Fork the active tab, preserving its pin state: a pinned clone (prefix+k / palette
    /// Duplicate) protects the same agent run the original was protecting. No-op for local PTYs.
    fn duplicate_active_preserving_pin(&mut self) {
        let was_pinned = self.pinned.get(self.app.active).copied().unwrap_or(false);
        let before = self.app.tabs.len();
        self.app.duplicate_active();
        if was_pinned && self.app.tabs.len() > before {
            self.pinned.resize(self.app.tabs.len(), false);
            self.pinned[self.app.tabs.len() - 1] = true;
            self.save_pin_state();
        }
    }

    /// `prefix+a` (default key `A`): toggle pin on the active tab. A pinned tab is protected from
    /// accidental close (`x` / prefix+close_tab refuse it with a flash), so a long-running agent a
    /// diver cares about stays until deliberately unpinned. Persists across restarts like mute.
    fn toggle_pin_active(&mut self) {
        if self.app.active < self.app.tabs.len() {
            let on = !self.pinned.get(self.app.active).copied().unwrap_or(false);
            self.pinned.resize(self.app.active + 1, false);
            self.pinned[self.app.active] = on;
            let state = if on { "PINNED 🔒" } else { "unpinned" };
            let head = self
                .app
                .active_session()
                .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
                .unwrap_or_default();
            self.flash = Some((format!("{head} {state}"), std::time::Instant::now()));
            self.save_pin_state();
        }
    }

    /// Toggle pin (protect-from-close) on an arbitrary tab — the per-row sibling of
    /// `toggle_pin_active`, used by the peek and fleet-grid triage so a diver can shield an agent
    /// straight from the list. Mirrors the active-arm's resize-into-range guard against a fresh tab.
    fn toggle_pin_at(&mut self, i: usize) {
        if i >= self.app.tabs.len() {
            return;
        }
        let on = !self.pinned.get(i).copied().unwrap_or(false);
        self.pinned.resize(i + 1, false);
        self.pinned[i] = on;
        let state = if on { "PINNED 🔒" } else { "unpinned" };
        let head = self
            .app
            .tabs
            .get(i)
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
            .unwrap_or_default();
        self.flash = Some((format!("{head} {state}"), std::time::Instant::now()));
        self.save_pin_state();
    }

    /// `prefix+v`: toggle focus mode — hide the tab bar + status line so the grid fills the whole
    /// window for a distraction-free dive. The resize runs `redraw` which re-sizes the session to the
    /// now-larger grid. Toggle again to bring the chrome back.
    fn toggle_focus(&mut self) {
        self.focus = !self.focus;
        let state = if self.focus { "focus" } else { "chrome" };
        self.flash = Some((state.to_string(), std::time::Instant::now()));
        self.redraw();
    }

    /// `prefix+I`: mark the whole fleet read at once. Re-baselines every backgrounded tab's seen
    /// history (so their busy `!N` badges, stale bell 🔔 and recovery ↻ badges clear) and swallows
    /// the not-yet-fired busy nudge. Turning back to the fleet, a diver often has a scatter of stale
    /// badges from runs that finished while they were elsewhere; this collapses them so the NEXT
    /// output is the only thing that nags. Fresh output still refills a badge normally — it's a
    /// baseline reset, not a mute.
    fn mark_all_read(&mut self) {
        let n = self.app.tabs.len();
        for i in 0..n {
            // Re-baseline every tab the way the active one already is each frame: future output
            // that exceeds where things stand now is what badges again.
            if let Some(s) = self.app.tabs.get(i) {
                self.seen_history[i] = s.history_len();
            }
            self.grew_delta[i] = 0;
            self.unread[i] = 0;
            self.notified[i] = false;
            // Drop any bell/recovery badges still fading.
            self.bell_until[i] = None;
            self.recover_until[i] = None;
        }
        self.flash = Some((
            "marked all tabs read".to_string(),
            std::time::Instant::now(),
        ));
    }

    fn redraw(&mut self) {
        // Native-tab mode: one window per session. Route every frame through the per-host renderer
        // instead of the single-window path (which draws the in-app fleet tab bar / status line —
        // the OS title-bar tab bar replaces those).
        if self.native_tabs {
            self.redraw_hosts();
            return;
        }
        let (Some(w), Some(h)) = (
            NonZeroU32::new(self.size.width),
            NonZeroU32::new(self.size.height),
        ) else {
            return;
        };
        let (width, height) = (w.get() as usize, h.get() as usize);
        // Keep the softbuffer surface buffer in lockstep with the window's physical size.
        // SOFTBUFFER-PITFALL: on macOS the CG backend only resizes its internal buffer when we
        // call `Surface::resize` — it does NOT track the window bounds on its own (unlike winit,
        // which reports Resized in physical px). If we skip this, an enlarged window renders into
        // a stale small buffer, so `buffer_mut` hands back a tiny top-left patch and the rest of
        // the window is transparent white. `resize` just sets two fields and `buffer_mut` reallocs
        // per frame anyway, so calling it every frame is both correct and free.
        if let Some(s) = &mut self.surface {
            let _ = s.resize(w, h);
        }
        // 0x00RRGGBB (softbuffer's native format). Start from the resolved theme background so the
        // margins around the grid (and any uncovered pixels) read as the theme's canvas rather
        // than a hard black frame — the grid, chrome panels, and overlays paint over it. The buffer
        // is the kept-across-frames scratch (capacity reused), so streaming doesn't heap-allocate a
        // whole new framebuffer every frame.
        let mut fb = std::mem::take(&mut self.scratch_fb);
        fb.resize(width, height);
        let fbargb = argb(255, self.colors.bg.0, self.colors.bg.1, self.colors.bg.2);
        for p in fb.pixels.iter_mut() {
            *p = fbargb;
        }
        // Render into the CPU framebuffer first (doesn't borrow the surface).
        self.frame = self.frame.wrapping_add(1);
        self.render(&mut fb);

        // Sync the OS window title to the active tab's live OSC title so the fleet Diver sees at a
        // glance what the pane is doing even when the window is minimized/unfocused. Only call
        // set_title when the string actually changed.
        if let Some(w) = &self.window {
            let title = match self.app.active_session() {
                Some(s) => match s.live_title() {
                    Some(t) => format!("{t} — harness-terminal"),
                    None => s
                        .meta
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{}@{}", s.meta.engine, s.meta.host)),
                },
                None => "harness-terminal".to_string(),
            };
            if title != self.window_title {
                w.set_title(&title);
                self.window_title = title;
            }
        }

        // Then present it via the softbuffer surface.
        let Some(surface) = &mut self.surface else {
            self.scratch_fb = fb;
            return;
        };
        let Ok(mut buffer) = surface.buffer_mut() else {
            self.scratch_fb = fb;
            return;
        };
        for (dst, src) in buffer.iter_mut().zip(fb.pixels.iter()) {
            *dst = *src;
        }
        let _ = buffer.present();
        self.scratch_fb = fb;
    }

    /// Render the whole frame into the framebuffer.
    fn render(&mut self, fb: &mut Framebuffer) {
        // In focus mode both bars collapse to zero so the grid fills the window edge to edge.
        let tab_h = self.chrome_top();
        let status_h = self.chrome_bottom();
        let term_top = tab_h;
        let term_bottom = fb.height.saturating_sub(status_h);
        let gline_px = self.cell_h as usize;
        let gcol_px = self.cell_w as usize;
        let grid_lines = term_bottom.saturating_sub(term_top).max(1) / gline_px;
        let grid_cols = fb.width.max(1) / gcol_px;

        // Size the active session to the on-screen grid.
        if let Some(active) = self.app.active_session() {
            let g = active.term.lock();
            let (gl, gc) = (g.screen_lines(), g.columns());
            drop(g);
            if gl != grid_lines || gc != grid_cols {
                let size = TermSize {
                    lines: grid_lines.max(1),
                    cols: grid_cols.max(1),
                };
                active.resize(size);
            }
        }

        // Terminal grid.
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            // With history, a non-zero display_offset is a live-follow "scroll" (bottom = offset 0).
            // Auto-return to the live view when new content pushes us to the bottom again: if we're
            // not mid-gesture (self.scrolled is false) force the view back to the latest line.
            let at_bottom = g.grid().display_offset() == 0;
            // Only force back to live if THIS session isn't pinned into history by the user.
            if !active.scrolled() && !at_bottom {
                use alacritty_terminal::grid::Scroll;
                g.grid_mut().scroll_display(Scroll::Bottom);
            }
            // Compute the current text-selection range (if any) so draw_grid can highlight it.
            let sel = g.selection.as_ref().and_then(|s| s.to_range(&g));
            let copy = if self.copy_mode {
                Some(self.copy_pos)
            } else {
                None
            };
            draw_grid(
                fb,
                &g,
                self.cell_w,
                self.cell_h,
                self.font_px,
                &mut self.cache,
                &self.colors,
                self.find_hit,
                &self.find_all,
                sel.as_ref(),
                copy,
            );
        } else {
            // No sessions open — draw a short 'how to start' hint so the window isn't a blank void.
            let cy = term_top + (grid_lines / 2) * gline_px;
            let chord = self.prefix_chord();
            let hint = format!(
                "no sessions ·  Cmd+T  new tab   {chord} n  new   {chord} r  attach remote   {chord} /  palette "
            );
            let hw = draw_text(fb, &mut self.cache, &hint, 0, cy, self.font_px, CHROME_DIM);
            let cx = fb.width.saturating_sub(hw) / 2;
            // Blank the row so the sizing pass above doesn't leave a left-aligned ghost, then paint
            // a single centered copy. (All pixels are black here to begin; clearing is a no-op but
            // keeps this robust if the grid later draws under the hint.)
            for py in cy.saturating_sub(self.font_px as usize)..(cy + self.font_px as usize) {
                for px in 0..fb.width {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = argb(0, 0, 0, 0);
                    }
                }
            }
            draw_text(fb, &mut self.cache, &hint, cx, cy, self.font_px, CHROME_DIM);
        }

        // Tab bar (top row). Flag backgrounded tabs that produced output since we last looked.
        // The activity pass (busy/bell detection + coalesced notifications) runs EVERY frame,
        // including focus mode where the bar is hidden — hiding the chrome must not silence the
        // fleet: a backgrounded agent finishing still nudges there.
        let activity = self.activity_flags();
        // Capture the live busy signal and refresh the idle detector's baseline: any tab producing
        // output right now keeps the loop at full rate for a smooth spinner; a content-length change
        // wakes an idle loop on the next tick so fresh output is never left un-drawn.
        self.live_busy = activity.iter().any(|&b| b);
        self.detect_len.resize(self.app.tabs.len(), 0);
        self.content_sig.resize(self.app.tabs.len(), 0);
        for (i, s) in self.app.tabs.iter().enumerate() {
            self.detect_len[i] = s.history_len();
            let sig = {
                let g = s.term.lock();
                crate::render::visible_signature(&g)
            };
            self.content_sig[i] = sig;
        }
        self.newtab_btn = None;
        if self.focus {
            // Focus mode: no tab bar — the grid owns the full height.
        } else {
            // Panel backdrop behind the whole (two-row) tab strip, plus a 1px hairline along its
            // bottom edge so the chrome reads as a distinct surface separated from the grid rather
            // than text bleeding into it (native tab-strip look).
            fill_rect(fb, 0, 0, fb.width, tab_h, CHROME_BG);
            fill_rect(fb, 0, tab_h.saturating_sub(1), fb.width, 1, CHROME_HAIR);
            let tab_base = self.chrome_top() / 2;
            // Raised active-tab sheet: inset from the strip top so rounded corners read as a tab
            // sticking up, running flush to the strip bottom so it visually connects to content.
            let inset_top = 3usize;
            let sheet_h = tab_h.saturating_sub(inset_top);
            let mut x = 6usize;
            self.tab_rects.clear();
            for (i, s) in self.app.tabs.iter().enumerate() {
                let active = i == self.app.active;
                // Color the active tab's dot by its host so a fleet diver reads 'which machine' at a
                // glance; matches the host hue used across tabs and it's stable across restarts.
                let dot = if active { "●" } else { "○" };
                // Show the pane's live OSC title (what the agent is doing) when it has announced one.
                let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
                let mut live = live.replace('\n', " ");
                if live.chars().count() > 18 {
                    live = live.chars().take(18).collect::<String>() + "…";
                }
                // A magnitude badge shows how much a backgrounded tab produced since we last looked
                // (rooted at 1 so one line displays as "!1", capped so it doesn't eat the bar); a muted
                // tab is dimmed and shows M instead so its silence is read at a glance.
                // Cumulative unread lines since last looked (fed by `unread`, cleared on focus /
                // mark-read) — shown whenever non-zero so a settled agent's badge lingers, per the
                // doc comment, instead of vanishing the instant output pauses.
                let delta = self.unread.get(i).copied().unwrap_or(0);
                let flag = if delta > 0 {
                    format!("!{}", delta.min(999))
                } else {
                    String::new()
                };
                let mute = if self.muted.get(i).copied().unwrap_or(false) {
                    " M "
                } else {
                    " "
                };
                // A small spinner next to the busy badge shows a tab that is producing output THIS
                // FRAME — output appearing on screen as you watch — distinct from the cumulative
                // `!N` badge (which lingers after an agent settles). The glyph cycles on the frame
                // counter, so live tabs visibly turn while idle ones sit still. Bottom frames are
                // graphite rather than the accent so the spinning reads as motion, not a badge.
                let spin = if activity[i] {
                    ["◐", "◓", "◑", "◒"][(self.frame as usize / 2) % 4]
                } else {
                    ""
                };
                // A pinned tab shows a small 🔒 so its protected status (won't close with x) is
                // readable at a glance, next to the mute marker.
                let pin = if self.pinned.get(i).copied().unwrap_or(false) {
                    "🔒"
                } else {
                    " "
                };
                // Show the user's rename if set; otherwise the plain engine id.
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                // For a pane-backed (non-local) tab, append @host so a diver reads WHERE each tab
                // runs without hovering — a local PTY has nothing to say here and stays bare.
                let where_s = if s.kind() == "pty" {
                    String::new()
                } else {
                    format!("@{}", s.meta.host)
                };
                // A 🔔 badge marks a terminal bell (a long agent run finishing) for a few seconds.
                // Drawn before the busy flag stays out of the label's own magnitude space.
                let bell = if self.bell_until.get(i).copied().flatten().is_some() {
                    "🔔 "
                } else {
                    ""
                };
                // A recovery badge marks a pane that just reconnected (down → alive), fading on its
                // own like the bell badge. Reading `↻` at a glance beats silently watching a host
                // come back.
                let recover = if self.recover_until.get(i).copied().flatten().is_some() {
                    "↻ "
                } else {
                    ""
                };
                // A persistent `↓` marks a pane that is currently down (disconnected / in reconnect
                // backoff), so in the fleet bar a dead pane reads at a glance instead of only
                // surfacing in the status count / peek. The counterpart to the transient `↻` for
                // recovery; live local PTYs are never "down", so they never get the marker.
                let down = if !s.alive() && s.kind() != "pty" {
                    "↓"
                } else {
                    ""
                };
                // A down pane with queued type-ahead shows how much is staged to flush on reconnect,
                // so input parked for a host coming back is visible in the fleet bar, not just the
                // status line. (This is a 5-tier red "queued" marker, drawn dim on non-active tabs.)
                let queued = s.pending_bytes();
                let queued_mark = if queued > 0 {
                    format!("⏳{queued}")
                } else {
                    String::new()
                };
                // Split the label so a pane-backed tab's `@host` suffix can be drawn in its own
                // per-host color (like peek does), letting a multi-machine fleet scan by host across
                // the whole bar rather than only the active pill. `pre`/`post` keep their engine-
                // accent color; the `@host` segment is tinted by the same stable `host_color` the
                // active pill already paints its full label in. Local PTYs have no host to color.
                let pre = format!(
                    " {}{}{}{}{}{}{} {}{}",
                    down, bell, recover, flag, spin, pin, head, live, mute
                );
                let post = format!("{queued_mark}{dot} ");
                // Active tab: tinted by a stable hash of its host (dive context). Inactive tabs fall back
                // to the engine's own accent color so you can spot the "claude" tab from across the bar.
                let color = if active {
                    host_color(&s.meta.host)
                } else {
                    engine_accent(&s.meta.engine, &self.colors.accents)
                };
                let pre_w = text_width(&mut self.cache, &pre, self.font_px);
                let host_w = if where_s.is_empty() {
                    0
                } else {
                    text_width(&mut self.cache, &where_s, self.font_px)
                };
                let post_w = text_width(&mut self.cache, &post, self.font_px);
                let label_w = pre_w + host_w + post_w;
                if active {
                    // Raised native tab: a rounded-top sheet (Safari/Chrome silhouette) with a thin
                    // top bevel and a 2px host-color underline at the strip bottom so the current
                    // session pops and reads 'which machine' at a glance.
                    filled_round_top(fb, x, inset_top, label_w, sheet_h, 7, CHROME_ACTIVE_BG);
                    fill_rect(fb, x, inset_top, label_w, 1, CHROME_SHEET_HI);
                    fill_rect(fb, x, tab_h.saturating_sub(2), label_w, 2, color);
                } else if self.hover_tab == Some(i) {
                    // Subtle hover chip so the pointer target reads before you commit to a switch.
                    filled_round_top(fb, x, inset_top, label_w, sheet_h, 7, CHROME_HOVER);
                }
                if !active && !where_s.is_empty() {
                    draw_text(fb, &mut self.cache, &pre, x, tab_base, self.font_px, color);
                    draw_text(
                        fb,
                        &mut self.cache,
                        &where_s,
                        x + pre_w,
                        tab_base,
                        self.font_px,
                        host_color(&s.meta.host),
                    );
                    draw_text(
                        fb,
                        &mut self.cache,
                        &post,
                        x + pre_w + host_w,
                        tab_base,
                        self.font_px,
                        color,
                    );
                } else {
                    // Active pill (already host-tinted) or a local tab with no host: one color.
                    let full = format!("{pre}{where_s}{post}");
                    draw_text(fb, &mut self.cache, &full, x, tab_base, self.font_px, color);
                }
                // The tab's full hit region is its label plus the inter-tab spacing; record it so
                // hover/close hit-testing tracks exactly what was painted.
                let x_end = x + label_w + 12;
                self.tab_rects.push((x, x_end));
                // A right-edge close × appears on the tab under the pointer (iTerm2/Chrome-style).
                // The × rides the active pill's right margin; pinned tabs still offer it, but the
                // click handler refuses and flashes the guard hint instead of closing.
                if self.hover_tab == Some(i) {
                    let cw = self.cache.glyph('×', self.font_px, false).0 as usize;
                    let cxx = x_end.saturating_sub(cw).saturating_sub(2);
                    let close_color = if active {
                        // On the active pill the label is already host-tinted; the × stays neutral
                        // dim so it doesn't fight the busy badge or spin glyph.
                        CHROME_DIM
                    } else {
                        CHROME_FG
                    };
                    draw_text(
                        fb,
                        &mut self.cache,
                        "×",
                        cxx,
                        tab_base,
                        self.font_px,
                        close_color,
                    );
                }
                x = x_end;
                if x > fb.width.saturating_sub(20) {
                    // Tabs past the window edge get clipped with no hint. Show how many are hidden
                    // so a fleet diver knows the bar is truncating rather than assuming fewer tabs.
                    let hidden = self.app.tabs.len() - i - 1;
                    if hidden > 0 {
                        draw_text(
                            fb,
                            &mut self.cache,
                            &format!("⋯ +{hidden}"),
                            fb.width.saturating_sub(58),
                            tab_base,
                            self.font_px,
                            CHROME_DIM,
                        );
                    }
                    break;
                }
            }
            // Fleet triage count, right edge: `↓N` panes down/reconnecting and `!M` busy, so when
            // the fleet is quiet (no per-tab badges, no notifications firing) a diver still sees at
            // a glance that 2 machines went dark or 3 agents are producing. Only shown when non-zero
            // (a fully-healthy, idle fleet draws nothing — no chrome noise). The active tab is
            // excluded from the busy count as it's what we're looking at.
            let down = self
                .app
                .tabs
                .iter()
                .filter(|s| !s.alive() && s.kind() != "pty")
                .count();
            let busy = activity
                .iter()
                .enumerate()
                .filter(|&(i, &b)| b && i != self.app.active)
                .count();
            // Queued type-ahead across down panes (sum of staged bytes) — a host coming back with
            // parked input deserves the triage's attention too.
            let queued: usize = self.app.tabs.iter().map(|s| s.pending_bytes()).sum();
            // Quiet (not recently-produced-output) live sessions — the inverse of busy: a session
            // that's sat silent past the threshold is likely done / parked waiting on you. Shown as
            // a dim `⌛N` alongside busy's `!M` so the two triage counters read together.
            let (any_quiet, quiet_n, _) = self.quiet_flags();
            // Panes that just came back (transient `↻` recovery badge) — the peek header already
            // reports these; echo them here so the always-on triage matches and a fleet-wide burst
            // of recovery is visible on the status line, not just when the peek is open.
            let rec_n = self.recover_until.iter().filter(|r| r.is_some()).count();
            if down > 0 || busy > 0 || queued > 0 || any_quiet || rec_n > 0 || self.dnd {
                let mut triage = String::new();
                if down > 0 {
                    triage += &format!("↓{down} ");
                }
                if rec_n > 0 {
                    triage += &format!("↻{rec_n} ");
                }
                if busy > 0 {
                    triage += &format!("!{busy} ");
                }
                if queued > 0 {
                    triage += &format!("⏳{queued} ");
                }
                if any_quiet {
                    triage += &format!("⌛{quiet_n} ");
                }
                if self.dnd {
                    triage += "🔕 ";
                }
                draw_text(
                    fb,
                    &mut self.cache,
                    &triage,
                    fb.width
                        .saturating_sub((triage.chars().count() * self.cell_w as usize) + 46),
                    tab_base,
                    self.font_px,
                    if down > 0 { CHROME_ERR } else { CHROME_DIM },
                );
            }
            // "New tab" (+) at the strip's right edge — the universal native affordance
            // (Chrome/Safari/iTerm2). The hover chip matches the active-tab sheet so it reads as a
            // tab you can drop a new one into; a click (via `newtab_btn` hit-testing) opens the
            // New-Session picker.
            let btn_w = 18usize;
            let btn_x = fb.width.saturating_sub(btn_w + 6);
            let hx = self.cursor.0 as usize;
            let hover_new =
                self.cursor.1 < self.chrome_top() as f64 && hx >= btn_x && hx < btn_x + btn_w;
            if hover_new {
                filled_round_top(fb, btn_x, inset_top, btn_w, sheet_h, 6, CHROME_ACTIVE_BG);
            }
            self.newtab_btn = Some((btn_x, 0, btn_w, tab_h));
            draw_text(
                fb,
                &mut self.cache,
                "+",
                btn_x + 5,
                tab_base,
                self.font_px,
                if hover_new { WHITE } else { CHROME_FG },
            );
        } // end if self.focus (tab bar)

        // Status line (bottom row): left = session info, right = hints.
        // `status_base` lives here (not inside the else) because the copy-mode banner below also
        // anchors to it and must keep rendering in focus mode.
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        if self.focus {
            // Focus mode: no status line either — just the grid.
        } else {
            // Panel backdrop behind the status line (bottom row), same surface as the tab bar.
            fill_rect(
                fb,
                0,
                fb.height.saturating_sub(status_h),
                fb.width,
                status_h,
                CHROME_BG,
            );
            // Top hairline on the status strip, mirroring the tab strip's bottom hairline, so the
            // two chrome bars bookend the grid with the same 1px edge (native panel symmetry).
            fill_rect(
                fb,
                0,
                fb.height.saturating_sub(status_h),
                fb.width,
                1,
                CHROME_HAIR,
            );
            let mut info = String::new();
            if let Some(s) = self.app.active_session() {
                let link = if s.alive() {
                    "●".to_string()
                } else {
                    // Show the live retry state (attempts + backoff seconds) so a dropped tunnel is
                    // visibly healing itself rather than silently sitting on a dead pane.
                    // If the diver typed into the dead pane, say how much is queued to flush on
                    // reconnect — otherwise they'd assume the keystrokes were lost.
                    let queued = s.pending_bytes();
                    let base = s
                        .retry_info()
                        .unwrap_or_else(|| "reconnecting…".to_string());
                    if queued > 0 {
                        format!("○ {base} · queued {}B", queued)
                    } else {
                        format!("○ {base}")
                    }
                };
                let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                info = format!(
                    " {} · {} · {} · [{} {}]",
                    s.meta.host,
                    head,
                    live,
                    s.kind(),
                    link
                );
                // Show how many scrollback lines this session has accumulated, so a diver monitoring a
                // long agent run sees growth at a glance without entering the tab.
                info += &format!(" · {} ln", s.history_len());
            }
            // Link-health badge for the whole fleet (refreshed on the throttled reconnect sweep): a
            // diver wants to know the e2ee tunnel to the harness daemon is up without opening the panel.
            let tunnel = if self.app.fleet.connected {
                "● tunnel up"
            } else {
                "○ tunnel down"
            };
            // Compact fleet-wide summary when several agents run at once: a glance at the status
            // line should surface the urgent counts without opening peek — N down then N busy.
            // A fleet that needs nothing reads as a quiet "ok". Pure aggregation of the already
            // computed per-tab live signals, so idle CPU is unaffected.
            let mut fleet = String::new();
            if self.app.tabs.len() > 1 {
                let down = self
                    .app
                    .tabs
                    .iter()
                    .filter(|s| s.kind() != "pty" && !s.alive())
                    .count();
                let busy = self.grew_delta.iter().filter(|&&d| d > 0).count();
                if down > 0 {
                    fleet += &format!("  {} ⚠ down", down);
                }
                if busy > 0 {
                    fleet += &format!("  {} ⚡ busy", busy);
                }
                if down == 0 && busy == 0 {
                    fleet = "  ✓ fleet ok".to_string();
                }
            }
            info = format!("  {}{} · {}", tunnel, fleet, info);
            // When the viewport is scrolled back from the live bottom, say so — a dead giveaway that
            // keys won't take you to fresh output until you press Escape (or the b key). Also show how
            // far back we are as a percentage so a long agent log stays navigable.
            let scrolled_now = self
                .app
                .active_session()
                .map(|s| s.scrolled())
                .unwrap_or(false);
            if scrolled_now {
                let pct = self
                    .app
                    .active_session()
                    .and_then(|s| {
                        let g = s.term.lock();
                        let hist = g.grid().history_size();
                        if hist == 0 {
                            None
                        } else {
                            let above = g.grid().display_offset().min(hist);
                            Some(((hist - above) * 100 / hist).min(100))
                        }
                    })
                    .unwrap_or(0);
                // A fully-scrolled-back log reads 100% (at the very top), which can look like 'live'; keep
                // it below 100 so '100% = at the top' stays unambiguous vs the live-bottom position.
                let pct = pct.min(99);
                info += &format!("  ▾ {pct}% (Esc/b to bottom)");
            }
            // A transient confirmation toast (e.g. "wrote 12k bytes → /path") overrides the session
            // info for a couple seconds so an export's destination is readable before it fades.
            if let Some((text, at)) = &self.flash {
                if at.elapsed() < std::time::Duration::from_secs(3) {
                    info = format!("  ⚑ {text}");
                } else {
                    self.flash = None;
                }
            }
            draw_text(
                fb,
                &mut self.cache,
                &info,
                6,
                status_base,
                self.font_px,
                CHROME_FG,
            );
            // While the prefix is armed (typed, next key is a command), show a distinct chip so the
            // user sees we're waiting for a command — and understands why the next key is consumed
            // rather than typed. This is how tmux-family UIs communicate the mode.
            if self.prefix_down {
                let chip = format!("  {} ", crate::keys::prefix_label(&self.prefix_key));
                let cw = text_width(&mut self.cache, &chip, self.font_px);
                let px0 = 6;
                let top = status_base.saturating_sub(self.font_px as usize);
                fill_rect(
                    fb,
                    px0,
                    top,
                    cw,
                    self.font_px as usize + 4,
                    CHROME_ACTIVE_BG,
                );
                draw_text(
                    fb,
                    &mut self.cache,
                    &chip,
                    px0,
                    status_base,
                    self.font_px,
                    WHITE,
                );
            }
            let hints = " prefix+/ palette  prefix+a broadcast  prefix+h search all  prefix+n new  prefix+r remote  prefix+s fleet  prefix+o busy  prefix+[ copy  prefix+p paste  prefix+l last  prefix+? help  prefix+q quit ";
            let hw = draw_text(
                fb,
                &mut self.cache,
                hints,
                6,
                status_base,
                self.font_px,
                CHROME_DIM,
            );
            // Move the hint to the right edge by re-drawing after clearing a wide column is complex;
            // simplest right-align: draw hints over the info end offset. We draw at the right edge:
            let hx = fb.width.saturating_sub(hw + 6);
            // Overwrite: clear the column first via black, then draw.
            for py in status_base.saturating_sub(self.font_px as usize)
                ..(status_base + self.font_px as usize)
            {
                for px in hx.min(fb.width)..fb.width {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = CHROME_BG_PX;
                    }
                }
            }
            draw_text(
                fb,
                &mut self.cache,
                hints,
                hx,
                status_base,
                self.font_px,
                CHROME_DIM,
            );
        } // end if self.focus (status line)

        // Copy mode banner: a prominent green status bar so the user knows keystrokes are captured
        // for navigation, with the current motion hints.
        if self.copy_mode {
            let selecting = if self.copy_anchor.is_some() {
                "[selecting]"
            } else {
                "[v=select]"
            };
            let msg = if self.copy_searching {
                format!(
                    " COPY SEARCH /{} · Enter jump · Esc cancel ",
                    self.copy_query
                )
            } else if !self.copy_query.is_empty() {
                format!(" COPY MODE · h/j/k/l/w/b move {} · n/N search /{} · Enter copy · / search · Esc quit ", selecting, self.copy_query)
            } else {
                format!(
                    " COPY MODE · h/j/k/l/w/b move {} · / search · Enter copy · Esc quit ",
                    selecting
                )
            };
            let cw = draw_text(
                fb,
                &mut self.cache,
                &msg,
                6,
                status_base,
                self.font_px,
                (0x00, 0x00, 0x00),
            );
            // Clear the region background to green behind the message for contrast.
            let green = argb(255, 0x18, 0xe0, 0x8a);
            for py in status_base.saturating_sub(self.font_px as usize)
                ..(status_base + self.font_px as usize)
            {
                for px in 0..cw.min(fb.width) {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = green;
                    }
                }
            }
            // Re-draw the message in black on green.
            draw_text(
                fb,
                &mut self.cache,
                &msg,
                6,
                status_base,
                self.font_px,
                (0x00, 0x00, 0x00),
            );
        }

        // Overlays render as modal dialogs: dim the whole frame first so the terminal content below
        // recedes and the overlay's own text is readable instead of fighting bright agent output.
        if self.app.overlay != Overlay::None {
            fb.dim(0.38);
        }
        match self.app.overlay {
            Overlay::Palette => self.render_palette(fb),
            Overlay::NewSession => self.render_new_session(fb),
            Overlay::RemoteAttach => self.render_remote(fb),
            Overlay::Find => self.render_find(fb),
            Overlay::FleetSearch => self.render_fleet_search(fb),
            Overlay::Fleet => self.render_fleet(fb),
            Overlay::Help => self.render_help(fb),
            Overlay::Rename => self.render_rename(fb),
            Overlay::Broadcast => self.render_broadcast(fb),
            Overlay::Peek => self.render_peek(fb),
            Overlay::FleetGrid => self.render_fleet_grid(fb),
            Overlay::CommandPalette => self.render_command_palette(fb),
            Overlay::Info => self.render_info(fb),
            Overlay::Hosts => self.render_hosts(fb),
            Overlay::None => {}
        }

        // Tab-bar hover tooltip draws last so it sits on top of everything (its own overlay-less
        // popover). Only shows in chrome mode (focus mode has no bar to hover).
        if self.hover_tab.is_some() {
            self.render_tooltip(fb);
        }
        // The right-click context menu draws on top of everything, including the tooltip.
        if self.ctx.is_some() {
            self.render_ctx_menu(fb);
        }
    }

    /// Height (device px) of the top chrome strip for the current focus state. The tab bar is a
    /// two-row strip so the raised native tab silhouette has room to breathe; focus mode hides it.
    fn chrome_top(&self) -> usize {
        if self.focus {
            0
        } else {
            self.cell_h as usize * 2
        }
    }

    /// Height (device px) of the bottom status strip for the current focus state.
    fn chrome_bottom(&self) -> usize {
        if self.focus {
            0
        } else {
            self.cell_h as usize
        }
    }

    fn overlay_base_y(&self) -> (usize, usize) {
        let line_px = self.font_px as usize + 6;
        (self.chrome_top() + 4, line_px)
    }

    /// Hover tooltip: a small popover under the hovered tab showing that session's live tail, so a
    /// fleet diver can check what a backgrounded agent is doing without switching out of the current
    /// tab. Draws last (on top of overlays). Click-through is deliberately not wired — it's purely a
    /// preview; switching still requires a click or the usual keys, so it never disrupts focus.
    fn render_tooltip(&mut self, fb: &mut Framebuffer) {
        let Some(i) = self.hover_tab else {
            return;
        };
        let Some(s) = self.app.tabs.get(i) else {
            return;
        };
        // Roughly the x center of the hovered tab: approximate by the hover cursor clamped to the
        // bar's width, which lands the popover near the label the pointer is actually over.
        let hx = (self
            .cursor
            .0
            .clamp(6.0, (self.size.width.max(60) - 40) as f64)) as usize;
        let base_y = self.chrome_top() + 8;

        // A busy tab's live screen is a moving blur (new chars land every frame), so show its
        // settled scrollback — the freshly-printed rows that have frozen in history — for a stable
        // read. An idle tab shows its live tail as usual.
        let mut lines: Vec<String> = if self.grew_delta.get(i).copied().unwrap_or(0) > 0 {
            let mut h = s.history_slice(5);
            if h.is_empty() {
                h = s.tail(5);
            }
            h
        } else {
            s.tail(5)
        };
        // Title / state header so the preview is self-describing.
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
        let alive = if s.alive() { "● live" } else { "○ down" };
        // Protection flags (pin 🔒 / mute M) appended so the peek preview shows shielding at a glance.
        let prot = match (
            self.pinned.get(i).copied().unwrap_or(false),
            self.muted.get(i).copied().unwrap_or(false),
        ) {
            (true, true) => " · pinned🔒 muted",
            (true, false) => " · pinned🔒",
            (false, true) => " · muted",
            (false, false) => "",
        };
        lines.insert(0, format!(" {} · {} · {}{}", head, live, alive, prot));
        // A down pane's hover explains how it's recovering and why it's down — the preview is
        // roomier than the tab/status line, so the reason fits here (clipped) without crowding
        // chrome. Only for non-local panes; a local pty has no transport to diagnose.
        if !s.alive() && s.kind() != "pty" {
            let retry = s
                .retry_info()
                .unwrap_or_else(|| "reconnecting…".to_string());
            lines.insert(1, format!("  ○ {}", clip_dots(&retry, 40)));
            if let Some(reason) = s.down_reason() {
                let reason = reason.trim();
                if !reason.is_empty() {
                    lines.insert(2, format!("  ↳ {}", clip_dots(reason, 56)));
                }
            }
        }
        if lines.len() > 1 {
            lines.push(" (click → switch to this tab) ".to_string());
        }

        // Measure the widest line so the panel hugs its content.
        let mut wmax = 0usize;
        let colpad = 18;
        for l in &lines {
            let mut w = 0usize;
            for ch in l.chars() {
                if ch != '\n' {
                    w += self.cache.glyph(ch, self.font_px, false).0 as usize;
                }
            }
            wmax = wmax.max(w);
        }
        let panel_w = wmax + colpad * 2;
        let row_px = self.font_px as usize + 4;
        let panel_h = lines.len() * row_px + 14;
        let px0 = hx.min(fb.width.saturating_sub(panel_w + 8));
        let py0 = base_y;
        // Remember the panel's rect so a click inside it (see `set_active`) can switch to this tab
        // without the pointer having to find the tab chip itself.
        self.tooltip_box = Some((px0, py0, panel_w, panel_h));
        // Fill the panel background (dim near-black) then a soft border.
        let bg = CHROME_BG_PX;
        for py in py0..(py0 + panel_h).min(fb.height) {
            for px in px0..(px0 + panel_w).min(fb.width) {
                fb.pixels[py * fb.width + px] = bg;
            }
        }
        let border = argb(255, 0x3a, 0x3a, 0x44);
        for px in px0..(px0 + panel_w).min(fb.width) {
            if py0 < fb.height {
                fb.pixels[py0 * fb.width + px] = border;
            }
            if py0 + panel_h - 1 < fb.height {
                fb.pixels[(py0 + panel_h - 1) * fb.width + px] = border;
            }
        }
        for py in py0..(py0 + panel_h).min(fb.height) {
            if px0 < fb.width {
                fb.pixels[py * fb.width + px0] = border;
            }
            if px0 + panel_w - 1 < fb.width {
                fb.pixels[py * fb.width + px0 + panel_w - 1] = border;
            }
        }
        // Draw the header bright, the tail dim.
        let ty = py0 + 8;
        for (k, l) in lines.iter().enumerate() {
            let color = if k == 0 { WHITE } else { CHROME_DIM };
            draw_text(
                fb,
                &mut self.cache,
                l,
                px0 + colpad,
                ty + k * row_px,
                self.font_px,
                color,
            );
        }
    }

    fn render_palette(&mut self, fb: &mut Framebuffer) {
        // Recompute the filter (mirrors tui::refresh_filter).
        self.app.refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            &format!("🔍 {}", self.app.query),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        let top = scroll_top(self.app.filtered.len(), self.app.selected, 12);
        for (row, &i) in self.app.filtered.iter().enumerate().skip(top).take(12) {
            let scr = row - top;
            let s = &self.app.tabs[i];
            let sel = row == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            if sel {
                overlay_row_sel(fb, base_y + (scr + 1) * line_px, line_px, 18);
            }
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            // Compact status flags so a jump carries context: live/pin/mute next to the name.
            // Status glyph so a jump carries triage context: `○` down, `!` busy (producing now),
            // `⌛` quiet (done / waiting on you). Reuses the already-sampled per-frame `grew_delta`
            // and the read-only `quiet_for`, so this is pure rendering — no re-sampling that could
            // double-fire notifications. A dead pane wins, then busy, then quiet, then blank live.
            let status = if !s.alive() {
                "○"
            } else if self.grew_delta.get(i).copied().unwrap_or(0) > 0 {
                "!"
            } else if self.quiet_for(i) {
                "⌛"
            } else {
                " "
            };
            let pin = if self.pinned.get(i).copied().unwrap_or(false) {
                "🔒"
            } else {
                " "
            };
            let mute = if self.muted.get(i).copied().unwrap_or(false) {
                "M"
            } else {
                " "
            };
            let flags = format!("{status}{pin}{mute}");
            let line = format!(
                "  {} {} · {} · {}  {}",
                flags,
                s.meta.host,
                name,
                s.meta.title,
                if sel { "◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (scr + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// New-session picker: a `dir:` working-directory line on top, then the engine list below
    /// (Up/Down selects the engine, typing edits the directory). Mirrors the RemoteAttach overlay.
    fn render_new_session(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  new session  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        // Rows: header(0), legend(1), dir(2), engines(3+). The engine list offsets below account
        // for the legend + dir slots.
        draw_text(
            fb,
            &mut self.cache,
            "  ✗ = not on PATH  ",
            32,
            base_y + line_px,
            self.font_px,
            CHROME_DIM,
        );
        if self.new_cwd.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  dir:  (blank = start_cwd · pre-filled from last use)  ",
                32,
                base_y + 2 * line_px,
                self.font_px,
                CHROME_FG,
            );
        } else {
            draw_text(
                fb,
                &mut self.cache,
                &format!("  dir: {}", self.new_cwd),
                32,
                base_y + 2 * line_px,
                self.font_px,
                CHROME_FG,
            );
        }
        let ordered = self.app.engine_order();
        // Cache which engines are actually on this machine's PATH so the picker isn't re-scanning
        // disk every frame (a half-dozen stat calls per tab-frame adds up); still a hint, not a
        // promise — a present binary can fail at spawn.
        let installed: Vec<bool> = ordered
            .iter()
            .map(|e| crate::engines::is_installed(e.cmd))
            .collect();
        for (i, e) in ordered.iter().enumerate() {
            let sel = i == self.app.selected;
            let present = installed[i];
            let color = if sel {
                WHITE
            } else if present {
                CHROME_DIM
            } else {
                CHROME_ERR
            };
            let mark = if sel {
                "◄"
            } else if present {
                ""
            } else {
                "✗"
            };
            let line = format!("  {}  {}  {}", e.id, e.label, mark);
            if sel {
                overlay_row_sel(fb, base_y + (i + 3) * line_px, line_px, 18);
            }
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (i + 3) * line_px,
                self.font_px,
                color,
            );
        }
        // Under the engine list, a running description of the selected engine so a diver can read
        // what each framework is before committing to a spawn.
        if let Some(e) = ordered.get(self.app.selected) {
            draw_text(
                fb,
                &mut self.cache,
                &format!("      {} — {}", e.label, e.desc),
                32,
                base_y + (ENGINES.len() + 3) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    /// Read-only fleet panel: the machine id + tunnel state, then one line per harness session with
    /// a live/stale marker. Esc (or any key) dismisses; `s` re-fetches. Never writes to a pane.
    fn render_fleet(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        const ROWS: usize = 20;
        // Recompute the filter each frame so typing filters live (mirrors the palette overlay).
        self.fleet_refresh_filter();
        let f = &self.app.fleet;
        let mid = if f.machine_id.is_empty() {
            "unknown".to_string()
        } else {
            f.machine_id.chars().take(6).collect()
        };
        let tunnel = if f.connected {
            "tunnel up"
        } else {
            "tunnel down"
        };
        let n = f.fleet.len();
        let shown = self.fleet_filtered.len();
        let q = if self.fleet_query.is_empty() {
            String::new()
        } else {
            format!("/{} ", self.fleet_query)
        };
        // Viewport for the scrolling list below; also feeds a "more above/below" hint in the header.
        let total = self.fleet_filtered.len();
        let top = scroll_top(total, self.app.selected, ROWS);
        let hidden_up = top;
        let hidden_down = total.saturating_sub(top + ROWS);
        let scroll = match (hidden_up, hidden_down) {
            (0, 0) => String::new(),
            (0, d) => format!(" · ▼{d} below"),
            (u, 0) => format!(" · ▲{u} above"),
            (u, d) => format!(" · ▲{u} ▼{d}"),
        };
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  fleet · {} · {} · {} session{} · {}{}type to filter · Up/Down+Enter to dive  ",
                mid,
                tunnel,
                n,
                if n == 1 { "" } else { "s" },
                scroll,
                q
            ),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if n == 0 {
            draw_text(
                fb,
                &mut self.cache,
                "  no harness sessions (daemon unreachable or nothing joined)  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        if shown == 0 && !self.fleet_query.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  no sessions match  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        // Scrolling viewport: keep the highlighted row on screen. Once the selection passes the
        // window it rides the bottom edge (classic terminal-list scroll), so a fleet bigger than a
        // screenful never hides sessions past the first 20 behind an invisible selection.
        for (row, &real) in self.fleet_filtered.iter().enumerate().skip(top).take(ROWS) {
            let scr = row - top;
            let s = &f.fleet[real];
            let live = s.is_live();
            let sel = row == self.app.selected;
            let mark = if live { "●" } else { "○" };
            if sel {
                overlay_row_sel(fb, base_y + (scr + 1) * line_px, line_px, 18);
            }
            let color = if sel {
                WHITE
            } else if live {
                (0x4a, 0xe0, 0x8a)
            } else {
                CHROME_DIM
            };
            let eng = if s.engine.is_empty() {
                "?"
            } else {
                s.engine.as_str()
            };
            let id = if s.session_id.is_empty() {
                s.tmux_pane.clone()
            } else {
                s.session_id.chars().take(8).collect()
            };
            // The daemon tags each session with its agent name/task (e.g. the harness `name`); show
            // it so a diver can tell which agent a row is at a glance, not just engine + id.
            let nm: String = if s.name.is_empty() {
                String::new()
            } else {
                let mut n = s.name.clone();
                if n.chars().count() > 12 {
                    n = n.chars().take(12).collect::<String>() + "…";
                }
                n
            };
            let line = format!(
                "  {} {}  {:<12} {}{}",
                mark,
                eng,
                nm,
                id,
                if sel { "  ◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (scr + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the fleet-search overlay: query prompt, total/selected match counts, the currently
    /// selected match's session + matching line, and a list of the first ~8 hits each prefixed with
    /// its tab label (engine / host). The selected row is highlighted.
    fn render_fleet_search(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let n = self.fleet_matches.len();
        // Header row: query, live match count, and "no matches" when the query misses everywhere.
        let (hdr, hdr_color) = if self.fleet_q.is_empty() {
            (
                "  search all sessions: (type to match every tab)  ".to_string(),
                CHROME_DIM,
            )
        } else if n == 0 {
            (
                format!("  search all sessions: {}  (no matches)", self.fleet_q),
                CHROME_DIM,
            )
        } else {
            let here = (self.fleet_sel % n) + 1;
            let totals = format!(
                "  search all sessions: {}  · {} match{} across {} tab{} · showing {} of {}  ",
                self.fleet_q,
                n,
                if n == 1 { "" } else { "es" },
                self.app.tabs.len(),
                if self.app.tabs.len() == 1 { "" } else { "s" },
                here,
                n,
            );
            (totals, WHITE)
        };
        draw_text(
            fb,
            &mut self.cache,
            &hdr,
            32,
            base_y,
            self.font_px,
            hdr_color,
        );

        // The list of matches: up to 8 rows, each prefixed with its tab's engine/host label. A
        // scrolling viewport keeps the selected match on screen when there are more than 8 hits.
        let rows = if self.fleet_q.is_empty() || n == 0 {
            0
        } else {
            8.min(n)
        };
        let top = scroll_top(n, self.fleet_sel, rows);
        for row in top..top + rows {
            let scr = row - top;
            let m = self.fleet_matches[row];
            // The session this match pointed at may have closed while the search overlay was open
            // (Cmd+W is handled in `about_to_wait`, outside this key handler). A stale `m.tab` into
            // `app.tabs` would panic — skip the row instead of crashing.
            if m.tab >= self.app.tabs.len() {
                continue;
            }
            let selected = row == self.fleet_sel;
            let color = if selected { WHITE } else { CHROME_DIM };
            // Tab label: user name (with engine when renamed) @ host; local (hostless) tabs get no @.
            let s = &self.app.tabs[m.tab];
            let base = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let identity = if base != s.meta.engine {
                format!("{base} ({})", s.meta.engine)
            } else {
                base
            };
            let label = if s.meta.host.is_empty() {
                identity
            } else {
                format!("{identity}@{}", s.meta.host)
            };
            // The matched line text, read live from that session's grid at render time.
            let raw: String = {
                let g = s.term.lock();
                let cols = g.columns();
                use alacritty_terminal::index::{Column, Line};
                // The matched row may have scrolled out or the grid resized since the match was
                // collected (e.g. streaming output pushed it off, or the pane shrank); guard with
                // the current valid display range so a stale line can't panic the renderer.
                let (top, bottom) = (g.grid().topmost_line().0, g.grid().bottommost_line().0);
                if m.line < top || m.line > bottom {
                    String::new()
                } else {
                    let row = &g.grid()[Line(m.line)];
                    row[Column(0)..Column(cols.min(row.len()))]
                        .iter()
                        .map(|c| c.c)
                        .collect()
                }
            };
            let snippet = if raw.trim().is_empty() {
                "(blank line)".to_string()
            } else {
                let budget = ((fb.width.saturating_sub(48)) / (self.cell_w.max(1) as usize))
                    .saturating_sub(label.chars().count() + 7)
                    .max(8);
                focus_snippet(raw.trim_end(), m.col, budget)
            };
            let line = format!(
                "  [{}] {}  {}",
                label,
                if selected { "◄" } else { " " },
                snippet
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (scr + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the host overview: one row per distinct host across open tabs, with its live/total
    /// tally, so a diver sees "which machines are up" at a glance. Enter jumps to that host's first
    /// tab, or `→` drills into the host's sessions (listing them so you can land on a specific one).
    /// Selection is the pure `host_tally` grouping, so the data is unit-tested.
    fn render_hosts(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        // Drill-in view: one row per session on the selected host.
        if let Some(host) = self.hosts_host.clone() {
            self.render_host_sessions(fb, base_y, line_px, &host);
            return;
        }
        let tally = self.owned_host_breakdown();
        draw_text(
            fb,
            &mut self.cache,
            "  hosts · which machines are up · ↑/↓ select · → drill · Enter → first tab · Esc close  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if tally.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  no hosts (no open sessions)  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        // A tab can close while the overlay is open; re-clamp like the other overlays.
        self.hosts_sel = self.hosts_sel.min(tally.len().saturating_sub(1));
        let top = scroll_top(tally.len(), self.hosts_sel, 20);
        for (row, (host, alive, total, mix)) in tally.iter().enumerate().skip(top).take(20) {
            let scr = row - top;
            let sel = row == self.hosts_sel;
            let label = host.as_str();
            let mark = if *alive == 0 {
                "○"
            } else if *alive == *total {
                "●"
            } else {
                "◐"
            };
            let state = if *alive == 0 {
                "down".to_string()
            } else if *alive < *total {
                format!("{alive}/{total} live")
            } else {
                "live".to_string()
            };
            let sess = if *total == 1 { "session" } else { "sessions" };
            let mix_s = format_engine_mix(mix);
            // A host that's fully down should say why, matching the rest of the app: pull the first
            // down pane's reconnect reason so a diver sees at a glance it's auth/refused/timeout
            // rather than a generic "down".
            let reason_tag = if *alive == 0 {
                let reason = self
                    .app
                    .tabs
                    .iter()
                    .filter(|t| t.meta.host == *host && t.kind() != "pty" && !t.alive())
                    .find_map(|t| t.down_reason())
                    .unwrap_or_else(|| "reconnecting…".to_string());
                let reason = clip_dots(&reason.trim().to_string(), 24);
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            } else {
                String::new()
            };
            if sel {
                overlay_row_sel(fb, base_y + (scr + 1) * line_px, line_px, 18);
            }
            let color = if sel {
                WHITE
            } else if *alive == 0 {
                CHROME_DIM
            } else {
                (0x4a, 0xe0, 0x8a)
            };
            draw_text(
                fb,
                &mut self.cache,
                &format!(
                    "  {mark} {label} · {state}{reason_tag} · {total} {sess} · {mix_s}{}",
                    if sel { "  ◄" } else { "" }
                ),
                32,
                base_y + (scr + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the drill-in list of sessions on `host` (the `→` sub-view of the host overview): one
    /// row per session in tab order, live/quiet marked, so a diver can land on a specific agent run.
    fn render_host_sessions(
        &mut self,
        fb: &mut Framebuffer,
        base_y: usize,
        line_px: usize,
        host: &str,
    ) {
        let idxs = self.host_session_indices(host);
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  {host} sessions · ↑/↓ select · Enter → open · r reconnect · b broadcast · ←/Esc back  "
            ),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if idxs.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  no sessions on this host  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        self.hosts_sel = self.hosts_sel.min(idxs.len().saturating_sub(1));
        let top = scroll_top(idxs.len(), self.hosts_sel, 20);
        // Available width for a row's tail preview (what the selected session is doing right now).
        let avail = (self.size.width.saturating_sub(72)).max(40) as usize;
        let mut extra = 0usize; // extra preview line rows inserted so later rows shift down.
        for (row, &tab) in idxs.iter().enumerate().skip(top).take(20) {
            let scr = row - top;
            let sel = row == self.hosts_sel;
            let y = base_y + (scr + 1 + extra) * line_px;
            if sel {
                overlay_row_sel(fb, y, line_px, 18);
            }
            let color = if sel {
                WHITE
            } else if self.app.tabs.get(tab).map(|s| s.alive()).unwrap_or(false) {
                (0x4a, 0xe0, 0x8a)
            } else {
                CHROME_DIM
            };
            let label = self.session_row_label(tab);
            draw_text(
                fb,
                &mut self.cache,
                &format!("  {label}{}", if sel { "  ◄" } else { "" }),
                32,
                y,
                self.font_px,
                color,
            );
            // Under the selected row, a dimmer one-line preview of what that agent is doing.
            if sel {
                if let Some(pv) = self.session_tail_preview(tab) {
                    let mut shown = 0usize;
                    let mut out = String::new();
                    let mut clipped = false;
                    for ch in pv.chars() {
                        let w = self.cache.glyph(ch, self.font_px, false).0 as usize;
                        if shown + w > avail {
                            clipped = true;
                            break;
                        }
                        shown += w;
                        out.push(ch);
                    }
                    if clipped {
                        out.push('…');
                    }
                    draw_text(
                        fb,
                        &mut self.cache,
                        &format!("   ↳ {out}"),
                        32,
                        y + line_px,
                        self.font_px, // dimmer, one size for simplicity
                        CHROME_DIM,
                    );
                    extra += 1;
                }
            }
        }
    }

    /// Owned, per-host tally over the open tabs (host as `String`, empty host normalized to
    /// `local`) — the data behind the host-overview overlay. Returns owned values so it can be
    /// called from a `&mut self` render/nav without borrowing the tab list past a mutation.
    fn owned_host_tally(&self) -> Vec<(String, usize, usize)> {
        let mut out: Vec<(String, usize, usize)> = Vec::new();
        for s in &self.app.tabs {
            let h = if s.meta.host.is_empty() {
                "local".to_string()
            } else {
                s.meta.host.clone()
            };
            match out.iter_mut().find(|(host, _, _)| *host == h) {
                Some((_, a, t)) => {
                    *t += 1;
                    if s.alive() {
                        *a += 1;
                    }
                }
                None => out.push((h, if s.alive() { 1 } else { 0 }, 1)),
            }
        }
        out
    }

    /// Owned per-host breakdown (host, alive, total, agent mix) over the open tabs, normalized like
    /// [`owned_host_tally`] — the display data for the host-overview. Owned so it can be called from
    /// a `&mut self` render without borrowing the tab list past a mutation.
    fn owned_host_breakdown(&self) -> Vec<(String, usize, usize, Vec<(String, usize)>)> {
        host_engine_breakdown(
            self.app
                .tabs
                .iter()
                .map(|s| (s.meta.host.as_str(), s.alive(), s.meta.engine.as_str())),
        )
    }

    /// Tab indices (in tab order) of sessions belonging to `host` (empty host normalized to
    /// `local`). The drill-in list for the host-overview; the returned order is the tab order.
    fn host_session_indices(&self, host: &str) -> Vec<usize> {
        session_indices_for_host(
            self.app
                .tabs
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.meta.host.as_str())),
            host,
        )
    }

    /// A one-line label for a tab in the host drill-in: `● claude (3) · name · osc-title`, where the
    /// (N) is the 1-based tab number and the trailing part is the session's live title if any.
    fn session_row_label(&self, tab: usize) -> String {
        let Some(s) = self.app.tabs.get(tab) else {
            return String::new();
        };
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let state = if s.alive() { "●" } else { "○" };
        let live = s
            .live_title()
            .map(|t| format!(" · {t}"))
            .unwrap_or_default();
        let where_s = if s.kind() == "pty" {
            String::new()
        } else {
            format!("@{}", s.meta.host)
        };
        // A down pane should say why in the drill-in too, matching the overview, peek, and status.
        let reason = if !s.alive() && s.kind() != "pty" {
            let r = s
                .down_reason()
                .unwrap_or_else(|| "reconnecting…".to_string());
            let r = clip_dots(&r.trim().to_string(), 18);
            if r.is_empty() {
                String::new()
            } else {
                format!(" ({r})")
            }
        } else {
            String::new()
        };
        // A quiet (awaiting-you) live session on this host says how long it has been parked,
        // matching the fleet grid and peek. Down wins (its reason is the more urgent signal).
        let idle = if !s.alive() && s.kind() != "pty" && self.quiet_for(tab) {
            let d = std::time::Instant::now()
                - self
                    .last_output
                    .get(tab)
                    .copied()
                    .unwrap_or(std::time::Instant::now());
            format!(" · ⌛{}", fmt_duration(d))
        } else {
            String::new()
        };
        format!(
            "{state} {} ({}){where_s} · {head}{reason}{idle}{live}",
            s.meta.engine,
            tab + 1
        )
    }

    /// Newest non-empty line of a session, trimmed — the one-line "what it's doing" preview shown
    /// under the selected drill-in row. None when the session is gone or has no printable output.
    fn session_tail_preview(&self, tab: usize) -> Option<String> {
        let s = self.app.tabs.get(tab)?;
        for line in s.tail(3) {
            let t = line.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        None
    }

    /// Keybinding reference overlay. Static list; dismiss on any key.
    /// The live key label for a prefix action, honoring any `[keybindings]` remap: `"prefix X"`.
    /// `key_action` is `key → action` (already resolved through config + defaults), so reverse it to
    /// find the active key for this action.
    fn prefix_label(&self, action: &str) -> String {
        let key = self
            .key_action
            .iter()
            .find(|(_, a)| a.as_str() == action)
            .map(|(k, _)| k.as_str())
            .unwrap_or(action);
        format!("prefix {key}")
    }

    /// The prefix chord to advertise in UI copy: `Ctrl+Space`, or `Ctrl+\` when macOS owns
    /// Ctrl+Space (a second input source makes the OS swallow it). The hint always matches what
    /// actually answers on this machine.
    fn prefix_chord(&self) -> String {
        crate::keys::prefix_label(&self.prefix_key)
    }

    fn render_help(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  harness-terminal keys  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        // Build the rows in display order. Remappable actions resolve their key label live (so a
        // `[keybindings]` remap shows the REAL key, not a stale default); fixed/multi-key rows are
        // literals. Each row is a `&str` key + `&str` description.
        let mut all: Vec<(String, String)> = Vec::new();
        for (action, desc) in [
            ("", "harness-terminal keys"),
            ("palette", "jump to any session"),
            ("command_palette", "command palette (all actions)"),
            ("new_session", "new session (engine + working dir)"),
            ("remote_attach", "attach to a remote pane@host"),
            ("fleet", "fleet status"),
            ("search", "search scrollback"),
            ("search_all", "search all sessions (fleet)"),
            ("copy_mode", "copy mode"),
            ("rename", "rename the active tab"),
            ("broadcast", "broadcast a line to all sessions"),
            ("paste", "paste clipboard (bracketed)"),
            ("next_busy", "jump to next busy tab"),
            ("next_quiet", "jump to next quiet (awaiting-you) tab"),
            ("next_down", "jump to next down/reconnecting tab"),
            ("next_pinned", "jump to next pinned tab"),
            ("next_host", "jump to next host (page the fleet by machine)"),
            ("hosts", "host overview: which machines are up"),
            ("dnd", "toggle do-not-disturb (mute all OS notifications)"),
            ("reconnect", "force reconnect active tab (bypass backoff)"),
            ("reconnect_all", "force reconnect ALL down panes at once"),
            ("destroy", "kill active tab's remote pane"),
            ("interrupt", "send Ctrl-C to active tab (stop the run)"),
            ("close_quiet", "close all quiet (done) tabs at once"),
            ("mute", "mute/unmute the active tab"),
            ("pin", "pin/unpin the active tab (protect from close)"),
            ("last_window", "flip to the previous tab"),
            ("undo_close", "undo close (reopen last closed tab)"),
            ("duplicate", "duplicate this tab (fork same engine@host)"),
            ("copy_scrollback", "copy whole scrollback to clipboard"),
            ("export_scrollback", "write scrollback to a .log file"),
            (
                "copy_identity",
                "copy session identity (engine@host) to clipboard",
            ),
            ("copy_fleet", "copy whole fleet summary (all tabs)"),
            ("peek", "peek tails of all sessions"),
            ("fleet_grid", "fleet grid: live tails of every session"),
            ("session_info", "show this tab's info (kind/host/task)"),
            (
                "mark_all_read",
                "clear every tab's busy + bell badges (mark the whole fleet read)",
            ),
            ("toggle_focus", "focus mode (hide tab bar + status)"),
            ("help", "this help"),
            ("quit", "quit"),
        ] {
            if action.is_empty() {
                all.push((self.prefix_chord(), "prefix (then a command)".to_string()));
            } else {
                all.push((self.prefix_label(action), desc.to_string()));
            }
        }
        for (k, d) in [
            ("prefix { }", "move tab left / right"),
            ("1-9 / 0 / Tab", "switch tab (0 = last)"),
            ("x / c", "close tab / go to tab 0"),
            ("g / G / b", "scroll up a page / to the top / jump to bottom"),
            ("Ctrl/Cmd+= / -", "font zoom (Ctrl+0 reset; Cmd+0 = last tab)"),
            ("Ctrl+Enter", "toggle fullscreen"),
            ("PgUp/PgDn", "scrollback"),
            ("Cmd/Ctrl+click", "open URL / file path"),
            (
                "Cmd+T / Cmd+N / Cmd+Shift+N",
                "new session (new native tab)",
            ),
            ("Cmd+W", "close active tab/window"),
            ("Cmd+Shift+T", "reopen last-closed tab"),
            ("Cmd+Shift+D", "duplicate active session"),
            ("Cmd+Q", "quit"),
            ("Cmd+Shift+[ / ]", "previous / next tab"),
            ("Ctrl+Tab / Ctrl+Shift+Tab", "previous / next tab"),
            ("Cmd+1-9 / 0", "jump straight to that tab (0 = last)"),
            ("Cmd+Shift+P", "command palette"),
            ("Cmd+F", "find in this session"),
            ("Cmd+G / Cmd+Shift+G", "next / previous find match"),
            ("Cmd+Shift+F", "search all sessions (fleet)"),
            ("Cmd+Shift+R", "force-reconnect ALL down panes"),
            ("Cmd+Shift+U / Cmd+Shift+M", "pin active tab / mute active tab"),
            ("Cmd+Shift+I", "show this tab's info (kind/host/task)"),
            ("Cmd+Shift+C", "copy whole scrollback to clipboard"),
            ("Cmd+Shift+S", "write scrollback to a .log file"),
            ("Cmd+Up / Cmd+Home · Cmd+Down / Cmd+End", "jump to top / bottom of scrollback"),
            ("Cmd+.", "interrupt the active session (stop the run)"),
            ("Cmd+Shift+J", "jump to the next quiet (awaiting-you) agent"),
            ("Cmd+Shift+K", "send Ctrl-C to every session (stop the fleet)"),
            ("Cmd+Shift+Y", "peek at every session's tail (war-room)"),
            (
                "Hosts drill · r / b",
                "reconnect this host / broadcast to this host",
            ),
            (
                "Peek · / filter · n down · r reconnect · m mute · p pin · x close",
                "narrow by host/engine/name · up/down/busy/quiet (compose: \"build05 down\") · n/r/m/p/x",
            ),
            (
                "Broadcast · Space · ⇧Space",
                "toggle one target · select all / clear all",
            ),
            (
                "Fleet grid · Space mark · b/C/m/x/X/r/R",
                "mark · broadcast · Ctrl-C · mute · close · bulk-close · reconnect sel/all · n next/prev trouble",
            ),
        ] {
            all.push((k.to_string(), d.to_string()));
        }

        for (row, (k, d)) in all.iter().enumerate() {
            draw_text(
                fb,
                &mut self.cache,
                &format!("  {:<16} {}", k, d),
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    fn render_info(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let Some(s) = self.app.active_session() else {
            return;
        };
        // Which line draws at which row. Title is the panel header; the rest are data rows.
        let title = format!("  {} · {}  ", s.kind(), s.meta.engine);
        draw_text(fb, &mut self.cache, &title, 32, base_y, self.font_px, WHITE);

        let mut rows: Vec<String> = Vec::new();
        let display = if let Some(n) = &s.meta.name {
            n.clone()
        } else {
            s.meta.engine.clone()
        };
        rows.push(format!("  name       {}", display));
        rows.push(format!("  host       {}", s.meta.host));
        rows.push(format!("  engine     {}", s.meta.engine));
        rows.push(format!("  transport  {}", s.kind()));
        rows.push(format!(
            "  state      {}",
            match s.retry_info() {
                Some(r) => format!("down · {r}"),
                None => "live".to_string(),
            }
        ));
        // Why it's down, when we know: the last reconnect failure's message (host unreachable,
        // auth rejected, timeout, …). Only in the info panel — the status line stays concise.
        if let Some(reason) = s.down_reason() {
            if !reason.trim().is_empty() {
                rows.push(format!("  reason     {reason}"));
            }
        }
        // Age / uptime — how long this session has been alive. Tells a long-running agent (hours
        // of work) from a just-spawned one at a glance, alongside the idle `silence` row below.
        rows.push(format!("  age        {}", fmt_duration(s.age())));
        // Idle age — how long this session has sat without producing output. Negative/zero means
        // it produced output this very frame (or we never sampled it); otherwise the readable time.
        if s.alive() && s.kind() != "pty" {
            let now = std::time::Instant::now();
            // Guard like every other tab-parallel read: a slot not yet sampled reads as idle-0.
            let last = self
                .last_output
                .get(self.app.active)
                .copied()
                .unwrap_or(now);
            let idle = now.saturating_duration_since(last);
            let unseen = self
                .seen_history
                .get(self.app.active)
                .copied()
                .unwrap_or(usize::MAX);
            let idle_txt = if idle.is_zero() || unseen == usize::MAX {
                "now".to_string()
            } else {
                format!("quiet {}", fmt_duration(idle))
            };
            rows.push(format!("  silence    {idle_txt}"));
        }
        // Staged type-ahead (visible in the bar too): how much input is parked to flush on reconnect.
        let queued = s.pending_bytes();
        if queued > 0 {
            rows.push(format!(
                "  queued     ⏳{}B staged, flush on reconnect",
                queued
            ));
        }
        rows.push(format!(
            "  view       {}",
            if s.scrolled() {
                "scrolled into history"
            } else {
                "live follow (bottom)"
            }
        ));
        // Fleet context on the active session's machine: how many of this host's sessions are open
        // and how many are dark. The host is what a diver manages, so it belongs in the per-session
        // details — a one-host micro-overview from a single tab's info panel.
        let host_norm = {
            let h = s.meta.host.clone();
            if h.is_empty() {
                "local".to_string()
            } else {
                h
            }
        };
        let same = session_indices_for_host(
            self.app
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| (i, t.meta.host.as_str())),
            &host_norm,
        );
        let down = same
            .iter()
            .filter(|&&i| self.app.tabs[i].kind() != "pty" && !self.app.tabs[i].alive())
            .count();
        rows.push(format!(
            "  fleet      {} session{} on {} · {} down",
            same.len(),
            if same.len() == 1 { "" } else { "s" },
            host_norm,
            down
        ));
        // Pin/mute protection status — same flags the tab bar badges, so the info panel is a
        // one-stop read of a tab's shielding without hunting the bar.
        rows.push(format!(
            "  protec     {}",
            match (
                self.pinned.get(self.app.active).copied().unwrap_or(false),
                self.muted.get(self.app.active).copied().unwrap_or(false),
            ) {
                (true, true) => "pinned 🔒 · muted M",
                (true, false) => "pinned 🔒",
                (false, true) => "muted M",
                (false, false) => "none",
            }
        ));
        // The OSC task title, if the shell/agent has set one — the running task context.
        if let Some(t) = s.live_title() {
            rows.push(format!("  task       {t}"));
        }

        // Terminal grid size = what the session actually render (its own lines × cols), read from
        // the emulator's current size without disturbing it.
        let size = {
            let term = s.term.lock();
            format!("  size       {}×{}", term.screen_lines(), term.columns())
        };
        rows.push(size);

        for (row, line) in rows.iter().enumerate() {
            draw_text(
                fb,
                &mut self.cache,
                line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    fn render_remote(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  attach to pane@host  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        draw_text(
            fb,
            &mut self.cache,
            &format!("  host[:port] {}", self.app.remote_host),
            32,
            base_y + line_px,
            self.font_px,
            CHROME_FG,
        );
        let hint = if self.app.recent_hosts.is_empty() {
            "  host[:port] = new engine · host[:port]/session = attach existing  ".to_string()
        } else {
            format!(
                "  Tab/⇧Tab = last {} host{} ({}) · host/session = attach existing · host = new engine  ",
                self.app.recent_hosts.len(),
                if self.app.recent_hosts.len() == 1 { "" } else { "s" },
                self.app.recent_hosts.first().map(String::as_str).unwrap_or("")
            )
        };
        draw_text(
            fb,
            &mut self.cache,
            &hint,
            32,
            base_y + 2 * line_px,
            self.font_px,
            CHROME_DIM,
        );
        for (i, e) in ENGINES.iter().enumerate() {
            let sel = i == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!("  {}  {}  {}", e.id, e.label, if sel { "◄" } else { "" });
            if sel {
                overlay_row_sel(fb, base_y + (i + 4) * line_px, line_px, 18);
            }
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (i + 4) * line_px,
                self.font_px,
                color,
            );
        }
        if let Some(e) = ENGINES.get(self.app.selected) {
            draw_text(
                fb,
                &mut self.cache,
                &format!("      {} — {}", e.label, e.desc),
                32,
                base_y + (ENGINES.len() + 4) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    /// Recompute the focused search match (from the top if none, else continue from it) and scroll
    /// the viewport so the match is visible at the top of the grid area.
    /// Scroll the viewport so the focused match's line is visible (at the top of the screen).
    fn find_scroll_to(
        &self,
        g: &mut alacritty_terminal::term::Term<crate::session::Listener>,
        l: i32,
    ) {
        use alacritty_terminal::grid::Scroll;
        let current = g.grid().display_offset() as i32;
        let desired = (-l).clamp(0, g.grid().history_size() as i32);
        g.grid_mut()
            .scroll_display(Scroll::Delta(desired - current));
    }

    /// Open the find-in-session overlay with a fresh query (used by Cmd+F and the palette/prefix
    /// `search` action). Centralizing the init so a new entry point can't forget to clear state.
    fn open_find(&mut self) {
        self.app.overlay = Overlay::Find;
        self.find_query.clear();
        self.find_hit = None;
        self.find_all = Vec::new();
        self.find_index = 0;
    }

    /// Record the current find query into the MRU (most-recent first, capped) and persist it, so
    /// Up-recall survives a restart. No-op for an empty query. Uses the shared pure `prepend_capped`.
    fn record_find_query(&mut self) {
        prepend_capped(&mut self.find_history, &self.find_query, 16);
        crate::restore::save_find_history(&self.find_history);
    }

    /// Persist the current find options so they survive a restart (iTerm2-style state memory).
    fn persist_find_opts(&self) {
        crate::restore::save_find_opts(self.find_opts.case_sensitive, self.find_opts.whole_word);
    }

    /// Recompute the occurrence list after a query edit; focuses the first match (or the match
    /// nearest the previous focus) so the viewport tracks the user.
    fn find_recompute(&mut self, _start: Option<i32>) {
        let Some(active) = self.app.active_session() else {
            self.find_hit = None;
            self.find_all = Vec::new();
            self.find_index = 0;
            return;
        };
        let mut g = active.term.lock();
        // Remember the previous focus position so edits keep roughly the same match in view.
        let prev_line = self.find_hit.map(|(l, _, _)| l);
        self.find_all = crate::render::all_matches_ex(&g, &self.find_query, self.find_opts);
        // Pick the first match at-or-after the old focus; else the very first match.
        let idx = if self.find_all.is_empty() {
            0
        } else {
            prev_line
                .and_then(|pl| self.find_all.iter().position(|&(l, _, _)| l >= pl))
                .unwrap_or(0)
        };
        self.find_index = idx;
        self.find_hit = self.find_all.get(idx).copied();
        if let Some((l, _, _)) = self.find_hit {
            self.find_scroll_to(&mut g, l);
            active.set_scrolled(true);
        }
    }

    /// Recompute the fleet-matches list across EVERY open session's scrollback for the current
    /// query. Collects each matching (tab, line, col) and sorts by tab then line, then keeps the
    /// selection within range. Called on every query edit while the FleetSearch overlay is open; a
    /// plain loop so each match carries its source tab index (unlike `all_matches`, which has none).
    fn fleet_recompute(&mut self) {
        self.fleet_matches.clear();
        let q = self.fleet_q.to_lowercase();
        if !q.is_empty() {
            // Gather the term locks into a slice so the shared, testable helper can run over them.
            let terms: Vec<_> = self.app.tabs.iter().map(|s| Arc::clone(&s.term)).collect();
            self.fleet_matches = collect_fleet_matches(&terms, &q);
        }
        if self.fleet_sel >= self.fleet_matches.len() {
            self.fleet_sel = self.fleet_matches.len().saturating_sub(1);
        }
    }

    /// Jump the fleet selection by `delta` rows (wrapping around the ends).
    fn fleet_jump(&mut self, delta: isize) -> bool {
        let m = self.fleet_matches.len();
        if m == 0 {
            return false;
        }
        self.fleet_sel = (self.fleet_sel as isize + delta).rem_euclid(m as isize) as usize;
        true
    }

    /// Fleet-search Enter: focus the matching session, scroll its scrollback so the hit line is
    /// visible, and leave the read cursor on the match's start column so it's on-screen. Closes the
    /// overlay. No-op if there's nothing selected.
    fn fleet_jump_to(&mut self) {
        let Some(m) = self.fleet_matches.get(self.fleet_sel) else {
            self.app.overlay = Overlay::None;
            return;
        };
        if m.tab >= self.app.tabs.len() {
            // The session closed under the search overlay; bail without indexing a stale tab.
            self.app.overlay = Overlay::None;
            return;
        }
        // Focus the session's tab first so the scroll/copy targets the same session the renderer
        // draws. `set_active` (not a direct field write) so native-tab mode also surfaces & focuses
        // the matching window — the same routing `Cmd+1-9` / the jump palette use.
        // Copy the match's scalar fields before `set_active` takes `&mut self` (we can't hold the
        // `fleet_matches` borrow across the mutation).
        let (tab, line, col) = (m.tab, m.line, m.col);
        let tab = tab.min(self.app.tabs.len().saturating_sub(1));
        self.set_active(tab);
        if let Some(s) = self.app.tabs.get(self.app.active) {
            let mut g = s.term.lock();
            // Scroll so the match line is at the top of the viewport.
            use alacritty_terminal::grid::Scroll;
            let current = g.grid().display_offset() as i32;
            let desired = (-line).clamp(0, g.grid().history_size() as i32);
            g.grid_mut()
                .scroll_display(Scroll::Delta(desired - current));
            s.set_scrolled(true);
        }
        // Place the read cursor at the match start so it's clearly visible where the hit landed.
        self.copy_pos = (line, col);
        self.app.overlay = Overlay::None;
    }

    /// Jump the focus by `delta` through the matches (wrapping around the ends), updating the
    /// focused highlight and the viewport. Returns false when there's nothing to step to.
    fn find_jump(&mut self, delta: isize) -> bool {
        let m = self.find_all.len();
        if m == 0 {
            return false;
        }
        let next = (self.find_index as isize + delta).rem_euclid(m as isize) as usize;
        self.find_index = next;
        self.find_hit = self.find_all.get(next).copied();
        let Some(active) = self.app.active_session() else {
            return false;
        };
        if let Some((l, _, _)) = self.find_hit {
            let mut g = active.term.lock();
            self.find_scroll_to(&mut g, l);
            active.set_scrolled(true);
        }
        true
    }

    /// Enter copy mode: anchor the read cursor at the top-left visible cell so the user starts
    /// where they can see, and keep the view scrolled (copy mode lives in the scrollback).
    fn start_copy_mode(&mut self) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        if g.grid().history_size() == 0 {
            return; // nothing to scroll/copy yet
        }
        // Place the cursor at the first visible (top of viewport) cell.
        let top = g.grid().display_offset();
        self.copy_pos = (-(top as i32), 0);
        self.copy_anchor = None;
        self.copy_query.clear();
        self.copy_searching = false;
        self.copy_mode = true;
        active.set_scrolled(true);
    }

    /// Copy the current copy-mode selection (anchor→pos inclusive) to the clipboard via
    /// selection_to_string, then leave copy mode. No-op if nothing is selected.
    fn copy_mode_copy(&mut self) {
        if self.copy_anchor.is_none() {
            self.copy_mode = false;
            return;
        }
        let Some(active) = self.app.active_session() else {
            return;
        };
        let mut g = active.term.lock();
        // Build a simple rectangular selection from the two grid points, install it as the live
        // selection, and read it via alacritty's range-to-string so line handling (wrap vs hard
        // newline) is correct. We restore/clear the live selection afterward.
        let (a, b) = (self.copy_anchor.unwrap(), self.copy_pos);
        let (lo, hi) = if (a, b) < (b, a) { (a, b) } else { (b, a) };
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let start = Point::new(Line(lo.0), Column(lo.1));
        let end = Point::new(Line(hi.0), Column(hi.1));
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        g.selection = Some(sel);
        let text = g.selection_to_string().unwrap_or_default();
        g.selection = None;
        self.copy_mode = false;
        self.copy_anchor = None;
        self.copy_query.clear();
        self.copy_searching = false;
        if !text.is_empty() {
            drop(g);
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
        }
    }

    /// Move the copy-mode read cursor by a grid delta, keeping it in-bounds and extending the
    /// selection if one is active.
    fn copy_move(&mut self, dl: i32, dc: i32) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let cols = g.columns() as i32;
        let max_line = g.grid().bottommost_line().0;
        let min_line = g.grid().topmost_line().0;
        let (l, c) = self.copy_pos;
        let l = (l + dl).clamp(min_line, max_line);
        let c = (c as i32 + dc).clamp(0, cols - 1) as usize;
        self.copy_pos = (l, c);
    }

    /// Jump the copy-mode read cursor to the next match of `copy_query` at-or-after (or, for
    /// `backwards`, at-or-before) the current position, wrapping around the scrollback. Sets the
    /// cursor to the match's start column so the user can Inspect/select from it. Returns whether a
    /// match was found.
    fn copy_goto(&mut self, backwards: bool) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        if self.copy_query.is_empty() {
            return;
        }
        let query = self.copy_query.clone();
        let (cur_line, _) = self.copy_pos;
        // Start the scan just past the cursor (forwards) or just before it (backwards) so `n`/`N`
        // walk distinct matches rather than re-selecting the current one.
        let search_start = if backwards {
            cur_line - 1
        } else {
            cur_line + 1
        };
        let g = active.term.lock();
        let full = crate::render::all_matches(&g, &query);
        drop(g);
        if full.is_empty() {
            // A query with no hit is otherwise completely silent (`n`/`N` and Enter just leave the
            // cursor put) — flash the miss so a diver isn't wondering why the jump didn't fire.
            self.flash = Some((copy_no_match_flash(&query), std::time::Instant::now()));
            self.copy_pos = (cur_line, 0);
            return;
        }
        // Pick the nearest match at/after `search_start` (forwards) or at/before it (backwards),
        // wrapping to the other end of the scrollback when there's no further hit on that side.
        let chosen = if backwards {
            full.iter()
                .filter(|m| m.0 <= search_start)
                .max_by_key(|m| m.0)
                .or_else(|| full.iter().max_by_key(|m| m.0))
        } else {
            full.iter()
                .filter(|m| m.0 >= search_start)
                .min_by_key(|m| m.0)
                .or_else(|| full.iter().min_by_key(|m| m.0))
        };
        if let Some((l, c, _)) = chosen.copied() {
            self.copy_pos = (l, c);
        }
    }

    /// Render the search overlay: query prompt + current match location in the status area.
    fn render_find(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        // Overlay status line with query and match count info.
        let flags = if self.find_opts.case_sensitive || self.find_opts.whole_word {
            let mut f = String::from("  [");
            if self.find_opts.case_sensitive {
                f.push_str("Aa");
            }
            if self.find_opts.whole_word {
                if self.find_opts.case_sensitive {
                    f.push(' ');
                }
                f.push_str("whole-word");
            }
            f.push(']');
            f
        } else {
            String::new()
        };
        let line = if self.find_query.is_empty() {
            let opts = "  c case · w whole-word";
            if self.find_history.is_empty() {
                format!("  find: (type to search · ↑ recalls history){flags}{opts}  ")
            } else {
                let last = &self.find_history[0];
                format!("  find: (up recalls {last}){flags}  ")
            }
        } else {
            let n = self.find_all.len();
            if n > 0 {
                let here = (self.find_index % n) + 1;
                format!(
                    "  find: {}  (match {} of {} · Enter/Tab next, Shift+Enter prev, Cmd+G repeat)",
                    self.find_query, here, n
                )
            } else {
                format!("  find: {}  (no match)", self.find_query)
            }
        };
        draw_text(
            fb,
            &mut self.cache,
            &line,
            6,
            status_base,
            self.font_px,
            WHITE,
        );
    }

    /// Render the rename overlay: show what the tab is currently called and the in-progress name.
    fn render_rename(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        let cur = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
            .unwrap_or_default();
        let prompt = if self.rename_query.is_empty() {
            "  rename tab: (type a name, Enter keeps, Esc cancels)  ".to_string()
        } else {
            format!("  rename tab: {} ▏", self.rename_query)
        };
        draw_text(
            fb,
            &mut self.cache,
            &format!("  currently: {}  ", cur),
            6,
            status_base - self.cell_h as usize,
            self.font_px,
            CHROME_DIM,
        );
        draw_text(
            fb,
            &mut self.cache,
            &prompt,
            6,
            status_base,
            self.font_px,
            WHITE,
        );
    }

    /// Render the broadcast overlay: the in-progress line, then a checkbox list of every session
    /// with its target state. Space toggles the focused row; Enter sends only the marked targets.
    fn render_broadcast(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let n = self.broadcast_targets.iter().filter(|&&t| t).count();
        // How many of the currently-selected targets are down: their command won't run now, it'll be
        // staged and flushed on reconnect. Surfacing it here prevents a silent "broadcast to 4" that
        // a diver assumes all four received instantly.
        let queued = self
            .app
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                self.broadcast_targets.get(*i).copied().unwrap_or(false)
                    && s.kind() != "pty"
                    && !s.alive()
            })
            .count();
        let qtag = if queued > 0 {
            format!(" · ⏳{queued} queued")
        } else {
            String::new()
        };
        // While recalling history (Shift+Up/Down), surface where the cursor sits in the MRU list so
        // the diver knows which prior fan-out they have staged before hitting Enter. 0 = newest.
        let hist_tag = match self.hist_sel {
            Some(i) if !self.broadcast_hist.is_empty() => {
                format!(" ⇧↑ {}/{}", i + 1, self.broadcast_hist.len())
            }
            _ => String::new(),
        };
        let prompt = if self.broadcast_query.is_empty() {
            format!("  send line to {n} of {} session{}{qtag} (↑/↓ focus · PgUp/Dn page · Space=toggle · ⇧Space=all · ⇧↑/⇧↓ history · Enter=broadcast · Esc=cancel)  ",
                self.app.tabs.len(), if n == 1 { "" } else { "s" })
        } else {
            format!(
                "  broadcast to {n} session{}{qtag}{hist_tag}: {} ▏",
                if n == 1 { "" } else { "s" },
                self.broadcast_query
            )
        };
        draw_text(
            fb,
            &mut self.cache,
            &prompt,
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        let top = scroll_top(self.app.tabs.len(), self.broadcast_sel, 20);
        for (row, s) in self.app.tabs.iter().enumerate().skip(top).take(20) {
            let scr = row - top;
            let on = self.broadcast_targets.get(row).copied().unwrap_or(false);
            let mark = if on { "☑" } else { "☐" };
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            // A down target will only receive the command on reconnect — say so right on the row
            // (and its reason), so the fan-out's fate is legible before Enter.
            let down = s.kind() != "pty" && !s.alive();
            let down_tag = if down {
                let reason = s
                    .down_reason()
                    .unwrap_or_else(|| "reconnecting…".to_string());
                let reason = clip_dots(&reason.trim().to_string(), 18);
                if reason.is_empty() {
                    "  ○ down".to_string()
                } else {
                    format!("  ○ down ({reason})")
                }
            } else {
                String::new()
            };
            let line = format!("  {} {} @ {}{down_tag}", mark, name, s.meta.host);
            let color = if row == self.broadcast_sel {
                WHITE
            } else if down {
                CHROME_ERR
            } else {
                CHROME_DIM
            };
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (scr + 2) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the peek overlay: a header, then one compact row per session with either the dimmed
    /// tail of every other session folded in and the highlighted row expanded to a ~4-line preview
    /// of its last scrollback lines (WHITE). The selection index is `self.peek_sel`.
    fn render_fleet_grid(&mut self, fb: &mut Framebuffer) {
        let (_, line_px) = self.overlay_base_y();
        let header_y = self.chrome_top() + 2;
        let n = self.app.tabs.len();
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  fleet grid · {} session{} · ↑/↓/PgUp/PgDn/1-9 select · n→next trouble · Space mark · b→broadcast · C→Ctrl-C · m mute · p pin · x/X→close sel/all · r/R→reconnect · Enter dive · Esc close  ",
                n,
                if n == 1 { "" } else { "s" }
            ),
            32,
            header_y,
            self.font_px,
            WHITE,
        );
        if n == 0 {
            return;
        }
        // Clamp the selected tile into range (tabs can close underneath the overlay).
        self.grid_sel = self.grid_sel.min(n - 1);
        // Same per-frame output-activity pass the tab bar / triage use, so each tile's status glyphs
        // match the rest of the chrome: busy = produced output, quiet = sat silent past threshold.
        let activity = self.activity_flags();
        let (_, _, _max_idle) = self.quiet_flags();
        // Layout the grid. Tile geometry is a full text cell (the grid rows draw just below the
        // header). Fit as many columns as the window width allows at the active cell width.
        let gcol = self.cell_w as usize;
        let grow = self.cell_h as usize;
        let x0 = 8usize;
        let y0 = header_y + line_px;
        let inner_w = fb.width.saturating_sub(x0 + 8);
        // Tile geometry is a full text cell (header + up to 3 tail lines). Fit as many columns as
        // the window allows; deeper rows simply clip at the window bottom.
        let cols = (inner_w / (gcol * 12)).max(1).min(n.max(1));
        let tw = inner_w / cols;
        let th = grow * 4;

        // We draw the tail with the session's own colors so the overview reads like the real panes.
        for (idx, s) in self.app.tabs.iter().enumerate() {
            let (c, r) = (idx % cols, idx / cols);
            let (tx, ty) = (x0 + c * (tw + 8), y0 + r * (th + 8));
            if ty + th > fb.height {
                break;
            }
            // A thin highlight border for the focused tile, tinted by the session's status so the
            // war-room is scannable at a glance (down=red, busy=amber, quiet=blue, reconnecting=green).
            // A local PTY reports no transport status and stays neutral; unaccented tiles fall back
            // to the white active / dim idle border used before.
            let selected = idx == self.grid_sel;
            let is_down = !s.alive() && s.kind() != "pty";
            let busy_accent = idx != self.grid_sel && activity[idx];
            let quiet_accent = idx != self.grid_sel && self.quiet_for(idx);
            let recovering = self.recover_until.get(idx).copied().flatten().is_some();
            let accent = status_accent(is_down, busy_accent, quiet_accent, recovering);
            let border = accent.unwrap_or(if selected { WHITE } else { CHROME_DIM });
            for (bord_x, color) in [(tx, border), (tx + tw.saturating_sub(1), border)] {
                for yy in ty..ty + th {
                    fb.set(bord_x, yy, argb(0xff, color.0, color.1, color.2));
                }
            }
            for yy in [ty, ty + th.saturating_sub(1)] {
                for xx in tx..tx + tw {
                    fb.set(xx, yy, argb(0xff, border.0, border.1, border.2));
                }
            }
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let host = if s.kind() == "pty" {
                String::new()
            } else {
                format!("@{}", s.meta.host)
            };
            // Status glyph, matching the tab-bar chrome: nothing for a live idle tab; `!N` busy,
            // `⌛` quiet (awaiting you), `○`/`↓` down, `↻` just-reconnected, `⏳N` queued-input,
            // `🔒` pinned, `M` muted.
            // Quiet and busy are mutually exclusive; down wins over both. The focused tile is the
            // one you're actively reading, so it's never flagged busy/quiet (same rule as the bar).
            // A down tile should say *why* in the war-room, not just flash `○`: the reason the
            // hover tooltip carries (host unreachable / auth rejected / timeout) clipped to the
            // tile's own width. Local PTYs have no transport to diagnose and are skipped.
            let down_reason = if is_down {
                let reason = s
                    .down_reason()
                    .unwrap_or_else(|| "reconnecting…".to_string())
                    .trim()
                    .to_string();
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            } else {
                String::new()
            };
            // The `!N` glyph uses the cumulative unread count (like the tab-bar badge), not the
            // per-frame delta: while a tile is streaming it still shows immediately, and once it
            // settles the count lingers so a war-room scan exposes output you haven't seen even
            // after the agent stopped writing. The live per-frame signal stays on the border accent
            // (`busy_accent`) so "producing right now" still reads as motion.
            let busy = self.unread.get(idx).copied().unwrap_or(0);
            let clipped = s.pending_bytes();
            let glyph = if is_down {
                "○".to_string()
            } else if idx != self.grid_sel && busy > 0 {
                format!("!{}", busy.min(999))
            } else if idx != self.grid_sel && self.quiet_for(idx) {
                // A quiet tile says HOW long it's been awaiting you (⌛3m) — more legible in the
                // war-room than a bare ⌛, so a diver sees at a glance which agents have been parked.
                let idle = std::time::Instant::now()
                    - self
                        .last_output
                        .get(idx)
                        .copied()
                        .unwrap_or(std::time::Instant::now());
                format!("⌛{}", fmt_duration(idle))
            } else {
                String::new()
            };
            let mut glyph_s = glyph;
            if self.recover_until.get(idx).copied().flatten().is_some() {
                glyph_s += "↻";
            }
            if self.pinned.get(idx).copied().unwrap_or(false) {
                glyph_s.push('🔒');
            }
            if self.muted.get(idx).copied().unwrap_or(false) {
                glyph_s.push('M');
            }
            if clipped > 0 {
                glyph_s.push_str(&format!("⏳{}", clipped));
            }
            let pfx = if glyph_s.is_empty() {
                String::new()
            } else {
                format!("{glyph_s} ")
            };
            let header = format!(
                "  {}{}{}  {head}{} {}{}",
                idx + 1,
                host,
                if self.grid_marks.get(idx).copied().unwrap_or(false) {
                    " ●"
                } else {
                    ""
                },
                if selected { " ◄" } else { "" },
                pfx,
                down_reason,
            );
            // Keep the header inside the tile's own cell width (a long reason must not spill over
            // a neighbor tile's border); char-clip reuses the hover-tooltip ellipsis rule.
            let header = clip_dots(&header, (tw / gcol).saturating_sub(2));
            draw_text(
                fb,
                &mut self.cache,
                &header,
                tx + 2,
                ty + grow,
                self.font_px,
                accent.unwrap_or(if selected { WHITE } else { CHROME_DIM }),
            );
            // Live near-tail lines, reversed (tail() is newest-first) so the tile reads top-to-bottom
            // as a terminal would — newest sits on the bottom row. Uses the grid's own foreground.
            let mut tail = s.tail(3);
            tail.reverse();
            for (k, tl) in tail.iter().enumerate().take(3) {
                let t = if tl.chars().count() > (tw / gcol).saturating_sub(2) {
                    let cut: String = tl.chars().take((tw / gcol).saturating_sub(2)).collect();
                    format!("{cut}…")
                } else {
                    tl.clone()
                };
                draw_text(
                    fb,
                    &mut self.cache,
                    &t,
                    tx + 2,
                    ty + (k + 2) * grow,
                    self.font_px,
                    self.colors.fg,
                );
            }
        }
    }

    /// Open the peek overlay. A diver dropping in wants to land on the host that needs
    /// attention, so if any remote pane is down the selection starts on the first one (and the
    /// scroll is positioned so its preview is already on screen); otherwise the top of the list.
    fn open_peek(&mut self) {
        self.app.overlay = Overlay::Peek;
        self.peek_sel = 0;
        self.peek_scroll = 0;
        self.peek_q.clear();
        self.peek_filtering = false;
        let kinds: Vec<&str> = self.app.tabs.iter().map(|t| t.kind()).collect();
        let alive: Vec<bool> = self.app.tabs.iter().map(|t| t.alive()).collect();
        if let Some(i) = first_down_session(&kinds, &alive) {
            self.peek_sel = i;
            // Slide the window up so the down session's preview is visible even near the bottom.
            self.peek_scroll = i
                .saturating_sub(9)
                .min(self.app.tabs.len().saturating_sub(10));
        }
    }

    /// Recompute `peek_filtered` (indices into `app.tabs`) matching `peek_q` via each session's
    /// `matches_filter`, then clamp `peek_sel`/`peek_scroll` into the filtered list. An empty query
    /// yields the identity mapping (every tab in order), so the unfiltered peek behaves exactly as
    /// before; a query that matches nothing clamps to an empty list (no rows, never a panic).
    fn peek_refresh_filter(&mut self) {
        let n = self.app.tabs.len();
        // The `/` filter understands live keywords (up/down/busy/quiet) plus plain substring
        // matching, and composes them: multiple space-separated tokens must ALL match (`build05
        // down`, `claude quiet`), so a diver can ask "which agents are down on this host?" in one
        // query. `busy`/`quiet` need native per-tab sampling (`grew_delta`/`quiet_for`), so they
        // are resolved here rather than in the session's own `matches_filter`. A blank query shows
        // every session (backward-compatible with single-token substring queries).
        let q = self.peek_q.trim().to_lowercase();
        let tokens: Vec<&str> = q.split_whitespace().collect();
        let mut filtered = Vec::with_capacity(n);
        for i in 0..n {
            let keep = if tokens.is_empty() {
                true
            } else {
                tokens.iter().all(|tok| match *tok {
                    "down" => self.app.tabs[i].kind() != "pty" && !self.app.tabs[i].alive(),
                    "up" => self.app.tabs[i].kind() != "pty" && self.app.tabs[i].alive(),
                    "busy" => self.grew_delta.get(i).copied().unwrap_or(0) > 0,
                    "quiet" => self.quiet_for(i),
                    _ => self.app.tabs[i].matches_filter(tok),
                })
            };
            if keep {
                filtered.push(i);
            }
        }
        self.peek_filtered = filtered;
        if self.peek_sel >= self.peek_filtered.len() {
            self.peek_sel = self.peek_filtered.len().saturating_sub(1);
        }
        if self.peek_scroll >= self.peek_filtered.len() {
            self.peek_scroll = self.peek_filtered.len().saturating_sub(10);
        }
    }

    fn render_peek(&mut self, fb: &mut Framebuffer) {
        self.peek_refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        // Live fleet-health count in the header: how many panes are down right now, so the peek
        // triage reads the whole fleet's state at a glance, not just the selected row.
        let down_n = self
            .app
            .tabs
            .iter()
            .filter(|t| t.kind() != "pty" && !t.alive())
            .count();
        // Panes that just came back (the transient `↻` badge) are worth a nod too, so the header
        // reads the fleet's whole recent state, not just what's still down.
        let rec_n = self.recover_until.iter().filter(|r| r.is_some()).count();
        let total = self.app.tabs.len();
        let shown = self.peek_filtered.len();
        // While the `/` filter is open the header leads with the live query and match count so a
        // diver sees exactly what the list is narrowed to (e.g. "only build05" or "just down panes").
        let filter_prefix = if self.peek_filtering {
            format!("  peek · /{} · {shown}/{total} match · ", self.peek_q)
        } else {
            "  peek · ".to_string()
        };
        let health = if down_n > 0 && rec_n > 0 {
            format!("{down_n} down · {rec_n} reconnected · ")
        } else if down_n > 0 {
            format!("{down_n} down · ")
        } else if rec_n > 0 {
            format!("{rec_n} reconnected · ")
        } else if !self.peek_filtering {
            "fleet healthy · ".to_string()
        } else {
            String::new()
        };
        let rest = if self.peek_filtering {
            "↑/↓ · Enter jump · Esc clear  "
        } else {
            "↑/↓ preview · PgUp/Dn page · n down · r reconnect · m mute · p pin · x close · Enter jump · Esc close  "
        };
        let header = format!("{filter_prefix}{health}{rest}");
        draw_text(
            fb,
            &mut self.cache,
            &header,
            32,
            base_y,
            self.font_px,
            if down_n > 0 { CHROME_ERR } else { WHITE },
        );
        // Cap the visible window (10 rows + preview lines), but scroll through ALL matches:
        // `peek_scroll` offsets the start, matching `peek_sel`. `peek_refresh_filter` re-clamps
        // both against the (possibly filtered) list each frame, so a tab closing while the overlay
        // is open can't leave a stale index.
        let rows = shown.min(10);
        for row in 0..rows {
            let i = self.peek_scroll + row;
            if i >= shown {
                break;
            }
            let real = self.peek_filtered[i];
            let s = &self.app.tabs[real];
            let sel = i == self.peek_sel;
            // A down remote pane is the row worth noticing: tag it red (unless selected, when the
            // white highlight already owns it) and surface its reconnect reason so the diver knows
            // the nature of the outage without even expanding the row.
            let down = s.kind() != "pty" && !s.alive();
            let color = if sel {
                WHITE
            } else if down {
                CHROME_ERR
            } else {
                CHROME_DIM
            };
            // Custom-named tabs hide their agent engine, which matters in a multi-engine fleet;
            // show `name (engine)` so a diver sees the agent type even after renaming.
            let base = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let name = if base != s.meta.engine {
                format!("{base} ({})", s.meta.engine)
            } else {
                base
            };
            let live = s
                .live_title()
                .unwrap_or_else(|| s.meta.title.clone())
                .replace('\n', " ");
            let reason_tag = if down {
                let reason = s
                    .down_reason()
                    .unwrap_or_else(|| "reconnecting…".to_string());
                format!(" ○ {}", clip_dots(&reason, 22))
            } else {
                String::new()
            };
            // A quiet (awaiting-you) row says how long it has been parked, matching the fleet grid.
            // Down wins (its ○ reason is the more urgent signal): an idle duration only reads when
            // the pane is live and hasn't been flagged busy/active this frame.
            let idle_tag = if !down && self.quiet_for(real) {
                let idle = std::time::Instant::now()
                    - self
                        .last_output
                        .get(real)
                        .copied()
                        .unwrap_or(std::time::Instant::now());
                format!(" · ⌛{}", fmt_duration(idle))
            } else {
                String::new()
            };
            // Per-row protection / activity badges, mirroring the fleet grid and tab-bar chrome so
            // the triage reads a row's full shield state at a glance: busy output `!N`, muted `M`,
            // pinned `🔒`, just-reconnected `↻`, and staged-input `⏳N`. The selected row is the one
            // being read, so it isn't flagged busy (same rule as the grid/bar); it still shows its
            // pin/mute/recover/queued badges.
            let mut badges = String::new();
            if !sel {
                // Lingering unread count (like the tab-bar `!N` badge), so a settled agent whose
                // output you haven't seen still reads `!N` here rather than dropping the instant it
                // stops writing — one triage list for every agent with something to show.
                let busy_n = self.unread.get(real).copied().unwrap_or(0);
                if busy_n > 0 {
                    badges.push_str(&format!(" · !{}", busy_n.min(999)));
                }
            }
            if self.recover_until.get(real).copied().flatten().is_some() {
                badges.push_str(" ↻");
            }
            if self.pinned.get(real).copied().unwrap_or(false) {
                badges.push('🔒');
            }
            if self.muted.get(real).copied().unwrap_or(false) {
                badges.push_str(" M");
            }
            let clipped = self.app.tabs[real].pending_bytes();
            if clipped > 0 {
                badges.push_str(&format!(" ⏳{clipped}"));
            }
            let row_y = base_y + (row + 1) * line_px;
            // Tint the host with its per-machine color so a multi-host fleet scans by machine at a
            // glance (`@build05` and `@edge1` read differently); the rest of the row keeps the
            // status color (white selected / red down / dim idle). The selected row stays uniform
            // white so the highlight reads as one sheet.
            let host_l = if s.meta.host.is_empty() {
                "local".to_string()
            } else {
                s.meta.host.clone()
            };
            let host_seg = format!("  {host_l}");
            let host_col = if sel { color } else { host_color(&host_l) };
            draw_text(
                fb,
                &mut self.cache,
                &host_seg,
                32,
                row_y,
                self.font_px,
                host_col,
            );
            let rest = format!(
                " · {} · {}{}{}{}{}",
                name,
                live,
                idle_tag,
                reason_tag,
                badges,
                if sel { " ◄" } else { "" }
            );
            let rest_x = 32 + text_width(&mut self.cache, &host_seg, self.font_px);
            draw_text(
                fb,
                &mut self.cache,
                &rest,
                rest_x,
                row_y,
                self.font_px,
                color,
            );
            // Expand the highlighted row: dim preview of the last ~4 scrollback lines underneath.
            // Use the cheap bounded reads (history_slice/tail walk a handful of rows, never the
            // whole history) — capturing the entire scrollback every frame would re-walk and
            // re-allocate a long agent log once per render, which is slow with the peek open.
            if sel {
                let h = s.history_slice(6);
                let src: Vec<String> = if h.iter().any(|l| !l.trim().is_empty()) {
                    h
                } else {
                    s.tail(6)
                };
                let lines: Vec<String> = src
                    .into_iter()
                    .map(|l| l.trim_end().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                let start = lines.len().saturating_sub(4);
                for (k, tl) in lines[start..].iter().enumerate().take(4) {
                    let t = if tl.chars().count() > 90 {
                        tl.chars().take(90).collect::<String>() + "…"
                    } else {
                        tl.to_string()
                    };
                    draw_text(
                        fb,
                        &mut self.cache,
                        &format!("      {}", t),
                        32,
                        row_y + (k + 1) * line_px,
                        self.font_px,
                        CHROME_DIM,
                    );
                }
            }
        }
    }

    /// Recompute `palette_filtered` (indices into `palette_rows`) matching `palette_q` by
    /// case-insensitive substring on the label, then clamp the selection into range. Mirrors the
    /// other overlays' per-frame recompute.
    fn palette_refresh_filter(&mut self) {
        let q = self.palette_q.to_lowercase();
        self.palette_filtered = (0..self.palette_rows.len())
            .filter(|&i| q.is_empty() || fuzzy_match(&q, &self.palette_rows[i].0.to_lowercase()))
            .collect();
        if self.palette_sel >= self.palette_filtered.len() {
            self.palette_sel = self.palette_filtered.len().saturating_sub(1);
        }
    }

    /// Render the command palette overlay: a filter prompt, then up to 12 matching action rows with
    /// the selected one highlighted. `palette_filtered` is recomputed each frame, mirroring the
    /// palette overlay.
    fn render_command_palette(&mut self, fb: &mut Framebuffer) {
        self.palette_refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        let total = self.palette_rows.len();
        let shown = self.palette_filtered.len();
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  ⚡ [action] {}  · {} of {}  ",
                self.palette_q, shown, total
            ),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if shown == 0 {
            draw_text(
                fb,
                &mut self.cache,
                "  no actions match  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        for (row, &i) in self.palette_filtered.iter().enumerate().take(12) {
            let sel = row == self.palette_sel;
            if sel {
                overlay_row_sel(fb, base_y + (row + 1) * line_px, line_px, 18);
            }
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!(
                "  {}  {}",
                self.palette_rows[i].0,
                if sel { "◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Run a command-palette action: close the overlay first, then perform the same effect as the
    /// corresponding prefix command so terminal state and overlays are kept consistent.
    fn run_palette_action(&mut self, a: PaletteAction) {
        use PaletteAction::*;
        self.app.overlay = Overlay::None;
        match a {
            NewSession => {
                self.app.overlay = Overlay::NewSession;
                self.app.select_default_engine();
                // Pre-fill the last repo a local tab was spawned in (MRU), so respawning is one Enter.
                self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
            }
            RemoteAttach => {
                self.app.overlay = Overlay::RemoteAttach;
                // Pre-fill the last server/session attached to so re-connecting is one Enter
                // (edit only if a different host is wanted).
                self.app.remote_host = self.app.recent_hosts.first().cloned().unwrap_or_default();
                self.app.selected = 0;
            }
            SessionPalette => {
                self.app.overlay = Overlay::Palette;
                self.app.query.clear();
                self.app.selected = 0;
                self.app.refresh_filter();
            }
            FindInTab => {
                self.open_find();
            }
            SearchAll => {
                self.app.overlay = Overlay::FleetSearch;
                self.fleet_q.clear();
                self.fleet_matches.clear();
                self.fleet_sel = 0;
            }
            CopyScrollback => self.copy_whole_scrollback(),
            ExportLog => self.export_scrollback(),
            CopyIdentity => self.copy_identity(),
            CopyFleet => self.copy_fleet(),
            Broadcast => {
                // Pre-fill the last-sent line so a repeat command to every host survives; Backspace
                // clears it if a fresh one is wanted. Targets still reset all-on (safety).
                self.broadcast_query = self.last_broadcast.clone();
                self.broadcast_targets.iter_mut().for_each(|t| *t = true);
                self.broadcast_sel = 0;
                self.app.overlay = Overlay::Broadcast;
            }
            Peek => self.open_peek(),
            FleetGrid => {
                self.grid_sel = 0;
                self.app.overlay = Overlay::FleetGrid;
            }
            UndoClose => self.app.reopen_last_closed(),
            Duplicate => {
                self.duplicate_active_preserving_pin();
                self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
            }
            SessionInfo => {
                self.app.overlay = Overlay::Info;
            }
            MarkAllRead => self.mark_all_read(),
            ToggleFocus => self.toggle_focus(),
            Pin => self.toggle_pin_active(),
            NextPinned => self.next_pinned(),
            NextBusy => self.next_busy(),
            NextQuiet => self.next_quiet(),
            NextDown => self.next_down(),
            MuteActive => self.toggle_mute_active(),
            Rename => {
                self.rename_query = self
                    .app
                    .active_session()
                    .map(|s| s.meta.name.clone().unwrap_or_default())
                    .unwrap_or_default();
                self.app.overlay = Overlay::Rename;
            }
            PageUp => {
                scroll_active(self, 20);
                if let Some(s) = self.app.active_session() {
                    s.set_scrolled(true);
                }
            }
            ScrollBottom => self.scroll_to_bottom(),
            ScrollTop => self.scroll_to_top(),
            NextHost => self.next_host(),
            Hosts => {
                self.hosts_sel = 0;
                self.hosts_host = None;
                self.app.overlay = Overlay::Hosts;
            }
            Dnd => self.toggle_dnd(),
            Reconnect => self.reconnect_active(),
            ReconnectAll => self.reconnect_all_down(),
            Destroy => self.destroy_active(),
            Interrupt => self.interrupt_active(),
            InterruptAll => self.interrupt_fleet(),
            CloseQuiet => self.close_quiet_tabs(),
            Help => {
                self.app.overlay = Overlay::Help;
            }
            Quit => self.quit(),
        }
    }

    /// Handle a key while in copy mode: vim-style motion keys move the read cursor; `v` starts
    /// (or re-anchors) a selection; Enter/Space copies and exits; Esc/Q exits; g/G go to top/bottom.
    fn handle_copy_key(&mut self, key: &Key, _mods: &ModifiersState) {
        // While the search prompt is open, typing builds `copy_query` (live); Enter jumps to the
        // next match and exits the prompt; Esc/Backspace-empty dismisses it back to nav.
        if self.copy_searching {
            match key {
                Key::Character(c) if c == "/" => { /* ignore a second slash */ }
                Key::Character(c) => {
                    self.copy_query.push_str(c);
                    return;
                }
                Key::Named(n) => match n {
                    winit::keyboard::NamedKey::Backspace => {
                        self.copy_query.pop();
                        return;
                    }
                    winit::keyboard::NamedKey::Enter => {
                        self.copy_searching = false;
                        self.copy_goto(false);
                        return;
                    }
                    winit::keyboard::NamedKey::Escape => {
                        self.copy_query.clear();
                        self.copy_searching = false;
                        return;
                    }
                    _ => return,
                },
                _ => return,
            }
        }
        match key {
            Key::Character(c) => match c.as_str() {
                " " => self.copy_mode_copy(),
                "/" => {
                    self.copy_query.clear();
                    self.copy_searching = true;
                }
                "n" => {
                    if !self.copy_query.is_empty() {
                        self.copy_goto(false);
                    }
                }
                "N" => {
                    if !self.copy_query.is_empty() {
                        self.copy_goto(true);
                    }
                }
                // vim motions
                "h" | "j" | "k" | "l" | "w" | "b" => {
                    let (dl, dc) = match c.as_str() {
                        "h" => (0, -1),
                        "j" => (1, 0),
                        "k" => (-1, 0),
                        "l" => (0, 1),
                        "w" => {
                            self.copy_word(true);
                            (0, 0)
                        }
                        "b" => {
                            self.copy_word(false);
                            (0, 0)
                        }
                        _ => (0, 0),
                    };
                    self.copy_move(dl, dc);
                }
                "v" => {
                    // Start/re-anchor the selection at the read cursor.
                    self.copy_anchor = Some(self.copy_pos);
                }
                "g" => {
                    let Some(active) = self.app.active_session() else {
                        return;
                    };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().topmost_line().0, 0);
                }
                "G" => {
                    let Some(active) = self.app.active_session() else {
                        return;
                    };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().bottommost_line().0, 0);
                }
                "q" => {
                    self.copy_mode = false;
                    self.copy_anchor = None;
                    self.copy_query.clear();
                    self.copy_searching = false;
                }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Enter => self.copy_mode_copy(),
                winit::keyboard::NamedKey::Space => self.copy_mode_copy(),
                winit::keyboard::NamedKey::ArrowUp => self.copy_move(-1, 0),
                winit::keyboard::NamedKey::ArrowDown => self.copy_move(1, 0),
                winit::keyboard::NamedKey::ArrowLeft => self.copy_move(0, -1),
                winit::keyboard::NamedKey::ArrowRight => self.copy_move(0, 1),
                winit::keyboard::NamedKey::PageUp => self.copy_move(-20, 0),
                winit::keyboard::NamedKey::PageDown => self.copy_move(20, 0),
                winit::keyboard::NamedKey::Escape => {
                    self.copy_mode = false;
                    self.copy_anchor = None;
                    self.copy_query.clear();
                    self.copy_searching = false;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Jump the copy cursor to the next/previous word boundary. `forward` moves right, otherwise
    /// left. Wraps selection extension implicitly because it just moves `copy_pos`.
    fn copy_word(&mut self, forward: bool) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let cols = g.columns();
        let (mut l, mut cur) = self.copy_pos;
        let max_line = g.grid().bottommost_line().0;
        // Work on the current line's text; map from copy_pos col to a byte index.
        use alacritty_terminal::index::{Column, Line};
        let line_text: String = g.grid()[Line(l)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        let ci = cur.min(line_text.len().saturating_sub(1));
        let bytes = line_text.as_bytes();
        let is_space_at = |i: usize| bytes.get(i).is_none_or(|b| b.is_ascii_whitespace());
        if forward {
            // Skip current word (or spaces), land on next word start.
            let mut i = ci;
            // If on a space, skip spaces first.
            while i < bytes.len() && is_space_at(i) {
                i += 1;
            }
            while i < bytes.len() && !is_space_at(i) {
                i += 1;
            }
            // If still within the line, land at the next non-space.
            let mut ni = i;
            while ni < bytes.len() && is_space_at(ni) {
                ni += 1;
            }
            if ni < bytes.len() {
                cur = ni;
            } else if l < max_line {
                l += 1;
                cur = 0;
            }
        } else {
            // Walk back over the current word, then any spaces, to the previous word's start.
            let mut i = ci;
            while i > 0 && !is_space_at(i.saturating_sub(1)) {
                i -= 1;
            }
            // i is now the start of the current word (or 0). Skip spaces before it.
            let mut si = i;
            while si > 0 && is_space_at(si.saturating_sub(1)) {
                si -= 1;
            }
            cur = if si < i { si } else { i };
        }
        self.copy_pos = (l, cur);
    }

    /// Handle a key. Mirrors the TUI's prefix→command→forward logic; prefix here is Ctrl+Space
    /// (tmux-like), so a plain space still types normally.
    fn handle_key(&mut self, key: &Key, mods: &ModifiersState) {
        // A right-click context menu (when open) owns the next few keypresses: Escape dismisses,
        // Enter runs the selected row, j/k/arrows navigate. Any other key dismisses it and falls
        // through to the terminal below.
        if self.handle_ctx_key(key, mods) {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        // Normalize the spacebar once: macOS winit reports it as Named(Space), but every text
        // field, the live shell, and the broadcast toggle match Character(" "). Without this a
        // plain space is silently swallowed everywhere; copy mode keeps its own "space = copy"
        // handling below.
        let key = crate::keys::normalize_space(key);

        // Enter command mode: Ctrl+H is primary; Ctrl+Space / Ctrl+\ are always-on fallbacks.
        // On a mac with a second input source enabled, macOS itself owns Ctrl+Space (input-source
        // switcher) and the keystroke never arrives — so the first time the backslash fallback
        // chord works, explain once that macOS owns the Space chord rather than the app being
        // broken.
        if crate::keys::is_prefix_press(&key, mods, &self.prefix_key)
            && self.app.overlay == Overlay::None
        {
            if matches!(key, Key::Character(c) if c == "\\")
                && self.prefix_claimed
                && !self.prefix_alt_notice
            {
                self.prefix_alt_notice = true;
                self.flash = Some((
                    crate::macos::ctrl_space_notice(&self.prefix_key),
                    std::time::Instant::now(),
                ));
            }
            self.prefix_down = true;
            return;
        }

        if self.prefix_down && self.app.overlay == Overlay::None {
            self.prefix_down = false;
            if self.command_key(&key, mods) {
                self.quit();
            }
            return;
        }

        self.forward_key(&key, mods);
    }

    /// Persist which tabs are muted (prefix+m) so a restart brings them back muted instead of the
    /// tab nagging again the moment the window reopens. Shared by `quit` and the close path.
    fn save_muted_state(&self) {
        let keys: Vec<(&str, &str, &str, Option<&str>)> = self
            .app
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| self.muted.get(*i).copied().unwrap_or(false))
            .map(|(_, s)| {
                (
                    s.kind(),
                    s.meta.host.as_str(),
                    s.meta.engine.as_str(),
                    s.attach_session.as_deref(),
                )
            })
            .collect();
        crate::restore::save_muted(&keys);
    }

    /// Persist which tabs are pinned (prefix+a) so a restart keeps protecting them. Shared by
    /// `quit` and the close path (a pinned tab should survive a relaunch as pinned).
    fn save_pin_state(&self) {
        let keys: Vec<(&str, &str, &str, Option<&str>)> = self
            .app
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| self.pinned.get(*i).copied().unwrap_or(false))
            .map(|(_, s)| {
                (
                    s.kind(),
                    s.meta.host.as_str(),
                    s.meta.engine.as_str(),
                    s.attach_session.as_deref(),
                )
            })
            .collect();
        crate::restore::save_pinned(&keys);
    }

    /// Apply the same save-then-exit dance as a window CloseRequested: persist open tabs, tab list,
    /// and geometry, then flag the loop to exit at the next `about_to_wait`.
    /// Apply one native-menu-bar command drained in `about_to_wait`. Each maps onto the same
    /// handler the matching in-app Cmd shortcut uses, so the OS menu and the keyboard stay in sync.
    fn apply_menu_action(&mut self, a: crate::macos::menu::MenuAction) {
        use crate::macos::menu::MenuAction as M;
        match a {
            M::NewTab => {
                self.app.overlay = Overlay::NewSession;
                self.app.select_default_engine();
                self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
            }
            M::CloseTab => self.close_active_requested = true,
            M::Quit => self.quit(),
            M::ReopenTab => self.app.reopen_last_closed(),
            M::NextTab => {
                if !self.app.tabs.is_empty() {
                    let n = self.app.tabs.len();
                    self.set_active((self.app.active + 1) % n);
                }
            }
            M::PrevTab => {
                if !self.app.tabs.is_empty() {
                    let n = self.app.tabs.len();
                    self.set_active((self.app.active + n - 1) % n);
                }
            }
            M::CommandPalette => {
                self.palette_q.clear();
                self.palette_sel = 0;
                self.palette_rows = PaletteAction::all_rows();
                self.app.overlay = Overlay::CommandPalette;
            }
        }
    }

    fn quit(&mut self) {
        self.app.save_all_scrollbacks();
        crate::restore::save(&self.app.tab_specs());
        self.save_muted_state();
        self.save_pin_state();
        crate::restore::save_geometry(self.size.width, self.size.height);
        self.quit_requested = true;
    }

    /// Command-mode (after prefix). Returns true to quit.
    ///
    /// The single key pressed after `Ctrl+Space` looks up `self.key_action` (key -> action name,
    /// resolved from config with the hardcoded defaults as fallback) and dispatches on the action
    /// name. Digit keys 1-9 and Tab are NOT part of the remappable table — they stay fixed, exactly
    /// as before, so a remap can never accidentally break tab switching.
    fn command_key(&mut self, key: &Key, mods: &ModifiersState) -> bool {
        match key {
            Key::Character(c) if c.len() == 1 && c.chars().next().unwrap().is_ascii_digit() => {
                // Numeric tabs: 1-9 jump to that tab, 0 jumps to the LAST tab (kept fixed outside
                // the keybinding table so a remap can't break tab switching).
                let idx = c.chars().next().unwrap() as u8;
                if (b'1'..=b'9').contains(&idx) {
                    let i = (idx - b'1') as usize;
                    if i < self.app.tabs.len() {
                        self.set_active(i);
                    }
                } else if idx == b'0' && !self.app.tabs.is_empty() {
                    self.set_active(self.app.tabs.len() - 1);
                }
            }
            Key::Character(c) => match self.key_action.get(c.as_str()).map(String::as_str) {
                Some("palette") => {
                    self.app.overlay = Overlay::Palette;
                    self.app.query.clear();
                    self.app.selected = 0;
                    self.app.refresh_filter();
                }
                Some("new_session") => {
                    self.app.overlay = Overlay::NewSession;
                    self.app.select_default_engine();
                    self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
                }
                Some("remote_attach") => {
                    self.app.overlay = Overlay::RemoteAttach;
                    self.app.remote_host =
                        self.app.recent_hosts.first().cloned().unwrap_or_default();
                    self.app.selected = 0;
                }
                Some("local_shell") => {
                    if self.app.spawn_tmux("this-host", "shell").is_none() {
                        self.persist_tabs();
                    }
                }
                Some("quit") => return true,
                Some("fleet") => {
                    // Fleet overlay: fetch status on open so it's fresh, then show it. Filter starts
                    // empty so the full list is visible; typing narrows it live.
                    self.app.selected = 0;
                    self.fleet_query.clear();
                    self.fleet_filtered.clear();
                    // Non-blocking: falls back to the last cached fleet snapshot and kicks a fresh
                    // background fetch instead of stalling the UI for the daemon's HTTP timeout.
                    self.app.refresh_fleet_nonblocking();
                    self.app.overlay = Overlay::Fleet;
                }
                Some("fleet_grid") => {
                    self.grid_sel = 0;
                    self.app.overlay = Overlay::FleetGrid;
                }
                Some("goto_tab0") => {
                    if !self.app.tabs.is_empty() {
                        self.set_active(0);
                    }
                }
                Some("next_busy") => self.next_busy(),
                Some("next_quiet") => self.next_quiet(),
                Some("next_down") => self.next_down(),
                Some("next_host") => self.next_host(),
                Some("hosts") => {
                    self.hosts_sel = 0;
                    self.hosts_host = None;
                    self.app.overlay = Overlay::Hosts;
                }
                Some("dnd") => self.toggle_dnd(),
                Some("mute") => self.toggle_mute_active(),
                Some("interrupt") => self.interrupt_active(),
                Some("pin") => self.toggle_pin_active(),
                Some("next_pinned") => self.next_pinned(),
                Some("reconnect") => self.reconnect_active(),
                Some("reconnect_all") => self.reconnect_all_down(),
                Some("destroy") => self.destroy_active(),
                Some("last_window") => self.last_window(),
                Some("paste") => self.paste_clipboard(),
                Some("broadcast") => {
                    // Broadcast one line to every open session. Pre-fills the last-sent line (see
                    // the palette arm); targets reset all-on on each open — a prior run's
                    // deselections must NOT silently carry over, or a user who meant to exclude one
                    // host could re-fan to it on the next send.
                    self.broadcast_query = self.last_broadcast.clone();
                    self.broadcast_targets.iter_mut().for_each(|t| *t = true);
                    self.broadcast_sel = 0;
                    self.app.overlay = Overlay::Broadcast;
                }
                Some("close_quiet") => self.close_quiet_tabs(),
                Some("close_tab") => {
                    let pin = self.pinned.get(self.app.active).copied().unwrap_or(false);
                    let closed = self.app.active;
                    let closed_ok = close_tab(&mut self.app, pin);
                    if !closed_ok && pin {
                        self.flash = Some((
                            "🔒 pinned — prefix A to unpin first".to_string(),
                            std::time::Instant::now(),
                        ));
                    }
                    if closed_ok {
                        self.forget_tab(closed);
                        if self.native_tabs {
                            self.native_remove_host(closed);
                        }
                    }
                    self.save_pin_state();
                }
                Some("copy_scrollback") => self.copy_whole_scrollback(),
                Some("export_scrollback") => self.export_scrollback(),
                Some("copy_identity") => self.copy_identity(),
                Some("copy_fleet") => self.copy_fleet(),
                Some("peek") => self.open_peek(),
                Some("undo_close") => self.app.reopen_last_closed(),
                Some("duplicate") => {
                    self.duplicate_active_preserving_pin();
                    self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
                }
                Some("page_up") => {
                    scroll_active(self, 20);
                    if let Some(s) = self.app.active_session() {
                        s.set_scrolled(true);
                    }
                }
                Some("scroll_bottom") => self.scroll_to_bottom(),
                Some("scroll_top") => self.scroll_to_top(),
                Some("search") => self.open_find(),
                Some("search_all") => {
                    self.app.overlay = Overlay::FleetSearch;
                    self.fleet_q.clear();
                    self.fleet_matches.clear();
                    self.fleet_sel = 0;
                }
                Some("move_left") => self.move_tab_parallel(-1),
                Some("move_right") => self.move_tab_parallel(1),
                Some("copy_mode") => self.start_copy_mode(),
                Some("help") => {
                    self.app.overlay = Overlay::Help;
                }
                Some("command_palette") => {
                    self.palette_q.clear();
                    self.palette_sel = 0;
                    self.palette_rows = PaletteAction::all_rows();
                    self.app.overlay = Overlay::CommandPalette;
                }
                Some("session_info") => {
                    self.app.overlay = Overlay::Info;
                }
                Some("mark_all_read") => self.mark_all_read(),
                Some("toggle_focus") => self.toggle_focus(),
                Some("rename") => {
                    // Rename the active tab. Pre-fill with the current custom name (if any) so
                    // editing doesn't start from scratch.
                    self.rename_query = self
                        .app
                        .active_session()
                        .map(|s| s.meta.name.clone().unwrap_or_default())
                        .unwrap_or_default();
                    self.app.overlay = Overlay::Rename;
                }
                _ => {}
            },
            Key::Named(winit::keyboard::NamedKey::Tab) if !self.app.tabs.is_empty() => {
                // Shift+Tab cycles backward (wrapping) through tabs; plain Tab goes forward.
                let n = self.app.tabs.len();
                if mods.shift_key() {
                    self.set_active((self.app.active + n - 1) % n);
                } else {
                    self.set_active((self.app.active + 1) % n);
                }
            }
            _ => {}
        }
        false
    }

    /// True when the frame should keep rendering at full rate right now: something is visibly
    /// animating or changing that needs frequent repaints. When false the loop can afford its slow
    /// idle tick, because a static terminal grid doesn't need a full-framebuffer present 60x/sec.
    fn has_live_animation(&self) -> bool {
        if self.app.overlay != Overlay::None {
            return true;
        }
        if self.copy_mode || self.ctx.is_some() {
            return true;
        }
        if self.tooltip_box.is_some() || self.hover_tab.is_some() || self.flash.is_some() {
            return true;
        }
        // A tab producing output right now drives the busy spinner, so keep it smooth while it's
        // actually pouring (not merely carrying an unread badge — a quiet badge is static and needs
        // no full-rate pump). Uses the per-frame live signal, not cumulative `grew_delta`, so a
        // backgrounded tab with unseen output doesn't peg the loop forever.
        if self.live_busy {
            return true;
        }
        let now = std::time::Instant::now();
        if self.bell_until.iter().any(|t| t.is_some_and(|tt| tt > now))
            || self
                .recover_until
                .iter()
                .any(|t| t.is_some_and(|tt| tt > now))
        {
            return true;
        }
        false
    }

    fn forward_key(&mut self, key: &Key, mods: &ModifiersState) {
        // Native macOS menu shortcuts. We don't install an AppKit menu (yet), so on macOS these
        // arrive here as ordinary key events — and without this intercept they were forwarded to the
        // session as plain characters (Cmd+T typed "t", Cmd+Q typed "q"). Route them to the same
        // actions the prefix does. The decision is pure (`cmd_shortcut`) so it's unit-tested; this
        // method only executes the chosen action.
        let sc = cmd_shortcut(key, mods);
        match sc {
            CmdShortcut::NewSession => {
                self.app.overlay = Overlay::NewSession;
                self.app.select_default_engine();
                self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
            }
            CmdShortcut::CloseActive => {
                // Closing a native host window needs the event loop we only hold in `about_to_wait`.
                self.close_active_requested = true;
            }
            CmdShortcut::Quit => self.quit(),
            CmdShortcut::NextTab => {
                if !self.app.tabs.is_empty() {
                    let n = self.app.tabs.len();
                    self.set_active((self.app.active + 1) % n);
                }
            }
            CmdShortcut::PrevTab => {
                if !self.app.tabs.is_empty() {
                    let n = self.app.tabs.len();
                    self.set_active((self.app.active + n - 1) % n);
                }
            }
            CmdShortcut::CommandPalette => {
                self.palette_q.clear();
                self.palette_sel = 0;
                self.palette_rows = PaletteAction::all_rows();
                self.app.overlay = Overlay::CommandPalette;
            }
            CmdShortcut::FleetSearch => {
                self.app.overlay = Overlay::FleetSearch;
                self.fleet_q.clear();
                self.fleet_matches.clear();
                self.fleet_sel = 0;
            }
            CmdShortcut::ReopenTab => {
                self.app.reopen_last_closed();
            }
            CmdShortcut::Duplicate => {
                self.duplicate_active_preserving_pin();
                self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
            }
            CmdShortcut::ReconnectAll => {
                self.reconnect_all_down();
            }
            CmdShortcut::Pin => {
                self.toggle_pin_active();
            }
            CmdShortcut::Mute => {
                self.toggle_mute_active();
            }
            CmdShortcut::Info => {
                self.app.overlay = Overlay::Info;
            }
            CmdShortcut::CopyScrollback => {
                self.copy_whole_scrollback();
            }
            CmdShortcut::ExportScrollback => {
                self.export_scrollback();
            }
            CmdShortcut::Interrupt => {
                self.interrupt_active();
            }
            // Cmd+G / Cmd+Shift+G cycle through the last search's matches from anywhere. `find_jump`
            // is a no-op when there's no match list yet, and still scrolls + lands the viewport on
            // the new hit; the key handler redraws afterwards.
            CmdShortcut::Find => {
                self.open_find();
            }
            CmdShortcut::FindNext => {
                self.find_jump(1);
            }
            CmdShortcut::FindPrev => {
                self.find_jump(-1);
            }
            CmdShortcut::NextQuiet => {
                self.next_quiet();
            }
            CmdShortcut::InterruptAll => {
                self.interrupt_fleet();
            }
            CmdShortcut::Peek => {
                self.open_peek();
            }
            CmdShortcut::GotoTab(i) => {
                if !self.app.tabs.is_empty() {
                    // `0` was encoded as usize::MAX → the last tab (mirrors prefix+0).
                    let idx = if i == usize::MAX {
                        self.app.tabs.len() - 1
                    } else {
                        i.min(self.app.tabs.len() - 1)
                    };
                    self.set_active(idx);
                }
            }
            CmdShortcut::None => {}
        }
        if sc != CmdShortcut::None {
            return;
        }
        match self.app.overlay {
            Overlay::Palette => {
                match key {
                    Key::Character(c) => {
                        self.app.query.push_str(c);
                        self.app.refresh_filter();
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => self.app.jump_to_selection(),
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = self
                                .app
                                .selected
                                .saturating_add(1)
                                .min(self.app.filtered.len().saturating_sub(1))
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        // Tab moves down / Shift+Tab up through the filtered list, so keyboard-only
                        // divers can page sessions without the arrows, matching the peek/palette
                        // conventions.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            self.app.selected = self.app.selected.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.app.selected = (self.app.selected + 1)
                                .min(self.app.filtered.len().saturating_sub(1));
                        }
                        // PgDn/PgUp page the jump list by a full window (the same 12-row slice the
                        // renderer shows), so a large fleet isn't browsed one row at a time —
                        // mirroring the fleet/broadcast/peek list overlays.
                        winit::keyboard::NamedKey::PageDown => {
                            self.app.selected = (self.app.selected + 12)
                                .min(self.app.filtered.len().saturating_sub(1));
                        }
                        winit::keyboard::NamedKey::PageUp => {
                            self.app.selected = self.app.selected.saturating_sub(12);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.app.query.pop();
                            self.app.refresh_filter();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::NewSession => {
                match key {
                    // Typing builds the per-tab working-directory field; Up/Down still select the
                    // engine; Backspace edits the directory.
                    Key::Character(c) => self.new_cwd.push_str(c),
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            if let Some(e) = self.app.selected_engine() {
                                let cwd = if self.new_cwd.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.new_cwd.trim().to_string())
                                };
                                if let Some(err) = self.app.spawn_local("this-host", e, cwd) {
                                    self.flash =
                                        Some((format!("⚠ {err}"), std::time::Instant::now()));
                                    // Keep the picker open on failure so a bad cwd/engine can be
                                    // fixed and retried instead of forcing a full retype.
                                    return;
                                }
                                self.persist_tabs();
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1)
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.new_cwd.pop();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::RemoteAttach => {
                match key {
                    Key::Character(c) => self.app.remote_host.push_str(c),
                    Key::Named(n) => match n {
                        // Tab cycles the host field through the remembered hosts (MRU) so a diver can
                        // page back to a machine they connected to before without retyping the addr.
                        winit::keyboard::NamedKey::Tab => {
                            // Tab cycles forward, Shift+Tab backward through the remembered hosts.
                            let hosts = &self.app.recent_hosts;
                            if hosts.is_empty() {
                                return;
                            }
                            let cur = self.app.remote_host.trim().to_string();
                            let pos = hosts.iter().position(|h| h == &cur);
                            let next = if mods.shift_key() {
                                match pos {
                                    Some(i) => (i + hosts.len() - 1) % hosts.len(),
                                    None => hosts.len() - 1,
                                }
                            } else {
                                match pos {
                                    Some(i) => (i + 1) % hosts.len(),
                                    None => 0,
                                }
                            };
                            self.app.remote_host = hosts[next].clone();
                            return;
                        }
                        winit::keyboard::NamedKey::Enter => {
                            let raw = self.app.remote_host.trim();
                            // `host[:port]` = spawn a fresh engine; `host[:port]/session` = attach an
                            // existing remote tmux session without spawning anything (no engine pick).
                            let (addr, attach) = parse_remote_attach(raw);
                            let (host, port) = if addr.is_empty() {
                                (
                                    "127.0.0.1".to_string(),
                                    crate::harness::HARNESS_PORT_DEFAULT,
                                )
                            } else if let Some((h, p)) = addr.split_once(':') {
                                (
                                    h.to_string(),
                                    p.parse().unwrap_or(crate::harness::HARNESS_PORT_DEFAULT),
                                )
                            } else {
                                (addr, crate::harness::HARNESS_PORT_DEFAULT)
                            };
                            let label =
                                format!("{}:{port}", if host.is_empty() { "?" } else { &host });
                            // Snapshot whether this is an attach (vs a fresh spawn) and the session
                            // name before `attach` is moved into the match below, so the success
                            // toast can describe what actually kicked off.
                            let is_attach = attach.is_some();
                            let attach_name = attach.clone().unwrap_or_default();
                            let err = match attach {
                                Some(session) => {
                                    self.app.spawn_tunnel_attach(&host, port, &session)
                                }
                                None => self
                                    .app
                                    .selected_engine()
                                    .and_then(|e| self.app.spawn_tunnel(&host, port, e)),
                            };
                            // A failed remote connect must not be silent: flash the real reason so a
                            // diver knows the daemon/session wasn't reached (host down, wrong port,
                            // no such session) instead of the tab just never appearing.
                            if let Some(e) = err {
                                self.flash =
                                    Some((format!("⚠ {label}: {e}"), std::time::Instant::now()));
                                // Keep the overlay open (and the typed address) so a typo'd host can
                                // be corrected and re-submitted rather than lost and retyped.
                                return;
                            }
                            // Success feedback: connecting to a faraway host shouldn't be a silent
                            // "did anything happen?" — flash what just kicked off. Attach resumes an
                            // existing session; spawn starts a fresh engine on that machine.
                            let ok = if is_attach {
                                format!("attached {attach_name} @ {label} ✓")
                            } else {
                                format!(
                                    "connecting {} @ {label} …",
                                    self.app.selected_engine().unwrap_or("engine")
                                )
                            };
                            self.flash = Some((ok, std::time::Instant::now()));
                            self.persist_tabs();
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1)
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.app.remote_host.pop();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Find => {
                match key {
                    // `c` toggles case-sensitivity, `w` toggles whole-word (iTerm2-style find
                    // options, shown in the status hint). Typed only when the query is empty so a
                    // search containing those letters isn't hijacked mid-entry.
                    Key::Character(c) if c == "c" && self.find_query.is_empty() => {
                        self.find_opts.case_sensitive = !self.find_opts.case_sensitive;
                        self.persist_find_opts();
                        self.find_query.clear();
                        self.find_recompute(None);
                    }
                    Key::Character(c) if c == "w" && self.find_query.is_empty() => {
                        self.find_opts.whole_word = !self.find_opts.whole_word;
                        self.persist_find_opts();
                        self.find_query.clear();
                        self.find_recompute(None);
                    }
                    Key::Character(c) => {
                        self.find_query.push_str(c);
                        self.find_recompute(None);
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter if mods.shift_key() => {
                            self.record_find_query();
                            self.find_jump(-1);
                        }
                        // Shift+Tab goes to the previous match (mirroring Shift+Enter and the other
                        // list overlays' Shift+Tab-up convention); plain Tab advances.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            self.find_jump(-1);
                        }
                        winit::keyboard::NamedKey::Enter => {
                            self.record_find_query();
                            self.find_jump(1);
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.find_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.find_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            // Empty query + history recalls the most recent search (iTerm2 memory);
                            // otherwise Up walks the previous match like before.
                            if self.find_query.is_empty() {
                                if let Some(q) = self.find_history.first().cloned() {
                                    self.find_query = q;
                                    self.find_recompute(None);
                                }
                            } else {
                                self.find_jump(-1);
                            }
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.find_query.pop();
                            self.find_recompute(None);
                        }
                        winit::keyboard::NamedKey::Escape => {
                            // Close the find bar but keep the match list + highlight (and the last
                            // query), so Cmd+G / Cmd+Shift+G keep cycling through the hits from
                            // anywhere, exactly like iTerm2 — the matches stay lit until the next
                            // Cmd+F starts a fresh search.
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::FleetSearch => {
                match key {
                    Key::Character(c) => {
                        self.fleet_q.push_str(c);
                        self.fleet_recompute();
                    }
                    Key::Named(n) => match n {
                        // Enter closes and jumps to the selected match across tables.
                        winit::keyboard::NamedKey::Enter => self.fleet_jump_to(),
                        // Tab moves the selection down (Shift+Tab up), wrapping.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            self.fleet_jump(-1);
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.fleet_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.fleet_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.fleet_jump(-1);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.fleet_q.pop();
                            self.fleet_recompute();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Fleet => {
                match key {
                    Key::Named(n) => match n {
                        // Up/Down move the highlighted row; Enter attaches to it (jump to an open
                        // tab for that engine, else open a fresh local tmux pane). Esc dismisses.
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            if !self.fleet_filtered.is_empty() {
                                self.app.selected =
                                    (self.app.selected + 1).min(self.fleet_filtered.len() - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1);
                        }
                        // PgUp/PgDn page by a fixed slice (the list overlays use a 20-row window)
                        // so a long fleet list doesn't need one press per row, matching the grid.
                        winit::keyboard::NamedKey::PageDown => {
                            if !self.fleet_filtered.is_empty() {
                                self.app.selected = (self.app.selected + 10)
                                    .min(self.fleet_filtered.len().saturating_sub(1));
                            }
                        }
                        winit::keyboard::NamedKey::PageUp => {
                            self.app.selected = self.app.selected.saturating_sub(10);
                        }
                        winit::keyboard::NamedKey::Enter => {
                            self.fleet_attach_selected();
                        }
                        // Backspace removes the last filter character (re-filter live).
                        winit::keyboard::NamedKey::Backspace => {
                            self.fleet_query.pop();
                            self.app.selected = 0;
                            self.fleet_refresh_filter();
                        }
                        _ => {}
                    },
                    // `s` re-fetches for a fresh view; any other character filters the list.
                    Key::Character(c) if c == "s" && self.fleet_query.is_empty() => {
                        // Non-blocking refresh: take the cached snapshot and refetch in the
                        // background rather than blocking the main loop on a wedged daemon.
                        self.app.refresh_fleet_nonblocking();
                    }
                    Key::Character(c) => {
                        self.fleet_query.push_str(c);
                        self.app.selected = 0;
                        self.fleet_refresh_filter();
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Hosts => {
                match key {
                    Key::Named(n) => match n {
                        // In the drill-in sub-list, Esc or Left returns to the host list; in the
                        // host list, Esc closes the overlay.
                        winit::keyboard::NamedKey::Escape => {
                            if self.hosts_host.take().is_some() {
                                self.hosts_sel = 0;
                            } else {
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::ArrowLeft => {
                            if self.hosts_host.take().is_some() {
                                self.hosts_sel = 0;
                            }
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            if let Some(host) = &self.hosts_host {
                                let n = self.host_session_indices(host).len();
                                if n > 0 {
                                    self.hosts_sel = (self.hosts_sel + 1).min(n - 1);
                                }
                            } else {
                                let tally = self.owned_host_tally();
                                if !tally.is_empty() {
                                    self.hosts_sel =
                                        (self.hosts_sel + 1).min(tally.len().saturating_sub(1));
                                }
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.hosts_sel = self.hosts_sel.saturating_sub(1);
                        }
                        // PgUp/PgDn page through either the host list or a host's drill-in sessions
                        // by a fixed 10-row slice (host list rows fit in a 20-row window).
                        winit::keyboard::NamedKey::PageDown => {
                            let bound = if let Some(host) = &self.hosts_host {
                                self.host_session_indices(host).len().saturating_sub(1)
                            } else {
                                self.owned_host_tally().len().saturating_sub(1)
                            };
                            self.hosts_sel = (self.hosts_sel + 10).min(bound);
                        }
                        winit::keyboard::NamedKey::PageUp => {
                            self.hosts_sel = self.hosts_sel.saturating_sub(10);
                        }
                        winit::keyboard::NamedKey::ArrowRight => {
                            // Drill from the host list into the selected host's sessions.
                            if self.hosts_host.is_none() {
                                let tally = self.owned_host_tally();
                                if let Some((host, _, _)) = tally.get(self.hosts_sel) {
                                    if self.host_session_indices(host).len() > 1 {
                                        self.hosts_host = Some(host.clone());
                                        self.hosts_sel = 0;
                                    }
                                }
                            }
                        }
                        winit::keyboard::NamedKey::Enter => {
                            if let Some(host) = self.hosts_host.clone() {
                                // Drill view: open the selected session on this host.
                                let idxs = self.host_session_indices(&host);
                                if let Some(&i) = idxs.get(self.hosts_sel) {
                                    self.set_active(i);
                                    self.flash = Some((
                                        format!("{host} · {}", i + 1),
                                        std::time::Instant::now(),
                                    ));
                                    self.app.overlay = Overlay::None;
                                }
                            } else {
                                let tally = self.owned_host_tally();
                                if let Some((host, _, _)) = tally.get(self.hosts_sel) {
                                    if let Some(i) = self.app.tabs.iter().position(|s| {
                                        let h = if s.meta.host.is_empty() {
                                            "local".to_string()
                                        } else {
                                            s.meta.host.clone()
                                        };
                                        &h == host
                                    }) {
                                        self.set_active(i);
                                        self.flash = Some((
                                            format!("host {host}"),
                                            std::time::Instant::now(),
                                        ));
                                        self.app.overlay = Overlay::None;
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    // In the drill-in view, `r` force-reconnects every down pane on this host — the
                    // per-machine cousin of prefix+T. Most useful when a whole box came back after a
                    // blip but its panes are still waiting on their backoff timers.
                    Key::Character(c) => {
                        if (c == "r" || c == "R") && self.hosts_host.is_some() {
                            let host = self.hosts_host.clone().unwrap_or_default();
                            let idxs = self.host_session_indices(&host);
                            let mut ok = 0usize;
                            let mut still = 0usize;
                            for i in idxs {
                                let (pty, alive) = {
                                    let s = &self.app.tabs[i];
                                    (s.kind() == "pty", s.alive())
                                };
                                if pty || alive {
                                    continue;
                                }
                                match self.app.tabs[i].reconnect_now() {
                                    Ok(()) => ok += 1,
                                    Err(_) => still += 1,
                                }
                            }
                            self.flash = Some((
                                if still > 0 {
                                    format!("{host}: {ok} reconnected, {still} still down")
                                } else {
                                    format!("{host}: {ok} reconnected")
                                },
                                std::time::Instant::now(),
                            ));
                        }
                        // `b` in the drill-in broadcasts one line to every session on this host —
                        // the fanned-out sibling of `r` (reconnect this host). Handy for "same
                        // command, every agent on build05".
                        if (c == "b" || c == "B") && self.hosts_host.is_some() {
                            let host = self.hosts_host.clone().unwrap_or_default();
                            self.host_broadcast(&host);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Help => {
                self.app.overlay = Overlay::None;
                return;
            }
            Overlay::Info => {
                self.app.overlay = Overlay::None;
                return;
            }
            Overlay::Rename => {
                match key {
                    Key::Character(c) => {
                        self.rename_query.push_str(c);
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Commit the rename (empty = clear back to the default engine label).
                            let name = if self.rename_query.trim().is_empty() {
                                None
                            } else {
                                Some(self.rename_query.trim().to_string())
                            };
                            if let Some(s) = self.app.active_session_mut() {
                                s.meta.name = name;
                            }
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.rename_query.pop();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Broadcast => {
                // Keep the target list sized to the current tab set (a tab may have opened/closed).
                if self.broadcast_targets.len() != self.app.tabs.len() {
                    self.broadcast_targets.resize(self.app.tabs.len(), true);
                    self.broadcast_sel = self
                        .broadcast_sel
                        .min(self.app.tabs.len().saturating_sub(1));
                }
                match key {
                    Key::Character(c) => {
                        if c == " " {
                            // Space toggles the focused session's target. Shift+Space toggles the whole
                            // selection between all-on and all-off — a quick reset after hand-curating
                            // a set, or a one-key fan-out to every session (the common open state).
                            if mods.shift_key() {
                                let all_on = self.broadcast_targets.iter().all(|&t| t);
                                self.broadcast_targets.iter_mut().for_each(|t| *t = !all_on);
                            } else if let Some(on) =
                                self.broadcast_targets.get_mut(self.broadcast_sel)
                            {
                                *on = !*on;
                            }
                        } else {
                            self.broadcast_query.push_str(c);
                        }
                        // Editing a fresh line leaves history recall.
                        self.hist_sel = None;
                    }
                    Key::Named(n) => {
                        match n {
                            winit::keyboard::NamedKey::Enter => {
                                // Fan the line out to the MARKED sessions only, then close. Unchecked
                                // sessions are left untouched — the whole point of targeting.
                                let bytes = broadcast_bytes(&self.broadcast_query);
                                if !bytes.is_empty() {
                                    let mut sent = 0usize;
                                    let mut queued = 0usize;
                                    for (i, s) in self.app.tabs.iter().enumerate() {
                                        if self.broadcast_targets.get(i).copied().unwrap_or(false) {
                                            // Count live vs down targets so the confirm below can say
                                            // how many landed now vs are staged for reconnect.
                                            if s.alive() {
                                                sent += 1;
                                            } else {
                                                queued += 1;
                                            }
                                            s.write(&bytes);
                                        }
                                    }
                                    // Confirm the fan-out right away. A command to a live pane ran now;
                                    // one to a down pane is staged and flushes on reconnect — the diver
                                    // shouldn't assume every target received it instantly.
                                    self.flash = Some((
                                        if queued > 0 {
                                            format!("broadcast to {sent} · {queued} queued on reconnect")
                                        } else {
                                            format!("broadcast to {sent}")
                                        },
                                        std::time::Instant::now(),
                                    ));
                                    // Remember what we sent so the next broadcast pre-fills it (a repeat
                                    // command to every host doesn't need retyping).
                                    self.last_broadcast = self.broadcast_query.clone();
                                    crate::restore::save_last_broadcast(&self.last_broadcast);
                                    // Push it onto the MRU history (dedup + front + cap 16).
                                    self.broadcast_hist.retain(|h| h != &self.broadcast_query);
                                    self.broadcast_hist.insert(0, self.broadcast_query.clone());
                                    self.broadcast_hist.truncate(16);
                                    crate::restore::save_broadcast_history(&self.broadcast_hist);
                                }
                                self.broadcast_query.clear();
                                self.hist_sel = None;
                                self.app.overlay = Overlay::None;
                            }
                            winit::keyboard::NamedKey::ArrowDown => {
                                if mods.shift_key() {
                                    // Shift+Down recalls newer history.
                                    self.hist_down();
                                } else {
                                    self.broadcast_sel = (self.broadcast_sel + 1)
                                        .min(self.app.tabs.len().saturating_sub(1));
                                }
                            }
                            winit::keyboard::NamedKey::ArrowUp => {
                                if mods.shift_key() {
                                    // Shift+Up recalls older history.
                                    self.hist_up();
                                } else {
                                    self.broadcast_sel = self.broadcast_sel.saturating_sub(1);
                                }
                            }
                            // Tab moves down / Shift+Tab up through the target list, so the focused
                            // row can be walked without the arrows (peek/palette convention). Shift
                            // here is distinct from Shift+arrows, which recall history.
                            winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                                self.broadcast_sel = self.broadcast_sel.saturating_sub(1);
                            }
                            winit::keyboard::NamedKey::Tab => {
                                self.broadcast_sel = (self.broadcast_sel + 1)
                                    .min(self.app.tabs.len().saturating_sub(1));
                            }
                            // PgDn/PgUp page the target list by a full window (the same 20-row slice
                            // the renderer shows), so a broadcast to a large fleet doesn't need one
                            // keypress per row — mirroring the fleet/palette/broadcast list overlays.
                            winit::keyboard::NamedKey::PageDown => {
                                self.broadcast_sel = (self.broadcast_sel + 20)
                                    .min(self.app.tabs.len().saturating_sub(1));
                            }
                            winit::keyboard::NamedKey::PageUp => {
                                self.broadcast_sel = self.broadcast_sel.saturating_sub(20);
                            }
                            winit::keyboard::NamedKey::Backspace => {
                                self.broadcast_query.pop();
                                self.hist_sel = None;
                            }
                            winit::keyboard::NamedKey::Escape => {
                                self.broadcast_query.clear();
                                self.app.overlay = Overlay::None;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Peek => {
                // Recompute the (possibly filtered) list before any navigation resolves an index,
                // so a first keypress right after the overlay opens reads fresh state.
                self.peek_refresh_filter();
                match key {
                    Key::Character(c) => {
                        if self.peek_filtering {
                            // While the `/` filter is open, letters build the query — type "down",
                            // "build05", "claude", etc. to narrow the fleet's triage. A second `/`
                            // closes the prompt and keeps the query applied.
                            if c == "/" {
                                self.peek_filtering = false;
                            } else {
                                self.peek_q.push_str(c);
                            }
                        } else if c == "/" {
                            self.peek_q.clear();
                            self.peek_filtering = true;
                            // Start the filter at the top of the (soon narrow) list so the first
                            // matches are immediately visible rather than inheriting a stale scroll.
                            self.peek_sel = 0;
                            self.peek_scroll = 0;
                        } else if c == "n" || c == "N" {
                            // Jump to the next down pane within the (possibly filtered) list.
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                let start = self.peek_sel;
                                for step in 1..=shown {
                                    let i = (start + step) % shown;
                                    let real = self.peek_filtered[i];
                                    if self.app.tabs[real].kind() != "pty"
                                        && !self.app.tabs[real].alive()
                                    {
                                        self.peek_sel = i;
                                        // Slide the window so the chosen row is the last visible.
                                        self.peek_scroll = i.saturating_add(1).saturating_sub(10);
                                        break;
                                    }
                                }
                            }
                        } else if c == "r" || c == "R" {
                            // Reconnect the selected pane straight from triage — no drill-in needed.
                            // Aimed at the down row under the cursor; it stays selected so the
                            // ○→● fix is visible the moment the transport comes back. A live row is
                            // left alone (nothing to reconnect).
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                let real = self.peek_filtered[self.peek_sel.min(shown - 1)];
                                let down = {
                                    let s = &self.app.tabs[real];
                                    !s.alive() && s.kind() != "pty"
                                };
                                if down {
                                    let id = self.tab_identity(real);
                                    match self.app.tabs[real].reconnect_now() {
                                        Ok(()) => {
                                            self.flash = Some((
                                                format!("reconnecting — {id}"),
                                                std::time::Instant::now(),
                                            ));
                                        }
                                        Err(_) => {
                                            self.flash = Some((
                                                format!("reconnect failed — {id}"),
                                                std::time::Instant::now(),
                                            ));
                                        }
                                    }
                                }
                            }
                        } else if c == "m" || c == "M" {
                            // Toggle mute on the selected pane straight from triage — the per-row
                            // sibling of prefix mute — so a noisy backgrounded agent is silenced from
                            // the list without a drill-in. Non-destructive (no pin/undo dance needed).
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                let real = self.peek_filtered[self.peek_sel.min(shown - 1)];
                                if let Some(m) = self.muted.get_mut(real) {
                                    *m = !*m;
                                    self.save_muted_state();
                                }
                            }
                        } else if c == "p" || c == "P" {
                            // Toggle pin on the selected pane straight from triage — shields it from
                            // any bulk close while triaging. Non-destructive; toggle again to unpin.
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                let real = self.peek_filtered[self.peek_sel.min(shown - 1)];
                                self.toggle_pin_at(real);
                            }
                        } else if c == "x" {
                            // Close the selected pane straight from triage — the per-row sibling of
                            // the fleet grid's `x`. Honours the pin guard (flashes instead) and
                            // stashes an undo spec, so a mistaken close is `prefix+u` / Cmd+Shift+T
                            // away. The row disappears from the list right away.
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                let real = self.peek_filtered[self.peek_sel.min(shown - 1)];
                                self.close_tab_at(real);
                                if self.app.tabs.is_empty() {
                                    self.app.overlay = Overlay::None;
                                } else {
                                    // Closing shifts the following indices; re-filter from the live
                                    // tab set and clamp the selection back into range.
                                    self.peek_refresh_filter();
                                    self.peek_sel = self
                                        .peek_sel
                                        .min(self.peek_filtered.len().saturating_sub(1));
                                    self.peek_scroll = self
                                        .peek_scroll
                                        .min(self.peek_filtered.len().saturating_sub(10));
                                }
                            }
                        }
                    }
                    Key::Named(n) => match n {
                        // Backspace edits the filter query while open; an empty query ends filtering.
                        winit::keyboard::NamedKey::Backspace => {
                            if self.peek_filtering {
                                self.peek_q.pop();
                                if self.peek_q.is_empty() {
                                    self.peek_filtering = false;
                                }
                            }
                        }
                        winit::keyboard::NamedKey::ArrowDown | winit::keyboard::NamedKey::Tab
                            if !mods.shift_key() =>
                        {
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                self.peek_sel = (self.peek_sel + 1).min(shown - 1);
                            }
                            // Keep the selection in the visible window as it walks past the bottom.
                            if self.peek_sel >= self.peek_scroll + 10 {
                                self.peek_scroll = self.peek_sel + 1 - 10;
                            }
                        }
                        winit::keyboard::NamedKey::Tab => {
                            if !self.peek_filtered.is_empty() {
                                self.peek_sel = self.peek_sel.saturating_sub(1);
                            }
                            // Pull the window up when the selection walks above the top.
                            if self.peek_sel < self.peek_scroll {
                                self.peek_scroll = self.peek_sel;
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            if !self.peek_filtered.is_empty() {
                                self.peek_sel = self.peek_sel.saturating_sub(1);
                            }
                            if self.peek_sel < self.peek_scroll {
                                self.peek_scroll = self.peek_sel;
                            }
                        }
                        // PgDn/PgUp page the triage list by a full window (the same 10-row slice the
                        // renderer shows), so a large fleet doesn't need one keypress per row —
                        // mirroring the fleet/palette/broadcast list overlays.
                        winit::keyboard::NamedKey::PageDown => {
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                self.peek_sel = (self.peek_sel + 10).min(shown - 1);
                            }
                            if self.peek_sel >= self.peek_scroll + 10 {
                                self.peek_scroll = self.peek_sel + 1 - 10;
                            }
                        }
                        winit::keyboard::NamedKey::PageUp => {
                            if !self.peek_filtered.is_empty() {
                                self.peek_sel = self.peek_sel.saturating_sub(10);
                            }
                            if self.peek_sel < self.peek_scroll {
                                self.peek_scroll = self.peek_sel;
                            }
                        }
                        winit::keyboard::NamedKey::Enter => {
                            let shown = self.peek_filtered.len();
                            if shown > 0 {
                                // Resolve the (possibly filtered) selection back to a real tab, then
                                // jump and focus it as before.
                                let real = self.peek_filtered[self.peek_sel.min(shown - 1)];
                                self.app.active = real.min(self.app.tabs.len().saturating_sub(1));
                                crate::restore::save_active(self.app.active);
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => {
                            // Esc first clears an active filter (back to all sessions), then closes.
                            if self.peek_filtering {
                                self.peek_q.clear();
                                self.peek_filtering = false;
                            } else {
                                self.app.overlay = Overlay::None;
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::FleetGrid => {
                match key {
                    Key::Named(n) => match n {
                        // A re-computed layout isn't needed per keypress: the first three/down/right
                        // edges are enough to clamp the selection without a full re-layout.
                        winit::keyboard::NamedKey::ArrowDown | winit::keyboard::NamedKey::Tab
                            if !mods.shift_key() =>
                        {
                            self.grid_sel =
                                (self.grid_sel + 1).min(self.app.tabs.len().saturating_sub(1));
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.grid_sel = self.grid_sel.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.grid_sel = self.grid_sel.saturating_sub(1);
                        }
                        // PgDn/PgUp jump the selection by a full row of tiles (the same column count
                        // the render uses), so covering a large fleet doesn't need one keypress per
                        // tile. Clamped to the first/last session like the arrow keys.
                        winit::keyboard::NamedKey::PageDown => {
                            let cols = self.fleet_grid_cols();
                            self.grid_sel =
                                (self.grid_sel + cols).min(self.app.tabs.len().saturating_sub(1));
                        }
                        winit::keyboard::NamedKey::PageUp => {
                            let cols = self.fleet_grid_cols();
                            self.grid_sel = self.grid_sel.saturating_sub(cols);
                        }
                        winit::keyboard::NamedKey::Enter => {
                            if !self.app.tabs.is_empty() {
                                self.app.active = self.grid_sel.min(self.app.tabs.len() - 1);
                                crate::restore::save_active(self.app.active);
                                self.app.overlay = Overlay::None;
                            }
                            // Marks are scoped to one grid session: dive or close clears them so a
                            // stale mark set can't silently seed a later `b`/`C`/`R`/`X` — especially
                            // `X`, a bulk close, which would otherwise undo the "requires marks so a
                            // stray press is never destructive" guard. Matches the broadcast overlay's
                            // fresh-reset-on-open behavior.
                            self.grid_marks = vec![false; self.app.tabs.len()];
                        }
                        winit::keyboard::NamedKey::Space => {
                            if let Some(m) = self.grid_marks.get_mut(self.grid_sel) {
                                *m = !*m;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                            // Same one-session scope as Enter above (marks don't outlive the grid view).
                            self.grid_marks = vec![false; self.app.tabs.len()];
                        }
                        _ => {}
                    },
                    // session index (1..=9) does the same. Space toggles a mark on the focused tile
                    // (a multi-select for the targeted-`b` broadcast); `b` opens the broadcast
                    // overlay pre-scoped to every marked tile. Everything else is ignored.
                    Key::Character(c) => {
                        let ch = c.chars().next();
                        if let Some(d) = ch.and_then(|ch| ch.to_digit(10)) {
                            if d >= 1 && d <= 9 {
                                let i = (d - 1) as usize;
                                if i < self.app.tabs.len() {
                                    self.grid_sel = i;
                                }
                            }
                        } else if ch == Some('b') {
                            // `b` opens the broadcast overlay pre-scoped to the marked tiles (Space
                            // is the only mark toggle, handled above). Without this explicit match
                            // the letter was swallowed by a stray mark-toggle and broadcast-marked
                            // was unreachable.
                            self.grid_broadcast_marked();
                        } else if ch == Some('n') || ch == Some('N') {
                            // `n`/`N` jump the selection to the next/previous tile that needs
                            // attention — the war-room sibling of peek's `n`: first a DOWN pane,
                            // else the next busy one, wrapping around the grid. Lets a diver hop
                            // pane-to-pane through a large fleet's trouble spots instead of paging
                            // one tile at a time. Capital `N` walks backward, so both directions are
                            // covered without arrowing. A fully healthy fleet flashes a legible no-op.
                            let n = self.app.tabs.len();
                            if n > 0 {
                                let down: Vec<bool> = (0..n)
                                    .map(|i| {
                                        let s = &self.app.tabs[i];
                                        s.kind() != "pty" && !s.alive()
                                    })
                                    .collect();
                                let busy = self.activity_flags();
                                let found = if ch == Some('n') {
                                    next_trouble_index(&down, &busy, self.grid_sel)
                                } else {
                                    prev_trouble_index(&down, &busy, self.grid_sel)
                                };
                                if let Some(idx) = found {
                                    self.grid_sel = idx;
                                } else {
                                    self.flash = Some((
                                        "fleet healthy — no down or busy panes".to_string(),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        } else if ch == Some('r') {
                            // `r` force-reconnects JUST the selected tile — the per-tile sibling of
                            // bulk `R` (all marked / all down) and of peek's `r`. Lets a war-room
                            // heal one dropped pane without touching the rest of the fleet.
                            let sel = self.grid_sel;
                            let down = self
                                .app
                                .tabs
                                .get(sel)
                                .map(|s| s.kind() != "pty" && !s.alive())
                                .unwrap_or(false);
                            if down {
                                let id = self.tab_identity(sel);
                                match self.app.tabs[sel].reconnect_now() {
                                    Ok(()) => {
                                        self.flash = Some((
                                            format!("reconnecting — {id}"),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                    Err(_) => {
                                        self.flash = Some((
                                            format!("reconnect failed — {id}"),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                            }
                        } else if ch == Some('R') {
                            // `R` force-reconnects every marked tile (falling back to all down) —
                            // the `b`-style bulk action for healing, complementing broadcast.
                            self.grid_reconnect_marked();
                        } else if ch == Some('m') {
                            // `m` toggles mute on the selected tile — the per-tile sibling of peek's
                            // `m` and of prefix mute — so a noisy backgrounded agent is silenced
                            // straight from the war-room without a drill-in. Non-destructive; the
                            // selected tile stays put so the toggle's `M` glyph change is visible.
                            let sel = self.grid_sel;
                            // Scope the mutable borrow so `save_muted_state`/`tab_identity` can run
                            // after the vector write (they can't while `get_mut` is still live).
                            let toggled = {
                                if let Some(m) = self.muted.get_mut(sel) {
                                    *m = !*m;
                                    Some(*m)
                                } else {
                                    None
                                }
                            };
                            if let Some(new_muted) = toggled {
                                self.save_muted_state();
                                let id = self.tab_identity(sel);
                                let state = if new_muted { "muted" } else { "unmuted" };
                                self.flash =
                                    Some((format!("{id} {state}"), std::time::Instant::now()));
                            }
                        } else if ch == Some('p') {
                            // `p` toggles pin on the selected tile — shields it from any bulk close
                            // (`X`, `prefix+close_quiet`) while triaging the war-room. Per-tile
                            // sibling of peek's `p` and of prefix pin.
                            self.toggle_pin_at(self.grid_sel);
                        } else if ch == Some('C') {
                            // `C` sends Ctrl-C to every marked tile (falling back to all non-muted) —
                            // the "stop the batch job" sibling of `R` reconnect and `b` broadcast.
                            self.grid_interrupt_marked();
                        } else if ch == Some('x') {
                            // `x` closes the selected tile (honoring the pin guard + undo, like the
                            // tab bar's ×) so a war-room can prune dead/finished panes in place.
                            self.close_tab_at(self.grid_sel);
                            if self.app.tabs.is_empty() {
                                self.app.overlay = Overlay::None;
                            } else {
                                self.grid_sel = self.grid_sel.min(self.app.tabs.len() - 1);
                            }
                        } else if ch == Some('X') {
                            // `X` bulk-closes every marked tile; unlike `b`/`C`/`R` it needs marks
                            // (no "close everything" fallback) so a stray press is never destructive.
                            self.grid_close_marked();
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::CommandPalette => {
                match key {
                    Key::Character(c) => {
                        self.palette_q.push_str(c);
                        self.palette_refresh_filter();
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Resolve the selected row to its action and run it (closes the overlay).
                            if let Some(&i) = self.palette_filtered.get(self.palette_sel) {
                                let a = self.palette_rows[i].1;
                                self.run_palette_action(a);
                            }
                        }
                        // Tab moves down, Shift+Tab up (wraps); arrows move within the filtered list.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            if self.palette_filtered.is_empty() {
                                self.palette_sel = 0;
                            } else {
                                self.palette_sel = self.palette_sel.saturating_sub(1);
                            }
                        }
                        winit::keyboard::NamedKey::Tab => {
                            let len = self.palette_filtered.len();
                            if len > 0 {
                                self.palette_sel = (self.palette_sel + 1).min(len - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            let len = self.palette_filtered.len();
                            if len > 0 {
                                self.palette_sel = (self.palette_sel + 1).min(len - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.palette_sel = self.palette_sel.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.palette_q.pop();
                            self.palette_refresh_filter();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.palette_q.clear();
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::None => {}
        }

        // Normal mode: send keystrokes to the active session.
        if self.app.overlay == Overlay::None {
            // Font zoom (Ctrl/Cmd+= / - / 0 to reset) — captured before anything reaches the shell,
            // like any terminal's, and a persistent per-window preference. Both the Ctrl and the
            // Cmd (macOS ⌘) forms map, matching how iTerm2 lets you zoom either way.
            if mods.control_key() || mods.super_key() {
                if let Key::Character(c) = key {
                    match c.as_str() {
                        "=" | "+" => {
                            self.zoom_font(1.1);
                            return;
                        }
                        "-" => {
                            self.zoom_font(1.0 / 1.1);
                            return;
                        }
                        "0" => {
                            self.zoom = 1.0;
                            crate::restore::save_zoom(1.0);
                            self.metrics_from_scale();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // Fullscreen toggle (Ctrl+Enter). A fleet diver drops a busy pane fullscreen to watch it
            // without the OS chrome; pressing again returns to windowed. Window-size events reflow
            // the grid automatically.
            if mods.control_key() && matches!(key, Key::Named(winit::keyboard::NamedKey::Enter)) {
                if let Some(w) = &self.window {
                    let fs = w.fullscreen().is_some();
                    w.set_fullscreen(if fs {
                        None
                    } else {
                        Some(winit::window::Fullscreen::Borderless(None))
                    });
                    self.flash = Some((
                        if fs { "windowed" } else { "fullscreen" }.to_string(),
                        std::time::Instant::now(),
                    ));
                }
                return;
            }
            // Copy mode intercepts keystrokes (navigation + selection) instead of forwarding.
            if self.copy_mode {
                self.handle_copy_key(key, mods);
                return;
            }
            // Scrollback navigation takes precedence over forwarding to the shell. While scrolled,
            // page/arrow keys move the viewport; Esc returns to the live (bottom) view. PageUp from
            // the live view also enters scroll mode.
            // iTerm2-style jump-to-end: Cmd+Up / Cmd+Home jump to the very top of the scrollback,
            // Cmd+Down / Cmd+End snap back to the live bottom. Works from the live view too (so no
            // prior PgUp is needed), and never forwards to the shell — it's pure view navigation.
            if mods.super_key() {
                let to_top = match key {
                    Key::Named(winit::keyboard::NamedKey::ArrowUp)
                    | Key::Named(winit::keyboard::NamedKey::Home) => Some(true),
                    Key::Named(winit::keyboard::NamedKey::ArrowDown)
                    | Key::Named(winit::keyboard::NamedKey::End) => Some(false),
                    _ => None,
                };
                if let Some(to_top) = to_top {
                    if to_top {
                        self.scroll_to_top();
                    } else {
                        self.scroll_to_bottom();
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
            }
            let scrolled_now = self
                .app
                .active_session()
                .map(|s| s.scrolled())
                .unwrap_or(false);
            if scrolled_now || matches!(key, Key::Named(winit::keyboard::NamedKey::PageUp)) {
                // Only the recognized scroll-navigation keys are consumed while scrolled. Any other
                // key (a letter, Enter, Space, a Left/Right arrow, Backspace, …) means the user
                // wants to type at the live cursor, so we jump back to the bottom and fall through
                // to forward the key below — exactly what Terminal.app / iTerm2 do. Before this, a
                // stray keypress while scrolled up was silently swallowed and never reached the
                // shell, so a command typed right after scrolling up simply vanished.
                let mut consumed = false;
                match key {
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::PageUp => {
                            scroll_active(self, 20);
                            if let Some(s) = self.app.active_session() {
                                s.set_scrolled(true);
                            }
                            consumed = true;
                        }
                        winit::keyboard::NamedKey::PageDown => {
                            scroll_active(self, -20);
                            consumed = true;
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            scroll_active(self, 1);
                            if let Some(s) = self.app.active_session() {
                                s.set_scrolled(true);
                            }
                            consumed = true;
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            scroll_active(self, -1);
                            consumed = true;
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.scroll_to_bottom();
                            consumed = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                // Snap scrolled state to reality once we hit bottom (offset no longer moves).
                if let Some(active) = self.app.active_session() {
                    if active.scrolled() {
                        let g = active.term.lock();
                        if g.grid().display_offset() == 0 {
                            active.set_scrolled(false);
                        }
                    }
                }
                if consumed {
                    return;
                }
                // A typing key while scrolled: snap to the live bottom, then forward it below.
                self.scroll_to_bottom();
            }

            if let Some(s) = self.app.active_session_mut() {
                // Copy selection: Cmd+C (mac convention) copies the current text selection to the
                // system clipboard and clears the highlight. Ctrl+C still goes to the session as
                // the interrupt byte unless a selection exists on mac.
                let is_copy = mods.super_key() && matches!(key, Key::Character(c) if c == "c");
                if is_copy {
                    // Copy without holding the session borrow: read the text, clear it, then store.
                    let (text, selected) = {
                        let g = s.term.lock();
                        (g.selection_to_string(), g.selection.is_some())
                    };
                    if selected {
                        if let Some(t) = text {
                            if !t.is_empty() {
                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                    let _ = cb.set_text(t);
                                }
                            }
                        }
                        s.term.lock().selection = None;
                    }
                    return;
                }
                // Paste clipboard: Ctrl+V (and Cmd+V, the mac convention) reads the system clipboard
                // and writes it to the session, bracketing with bracketed-paste if the app asked.
                let is_paste = (mods.control_key() && matches!(key, Key::Character(c) if c == "v"))
                    || (mods.super_key() && matches!(key, Key::Character(c) if c == "v"));
                if is_paste {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if let Ok(text) = cb.get_text() {
                            // Bracketed paste is negotiated; we always bracket to be safe with
                            // multi-line input (most modern shells/clis accept it).
                            let seq = format!("\x1b[200~{}\x1b[201~", text);
                            s.write(seq.as_bytes());
                        }
                    }
                    return;
                }
                match key {
                    Key::Character(c) => {
                        if mods.control_key() {
                            if let Some(b) = ctrl_byte(c) {
                                s.write(&[b]);
                            }
                        } else {
                            s.write(c.as_bytes());
                        }
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => s.write(b"\r"),
                        winit::keyboard::NamedKey::Backspace => s.write(b"\x7f"),
                        // Shift+Tab must arrive as the reverse-tab sequence (`ESC [ Z`), not a bare
                        // `\t` — dropping the modifier makes Claude Code / shells read it as a plain
                        // Tab and their back-cycling (Shift+Tab) shortcuts silently stop working.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => s.write(b"\x1b[Z"),
                        winit::keyboard::NamedKey::Tab => s.write(b"\t"),
                        winit::keyboard::NamedKey::Escape => s.write(b"\x1b"),
                        winit::keyboard::NamedKey::ArrowUp => s.write(arrow_seq(b'A', mods)),
                        winit::keyboard::NamedKey::ArrowDown => s.write(arrow_seq(b'B', mods)),
                        winit::keyboard::NamedKey::ArrowRight => s.write(arrow_seq(b'C', mods)),
                        winit::keyboard::NamedKey::ArrowLeft => s.write(arrow_seq(b'D', mods)),
                        winit::keyboard::NamedKey::Space => s.write(b" "),
                        other => {
                            // Home/End/forward-Delete/Insert/F-keys aren't used by the app and were
                            // previously dropped; forward the standard terminal sequences so they
                            // work in bash/readline/TUIs.
                            if let Some(seq) = extra_named_seq(&other) {
                                s.write(seq);
                            }
                        }
                    },
                    _ => {}
                }
            }
        }
    }

    /// Cheaply detect whether any pane produced new scrollback since we last rendered, by comparing
    /// each session's current history length against the baseline captured at the last render. Only
    /// consulted while the loop is idle; keeps the idle path near-0% CPU while still waking the
    /// instant a backgrounded agent prints. Refreshes the baseline here so repeated idle ticks don't
    /// re-fire on the same content.
    fn detect_content_change(&mut self) -> bool {
        let mut changed = false;
        let n = self.app.tabs.len();
        self.detect_len.resize(n, 0);
        self.content_sig.resize(n, 0);
        for (i, s) in self.app.tabs.iter().enumerate() {
            let h = s.history_len();
            if self.detect_len[i] != h {
                changed = true;
                self.detect_len[i] = h;
            }
            // The visible signature catches output that redraws the screen WITHOUT growing the
            // scrollback (a vim cursor move, an htop refresh, a TUI pane) — `history_len` alone
            // would leave the idle loop sleeping and the pane frozen on stale content.
            let sig = {
                let g = s.term.lock();
                crate::render::visible_signature(&g)
            };
            if self.content_sig[i] != sig {
                changed = true;
                self.content_sig[i] = sig;
            }
        }
        changed
    }

    /// Central repaint request. Native-tab mode repaints EVERY session window (each renders its own
    /// session); single-window mode repaints the one window. Every `request_redraw` site funnels
    /// through here so a new mode can't forget a window.
    fn request_redraw(&self) {
        if self.native_tabs {
            for h in &self.hosts {
                h.window.request_redraw();
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Create a real window hosting `tab`'s session (native-tab mode). Mirrors the geometry/config
    /// the single-window path uses. None on window/surface creation failure.
    fn create_host(&mut self, event_loop: &ActiveEventLoop, tab: usize) -> Option<Host> {
        let size = crate::restore::load_geometry()
            .map(|(w, h)| Size::Physical(PhysicalSize::new(w, h)))
            .unwrap_or(Size::Logical(LogicalSize::new(110.0, 34.0)));
        let attribs = winit::window::Window::default_attributes()
            .with_title("harness-terminal")
            .with_inner_size(size)
            .with_position({
                // Offset each tab window a little so the native tab bar has distinct siblings to
                // show; the first window lands where the user last parked it.
                let (x, y) = crate::restore::load_position().unwrap_or((200, 120));
                winit::dpi::Position::Physical(winit::dpi::PhysicalPosition::new(
                    x + 22 * tab as i32,
                    y + 18 * tab as i32,
                ))
            });
        let w = event_loop.create_window(attribs).ok()?;
        let wsize = w.inner_size();
        let w = Rc::new(w);
        crate::macos::tabs::enable_tabbing(&w);
        let context = Context::new(Rc::clone(&w)).ok();
        let surface = context
            .as_ref()
            .and_then(|c| Surface::new(c, Rc::clone(&w)).ok());
        w.request_redraw();
        Some(Host {
            window: w,
            _context: context,
            surface,
            size: wsize,
            tab,
            grouped: false,
            title: String::new(),
        })
    }

    /// Make `idx` (into `hosts`) the focused/looking-at window and keep `self.app.active` aligned to
    /// its session. A no-op when the index is unchanged.
    fn focus_host(&mut self, idx: usize) {
        if idx >= self.hosts.len() || idx == self.active_host {
            return;
        }
        self.active_host = idx;
        let tab = self.hosts[idx].tab;
        if tab < self.app.tabs.len() {
            self.app.active = tab;
            // Persist which session was focused on a real window click/switch, matching
            // `set_active`, so a relaunch reopens on the tab the user was last actually looking at
            // (not a stale earlier switch).
            crate::restore::save_active(self.app.active);
        }
        self.alias_active();
        // Each window may sit on a different display with a different backing scale; recompute the
        // font/grid metrics for the window we just switched to so text isn't stuck at the previous
        // display's density.
        self.metrics_from_scale();
        self.request_redraw();
    }

    /// Point `self.window`/`self.size` at the currently-looked-at host so every shared input/render
    /// path (key handling, cursor, title sync, metrics) keeps operating on the window the user is
    /// actually looking at. `self.context`/`self.surface` stay null in native mode — they only back
    /// the single-window `redraw()` path, which native mode never reaches.
    fn alias_active(&mut self) {
        if self.hosts.is_empty() {
            return;
        }
        let hi = self.active_host.min(self.hosts.len() - 1);
        self.window = Some(Rc::clone(&self.hosts[hi].window));
        self.size = self.hosts[hi].size;
        self.app.active = self.hosts[hi]
            .tab
            .min(self.app.tabs.len().saturating_sub(1));
    }

    /// Reconcile `hosts` with the session set (`app.tabs`): create a window for any new tab, drop a
    /// window whose tab closed, and keep `host.tab`/`active_host`/`app.active` aligned. Runs after
    /// any spawn/close so a native tab exists for every live session. Best-effort — a window that
    /// fails to create is skipped rather than aborting the app.
    fn sync_hosts(&mut self, event_loop: &ActiveEventLoop) {
        if !self.native_tabs {
            return;
        }
        if self.hosts.is_empty() {
            // First window: create one host per session (or a bare window with none yet).
            let n = self.app.tabs.len().max(1);
            for i in 0..n {
                if let Some(h) = self.create_host(event_loop, i) {
                    self.hosts.push(h);
                } else {
                    break;
                }
            }
            if self.hosts.is_empty() {
                return;
            }
            // Splice every sibling into the first window's tab group (AppKit `addTabbedWindow:`).
            let primary = self.hosts[0].window.clone();
            for h in self.hosts.iter_mut().skip(1) {
                crate::macos::tabs::join_tab_group(&primary, &h.window);
                h.grouped = true;
            }
            self.hosts[0].grouped = true;
            // Honor the active tab that was restored at startup (main.rs) rather than forcing tab
            // 0, so a relaunch with native tabs reopens focused on the session you left active.
            self.active_host = self.app.active.min(self.hosts.len().saturating_sub(1));
            self.alias_active();
            return;
        }
        // Keep exactly one window per session, but never drop to zero: a bare window stays up to
        // host the "no sessions" hint (and the New-Session overlay) until a session exists.
        let target = self.app.tabs.len().max(1);
        // A tab closed: drop any host beyond the session count.
        while self.hosts.len() > target {
            self.hosts.pop();
        }
        // A tab was added: extend hosts to match, splicing each new window into the group.
        let mut grew = false;
        while self.hosts.len() < target {
            let i = self.hosts.len();
            match self.create_host(event_loop, i) {
                Some(h) => {
                    let primary = self.hosts[0].window.clone();
                    crate::macos::tabs::join_tab_group(&primary, &h.window);
                    self.hosts.push(h);
                    let last = self.hosts.len() - 1;
                    self.hosts[last].grouped = true;
                    grew = true;
                }
                None => break,
            }
        }
        // Realign each host to its same-index session, then fix the active pointers.
        for (i, h) in self.hosts.iter_mut().enumerate() {
            h.tab = i;
        }
        self.active_host = self.active_host.min(self.hosts.len().saturating_sub(1));
        if self.app.active >= self.app.tabs.len() && !self.app.tabs.is_empty() {
            self.app.active = self.app.tabs.len().saturating_sub(1);
        }
        // A tab was created this pass (hosts grew): bring its window to front and make it the
        // visible native tab. Every tab-creation path (`spawn_local`/`spawn_remote`/`spawn_tunnel`
        // /attach/reopen/duplicate) already points `app.active` at the new session, so focusing the
        // active host selects the just-opened tab — matching iTerm2, where a new tab takes focus.
        // Guarded by `grew` so a steady-state `about_to_wait` never steals focus, and the startup
        // splash/restore branch above deliberately does NOT focus (it honors the restored tab).
        if grew {
            self.set_active(self.app.active);
        }
        self.alias_active();
    }

    /// Per-frame native-mode draw: repaint every session window into its own surface, presenting
    /// each, then re-point the shared aliases at the window the user is looking at.
    fn redraw_hosts(&mut self) {
        let n = self.hosts.len();
        if n == 0 {
            return;
        }
        // Native mode has no in-app chrome, so the per-frame activity pass (busy/bell/recover
        // detection + coalesced OS notifications + grew_delta/last_output sampling) must run here —
        // the single-window `render()` is what runs it when the in-app strip is shown. It also
        // updates `live_busy`, which drives the fast 60fps pump while an agent streams output
        // (without it native mode would pump at only ~8fps and fire no "done" notifications).
        let activity = self.activity_flags();
        self.live_busy = activity.iter().any(|&b| b);
        let active = self.active_host.min(n - 1);
        // Reuse one scratch framebuffer across every host this frame (resized per window), so
        // streaming with several sessions doesn't heap-allocate a fresh ~7MB buffer per window per
        // frame.
        let mut fb = std::mem::take(&mut self.scratch_fb);
        for i in 0..n {
            self.render_host_window(i, i == active, &mut fb);
        }
        self.scratch_fb = fb;
        self.alias_active();
    }

    /// Render one session window: its own session's grid full-bleed (no in-app chrome), plus the
    /// app-global overlays when this is the focused window. Presents into the host's own softbuffer
    /// surface. Renders into the shared `fb` (resized to this window) so the hot path reuses one
    /// allocation across all hosts.
    fn render_host_window(&mut self, i: usize, focused: bool, fb: &mut Framebuffer) {
        let (width, height) = {
            let h = &self.hosts[i];
            (h.size.width as usize, h.size.height as usize)
        };
        let (Some(w), Some(h)) = (
            NonZeroU32::new(width as u32),
            NonZeroU32::new(height as u32),
        ) else {
            return;
        };
        fb.resize(width, height);
        let bg = argb(255, self.colors.bg.0, self.colors.bg.1, self.colors.bg.2);
        for p in fb.pixels.iter_mut() {
            *p = bg;
        }
        self.frame = self.frame.wrapping_add(1);
        // Point the shared "active session" at this window's session for the duration of the frame.
        let tab = self.hosts[i].tab.min(self.app.tabs.len().saturating_sub(1));
        let prev = self.app.active;
        self.app.active = tab;
        // Each window can sit on a different-density display; render and size this window at its own
        // backing scale so a background tab on another screen isn't stuck at the focused window's
        // density (or re-flows when you focus it). The focused window's metrics equal the current
        // global ones, so this is a no-op for what you're looking at; a single-display setup is
        // entirely unaffected. Restore afterward so the global metrics always stay the focused
        // window's (overlays and subsequent focus switches read the right values).
        let host_scale = self.hosts[i].window.scale_factor() as f32 * self.zoom;
        let (saved_font, saved_cw, saved_ch) = (self.font_px, self.cell_w, self.cell_h);
        self.font_px = (self.base_font * host_scale).round().clamp(8.0, 40.0) as u32;
        self.cell_w = (8.0 * host_scale).round().max(2.0) as u32;
        self.cell_h = (18.0 * host_scale).round().max(1.0) as u32;
        self.render_host_grid(fb, width, height, focused);
        self.font_px = saved_font;
        self.cell_w = saved_cw;
        self.cell_h = saved_ch;
        self.app.active = prev;

        // Sync the OS window title to its session's live OSC title, falling back to the session's
        // identity (custom name, else engine@host) so separate agent tabs are distinguishable in the
        // system title-bar tab bar even before an engine announces a title. Only call set_title (a
        // platform round-trip) when the resolved title actually changed.
        let title = match self.app.tabs.get(tab) {
            Some(s) => match s.live_title() {
                Some(t) => format!("{t} — harness-terminal"),
                None => {
                    let label = s
                        .meta
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{}@{}", s.meta.engine, s.meta.host));
                    format!("{label} — harness-terminal")
                }
            },
            None => "harness-terminal".to_string(),
        };
        if self.hosts[i].title != title {
            self.hosts[i].window.set_title(&title);
            self.hosts[i].title = title;
        }

        // Present this window's own framebuffer.
        if let Some(surface) = &mut self.hosts[i].surface {
            let _ = surface.resize(w, h);
            if let Ok(mut buffer) = surface.buffer_mut() {
                for (dst, src) in buffer.iter_mut().zip(fb.pixels.iter()) {
                    *dst = *src;
                }
                let _ = buffer.present();
            }
        }
    }

    /// Draw a host's session into `fb` as a grid that fills the whole window (native tabs own the
    /// chrome, so there's no in-app tab bar or status line). When `focused`, the app-global overlays
    /// (palette, find, context menu, …) paint on top so they appear in the window you're using.
    fn render_host_grid(
        &mut self,
        fb: &mut Framebuffer,
        width: usize,
        height: usize,
        focused: bool,
    ) {
        let gline_px = self.cell_h as usize;
        let gcol_px = self.cell_w as usize;
        // Reserve the bottom cell for the native status strip (iTerm2-style) when enabled, across
        // every window so the terminal size is identical whether a tab is focused or not — no
        // resize/reflow churn when you flip between tabs. Turned off, the grid is full-bleed.
        let grid_lines = if self.native_status_bar {
            (height.max(2) / gline_px).saturating_sub(1).max(1)
        } else {
            (height.max(1) / gline_px).max(1)
        };
        let grid_cols = width.max(1) / gcol_px;

        // Size this window's session to its grid.
        if let Some(active) = self.app.active_session() {
            let g = active.term.lock();
            let (gl, gc) = (g.screen_lines(), g.columns());
            drop(g);
            if gl != grid_lines || gc != grid_cols {
                let size = TermSize {
                    lines: grid_lines.max(1),
                    cols: grid_cols.max(1),
                };
                active.resize(size);
            }
        }

        // Terminal grid.
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            let at_bottom = g.grid().display_offset() == 0;
            if !active.scrolled() && !at_bottom {
                use alacritty_terminal::grid::Scroll;
                g.grid_mut().scroll_display(Scroll::Bottom);
            }
            let sel = g.selection.as_ref().and_then(|s| s.to_range(&g));
            let copy = if self.copy_mode {
                Some(self.copy_pos)
            } else {
                None
            };
            draw_grid(
                fb,
                &g,
                self.cell_w,
                self.cell_h,
                self.font_px,
                &mut self.cache,
                &self.colors,
                self.find_hit,
                &self.find_all,
                sel.as_ref(),
                copy,
            );
        } else {
            let cy = (grid_lines / 2) * gline_px;
            let chord = self.prefix_chord();
            let hint = format!(
                "no sessions ·  Cmd+T  new tab   {chord} n  new   {chord} r  attach remote   {chord} /  palette "
            );
            let hw = draw_text(fb, &mut self.cache, &hint, 0, cy, self.font_px, CHROME_DIM);
            let cx = width.saturating_sub(hw) / 2;
            for py in cy.saturating_sub(self.font_px as usize)..(cy + self.font_px as usize) {
                for px in 0..width {
                    if py < height {
                        fb.pixels[py * width + px] = argb(0, 0, 0, 0);
                    }
                }
            }
            draw_text(fb, &mut self.cache, &hint, cx, cy, self.font_px, CHROME_DIM);
        }

        // Native status strip: this window's session info on the left, the fleet triage on the
        // right — the signal the in-app status line used to carry, now that the OS tab bar owns the
        // top chrome. Only drawn when enabled.
        if self.native_status_bar {
            self.draw_native_status(fb, width, height);
        }

        // Overlays paint only in the focused window.
        if focused {
            if self.app.overlay != Overlay::None {
                fb.dim(0.38);
            }
            match self.app.overlay {
                Overlay::Palette => self.render_palette(fb),
                Overlay::NewSession => self.render_new_session(fb),
                Overlay::RemoteAttach => self.render_remote(fb),
                Overlay::Find => self.render_find(fb),
                Overlay::FleetSearch => self.render_fleet_search(fb),
                Overlay::Fleet => self.render_fleet(fb),
                Overlay::Help => self.render_help(fb),
                Overlay::Rename => self.render_rename(fb),
                Overlay::Broadcast => self.render_broadcast(fb),
                Overlay::Peek => self.render_peek(fb),
                Overlay::FleetGrid => self.render_fleet_grid(fb),
                Overlay::CommandPalette => self.render_command_palette(fb),
                Overlay::Info => self.render_info(fb),
                Overlay::Hosts => self.render_hosts(fb),
                Overlay::None => {}
            }
            if self.ctx.is_some() {
                self.render_ctx_menu(fb);
            }
        }
    }

    /// Bottom status strip for a native host window (iTerm2-style). The OS title-bar tab bar owns
    /// the top chrome in native mode, so the in-app status line is gone; this puts the session's
    /// own identity + health back on the left and the fleet triage (down/busy/quiet/queued/DND) on
    /// the right, so a diver still sees at a glance whether machines went dark without leaving tabs.
    fn draw_native_status(&mut self, fb: &mut Framebuffer, width: usize, height: usize) {
        let gline_px = self.cell_h as usize;
        let top = height.saturating_sub(gline_px);
        fill_rect(fb, 0, top, width, gline_px, CHROME_BG);
        // Hairline separator, mirroring the single-window chrome's edge.
        fill_rect(fb, 0, top, width, 1, CHROME_HAIR);
        let base = height.saturating_sub(self.cell_h as usize / 2);

        // Left: this window's session — identity, health, and the reconnect reason if it's down.
        let info = match self.app.active_session() {
            Some(s) => {
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                let is_pty = s.kind() == "pty";
                let state = if is_pty {
                    String::new()
                } else if s.alive() {
                    "  ● live".to_string()
                } else {
                    let reason = s
                        .down_reason()
                        .unwrap_or_else(|| "reconnecting…".to_string());
                    let reason = clip_dots(&reason.trim().to_string(), 22);
                    let tag = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" ({reason})")
                    };
                    format!("  ○ down{tag}")
                };
                format!("{head}@{}  {}", s.meta.host, state)
            }
            None => "no session".to_string(),
        };
        draw_text(fb, &mut self.cache, &info, 6, base, self.font_px, CHROME_FG);

        // Right: fleet triage, only when non-zero (a fully healthy, idle fleet draws nothing).
        let down = self
            .app
            .tabs
            .iter()
            .filter(|t| !t.alive() && t.kind() != "pty")
            .count();
        let busy = self
            .activity_flags()
            .iter()
            .enumerate()
            .filter(|&(i, &b)| b && i != self.app.active)
            .count();
        let (any_quiet, quiet_n, _) = self.quiet_flags();
        let queued: usize = self.app.tabs.iter().map(|t| t.pending_bytes()).sum();
        let mut triage = String::new();
        if down > 0 {
            // Name the machine(s) when only a couple are down so a diver knows WHERE to look, not
            // just the count; beyond that the count is the signal (the hosts overview has the rest).
            if down <= 2 {
                let mut hosts: Vec<String> = Vec::new();
                for t in self
                    .app
                    .tabs
                    .iter()
                    .filter(|t| !t.alive() && t.kind() != "pty")
                {
                    let h = if t.meta.host.is_empty() {
                        "?".to_string()
                    } else {
                        t.meta.host.clone()
                    };
                    if !hosts.contains(&h) {
                        hosts.push(h);
                    }
                }
                let joined = clip_dots(&hosts.join(", "), 16);
                triage += &format!("↓{down} {joined} ");
            } else {
                triage += &format!("↓{down} ");
            }
        }
        if busy > 0 {
            triage += &format!("!{busy} ");
        }
        if queued > 0 {
            triage += &format!("⏳{queued} ");
        }
        if any_quiet {
            triage += &format!("⌛{quiet_n} ");
        }
        if self.dnd {
            triage += "🔕 ";
        }
        if !triage.is_empty() {
            let color = if down > 0 { CHROME_ERR } else { CHROME_DIM };
            let tw = triage.chars().count() * self.cell_w as usize;
            let x = width.saturating_sub(tw + 10);
            // Clear the right column before redrawing so nothing shows through.
            for py in base.saturating_sub(self.font_px as usize)..(base + self.font_px as usize) {
                for px in x..width {
                    if py < height {
                        fb.pixels[py * width + px] = CHROME_BG_PX;
                    }
                }
            }
            draw_text(fb, &mut self.cache, &triage, x, base, self.font_px, color);
        }

        // While the prefix is armed, show the waiting chip so a typed prefix chord is legible.
        if self.prefix_down {
            let chip = format!("  {} ", crate::keys::prefix_label(&self.prefix_key));
            let cw = text_width(&mut self.cache, &chip, self.font_px);
            let px0 = 64;
            let top_chip = base.saturating_sub(self.font_px as usize);
            fill_rect(
                fb,
                px0,
                top_chip,
                cw,
                self.font_px as usize + 4,
                CHROME_ACTIVE_BG,
            );
            draw_text(fb, &mut self.cache, &chip, px0, base, self.font_px, WHITE);
        }
    }

    /// Closing a native tab window: destroy that window + its session, realign indices, and exit the
    /// whole app when it was the last window. The OS × on the last tab quits (matching how the
    /// single-window close behaves) rather than leaving a zero-window app.
    fn close_native_tab(&mut self, hi: usize, event_loop: &ActiveEventLoop) {
        if hi >= self.hosts.len() {
            return;
        }
        if self.hosts.len() <= 1 {
            self.app.save_all_scrollbacks();
            crate::restore::save(&self.app.tab_specs());
            self.save_muted_state();
            crate::restore::save_geometry(self.size.width, self.size.height);
            event_loop.exit();
            return;
        }
        let tab = self.hosts[hi].tab;
        // Shut the session down (stash an undo spec) exactly as the in-app close does.
        if let Some(s) = self.app.tabs.get(tab) {
            self.app.last_closed = Some(crate::restore::TabSpec {
                kind: s.kind().to_string(),
                host: s.meta.host.clone(),
                engine: s.meta.engine.clone(),
                port: s.port(),
                session: s.attach_session.clone(),
                name: s.meta.name.clone(),
            });
        }
        self.forget_tab(tab);
        self.app.tabs.remove(tab);
        // Destroy the window (drops the last strong Rc → window closes).
        self.hosts.remove(hi);
        for h in self.hosts.iter_mut() {
            if h.tab > tab {
                h.tab -= 1;
            }
        }
        if self.app.active >= self.app.tabs.len() {
            self.app.active = self.app.tabs.len().saturating_sub(1);
        }
        if self.active_host >= self.hosts.len() {
            self.active_host = self.hosts.len().saturating_sub(1);
        }
        self.save_pin_state();
        crate::restore::save(&self.app.tab_specs());
        self.alias_active();
        // After closing the focused tab, hand keyboard + window focus to the (clamped) active host so
        // the next native tab actually becomes the key window — otherwise the OS can leave focus on
        // the just-closed window until the user clicks, mirroring the new-create focus fix above.
        if let Some(h) = self.hosts.get(self.active_host) {
            h.window.focus_window();
        }
        self.request_redraw();
    }

    /// Drop the native window for the session that was just closed at index `closed` (native-tab
    /// mode). The app-level tab removal has already happened; this keeps `hosts`/`active_host`/tab
    /// indices aligned with the shrunk session set. No-op outside native mode.
    fn native_remove_host(&mut self, closed: usize) {
        if !self.native_tabs || closed >= self.hosts.len() {
            return;
        }
        self.hosts.remove(closed);
        for h in self.hosts.iter_mut() {
            if h.tab > closed {
                h.tab -= 1;
            }
        }
        // A non-active window closing before the focused one shifts the focused host down too, so
        // keep focus on the same session. (Closing the focused host itself is handled by the clamp.)
        if closed < self.active_host {
            self.active_host -= 1;
        }
        if self.active_host >= self.hosts.len() {
            self.active_host = self.hosts.len().saturating_sub(1);
        }
        self.alias_active();
        self.request_redraw();
    }

    /// Move the broadcast overlay's recall cursor by `delta` slots (`-1` older, `+1` newer) and set
    /// the query to the recalled line. Wraps around the history; a fresh (unsaved) line sits above the
    /// newest entry and is yielded back to when stepping past the top. No-op with an empty history.
    /// Walk broadcast history with Shift+Up (older).
    fn hist_up(&mut self) {
        self.recall_broadcast(-1)
    }

    /// Walk broadcast history with Shift+Down (newer).
    fn hist_down(&mut self) {
        self.recall_broadcast(1)
    }

    fn recall_broadcast(&mut self, delta: isize) {
        let n = self.broadcast_hist.len();
        if n == 0 {
            return;
        }
        self.hist_sel = Some(recall_index(n, delta, self.hist_sel));
        self.broadcast_query = self.broadcast_hist[self.hist_sel.unwrap()].clone();
    }

    /// Return to the latest (live) view: clear the display offset and end scroll mode.
    fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            g.grid_mut().scroll_display(Scroll::Bottom);
            active.set_scrolled(false);
        }
    }

    /// `prefix+G`: jump straight to the very top of the active session's scrollback (the oldest
    /// line), pinning the view in history. The complement of `scroll_to_bottom` — a diver opening a
    /// long agent log can read the run from its start without paging through every screenful.
    fn scroll_to_top(&mut self) {
        use alacritty_terminal::grid::Scroll;
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            g.grid_mut().scroll_display(Scroll::Top);
            active.set_scrolled(true);
        }
    }

    /// A left-press on the scrollbar track jumps the view to that position (iTerm2-style), instead
    /// of selecting text in the rightmost column. Only active in the single-window chrome path (the
    /// scrollbar overlays the grid's right edge there); native-tab mode and focus mode are left
    /// alone. Returns true when the click was consumed as a scrollbar jump.
    fn scrollbar_click(&mut self, x: f64, y: f64) -> bool {
        if self.focus || self.native_tabs {
            return false;
        }
        let Some(active) = self.app.active_session() else {
            return false;
        };
        // The scrollbar occupies the far right few px of the grid; widen the hit zone a little so
        // a click aimed at it lands without needing pixel precision.
        if x < (self.size.width as f64) - 12.0 {
            return false;
        }
        let handled = {
            let mut g = active.term.lock();
            let hist = g.grid().history_size();
            if hist == 0 {
                false
            } else {
                let viewport = g.screen_lines().max(1);
                let track_h = (self.size.height as usize).max(1);
                let scrolled = crate::render::scroll_from_track_y(y, track_h, hist, viewport);
                use alacritty_terminal::grid::Scroll;
                let cur = g.grid().display_offset() as i32;
                g.grid_mut().scroll_display(Scroll::Delta(scrolled - cur));
                active.set_scrolled(true);
                true
            }
        };
        handled
    }

    /// Clear the "scrolled into history" pin once the view reaches the live bottom (offset 0), so a
    /// user who wheel-scrolled down to the latest line is back in live-follow and the status label
    /// stops claiming the pane is pinned in history. A no-op when the offset can't go lower (already
    /// at the bottom) or a PgUp/PgDn gesture is still mid-way through history.
    fn unpin_if_at_bottom(&mut self) {
        if let Some(active) = self.app.active_session() {
            let at_bottom = { active.term.lock().grid().display_offset() == 0 };
            if at_bottom {
                active.set_scrolled(false);
            }
        }
    }

    /// Map a framebuffer pixel position to the terminal-cell it lands on (viewing row 0 = the
    /// visually-top line, which with scrollback is history). Returns None if the point is outside
    /// the grid area (tab/status chrome or the right/left gutter).
    fn mouse_to_cell(&self, x: f64, y: f64) -> Option<Point> {
        let x = x as i64;
        let y = y as i64;
        // Grid area origin: below the tab bar (cell_h), above the status bar.
        let top = self.cell_h as i64;
        let bottom = self.size.height as i64 - self.cell_h as i64;
        if y < top || y >= bottom || x < 0 {
            return None;
        }
        let row = ((y - top) / self.cell_h as i64) as i64;
        let col = (x / self.cell_w as i64) as i64;
        if row < 0 || col < 0 {
            return None;
        }
        Some(Point::new(
            Line((row as usize).try_into().unwrap()),
            Column(col as usize),
        ))
    }

    /// Whether the focused session's PTY has requested SGR mouse reporting and the app isn't
    /// otherwise in a mode that owns the pointer (an overlay, the context menu, or copy mode). When
    /// true we forward mouse events to the PTY instead of driving our own selection/scroll, exactly
    /// like a real terminal. Also returns whether motion/drag reporting is on (a subset of the
    /// flags the caller needs). Pure read — strictly gated so the non-mouse path is untouched.
    fn mouse_report_flags(&self) -> Option<(bool, bool)> {
        if self.app.overlay != Overlay::None || self.ctx.is_some() || self.copy_mode {
            return None;
        }
        self.app.active_session().and_then(|s| {
            let m = s.term.lock();
            let mode = m.mode();
            if !(mode.contains(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE)) {
                return None;
            }
            let motion =
                mode.contains(TermMode::MOUSE_MOTION) || mode.contains(TermMode::MOUSE_DRAG);
            Some((true, motion))
        })
    }

    /// Forward a mouse button press/release as an SGR sequence to a mouse-mode PTY. Returns true
    /// when consumed (the app must not also select/scroll). Right-click is kept for the app's
    /// context menu unless Ctrl is held (the terminal convention), so the triage menu survives
    /// inside a mouse-using TUI; pointer-over-chrome is not forwarded.
    fn forward_mouse_button(&self, button: MouseButton, state: ElementState) -> bool {
        if self.mouse_report_flags().is_none() {
            return false;
        }
        if button == MouseButton::Right && !self.mods.control_key() {
            return false;
        }
        let Some(pt) = self.mouse_to_cell(self.cursor.0, self.cursor.1) else {
            return false;
        };
        let cb = sgr_button_code(button, state, &self.mods);
        let seq = sgr_mouse(
            cb,
            pt.column.0 + 1,
            pt.line.0 as usize + 1,
            state == ElementState::Released,
        );
        if let Some(s) = self.app.active_session() {
            s.write(&seq);
        }
        true
    }

    /// Forward pointer movement to a mouse-mode PTY when it asked for motion or drag reporting.
    /// Throttles to a new grid cell so a fast flick doesn't flood the pipe. Returns true (consumed)
    /// when a report was written.
    fn forward_mouse_motion(&mut self) -> bool {
        let Some((_, motion)) = self.mouse_report_flags() else {
            return false;
        };
        if !motion {
            return false;
        }
        let Some(pt) = self.mouse_to_cell(self.cursor.0, self.cursor.1) else {
            return false;
        };
        let (col, row) = (pt.column.0 + 1, pt.line.0 as usize + 1);
        if self.last_motion_cell == Some((col, row)) {
            return true; // consumed but no duplicate report needed
        }
        self.last_motion_cell = Some((col, row));
        let cb = sgr_motion_code(self.mouse_left_down, &self.mods);
        let seq = sgr_mouse(cb, col, row, false);
        if let Some(s) = self.app.active_session() {
            s.write(&seq);
        }
        true
    }

    /// Forward a scroll-wheel notch to a mouse-mode PTY as wheel-up (64) / wheel-down (65). Returns
    /// true when consumed (the app must not scroll the scrollback instead).
    fn forward_mouse_wheel(&self, mag: f64) -> bool {
        if self.mouse_report_flags().is_none() {
            return false;
        }
        let Some(pt) = self.mouse_to_cell(self.cursor.0, self.cursor.1) else {
            return false;
        };
        let cb = sgr_wheel_code(mag, &self.mods);
        let seq = sgr_mouse(cb, pt.column.0 + 1, pt.line.0 as usize + 1, false);
        if let Some(s) = self.app.active_session() {
            s.write(&seq);
        }
        true
    }

    /// Which tab the pointer is over in the tab bar (if any). Mirrors the render loop's label x
    /// positions so the preview tooltip lines up with the painted labels: starts at x=6, each label
    /// advances by its drawn width + 12, and the bar stops at `width - 20` the same way. Only the
    /// top chrome row counts (y within the tab bar and below it); focus mode has no bar.
    fn tab_at(&mut self, x: f64, y: f64) -> Option<usize> {
        if self.focus {
            return None;
        }
        // The tab bar occupies the first cell row, vertically centered in it.
        if y < 0.0 || y >= self.chrome_top() as f64 || x < 6.0 {
            return None;
        }
        // Prefer the rects recorded by the last paint so hit-testing matches the visible bar
        // exactly (badges/busy spin included); fall back to the cheap geometric estimate below
        // before the first frame is drawn.
        if self.tab_rects.len() == self.app.tabs.len() && !self.tab_rects.is_empty() {
            for (i, &(x0, x1)) in self.tab_rects.iter().enumerate() {
                let fx = x as usize;
                if fx >= x0 && fx < x1 {
                    return Some(i);
                }
            }
            return None;
        }
        let mut cx = 6i64;
        for (i, s) in self.app.tabs.iter().enumerate() {
            // A stable, cheap label (dot + tail of live title + head + status dot) for hit-testing;
            // same font as the painted bar so the hover zone tracks the visible tab. Busy/bell badges
            // shift a tab's painted edge by a few glyphs, which barely moves where the preview pops,
            // so they're omitted here for speed and stability.
            let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
            let mut live = live.replace('\n', " ");
            if live.chars().count() > 18 {
                live = live.chars().take(18).collect::<String>() + "…";
            }
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let label = format!(" ○ {} {} ○ ", head, live);
            // Measure the label width with the glyph cache (same font advance as render).
            let mut w = 0i64;
            for ch in label.chars() {
                w += self.cache.glyph(ch, self.font_px, false).0 as i64;
            }
            if x >= cx as f64 && x < (cx + w + 12) as f64 {
                return Some(i);
            }
            cx += w + 12;
            if cx > (self.size.width as i64).saturating_sub(20) {
                break;
            }
        }
        None
    }

    /// Is the pointer over a tab's close × (the rightmost ~14px of a hovered tab's painted rect)?
    /// Returns the tab index. `tab_at` has already decided the pointer is in the tab bar.
    fn tab_close_at(&self, x: f64, y: f64) -> Option<usize> {
        let hi = self.hover_tab?;
        // The close × only lives inside the tab bar's own row; a click lower in the window must
        // never be treated as a tab close even if the tracker still holds a stale hover tab.
        if y < 0.0 || y >= self.chrome_top() as f64 {
            return None;
        }
        let (_, x1) = *self.tab_rects.get(hi)?;
        let fx = x as usize;
        if fx >= x1.saturating_sub(14) && fx < x1 {
            Some(hi)
        } else {
            None
        }
    }

    /// Does a URL begin/overlap the cell at framebuffer (x, y)? Used to show the hand cursor over
    /// links, mirroring exactly what `mouse_open` would open so the affordance never lies. Cheap: no
    /// grid write, no selection — just a read of the row under the pointer.
    fn cell_has_link(&self, x: f64, y: f64) -> bool {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return false;
        };
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return false;
        }
        let Some(active) = self.app.active_session() else {
            return false;
        };
        let g = active.term.lock();
        let row = pt.line.0 as usize;
        let cols = g.columns();
        if row >= g.screen_lines() || cols == 0 {
            return false;
        }
        let col = (pt.column.0 as usize).min(cols - 1);
        let line_text: String = g.grid()[Line(row as i32)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        drop(g);
        crate::links::url_span(&line_text, col)
            .map(|s| !s.as_str(&line_text).is_empty())
            .unwrap_or(false)
    }

    /// Detect how many clicks this press represents. A press within ~250ms and ~4px of the previous
    /// one increments the count (up to 3); anything else resets to 1.
    fn click_count(&mut self, x: f64, y: f64) -> u32 {
        const THRESHOLD_MS: u64 = 250;
        const THRESHOLD_PX: f64 = 4.0;
        let now = std::time::Instant::now();
        let click = if let Some((prev_at, (px, py), prev_count)) = self.last_press {
            let dt = now.duration_since(prev_at).as_millis() as u64;
            let dist = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
            if dt < THRESHOLD_MS && dist < THRESHOLD_PX {
                (prev_count + 1).min(3)
            } else {
                1
            }
        } else {
            1
        };
        self.last_press = Some((now, (x, y), click));
        click
    }

    /// Start a text selection at a pressed cursor position, or clear the selection when clicking.
    /// Single click = simple drag; double click = expand to a word (semantic); triple = whole line.
    /// Alt+click: reposition the shell/readline cursor at the clicked cell. We report the cursor
    /// position with the standard "active position report" sequence (`ESC [ row ; col R`), which
    /// line editors that support click-to-move (zsh, fish, and readline via inputrc) honor.
    fn mouse_alt_click(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        // Only meaningful against live (unscrolled) screen coordinates; report 1-based.
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return;
        }
        let seq = format!("\x1b[{};{}R", pt.line.0 + 1, pt.column.0 + 1);
        if let Some(active) = self.app.active_session() {
            active.write(seq.as_bytes());
        }
    }

    /// Cmd/Ctrl+click: jump to whatever is under the cursor. A URL opens in the default browser; a
    /// relative path opens in a text editor via `open` (macOS). Reads the word containing the
    /// clicked cell by expanding to the nearest whitespace/bracket boundaries on the grid row.
    /// Best-effort — a click on non-text just does nothing.
    fn mouse_open(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return;
        }
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let row = pt.line.0 as usize;
        let cols = g.columns();
        if row >= g.screen_lines() {
            return;
        }
        let col = (pt.column.0 as usize).min(cols - 1);
        // Read the whole visible row and expand left/right from the click to word boundaries.
        let line_text: String = g.grid()[Line(row as i32)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        drop(g);
        // Prefer the detected URL at the clicked cell; fall back to the historical "word under the
        // cursor" (which covers relative file paths like `src/main.rs`).
        let target = crate::links::url_span(&line_text, col)
            .map(|s| s.as_str(&line_text).to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| expand_click_word(&line_text, col).to_owned());
        if target.is_empty() {
            return;
        }
        // Shell it through `open`: macOS routes `http(s)://…` to the browser and a relative path to
        // a text editor (XDG on Linux needs a different incantation; we're the mac build today).
        let _ = std::process::Command::new("open").arg(&target).spawn();
    }

    fn mouse_press(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            // Click outside the grid (on chrome) clears any selection.
            if let Some(active) = self.app.active_session() {
                let mut g = active.term.lock();
                g.selection = None;
            }
            self.mouse_anchor = None;
            self.last_press = None;
            return;
        };
        let clicks = self.click_count(x, y);
        self.mouse_anchor = Some(pt);
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            let sel = match clicks {
                // Double-click: word selection drives semantic expansion off a non-empty range (a
                // distinct end point keeps bracket-search from hijacking a single cell).
                2 => {
                    let s = Selection::new(SelectionType::Semantic, pt, Side::Left);
                    let mut s2 = s;
                    s2.update(Point::new(pt.line + Line(1), Column(0)), Side::Right);
                    s2
                }
                3 => Selection::new(SelectionType::Lines, pt, Side::Left),
                // Single click: plain drag-select; on the same cell it just anchors.
                _ => Selection::new(SelectionType::Simple, pt, Side::Left),
            };
            g.selection = Some(sel);
        }
    }

    /// Grow the drag selection to the cursor's current cell while the button is held.
    fn mouse_drag(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            if let Some(sel) = g.selection.as_mut() {
                sel.update(pt, Side::Right);
            }
        }
    }

    /// The user released the button: finalize the drag (if any) and copy the selection to the
    /// system clipboard, keeping it highlighted until the next click (standard terminal behavior).
    fn mouse_release(&mut self, x: f64, y: f64) {
        if self.mouse_anchor.is_some() {
            self.mouse_drag(x, y);
            self.mouse_anchor = None;
            self.copy_selection();
        }
    }

    /// Drag the hovered tab: as the pointer crosses a neighbor's midpoint, live-swap the dragged tab
    /// with it so the bar reflows under the cursor (standard tab-drag feel). `drag_tab` holds the
    /// current source slot; each swap keeps the dragged session under the pointer.
    fn tab_drag(&mut self, x: f64) {
        let src = match self.drag_tab {
            Some(i) => i,
            None => return,
        };
        if self.app.tabs.len() < 2 {
            return;
        }
        // Destination is the tab currently hovered (by the same hit-test the press used).
        let Some(dst) = self.tab_at(x, 4.0) else {
            return;
        };
        if dst == src {
            return;
        }
        self.app.move_tab_from_to(src, dst);
        self.reorder_parallel(src, dst);
        // The dragged session landed at final index `dst`; resume dragging it from there so a
        // further move keeps the same session under the pointer.
        self.drag_tab = Some(dst);
    }

    /// Land a drag: if the tab moved from where it was pressed, leave it reordered; if it didn't
    /// move, treat the release as a click that switches to that tab. Always clears the drag state.
    /// The `from` slot is recovered as the tab the press started on via `drag_tab`; we track the
    /// original so a no-move release still switches.
    fn tab_drop(&mut self, x: f64, y: f64) {
        if let Some(i) = self.drag_tab.take() {
            // A plain click on a tab (press→release over the same tab) switches to it.
            if self.tab_at(x, y) == Some(i) {
                self.set_active(i);
            }
        }
        // Persist the (possibly re-arranged) tab order once the drag/click settles, so a
        // drag-to-reorder survives a relaunch like the `{`/`}` brace path.
        self.persist_tabs();
    }

    /// Copy the active session's text selection to the system clipboard. No-op when empty.
    fn copy_selection(&mut self) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let Some(text) = g.selection_to_string() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
    }

    /// Open the right-click context menu at the pointer, populated contextually for the current
    /// tab (plus "Open Link" / "Search for Selection" only when they apply).
    fn open_context_menu(&mut self, x: f64, y: f64) {
        if self.app.active_session().is_none() {
            return;
        }
        let sel = self.selected_text();
        let over_link = self.cell_has_link(x, y);
        let mut items = Vec::new();
        items.push(CtxAction::Copy);
        items.push(CtxAction::Paste);
        if over_link {
            items.push(CtxAction::OpenLink);
        }
        items.push(CtxAction::SelectAll);
        if !sel.is_empty() {
            items.push(CtxAction::SearchSelection);
        }
        items.push(CtxAction::Separator);
        // Fleet actions reachable by right-click so a diver can act on the hovered tab without
        // reaching for the prefix chord. Reconnect only appears for a truly down (non-PTY) pane.
        items.push(CtxAction::Interrupt);
        if self
            .app
            .active_session()
            .map(|s| !s.alive() && s.kind() != "pty")
            .unwrap_or(false)
        {
            items.push(CtxAction::Reconnect);
        }
        items.push(CtxAction::Duplicate);
        items.push(CtxAction::MuteToggle);
        items.push(CtxAction::Separator);
        items.push(CtxAction::NewSession);
        items.push(CtxAction::CloseTab);

        let row_h = (self.cell_h as usize).max(20);
        let sep_h = row_h / 2;
        let pad = 12;
        // Panel width = widest label + padding; height = one row per action + a half-row per divider.
        let mut w = 0usize;
        let mut h = 0usize;
        let mut first_sel = None;
        for (i, &a) in items.iter().enumerate() {
            if a == CtxAction::Separator {
                h += sep_h;
            } else {
                let lw = text_width(&mut self.cache, ctx_label(a), self.font_px);
                w = w.max(lw);
                h += row_h;
                if first_sel.is_none() {
                    first_sel = Some(i);
                }
            }
        }
        w += pad * 2;
        let vw = self.size.width as usize;
        let vh = self.size.height as usize;
        let px = (x as usize).saturating_sub(4).min(vw.saturating_sub(w));
        let py = (y as usize).saturating_sub(4).min(vh.saturating_sub(h));
        self.ctx = Some(CtxMenu {
            px,
            py,
            w,
            items,
            sel: first_sel.unwrap_or(0),
            mx: x,
            my: y,
        });
    }

    /// The active session's current text selection ("" when none / empty).
    fn selected_text(&self) -> String {
        let Some(active) = self.app.active_session() else {
            return String::new();
        };
        let g = active.term.lock();
        g.selection_to_string().unwrap_or_default()
    }

    /// Move the context-menu keyboard selection by `delta` (±1), skipping divider rows.
    fn ctx_navigate(&mut self, delta: isize) {
        let Some(menu) = self.ctx.as_ref() else {
            return;
        };
        let n = menu.items.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = menu.sel as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if menu.items[i as usize] != CtxAction::Separator {
                self.ctx.as_mut().unwrap().sel = i as usize;
                return;
            }
        }
    }

    /// Return the action whose row contains the given pixel position, or None.
    fn ctx_action_at(&self, x: f64, y: f64) -> Option<CtxAction> {
        let menu = self.ctx.as_ref()?;
        let row_h = (self.cell_h as usize).max(20);
        let sep_h = row_h / 2;
        let (px, py) = (menu.px as i64, menu.py as i64);
        let (mx, my) = (x as i64, y as i64);
        if mx < px || mx >= px + menu.w as i64 || my < py {
            return None;
        }
        let mut yy = py;
        for &a in &menu.items {
            if a == CtxAction::Separator {
                yy += sep_h as i64;
                continue;
            }
            if my >= yy && my < yy + row_h as i64 {
                return Some(a);
            }
            yy += row_h as i64;
        }
        None
    }

    /// Run a context-menu action. "Close Tab" mirrors the `prefix+x` handler so pin-guarding and
    /// persistence behave identically.
    fn run_ctx_action(&mut self, a: CtxAction) {
        match a {
            CtxAction::Copy => {
                self.copy_selection();
                self.flash = Some(("copied selection".to_string(), std::time::Instant::now()));
            }
            CtxAction::Paste => self.paste_clipboard(),
            CtxAction::OpenLink => {
                let (mx, my) = self
                    .ctx
                    .as_ref()
                    .map(|c| (c.mx, c.my))
                    .unwrap_or((0.0, 0.0));
                self.mouse_open(mx, my);
            }
            CtxAction::SelectAll => {
                self.select_all();
            }
            CtxAction::SearchSelection => self.search_selection(),
            CtxAction::Separator => {}
            CtxAction::Interrupt => {
                self.interrupt_active();
                self.flash = Some(("interrupted".to_string(), std::time::Instant::now()));
            }
            CtxAction::Reconnect => self.reconnect_active(),
            CtxAction::Duplicate => {
                self.duplicate_active_preserving_pin();
                self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
            }
            CtxAction::MuteToggle => self.toggle_mute_active(),
            CtxAction::NewSession => {
                // Match Cmd+T / palette: pre-select the default engine and pre-fill the last repo
                // so the picker is predictable no matter how it's opened.
                self.app.overlay = Overlay::NewSession;
                self.app.select_default_engine();
                self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
            }
            CtxAction::CloseTab => {
                let pin = self.pinned.get(self.app.active).copied().unwrap_or(false);
                let closed = self.app.active;
                let closed_ok = close_tab(&mut self.app, pin);
                if !closed_ok && pin {
                    self.flash = Some((
                        "🔒 pinned — prefix A to unpin first".to_string(),
                        std::time::Instant::now(),
                    ));
                }
                if closed_ok {
                    self.forget_tab(closed);
                    if self.native_tabs {
                        self.native_remove_host(closed);
                    }
                }
                self.save_pin_state();
            }
        }
    }

    /// Select the entire scrollback (history + visible) as one terminal selection, then copy it —
    /// the standard "Select All" terminal gesture. No visible motion cue beyond the highlight.
    fn select_all(&mut self) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        use alacritty_terminal::index::{Column, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let (top, bottom) = {
            let g = active.term.lock();
            let grid = g.grid();
            (grid.topmost_line(), grid.bottommost_line())
        };
        let cols_last = {
            let g = active.term.lock();
            Column(g.columns().saturating_sub(1))
        };
        let mut g = active.term.lock();
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(top, Column(0)),
            Side::Left,
        );
        sel.update(Point::new(bottom, cols_last), Side::Right);
        g.selection = Some(sel);
        drop(g);
        self.copy_selection();
        self.flash = Some((
            "selected + copied entire scrollback".to_string(),
            std::time::Instant::now(),
        ));
    }

    /// Open the search overlay with the current selection as the query, jumping to the first match.
    fn search_selection(&mut self) {
        let sel = self.selected_text();
        if sel.is_empty() {
            return;
        }
        self.find_query = sel;
        self.find_hit = None;
        self.find_all = Vec::new();
        self.app.selected = 0;
        self.app.overlay = Overlay::Find;
        self.find_recompute(None);
    }

    /// Drop tab-parallel bookkeeping for index `i` before the tab itself is removed, so every
    /// vector that's indexed by tab stays in lockstep with `tabs` (a later frame's `resize(n, …)`
    /// only truncates the tail and would otherwise leave a shifted stale flag at `i`).
    fn forget_tab(&mut self, i: usize) {
        // Closing/reordering any tab shifts the remaining indices, so the stored tmux-style
        // "previous tab" index (`prefix+l`) would silently point at a different session after the
        // removal. Drop it here — every close path funnels through this method — and let the next
        // `set_active` re-record which tab was actually current. A plain focus switch never calls
        // this, so the flip-back feature is unaffected.
        self.last_active = None;
        // Drop the slot from every tab-parallel vector, split by element type (Rust arrays are
        // homogeneous, so each type is removed in its own pass).
        if i < self.seen_history.len() {
            self.seen_history.remove(i);
        }
        if i < self.grew_delta.len() {
            self.grew_delta.remove(i);
        }
        if i < self.unread.len() {
            self.unread.remove(i);
        }
        for v in [
            &mut self.notified,
            &mut self.muted,
            &mut self.pinned,
            &mut self.grid_marks,
            &mut self.broadcast_targets,
            &mut self.was_down,
            &mut self.was_alive,
        ] {
            if i < v.len() {
                v.remove(i);
            }
        }
        for v in [&mut self.bell_until, &mut self.recover_until] {
            if i < v.len() {
                v.remove(i);
            }
        }
        if i < self.last_output.len() {
            self.last_output.remove(i);
        }
        if i < self.detect_len.len() {
            self.detect_len.remove(i);
        }
        if i < self.content_sig.len() {
            self.content_sig.remove(i);
        }
        // Stale pointer state referencing the removed slot is reset so it can't dangle.
        self.hover_tab = None;
        self.drag_tab = None;
        self.tooltip_box = None;
    }

    /// Close the tab at an arbitrary index (not just the active one), used by the tab-bar close ×.
    /// Honors the pin guard, stashes an undo spec, and keeps every parallel bookkeeping vector in
    /// sync. Returns true when a tab was actually closed.
    fn close_tab_at(&mut self, i: usize) -> bool {
        if i >= self.app.tabs.len() {
            return false;
        }
        if self.pinned.get(i).copied().unwrap_or(false) {
            self.flash = Some((
                "🔒 pinned — prefix A to unpin first".to_string(),
                std::time::Instant::now(),
            ));
            return false;
        }
        if let Some(s) = self.app.tabs.get(i) {
            self.app.last_closed = Some(crate::restore::TabSpec {
                kind: s.kind().to_string(),
                host: s.meta.host.clone(),
                engine: s.meta.engine.clone(),
                port: s.port(),
                session: s.attach_session.clone(),
                name: s.meta.name.clone(),
            });
        }
        self.forget_tab(i);
        self.app.tabs.remove(i);
        if self.native_tabs {
            // Native mode: drop the matching window host too, or the closed session's window would
            // linger as a native tab with nothing behind it. `native_remove_host` also re-derives
            // the active session from the now-focused window (like `close_quiet_tabs`), so the
            // manual single-window re-anchor below is skipped here.
            self.native_remove_host(i);
        } else if self.app.active == i {
            self.app.active = self.app.active.min(self.app.tabs.len().saturating_sub(1));
        } else if self.app.active > i {
            self.app.active -= 1;
        }
        self.save_pin_state();
        crate::restore::save(&self.app.tab_specs());
        true
    }

    /// Keyboard handling while the context menu is open: Escape dismisses, Enter runs, j/k/arrows
    /// navigate, and any other key dismisses the menu and lets the keystroke fall through.
    fn handle_ctx_key(&mut self, key: &Key, mods: &ModifiersState) -> bool {
        if self.app.overlay != Overlay::None || self.ctx.is_none() {
            return false;
        }
        let mut handled = true;
        match key {
            Key::Named(winit::keyboard::NamedKey::Escape) => {
                self.ctx = None;
            }
            Key::Named(winit::keyboard::NamedKey::Enter) => {
                let act = self.ctx.as_ref().and_then(|m| m.items.get(m.sel).copied());
                if let Some(a) = act {
                    self.run_ctx_action(a);
                }
                self.ctx = None;
            }
            Key::Named(winit::keyboard::NamedKey::ArrowUp) => self.ctx_navigate(-1),
            Key::Named(winit::keyboard::NamedKey::ArrowDown) => self.ctx_navigate(1),
            Key::Character(c) if c == "k" => self.ctx_navigate(-1),
            Key::Character(c) if c == "j" => self.ctx_navigate(1),
            _ => {
                // Any other key: dismiss the menu so normal typing / shortcut flow resumes.
                self.ctx = None;
                handled = false;
            }
        }
        let _ = mods;
        handled
    }

    /// Draw the right-click context menu popover on top of everything.
    fn render_ctx_menu(&mut self, fb: &mut Framebuffer) {
        let Some(menu) = self.ctx.as_ref() else {
            return;
        };
        let row_h = (self.cell_h as usize).max(20);
        let sep_h = row_h / 2;
        let (px, py, w) = (menu.px, menu.py, menu.w);
        // Compute panel height to know where to stop the backdrop.
        let mut ph = 0usize;
        for &a in &menu.items {
            ph += if a == CtxAction::Separator {
                sep_h
            } else {
                row_h
            };
        }
        // Backdrop + 1px border (slightly lighter than the tab bar so it floats above the grid).
        fill_rect(fb, px, py, w, ph, CHROME_BG);
        let border = (0x3a, 0x3a, 0x44);
        for dx in 0..w {
            fb.set(px + dx, py, argb(255, border.0, border.1, border.2));
            if ph > 1 {
                fb.set(
                    px + dx,
                    py + ph - 1,
                    argb(255, border.0, border.1, border.2),
                );
            }
        }
        for dy in 0..ph {
            fb.set(px, py + dy, argb(255, border.0, border.1, border.2));
            if w > 1 {
                fb.set(px + w - 1, py + dy, argb(255, border.0, border.1, border.2));
            }
        }
        let mut yy = py;
        let text_pad = 12;
        for (i, &a) in menu.items.iter().enumerate() {
            if a == CtxAction::Separator {
                // Divider: a mid-gray hairline inset from the panel edges.
                let ly = yy + sep_h / 2;
                for xx in (px + 4)..(px + w - 4) {
                    fb.set(xx, ly, argb(255, 0x33, 0x33, 0x3d));
                }
                yy += sep_h;
                continue;
            }
            let sel = i == menu.sel;
            if sel {
                fill_rect(fb, px, yy, w, row_h, CHROME_ACTIVE_BG);
            }
            let color = if sel { WHITE } else { CHROME_FG };
            draw_text(
                fb,
                &mut self.cache,
                ctx_label(a),
                px + text_pad,
                yy + row_h / 2,
                self.font_px,
                color,
            );
            yy += row_h;
        }
    }
}

/// A stable accent color for a host string, so every tab pointing at the same machine (and the
/// details pane) shares a hue and a fleet diver can tell at a glance which host a tab is on.
/// Deterministic — same host always yields the same color, across sessions and restarts.
/// The engine's own accent color as an RGB tuple, for the inactive-tab label. This is the deliberate
/// "which engine is this" signal (a brand color from the engine table), complementing `host_color`
/// which tells you *which machine*. A `[theme.accents]` override for the engine wins; otherwise the
/// built-in brand accent. Unknown engines fall back to the neutral chrome dim.
fn engine_accent(
    engine: &str,
    accents: &std::collections::BTreeMap<String, (u8, u8, u8)>,
) -> (u8, u8, u8) {
    if let Some(c) = accents.get(engine) {
        return *c;
    }
    static CACHE: std::sync::OnceLock<std::collections::HashMap<&'static str, (u8, u8, u8)>> =
        std::sync::OnceLock::new();
    let map = CACHE.get_or_init(|| {
        ENGINES
            .iter()
            .map(|e| (e.id, argb_to_rgb(e.color)))
            .collect()
    });
    map.get(engine).copied().unwrap_or(CHROME_DIM)
}

fn argb_to_rgb(argb: u32) -> (u8, u8, u8) {
    (
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
    )
}

/// Map a fleet tile's status to its accent color, used to tint the focused tile's header/border so
/// the war-room is scannable at a glance (down=red, busy=amber, quiet=blue, reconnecting=green).
/// Precedence is down > busy > quiet so a dark pane is never mistaken for merely busy; a session
/// flagged as both busy and awaiting you reads as busy. Local PTYs have no transport and report no
/// down/quiet state, so they return the neutral `None`. Pure so the ordering rules are unit-tested.
fn status_accent(is_down: bool, busy: bool, quiet: bool, recovering: bool) -> Option<(u8, u8, u8)> {
    if is_down {
        Some(CHROME_ERR)
    } else if busy {
        Some(CHROME_BUSY)
    } else if quiet {
        Some(CHROME_QUIET)
    } else if recovering {
        Some(CHROME_RECOVER)
    } else {
        None
    }
}

fn host_color(host: &str) -> (u8, u8, u8) {
    // FNV-1a over the host; pick a hue from the warm-to-cool range and keep it readable on black.
    let h = host.bytes().fold(0x811c_9dc5u32, |acc, b| {
        (acc ^ b as u32).wrapping_mul(0x0100_0193)
    });
    // 32 hues across the spectrum, OSV below is luminance-boosted so text reads on near-black.
    let hue = (h >> 4) % 32;
    const TABLE: [(u8, u8, u8); 32] = [
        (0xe0, 0x5b, 0x5b),
        (0xe0, 0x8b, 0x5b),
        (0xe0, 0xb8, 0x5b),
        (0xbf, 0xe0, 0x5b),
        (0x8b, 0xe0, 0x5b),
        (0x5b, 0xe0, 0x8b),
        (0x5b, 0xe0, 0xbf),
        (0x5b, 0xdd, 0xe0),
        (0x5b, 0xa8, 0xe0),
        (0x5b, 0x74, 0xe0),
        (0x8b, 0x5b, 0xe0),
        (0xbf, 0x5b, 0xe0),
        (0xe0, 0x5b, 0xd0),
        (0xe0, 0x5b, 0x9b),
        (0x9b, 0x7b, 0x5b),
        (0x9b, 0x9b, 0x5b),
        (0x5b, 0x9b, 0x7b),
        (0x5b, 0x7b, 0x9b),
        (0x7b, 0x5b, 0x9b),
        (0x9b, 0x5b, 0x7b),
        (0xf7, 0x9e, 0x8b),
        (0xf7, 0xbe, 0x8b),
        (0xd9, 0xf7, 0x8b),
        (0xa7, 0xf7, 0x8b),
        (0x8b, 0xf7, 0xa7),
        (0x8b, 0xf7, 0xd9),
        (0x8b, 0xdd, 0xf7),
        (0x8b, 0xa7, 0xf7),
        (0xa7, 0x8b, 0xf7),
        (0xd9, 0x8b, 0xf7),
        (0xf7, 0x8b, 0xd9),
        (0xf7, 0x8b, 0xa7),
    ];
    TABLE[(hue % 32) as usize]
}

/// Expand the word containing byte index `col` in a line of text, growing to whitespace/bracket
/// boundaries on both sides. Returns the substring (may be empty if `col` sits on a boundary).
fn expand_click_word(line: &str, col: usize) -> &str {
    let bytes = line.as_bytes();
    let col = col.min(bytes.len());
    let is_boundary = |b: u8| {
        b.is_ascii_whitespace()
            || matches!(b, b'(' | b')' | b'"' | b'\'' | b'<' | b'>' | b'[' | b']')
    };
    let mut start = col;
    while start > 0 && !is_boundary(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < bytes.len() && !is_boundary(bytes[end]) {
        end += 1;
    }
    &line[start..end]
}

/// Map a typed letter to its control byte (a-z → 1-26), for Ctrl+key.
fn ctrl_byte(c: &str) -> Option<u8> {
    let ch = c.chars().next()?;
    let l = ch.to_ascii_lowercase();
    if ('a'..='z').contains(&l) {
        Some(l as u8 - b'a' + 1)
    } else {
        None
    }
}

fn broadcast_bytes(q: &str) -> Vec<u8> {
    if q.is_empty() {
        Vec::new()
    } else {
        format!("{}\n", q).into_bytes()
    }
}

fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Truncate `s` to at most `max` chars, appending "…" when it was cut, so long status strings
/// (e.g. a remote reconnect reason) fit a bounded panel without overflowing the window. Pure so
/// it is unit-testable; counts chars, not raw bytes, so multi-byte glyphs survive intact.
fn clip_dots(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else if max == 0 {
        "…".to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Format the copy-mode-search "no match" flash text for the given live query. A query that
/// matched nothing is otherwise silent (`n`/`N`/Enter just leave the cursor put), so the miss is
/// surfaced; blank/whitespace queries get the generic message. Pure so it's easy to unit-test.
fn copy_no_match_flash(query: &str) -> String {
    let q = clip_dots(query.trim(), 40);
    if q.is_empty() {
        "copy: no match".to_string()
    } else {
        format!("copy: no match /{q}")
    }
}

/// One-line reconnect-all summary: how many panes came back, and — when any didn't — which hosts and
/// why (clipped so the toast stays short even in a large fleet). Pure so it's unit-testable.
fn fmt_reconnect_summary(ok: usize, still: &[(String, String)]) -> String {
    let base = format!("reconnect-all: {ok} reached");
    if still.is_empty() {
        return base;
    }
    let detail = clip_dots(
        &still
            .iter()
            .map(|(h, r)| format!("{h}: {r}"))
            .collect::<Vec<_>>()
            .join("; "),
        48,
    );
    format!("{base}, {} still down — {detail}", still.len())
}

fn recall_index(n: usize, delta: isize, cur: Option<usize>) -> usize {
    match cur {
        Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
        None => {
            if delta < 0 {
                n - 1
            } else {
                0
            }
        }
    }
}

/// Map a window pixel point to the fleet-grid tile under it — the inverse of the renderer's tile
/// layout — so click-to-select and double-click-to-dive work in the war-room. Returns the session
/// index, or None when the point is over the header/gutter, right of the last column, below the last
/// drawn row, or past the end of the session list. Pure so the geometry is unit-testable.
fn grid_tile_at(
    x: usize,
    y: usize,
    x0: usize,
    y0: usize,
    tw: usize,
    th: usize,
    cols: usize,
    n: usize,
    height: usize,
) -> Option<usize> {
    if x < x0 || y < y0 || cols == 0 || tw == 0 {
        return None;
    }
    let sx = tw + 8;
    let sy = th + 8;
    let c = (x - x0) / sx;
    if c >= cols {
        return None;
    }
    let r = (y - y0) / sy;
    let ty = y0 + r * sy;
    // A row that the renderer would have clipped (break) has no tile to hit.
    if ty + th > height {
        return None;
    }
    let idx = r * cols + c;
    if idx >= n {
        return None;
    }
    Some(idx)
}

/// Index of the next session that needs attention, searching forward from `start` with wrap: the
/// first DOWN pane (the priority signal), else the first busy pane, else `None`. Used by the fleet
/// grid `n` key so a diver can hop pane-to-pane through a large fleet's trouble spots without
/// paging tile-by-tile. Pure so the wrap and down-over-busy precedence are unit-tested.
fn next_trouble_index(down: &[bool], busy: &[bool], start: usize) -> Option<usize> {
    let n = down.len();
    if n == 0 {
        return None;
    }
    // Prefer a down pane anywhere before a busy pane (a pane that went dark outranks one that just
    // produced output). Each pass wraps and always lands on the nearest qualifying index from start.
    for step in 1..=n {
        let i = (start + step) % n;
        if down[i] {
            return Some(i);
        }
    }
    for step in 1..=n {
        let i = (start + step) % n;
        if busy[i] {
            return Some(i);
        }
    }
    None
}

/// Backward sibling of `next_trouble_index`: the previous session needing attention, searching
/// backward from `start` with wrap — the first DOWN pane, else the first busy one, else `None`.
/// Mirrors peek's up-cycling so a diver can walk both directions through a fleet's trouble spots.
/// Pure so the backward wrap and down-over-busy precedence are unit-tested.
fn prev_trouble_index(down: &[bool], busy: &[bool], start: usize) -> Option<usize> {
    let n = down.len();
    if n == 0 {
        return None;
    }
    for step in 1..=n {
        let i = (start + n - (step % n)) % n;
        if down[i] {
            return Some(i);
        }
    }
    for step in 1..=n {
        let i = (start + n - (step % n)) % n;
        if busy[i] {
            return Some(i);
        }
    }
    None
}

/// Top offset of a scrolling viewport given `total` rows and a 0-based `selected` index. Keeps the
/// highlighted row on screen and rides the bottom edge once the list is taller than `rows`, so a
/// fleet/palette/broadcast list bigger than the window never hides rows behind an invisible
/// selection. Shared by the fleet, palette and broadcast overlays.
fn scroll_top(total: usize, selected: usize, rows: usize) -> usize {
    let hi = selected.min(total.saturating_sub(1));
    let by_bottom = if hi >= rows { hi - rows + 1 } else { 0 };
    by_bottom.min(total.saturating_sub(rows))
}

/// Aggregate `(host, alive)` pairs into a per-host tally of `(host, alive_count, total_count)`,
/// preserving first-seen host order. Pure and unit-tested so the fleet-summary host block and any
/// future chrome can share one grouping.
#[cfg(test)]
fn host_tally<'a>(tabs: impl Iterator<Item = (&'a str, bool)>) -> Vec<(&'a str, usize, usize)> {
    let mut out: Vec<(&str, usize, usize)> = Vec::new();
    for (h, alive) in tabs {
        match out.iter_mut().find(|(host, _, _)| *host == h) {
            Some((_, a, t)) => {
                *t += 1;
                if alive {
                    *a += 1;
                }
            }
            None => out.push((h, if alive { 1 } else { 0 }, 1)),
        }
    }
    out
}

/// Per-host aggregate with the agent mix, in first-seen host and engine order. Each entry is
/// `(host, alive, total, Vec<(engine, count)>)`, with an empty host normalized to `local` and an
/// engine counted once per session. Pure so it's unit-testable.
fn host_engine_breakdown<'a>(
    tabs: impl Iterator<Item = (&'a str, bool, &'a str)>,
) -> Vec<(String, usize, usize, Vec<(String, usize)>)> {
    let mut out: Vec<(String, usize, usize, Vec<(String, usize)>)> = Vec::new();
    for (h, alive, engine) in tabs {
        let host = if h.is_empty() { "local" } else { h };
        match out.iter_mut().find(|(hh, _, _, _)| *hh == host) {
            Some((_, a, t, mix)) => {
                *t += 1;
                if alive {
                    *a += 1;
                }
                match mix.iter_mut().find(|(e, _)| e == engine) {
                    Some((_, c)) => *c += 1,
                    None => mix.push((engine.to_string(), 1)),
                }
            }
            None => out.push((
                host.to_string(),
                if alive { 1 } else { 0 },
                1,
                vec![(engine.to_string(), 1)],
            )),
        }
    }
    out
}

/// Format an engine mix like `claude`×`2, codex` for a host row's trailing label (compact: a
/// single agent is just its name; multiples carry a ×count). Pure so it's unit-testable.
fn format_engine_mix(mix: &[(String, usize)]) -> String {
    mix.iter()
        .map(|(e, c)| {
            if *c == 1 {
                e.clone()
            } else {
                format!("{e}×{c}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One fleet-summary host line: `● build02 · live · claude×2, codex` with a local empty host
/// normalized. This is the copied-summary counterpart to the on-screen host-overview rows, built
/// from the same pure data so the two always agree. Pure so it's unit-testable.
fn fleet_host_line(host: &str, alive: usize, total: usize, mix: &[(String, usize)]) -> String {
    let label = if host.is_empty() { "local" } else { host };
    let mark = if alive == 0 {
        "○"
    } else if alive == total {
        "●"
    } else {
        "◐"
    };
    let state = if alive == 0 {
        "down".to_string()
    } else if alive < total {
        format!("{alive}/{total} live")
    } else {
        "live".to_string()
    };
    let mix_s = format_engine_mix(mix);
    if mix_s.is_empty() {
        format!("{mark} {label} · {state}")
    } else {
        format!("{mark} {label} · {state} · {mix_s}")
    }
}

/// Tab indices (in tab order) whose session's host equals `host` (empty host normalized to
/// `local`). Backs the host-overview drill-in list. Pure so it's unit-testable.
fn session_indices_for_host<'a>(
    tabs: impl Iterator<Item = (usize, &'a str)>,
    host: &str,
) -> Vec<usize> {
    tabs.filter(|(_, h)| {
        let hx = if h.is_empty() { "local" } else { *h };
        hx == host
    })
    .map(|(i, _)| i)
    .collect()
}

fn collect_fleet_matches(
    terms: &[Arc<
        alacritty_terminal::sync::FairMutex<
            alacritty_terminal::term::Term<crate::session::Listener>,
        >,
    >],
    query_lower: &str,
) -> Vec<FleetMatch> {
    let mut out = Vec::new();
    for (tab, term) in terms.iter().enumerate() {
        let g = term.lock();
        // Reuse the public `all_matches` per-tab, tagging each hit with its tab index.
        for (line, col, _) in crate::render::all_matches(&g, query_lower) {
            out.push(FleetMatch { tab, line, col });
        }
    }
    out.sort_by(|a, b| (a.tab, a.line, a.col).cmp(&(b.tab, b.line, b.col)));
    out
}

/// A window of `row` centered on byte `col` (the match start), at most `max` characters with `…`
/// on the cut side(s). Used by the fleet-search rows so a long agent line shows the region AROUND
/// the hit instead of the line head — a match mid-line would otherwise sit off-screen. Short lines
/// (that already fit) pass through whole. Pure; unit-tested.
pub(crate) fn focus_snippet(row: &str, col: usize, max: usize) -> String {
    let chars: Vec<char> = row.chars().collect();
    if chars.len() <= max || max == 0 {
        return row.chars().take(max).collect();
    }
    let col = col.min(chars.len().saturating_sub(1));
    let window = max.saturating_sub(2);
    let half = window / 2;
    let mut start = col.saturating_sub(half);
    let mut end = start + window;
    if end > chars.len() {
        end = chars.len();
        start = end.saturating_sub(window);
    }
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(&chars[start..end]);
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// The macOS menu-style Cmd shortcuts we intercept before forwarding anything to the session.
/// Kept as a pure (key, mods) → action decision so the behavior is unit-testable without a window,
/// and so `forward_key` only executes the chosen action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CmdShortcut {
    /// Cmd+T / Cmd+N — open the New-Session picker (mirrors prefix+n).
    NewSession,
    /// Cmd+W — close the active tab / document window.
    CloseActive,
    /// Cmd+Q — quit (same save-then-exit dance as prefix+q).
    Quit,
    /// Cmd+Shift+] — next tab.
    NextTab,
    /// Cmd+Shift+[ — previous tab.
    PrevTab,
    /// Cmd+Shift+P — open the command palette (VS Code / iTerm muscle memory).
    CommandPalette,
    /// Cmd+Shift+F — open the fleet search (search every session's scrollback at once).
    FleetSearch,
    /// Cmd+1..9 — jump straight to that tab (1-based); Cmd+0 jumps to the last. The universal
    /// macOS/browser/iTerm way to page between many agent sessions without cycling.
    GotoTab(usize),
    /// Cmd+Shift+T — reopen the last-closed tab (the browser/iTerm recovery muscle memory).
    ReopenTab,
    /// Cmd+Shift+D — duplicate the active session (VS Code / iTerm muscle memory).
    Duplicate,
    /// Cmd+Shift+R — force-reconnect ALL down panes at once (the browser "reload" muscle memory for
    /// bringing a whole fleet back).
    ReconnectAll,
    /// Cmd+Shift+U — toggle pin on the active tab (protect it from close), the system shortcut for
    /// prefix+pin. Lets a diver shield a long-running agent without dropping into the prefix chord.
    Pin,
    /// Cmd+Shift+M — toggle mute on the active tab (stop its busy badge + OS ping), the system
    /// shortcut for prefix+mute. Quick way to silence a noisy backgrounded agent from anywhere.
    Mute,
    /// Cmd+Shift+I — open this tab's info panel (kind/host/task), the system shortcut for
    /// prefix+i. Fast way to read a session's identity + fleet context without the prefix chord.
    Info,
    /// Cmd+Shift+J — jump to the next quiet (awaiting-you) agent. The shift-guarded J: quicker
    /// than cycling tabs when the fastest thing to do is find who's waiting on input.
    NextQuiet,
    /// Cmd+Shift+K — send Ctrl-C to every session (stop the whole fleet). The shift-guarded K:
    /// one reflex to halt every runaway agent at once, like Cmd+. but fleet-wide.
    InterruptAll,
    /// Cmd+Shift+Y — open peek, the tail of every session at once. The shift-guarded Y: a war-room
    /// glance at all agents without dropping into the prefix chord.
    Peek,
    /// Cmd+Shift+C — copy the active tab's whole scrollback to the clipboard, the system shortcut
    /// for prefix+copy_scrollback. Grab an agent's full session log fast, without the prefix chord.
    CopyScrollback,
    /// Cmd+Shift+S — write the active tab's whole scrollback to a .log file, the system shortcut
    /// for prefix+export_scrollback. Hand an agent's session off to a file fast, without the chord.
    ExportScrollback,
    /// Cmd+. — send Ctrl-C to the active session. The macOS "stop the running thing" key, so a
    /// runaway agent is halted with the same reflex you'd use to stop a Python script in Xcode.
    Interrupt,
    /// Cmd+F — open find-in-session (the universal iTerm/editor muscle memory for searching the
    /// current pane's scrollback).
    Find,
    /// Cmd+G — jump to the next find match (the iTerm2 muscle memory for stepping through a search
    /// after the find bar closes).
    FindNext,
    /// Cmd+Shift+G — jump to the previous find match.
    FindPrev,
    /// Not a Cmd shortcut we own (forward as normal).
    None,
}

/// The terminal escape sequence for an app-level key the app doesn't otherwise consume: Home,
/// End, forward-Delete, Insert, and F1-F12. These currently reached neither the shell nor any app
/// handler (they fell into the generic `_ => {}` and were dropped), so Home/End/forward-delete did
/// nothing in bash/readline/TUIs. Arrows, Tab, PageUp/PageDown, etc. are handled elsewhere by the
/// app and deliberately return None here. Pure so the portability table is unit-tested.
/// Arrow-key escape sequence, honoring Ctrl (word/paragraph move) and Option/Alt (object move)
/// per the xterm modifier encoding (3 = Alt, 5 = Ctrl, 7 = Ctrl+Alt). Bare arrows produce the
/// plain `ESC [ A-D` sequence. Previously Ctrl+arrow silently dropped the modifier (readline got a
/// plain char move instead of a word move). Pure so the encoding table is unit-tested.
fn arrow_seq(letter: u8, mods: &ModifiersState) -> &'static [u8] {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    match (ctrl, alt, letter) {
        (false, false, b'A') => b"\x1b[A",
        (false, false, b'B') => b"\x1b[B",
        (false, false, b'C') => b"\x1b[C",
        (false, false, _) => b"\x1b[D",
        (false, true, b'A') => b"\x1b[1;3A",
        (false, true, b'B') => b"\x1b[1;3B",
        (false, true, b'C') => b"\x1b[1;3C",
        (false, true, _) => b"\x1b[1;3D",
        (true, false, b'A') => b"\x1b[1;5A",
        (true, false, b'B') => b"\x1b[1;5B",
        (true, false, b'C') => b"\x1b[1;5C",
        (true, false, _) => b"\x1b[1;5D",
        (true, true, b'A') => b"\x1b[1;7A",
        (true, true, b'B') => b"\x1b[1;7B",
        (true, true, b'C') => b"\x1b[1;7C",
        (true, true, _) => b"\x1b[1;7D",
    }
}

fn extra_named_seq(n: &winit::keyboard::NamedKey) -> Option<&'static [u8]> {
    use winit::keyboard::NamedKey as K;
    Some(match n {
        K::Home => b"\x1b[H",
        K::End => b"\x1b[F",
        K::Delete => b"\x1b[3~",
        K::Insert => b"\x1b[2~",
        K::F1 => b"\x1bOP",
        K::F2 => b"\x1bOQ",
        K::F3 => b"\x1bOR",
        K::F4 => b"\x1bOS",
        K::F5 => b"\x1b[15~",
        K::F6 => b"\x1b[17~",
        K::F7 => b"\x1b[18~",
        K::F8 => b"\x1b[19~",
        K::F9 => b"\x1b[20~",
        K::F10 => b"\x1b[21~",
        K::F11 => b"\x1b[23~",
        K::F12 => b"\x1b[24~",
        _ => return None,
    })
}

/// Encode one SGR (mode 1006) mouse event: `ESC [ < Cb ; Cx ; Cy M` for a press/motion, `m` for a
/// release. Coordinates are 1-based (terminal grid cells), `cb` is the xterm button/motion code
/// with modifier bits folded in (see the helpers below). Pure so it can be unit-tested byte-for-byte.
/// Prepend `q` to an MRU list, dropping any older equal entry so the list stays unique, capping at
/// `cap`. No-op for an empty query. Pure so find/broadcast recall is unit-tested once.
fn prepend_capped(hist: &mut Vec<String>, q: &str, cap: usize) {
    if q.is_empty() {
        return;
    }
    hist.retain(|h| h != q);
    hist.insert(0, q.to_string());
    hist.truncate(cap);
}

fn sgr_mouse(cb: u16, col: usize, row: usize, release: bool) -> Vec<u8> {
    format!(
        "\x1b[<{};{};{}{}",
        cb,
        col,
        row,
        if release { "m" } else { "M" }
    )
    .into_bytes()
}

/// xterm button code for a MouseInput: 0/1/2 = left/middle/right press, 3 = any release, with the
/// Shift(4)/Alt(8)/Ctrl(16) modifier bits added. Right-click releases use the same code as any
/// other release; the caller decides whether right-click is forwarded at all.
fn sgr_button_code(button: MouseButton, state: ElementState, mods: &ModifiersState) -> u16 {
    let base: u16 = match (button, state) {
        (MouseButton::Left, ElementState::Pressed) => 0,
        (MouseButton::Middle, ElementState::Pressed) => 1,
        (MouseButton::Right, ElementState::Pressed) => 2,
        _ => 3,
    };
    let mut cb = base;
    if mods.shift_key() {
        cb += 4;
    }
    if mods.alt_key() {
        cb += 8;
    }
    if mods.control_key() {
        cb += 16;
    }
    cb
}

/// xterm wheel code: 64 = up, 65 = down (matching `mag`'s app convention where positive = up into
/// history), plus the same modifier bits.
fn sgr_wheel_code(mag: f64, mods: &ModifiersState) -> u16 {
    let mut cb: u16 = if mag > 0.0 { 64 } else { 65 };
    if mods.shift_key() {
        cb += 4;
    }
    if mods.alt_key() {
        cb += 8;
    }
    if mods.control_key() {
        cb += 16;
    }
    cb
}

/// xterm motion code: 32 = drag with the left button held, 35 = buttonless motion, plus modifiers.
fn sgr_motion_code(held: bool, mods: &ModifiersState) -> u16 {
    let mut cb: u16 = if held { 32 } else { 35 };
    if mods.shift_key() {
        cb += 4;
    }
    if mods.alt_key() {
        cb += 8;
    }
    if mods.control_key() {
        cb += 16;
    }
    cb
}

fn cmd_shortcut(key: &Key, mods: &ModifiersState) -> CmdShortcut {
    use CmdShortcut::*;
    // Browser-style Ctrl+Tab / Ctrl+Shift+Tab cycle tabs too (many terminal users expect the
    // Chrome/Firefox muscle memory, in addition to Cmd+Shift+[/]). Pure and unit-tested like every
    // other shortcut; Ctrl on its own otherwise never reaches the shell as a 	.
    if mods.control_key() && matches!(key, Key::Named(winit::keyboard::NamedKey::Tab)) {
        return if mods.shift_key() { PrevTab } else { NextTab };
    }
    if !mods.super_key() {
        return None;
    }
    match key {
        Key::Character(c) => match c.as_str() {
            "t" | "n" | "N" => NewSession,
            "w" => CloseActive,
            "q" => Quit,
            // On a real US keyboard, holding Shift transforms the bracket into `}` / `{`, so we
            // match both the un-shifted and shifted glyphs: Cmd+Shift+] / Cmd+Shift+[ switch tabs no
            // matter how winit reports the produced character. Plain Cmd+] / Cmd+[ stay with the shell.
            "]" | "}" if mods.shift_key() => NextTab,
            "[" | "{" if mods.shift_key() => PrevTab,
            // Cmd+Shift+P is the (uppercase, shift-held) P — the conventional command palette.
            "P" if mods.shift_key() => CommandPalette,
            // Cmd+Shift+F — search all sessions (the browser/editor "find in all" muscle memory).
            "F" if mods.shift_key() => FleetSearch,
            // Cmd+number jumps to a tab: 1..9 are 1-based indexes, 0 wraps to the last. Standard
            // iTerm/browser muscle memory for fast switching between many agent windows.
            "0" => GotoTab(usize::MAX),
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                GotoTab(d.as_bytes()[0] as usize - b'1' as usize)
            }
            // Cmd+Shift+T reopens the last-closed tab (U is shift; T is shift-pressed too).
            // Must be checked for the shifted 'T' since Cmd+T alone is NewSession.
            "T" if mods.shift_key() => ReopenTab,
            // Cmd+Shift+D duplicates the active session — the VS Code/iTerm "Duplicate" muscle
            // memory. Plain Cmd+D stays with the session.
            "D" if mods.shift_key() => Duplicate,
            // Cmd+Shift+R force-reconnects every down pane at once (browser "reload" habit).
            "R" if mods.shift_key() => ReconnectAll,
            // Cmd+Shift+U toggles pin (protect-from-close) — the "U" for the un-shifted prefix+pin.
            "U" if mods.shift_key() => Pin,
            // Cmd+Shift+M toggles mute (silence a noisy agent's badge + OS ping).
            "M" if mods.shift_key() => Mute,
            // Cmd+Shift+I shows the active tab's info (kind/host/task) — prefix+i muscle memory.
            "I" if mods.shift_key() => Info,
            // Cmd+Shift+C copies the whole scrollback (the shift-guarded C: plain Cmd+C stays the
            // normal copy-selection key, exactly as copy mode expects).
            "C" if mods.shift_key() => CopyScrollback,
            // Cmd+Shift+S exports the scrollback to a .log (shift-guarded S: plain Cmd+S stays free).
            "S" if mods.shift_key() => ExportScrollback,
            // Cmd+. is the macOS universal "stop" keystroke — interrupt the active session.
            "." => Interrupt,
            // Cmd+Shift+J jumps to the next quiet (fellow-agent-waiting) session, shift-guarded so
            // plain Cmd+J stays free.
            "J" if mods.shift_key() => NextQuiet,
            // Cmd+Shift+K stops the whole fleet — Ctrl-C to every non-muted session.
            "K" if mods.shift_key() => InterruptAll,
            // Cmd+Shift+Y opens peek — every session's tail in one war-room list.
            "Y" if mods.shift_key() => Peek,
            // Cmd+F opens find-in-session; Cmd+G / Cmd+Shift+G step to the next / previous match
            // afterwards (plain iTerm2 muscle memory; F/G are otherwise free).
            "f" => Find,
            "g" => FindNext,
            "G" if mods.shift_key() => FindPrev,
            // Plain Cmd+[ is left to the shell; only the Shift variant switches tabs.
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn close_tab(app: &mut App, pinned: bool) -> bool {
    if app.tabs.is_empty() {
        return false;
    }
    if app.active < app.tabs.len() && pinned {
        return false;
    }
    // Stash the closed tab's spec so prefix+u can undo a mistaken close. Kind + host + engine
    // are enough to re-spawn the same identity (TMUX/etc. re-attach to the same pane@host).
    if let Some(s) = app.tabs.get(app.active) {
        app.last_closed = Some(crate::restore::TabSpec {
            kind: s.kind().to_string(),
            host: s.meta.host.clone(),
            engine: s.meta.engine.clone(),
            port: s.port(),
            name: s.meta.name.clone(),
            session: s.attach_session.clone(),
        });
    }
    app.tabs.remove(app.active);
    if app.active >= app.tabs.len() {
        app.active = app.tabs.len().saturating_sub(1);
    }
    crate::restore::save(&app.tab_specs());
    true
}

/// Re-anchor the active index after a batch of tab closes, none of which is the active tab
/// itself. Each closed tab at an index below the active slot shifts focus down by one; a closed tab
/// above it leaves focus untouched. Returns the new active index.
pub(crate) fn reanchor_active_after_batch(active: usize, closed: &[usize]) -> usize {
    active.saturating_sub(closed.iter().filter(|&&i| i < active).count())
}

fn scroll_active(app: &Application, delta: i32) {
    use alacritty_terminal::grid::Scroll;
    if let Some(active) = app.app.active_session() {
        let mut g = active.term.lock();
        g.grid_mut().scroll_display(Scroll::Delta(delta));
    }
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Native-tab mode: every session is its own real window, grouped into AppKit's title-bar
        // tab bar. `sync_hosts` creates the windows, splices them into one tab group, and points the
        // shared window alias at the first. A total creation failure falls back to single-window.
        if self.native_tabs {
            self.sync_hosts(event_loop);
            if self.hosts.is_empty() {
                self.native_tabs = false;
            } else {
                self.metrics_from_scale();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                // Focus the window of the session that was active when we quit (alias_active has
                // already pointed self.window at it), not always the first tab.
                if let Some(w) = &self.window {
                    w.focus_window();
                }
                return;
            }
        }
        let size = crate::restore::load_geometry()
            .map(|(w, h)| Size::Physical(PhysicalSize::new(w, h)))
            .unwrap_or(Size::Logical(LogicalSize::new(110.0, 34.0)));
        let attribs = winit::window::Window::default_attributes()
            .with_title("harness-terminal")
            .with_inner_size(size)
            .with_position({
                let (x, y) = crate::restore::load_position().unwrap_or((200, 120));
                winit::dpi::Position::Physical(winit::dpi::PhysicalPosition::new(x, y))
            });
        match event_loop.create_window(attribs) {
            Ok(w) => {
                let w = Rc::new(w);
                self.window = Some(Rc::clone(&w));
                self.context = Context::new(Rc::clone(&w)).ok();
                self.surface = self
                    .context
                    .as_ref()
                    .and_then(|c| Surface::new(c, Rc::clone(&w)).ok());
                self.size = w.inner_size();
                self.metrics_from_scale();
                // Ask macOS to treat this window as natively tabbable (system title-bar tab bar).
                // This is the OS-level hook; grouping multiple real windows into a tab set is done
                // per-session as those windows come up (see `macos::tabs`).
                if self.native_tabs {
                    crate::macos::tabs::enable_tabbing(&w);
                }
                w.request_redraw();
            }
            Err(e) => {
                eprintln!("harness-terminal: failed to create window: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Native-tab mode routing: resolve which host this event belongs to, and if it isn't the
        // window the user was last looking at, adopt it so the shared handlers below (keyboard,
        // mouse, overlay) operate on the right session. Closing/resizing additionally need the host
        // index, which `hidx` carries.
        let hidx = if self.native_tabs {
            self.hosts.iter().position(|h| h.window.id() == id)
        } else {
            None
        };
        if let Some(hi) = hidx {
            // Don't steal focus from a mouse hover over a background native tab (cursor tracking
            // shouldn't flip which session is active); real intent to switch arrives via
            // `Focused(true)` or an actual key/click.
            let hover_only = matches!(event, WindowEvent::CursorMoved { .. });
            if !hover_only && !matches!(event, WindowEvent::CloseRequested) {
                self.focus_host(hi);
            }
        }
        match event {
            WindowEvent::Focused(focused) => {
                if self.native_tabs && focused {
                    if let Some(hi) = hidx {
                        self.focus_host(hi);
                    }
                }
                if !focused {
                    // The pointer may have been released outside the window (or the window lost
                    // focus mid-drag), which would leave `mouse_left_down` stuck true and make a
                    // mouse-mode app see drag-motion (code 32) when the user isn't holding anything.
                    self.mouse_left_down = false;
                }
            }
            WindowEvent::CursorLeft { .. } => {
                // Same stuck-release concern: if the user presses inside a mouse-mode TUI, drags
                // out, and releases outside the window, no release event arrives; drop the held
                // state so a mouse app never stays "dragging" forever.
                self.mouse_left_down = false;
            }
            WindowEvent::CloseRequested => {
                if self.native_tabs {
                    if let Some(hi) = hidx {
                        self.close_native_tab(hi, event_loop);
                    }
                    return;
                }
                // Persist open tabs so they come back on the next launch; keep the window size too.
                self.app.save_all_scrollbacks();
                crate::restore::save(&self.app.tab_specs());
                self.save_muted_state();
                crate::restore::save_geometry(self.size.width, self.size.height);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if self.native_tabs {
                    if let Some(hi) = hidx {
                        self.hosts[hi].size = size;
                    }
                    self.alias_active();
                    if size.width > 0 && size.height > 0 {
                        crate::restore::save_geometry(size.width, size.height);
                    }
                } else {
                    self.size = size;
                    if size.width > 0 && size.height > 0 {
                        crate::restore::save_geometry(size.width, size.height);
                    }
                }
                if size.width > 0 && size.height > 0 {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::Moved(pos) => {
                // Persist the primary (or only) window's top-left so a relaunch returns to the same
                // spot; sibling windows are offset for the native tab fan-out and shouldn't clobber
                // where the user parks the main window.
                if !self.native_tabs || hidx == Some(0) {
                    crate::restore::save_position(pos.x, pos.y);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.metrics_from_scale();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mods = self.mods;
                    self.handle_key(&event.logical_key, &mods);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                // A mouse-mode PTY that asked for motion/drag owns pointer movement over the grid:
                // forward it (throttled) and skip our hover/hand-cursor affordances for this event.
                if self.forward_mouse_motion() {
                    return;
                }
                // Track which tab the pointer is over so render can show a tail-preview tooltip.
                // Only request a redraw when it changes (or exits) a tab, not on every mouse move.
                let ht = self.tab_at(position.x, position.y);
                if ht != self.hover_tab {
                    self.hover_tab = ht;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                // Hand cursor over clickable links: switch the icon only when the state actually
                // changes so we don't spam the OS every mouse move.
                let link = self.cell_has_link(position.x, position.y);
                if link != self.over_link {
                    self.over_link = link;
                    if let Some(w) = &self.window {
                        use winit::window::CursorIcon;
                        w.set_cursor(if link {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        });
                    }
                }
                // Dragging a tab reorders the bar (live swap as you cross a neighbor's midpoint);
                // dragging in the grid grows a text selection. Both are mutually exclusive.
                if self.drag_tab.is_some() && self.app.overlay == Overlay::None {
                    self.tab_drag(position.x);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                } else if self.mouse_anchor.is_some() && self.app.overlay == Overlay::None {
                    self.mouse_drag(position.x, position.y);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Scroll wheel navigates the scrollback: up = back into history, down = forward.
                // Natural/line/other deltas are normalized to a sign; each notch ~3 lines keeps it
                // snappy without racing the keyboard.
                use winit::event::MouseScrollDelta;
                let mag = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => -p.y / 40.0,
                };
                if mag == 0.0 {
                    return;
                }
                // A mouse-mode PTY owns the wheel over the grid: forward it as SGR wheel-up/down and
                // skip our scrollback navigation (which only applies when no app asked for mouse).
                if self.forward_mouse_wheel(mag) {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Scrolling over the tab bar cycles tabs instead of scrolling the pane (iTerm2 /
                // Chrome-style): wheel up steps left, down steps right, wrapping at the edges.
                if self.cursor.1 < self.chrome_top() as f64 && self.app.tabs.len() > 1 {
                    let n = self.app.tabs.len() as isize;
                    let dir = if mag > 0.0 { -1 } else { 1 };
                    let next = ((self.app.active as isize + dir).rem_euclid(n)) as usize;
                    self.set_active(next);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Positive magnitude = scroll up (into history), negative = down (toward live).
                let lines = (mag * 3.0) as i32;
                scroll_active(self, lines);
                if lines > 0 {
                    if let Some(s) = self.app.active_session() {
                        s.set_scrolled(true);
                    }
                } else {
                    // Scrolling down to the live bottom un-pins history (mirrors PgDn): once the
                    // offset can't go lower, the view is live again so new output follows and the
                    // "scrolled into history" label clears. Without this, wheel-scrolling up then
                    // back to the bottom left the tab stuck labeled "scrolled" with-follow off.
                    self.unpin_if_at_bottom();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Track left-button state for drag-motion reporting, and forward the click to a
                // mouse-mode PTY before our own selection/chrome logic runs (strictly gated, so the
                // non-mouse path below is untouched).
                if button == MouseButton::Left {
                    self.mouse_left_down = state == ElementState::Pressed;
                }
                if self.forward_mouse_button(button, state) {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                if self.app.overlay == Overlay::FleetGrid {
                    // The war-room is mouse-friendly: a left press selects the tile under the
                    // cursor; a double/triple click dives into that session. Other buttons/states
                    // (and clicks on gutter/header) are ignored while the overlay is open.
                    if button == MouseButton::Left && state == ElementState::Pressed {
                        let (x, y) = self.cursor;
                        let (x0, y0, tw, th, cols, n, height) = self.fleet_grid_geom();
                        if let Some(idx) =
                            grid_tile_at(x as usize, y as usize, x0, y0, tw, th, cols, n, height)
                        {
                            self.grid_sel = idx;
                            if self.click_count(x, y) >= 2 && idx < self.app.tabs.len() {
                                self.app.active = idx;
                                crate::restore::save_active(self.app.active);
                                self.app.overlay = Overlay::None;
                            }
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                    return;
                }
                if self.app.overlay != Overlay::None {
                    return;
                }
                let (x, y) = self.cursor;
                // Right-click opens the contextual menu; a second right-click (or one outside) while
                // it is open dismisses it. Only the press matters.
                if button == MouseButton::Right {
                    if state == ElementState::Pressed {
                        if self.ctx.is_some() {
                            self.ctx = None;
                        } else {
                            self.open_context_menu(x, y);
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // A left click while the context menu is open runs the row under the pointer (or
                // dismisses the menu when it lands outside), then suppresses normal selection.
                if self.ctx.is_some() {
                    if state == ElementState::Pressed {
                        if let Some(a) = self.ctx_action_at(x, y) {
                            self.run_ctx_action(a);
                        }
                        self.ctx = None;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Middle-click = paste the system clipboard (X11 muscle memory). Only the press
                // matters; the release is discarded.
                if button == MouseButton::Middle {
                    if state == ElementState::Pressed {
                        self.paste_raw();
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                if button != MouseButton::Left {
                    return;
                }
                match state {
                    ElementState::Pressed => {
                        if self.mods.alt_key() {
                            // Alt+click moves the shell cursor instead of selecting.
                            self.mouse_alt_click(x, y);
                            return;
                        }
                        if self.mods.control_key() || self.mods.super_key() {
                            // Cmd/Ctrl+click opens the URL/path under the cursor.
                            self.mouse_open(x, y);
                            return;
                        }
                        // Pressing the "+" at the strip's right edge opens the New-Session picker
                        // (native tab-strip behavior — same path prefix+n and the context menu use).
                        if let Some((bx, by, bw, bh)) = self.newtab_btn {
                            let (mx, my) = (x as usize, y as usize);
                            if mx >= bx && mx < bx + bw && my >= by && my < by + bh {
                                self.app.overlay = Overlay::NewSession;
                                self.ctx = None;
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }
                        // A press on a hovered tab's right-edge close × closes that tab (pinned
                        // tabs refuse with a flash). Handled before drag-to-reorder so the × is a
                        // dedicated hit target.
                        if let Some(ci) = self.tab_close_at(x, y) {
                            self.close_tab_at(ci);
                            self.ctx = None;
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        // Pressing on a tab starts a drag-reorder (or a click-to-switch); pressing
                        // in the grid starts normal text selection. Both are mutually exclusive.
                        if let Some(i) = self.tab_at(x, y) {
                            self.drag_tab = Some(i);
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        // A click inside the hover tooltip switches to that session's tab — the peek
                        // panel is click-through, so "hover to preview, click to go" is one gesture.
                        if let Some((px0, py0, pw, ph)) = self.tooltip_box {
                            let (mx, my) = (x as usize, y as usize);
                            if mx >= px0 && mx < px0 + pw && my >= py0 && my < py0 + ph {
                                if let Some(i) = self.hover_tab {
                                    self.set_active(i);
                                }
                                self.tooltip_box = None;
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }
                        // A press on the scrollbar jumps the view to that position; otherwise
                        // fall through to text selection in the grid.
                        if self.scrollbar_click(x, y) {
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        self.drag_tab = None;
                        self.mouse_press(x, y);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    ElementState::Released => {
                        if self.drag_tab.is_some() {
                            self.tab_drop(x, y);
                        } else {
                            self.mouse_release(x, y);
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain native-menu-bar commands dispatched since the last frame and map them onto the same
        // handlers the in-app Cmd shortcuts use. The menu's key equivalents consume their chords
        // before winit, so these never double-fire with `cmd_shortcut` (that path is just a fallback
        // for chords the menu does not own).
        #[cfg(target_os = "macos")]
        for a in crate::macos::menu::drain_actions() {
            self.apply_menu_action(a);
        }
        // A quit request (prefix+q, the palette's Quit action, or Cmd+Q) is honored here, after the
        // key handler's borrows have ended.
        if self.quit_requested {
            event_loop.exit();
            return;
        }
        // Fleet overview auto-refresh: while the remote-fleet overlay is open, re-poll the daemon
        // every few seconds (non-blocking) so a host that comes back appears without pressing `s`.
        // Cheap and contained — skipped the instant the overlay is dismissed.
        if self.app.overlay == Overlay::Fleet
            && self.fleet_last_poll.elapsed() >= std::time::Duration::from_secs(3)
        {
            self.fleet_last_poll = std::time::Instant::now();
            self.app.refresh_fleet_nonblocking();
        }
        // Cmd+W: close the active tab. In native mode we close the focused host's window (which
        // closes that session); otherwise we close the in-app tab. Needs `event_loop`, hence here.
        if self.close_active_requested {
            self.close_active_requested = false;
            if self.native_tabs && !self.hosts.is_empty() {
                // Respect pin in native mode too: our Cmd+W / menu CloseTab must not close a pinned
                // session any more than the in-app `x` does. The OS traffic-light close is a separate
                // deliberate gesture on the window (handled via CloseRequested, not here) and is not
                // blocked — but our own close stays honest to the shield.
                // Clamp active_host defensively: if a window was dropped without the index being
                // re-derived (e.g. a session closed under a race), an unclamped index would panic.
                let hi = self.active_host.min(self.hosts.len() - 1);
                let tab = self.hosts[hi].tab;
                if self.pinned.get(tab).copied().unwrap_or(false) {
                    self.flash = Some((
                        "🔒 pinned — prefix A to unpin first".to_string(),
                        std::time::Instant::now(),
                    ));
                    self.request_redraw();
                    return;
                }
                self.close_native_tab(hi, event_loop);
            } else if !self.app.tabs.is_empty() {
                self.close_tab_at(self.app.active);
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        // Native-tab mode: reconcile hosts with the session set (a new tab from the New-Session /
        // palette / undo path gets its own window here), then drive focus aliasing.
        self.sync_hosts(event_loop);
        // Reconnect_sweep is throttled internally, so piggyback a cheap link-health refresh on it:
        // a periodic ping to the local harness daemon keeps the status-line tunnel badge current.
        self.app.reconnect_sweep_refresh();
        // A soft-present loop pumps at ~60fps only while something is visibly alive (a tab pouring
        // output this frame, a fading bell/recovery badge, an open overlay/tooltip, copy mode,
        // hover). Otherwise it sleeps on a slow ~8fps idle tick and skips the full-frame present, so
        // a quiet terminal doesn't peg a whole core on re-uploading the framebuffer via QuartzCore
        // every 16ms. While idle, a cheap content-length detector still wakes the loop the moment any
        // pane produces output (and the fast pump resumes), so fresh output is never left un-drawn.
        let hot = self.has_live_animation();
        let dirty = hot || self.detect_content_change();
        if dirty {
            if self.native_tabs {
                // Repaint every session window so output landing in a backgrounded tab shows up
                // immediately in that window's own surface.
                self.request_redraw();
            } else if let Some(w) = &self.window {
                // Full-rate while live; otherwise a single repaint to show newly-arrived output,
                // then back to sleep.
                w.request_redraw();
            }
        }
        let wait = if hot {
            std::time::Duration::from_millis(16)
        } else {
            std::time::Duration::from_millis(120)
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now() + wait));
    }
}

/// Fire one coalesced macOS notification (title + body). Shells out to `osascript` — no extra crate
/// dependency, and the terminal already assumes macOS. Best-effort: a missing/denied osascript
/// (rare, headless) must not touch the terminal.
fn notify_simple(title: &str, body: &str) {
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Parse the remote-attach overlay's `host[:port][/session]` input into `(addr, attach)`. A `/`
/// names an existing remote tmux session to attach to (no spawn/kill); without one the input is a
/// plain `host[:port]` for a fresh engine spawn. Pure so it's unit-testable.
pub(crate) fn parse_remote_attach(raw: &str) -> (String, Option<String>) {
    let t = raw.trim();
    match t.split_once('/') {
        // `host[/session]` with a non-empty session name is a classic attach.
        Some((a, s)) if !s.trim().is_empty() => (a.trim().to_string(), Some(s.trim().to_string())),
        // No session: `host`, `host:port`, `host/`, or blank. A stray trailing `/` (a typo or a
        // paste artifact, e.g. `build.example.com/`) must not become part of the DNS name — strip
        // it so the fallback still reaches the real host.
        _ => (t.trim_end_matches('/').trim().to_string(), None),
    }
}

/// The first session that is a down remote pane (a non-PTY transport that has dropped) —
/// the host a fleet diver most needs to notice when they open the peek list. Pure so the
/// peek-landing rule is unit-testable without building real transports.
fn first_down_session(kinds: &[&str], alive: &[bool]) -> Option<usize> {
    kinds
        .iter()
        .zip(alive.iter())
        .position(|(&k, &a)| k != "pty" && !a)
}

/// Case-insensitive fuzzy (subsequence) matcher for palette filtering: every query character must
/// appear in `hay`, in order, but not necessarily contiguously. So `crd` matches "cursor codex",
/// `fleet` matches "fleet grid", and a blank query matches everything. Pure, so it's unit-testable.
pub(crate) fn fuzzy_match(query: &str, hay: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q: Vec<char> = query.chars().collect();
    let mut qi = 0;
    for ch in hay.chars() {
        if ch.to_ascii_lowercase() == q[qi].to_ascii_lowercase() {
            qi += 1;
            if qi == q.len() {
                return true;
            }
        }
    }
    false
}

/// Group queued (kind, tab) notifications into coalesced batches: one batch per kind, in queued
/// order, listing every tab of that kind. Pure so it's unit-testable without an Application.
/// `pending` is drained by the caller; a multi-tab `broadcast` (busy in N tabs at once) collapses
/// to one "busy" batch instead of N popups.
pub(crate) fn group_notifications(pending: &[(String, usize)]) -> Vec<(String, Vec<usize>)> {
    // One bucket per kind, in first-seen order, collecting every tab of that kind regardless of how
    // busy/bell interleave — a frame with mixed kinds still yields exactly two popups (one per kind).
    let mut batches: Vec<(String, Vec<usize>)> = Vec::new();
    for (kind, tab) in pending {
        match batches.iter_mut().find(|(k, _)| *k == *kind) {
            Some((_, tabs)) => tabs.push(*tab),
            None => batches.push((kind.clone(), vec![*tab])),
        }
    }
    batches
}

/// Join session labels for a coalesced-notification list. A handful reads as `a, b, c`; a fleet-wide
/// broadcast (every host) collapses to `all N` so the popup stays short.
fn join_labels(labels: &[String]) -> String {
    if labels.len() <= 3 {
        labels.join(", ")
    } else {
        format!("all {} sessions", labels.len())
    }
}

/// Behind the selected row of a list overlay, fill a full-width highlight pill in the chrome's
/// active-tab color so the selected entry reads at a glance (the same affordance as the context
/// menu). Free function so overlay renderers that borrow `self.app` can still call it.
fn overlay_row_sel(fb: &mut Framebuffer, y_center: usize, line_px: usize, margin: usize) {
    let top = y_center.saturating_sub(line_px / 2);
    let w = fb.width.saturating_sub(margin * 2);
    fill_rect(fb, margin, top, w, line_px, CHROME_ACTIVE_BG);
}

/// Entry point: create the native window and run the event loop.
pub fn run(app: App) -> Result<(), Box<dyn std::error::Error>> {
    // Install the native macOS menu bar (Cmd+T/W/Q, prev/next tab, palette) before any window
    // appears so it's up for the whole session. Best-effort: a failure just means no menus.
    #[cfg(target_os = "macos")]
    unsafe {
        crate::macos::menu::install_main_menu();
    }
    let event_loop = EventLoop::new()?;
    let mut application = Application::new(app);
    event_loop.run_app(&mut application)?;
    Ok(())
}

/// The tab `prefix+H` should land on given each tab's host (in tab order): the first tab of the
/// next distinct host after `active`, in host-first-occurrence order, wrapping around. Returns None
/// when `hosts` is empty or has only one distinct host (nothing to page by). Pure so it can be
/// unit-tested without building a window or PTYs.
fn next_host_index(hosts: &[&str], active: usize) -> Option<usize> {
    if hosts.is_empty() {
        return None;
    }
    // Distinct hosts in tab order (a monotonic scan keeps the first tab per host).
    let mut uniq: Vec<&str> = Vec::new();
    for h in hosts {
        if !uniq.contains(h) {
            uniq.push(h);
        }
    }
    if uniq.len() < 2 {
        return None;
    }
    let cur = *hosts.get(active)?;
    let cur_idx = uniq.iter().position(|h| *h == cur).unwrap_or(0);
    let target = uniq[(cur_idx + 1) % uniq.len()];
    // First tab of `target` AFTER `active`, else (wrap) the first tab of `target` overall.
    hosts
        .iter()
        .enumerate()
        .skip(active + 1)
        .find(|&(_, h)| *h == target)
        .map(|(i, _)| i)
        .or_else(|| hosts.iter().position(|h| *h == target))
}

/// Swap slots `a` and `b` in a tab-parallel vector, no-op when either is out of range. Used when
/// a tab is moved (swap) so the session's pin/mute/busy/quiet/badge state follows it.
fn swap_slot<T>(v: &mut [T], a: usize, b: usize) {
    if a != b && a < v.len() && b < v.len() {
        v.swap(a, b);
    }
}

/// Relocate slot `from` to `to` in a tab-parallel vector with the same remove/insert semantics as
/// `App::move_tab_from_to`, guarding against a vector that is transiently shorter than `tabs`.
fn move_slot<T>(v: &mut Vec<T>, from: usize, to: usize) {
    if from < v.len() {
        let x = v.remove(from);
        v.insert(to.min(v.len()), x);
    }
}

/// Resolve which fleet-grid index a bulk action (broadcast/reconnect/interrupt) targets: every
/// marked index, or — when nothing is marked and a `fallback` mask is given — every index where the
/// mask is true (e.g. "all down" for reconnect, "all non-muted" for interrupt). A `None` fallback
/// (close) means an empty mark set targets nothing. Pure so the fallback semantics are unit-tested
/// once instead of being copy-pasted into every grid action.
fn grid_targets(marks: &[bool], fallback: Option<&[bool]>) -> Vec<usize> {
    let marked: Vec<usize> = marks
        .iter()
        .enumerate()
        .filter(|(_, &m)| m)
        .map(|(i, _)| i)
        .collect();
    if !marked.is_empty() {
        return marked;
    }
    match fallback {
        Some(mask) => mask
            .iter()
            .enumerate()
            .filter(|(_, &f)| f)
            .map(|(i, _)| i)
            .collect(),
        None => Vec::new(),
    }
}

/// Whether a freshly-grew backgrounded (non-muted) tab should fire a one-shot "busy" nudge.
/// Suppressed while the tab is still inside its recovery window (down→alive within the last few
/// seconds), because it just got a `recover` toast for the same reconnect — nudging again here
/// would be a redundant double-nag. Pure so the coalescing rule is unit-tested once.
fn should_busy_nudge(grew: bool, already_notified: bool, in_recovery: bool) -> bool {
    grew && !already_notified && !in_recovery
}
#[cfg(test)]
mod tests {
    use super::{
        argb_to_rgb, arrow_seq, broadcast_bytes, clip_dots, cmd_shortcut, collect_fleet_matches,
        copy_no_match_flash, engine_accent, expand_click_word, extra_named_seq, first_down_session,
        fleet_host_line, fmt_duration, fmt_reconnect_summary, focus_snippet, format_engine_mix,
        fuzzy_match, grid_targets, grid_tile_at, group_notifications, host_color,
        host_engine_breakdown, host_tally, join_labels, move_slot, next_host_index,
        next_trouble_index, parse_remote_attach, prepend_capped, prev_trouble_index,
        reanchor_active_after_batch, recall_index, scroll_top, session_indices_for_host,
        sgr_button_code, sgr_motion_code, sgr_mouse, sgr_wheel_code, should_busy_nudge,
        status_accent, swap_slot, CmdShortcut, FleetMatch, CHROME_BUSY, CHROME_ERR, CHROME_QUIET,
        CHROME_RECOVER,
    };

    use winit::event::{ElementState, MouseButton};

    use std::sync::Arc;

    /// The macOS Cmd shortcuts map to the right actions, and crucially do NOT fire without the
    /// Cmd (super) modifier or with a modifier they don't own (so plain typing is never hijacked).
    #[test]
    fn cmd_shortcut_routes_native_shortcuts_only() {
        use winit::keyboard::{Key, ModifiersState};
        let mut s = ModifiersState::empty();
        s.insert(ModifiersState::SUPER);
        let mut sc = ModifiersState::empty();
        sc.insert(ModifiersState::SUPER | ModifiersState::SHIFT);
        let mut no_cmd = ModifiersState::empty();
        no_cmd.insert(ModifiersState::SHIFT);

        let chars =
            |c: &str, m: ModifiersState| cmd_shortcut(&Key::Character(c.to_string().into()), &m);

        // Cmd+T / Cmd+N / Cmd+Shift+N open the New-Session picker.
        assert_eq!(chars("t", s), CmdShortcut::NewSession);
        assert_eq!(chars("n", s), CmdShortcut::NewSession);
        assert_eq!(
            chars("N", sc),
            CmdShortcut::NewSession,
            "Cmd+Shift+N also opens"
        );
        // Cmd+W closes, Cmd+Q quits.
        assert_eq!(chars("w", s), CmdShortcut::CloseActive);
        assert_eq!(chars("q", s), CmdShortcut::Quit);
        // With Shift they switch tabs; without Shift they're left alone. Accept both the
        // un-shifted (`]`/`[`) and shifted (`}`/`{`) glyphs a real US-layout Shift+[/] produces.
        assert_eq!(chars("]", sc), CmdShortcut::NextTab);
        assert_eq!(chars("}", sc), CmdShortcut::NextTab, "shifted ] glyph");
        assert_eq!(chars("[", sc), CmdShortcut::PrevTab);
        assert_eq!(chars("{", sc), CmdShortcut::PrevTab, "shifted [ glyph");
        // Cmd+Shift+D duplicates the active session; plain Cmd+D is left alone.
        assert_eq!(chars("D", sc), CmdShortcut::Duplicate);
        assert_eq!(
            chars("d", s),
            CmdShortcut::None,
            "plain Cmd+D must not hijack"
        );
        // Cmd+Shift+P opens the command palette; plain Cmd+P is left alone.
        assert_eq!(chars("P", sc), CmdShortcut::CommandPalette);
        assert_eq!(
            chars("p", sc),
            CmdShortcut::None,
            "plain Cmd+P is not hijacked"
        );
        // Cmd+Shift+F opens fleet search; plain Cmd+F opens find-in-session (both the shifted and
        // un-shifted `f` glyph route to Find — the uppercase `F` with the shift modifier is the
        // fleet tag, so Cmd+Shift+F is what must differ, and it does).
        assert_eq!(chars("F", sc), CmdShortcut::FleetSearch);
        assert_eq!(chars("f", sc), CmdShortcut::Find);
        assert_eq!(chars("f", s), CmdShortcut::Find);
        // Cmd+Shift+R force-reconnects all down panes; plain Cmd+R is left alone.
        assert_eq!(chars("R", sc), CmdShortcut::ReconnectAll);
        assert_eq!(
            chars("r", s),
            CmdShortcut::None,
            "plain Cmd+R must not hijack the shell"
        );
        // Cmd+Shift+U toggles pin; Cmd+Shift+M toggles mute; both plain Cmd forms stay free.
        assert_eq!(chars("U", sc), CmdShortcut::Pin);
        assert_eq!(
            chars("u", s),
            CmdShortcut::None,
            "plain Cmd+U is not hijacked"
        );
        assert_eq!(chars("M", sc), CmdShortcut::Mute);
        assert_eq!(
            chars("m", s),
            CmdShortcut::None,
            "plain Cmd+M is not hijacked"
        );
        // Cmd+Shift+I shows the active tab's info; plain Cmd+I stays with the shell.
        assert_eq!(chars("I", sc), CmdShortcut::Info);
        assert_eq!(
            chars("i", s),
            CmdShortcut::None,
            "plain Cmd+I is not hijacked"
        );
        // Cmd+Shift+C copies the whole scrollback; plain Cmd+C stays the copy-selection key.
        assert_eq!(chars("C", sc), CmdShortcut::CopyScrollback);
        assert_eq!(
            chars("c", s),
            CmdShortcut::None,
            "plain Cmd+C is not hijacked"
        );
        // Cmd+Shift+S exports the scrollback to a .log; plain Cmd+S stays with the shell.
        assert_eq!(chars("S", sc), CmdShortcut::ExportScrollback);
        assert_eq!(
            chars("s", s),
            CmdShortcut::None,
            "plain Cmd+S is not hijacked"
        );
        // Cmd+. interrupts the active session — the macOS stop key.
        assert_eq!(chars(".", s), CmdShortcut::Interrupt);
        // Cmd+Shift+J jumps to the next quiet (awaiting-you) agent; plain Cmd+J stays free.
        assert_eq!(chars("J", sc), CmdShortcut::NextQuiet);
        assert_eq!(
            chars("j", s),
            CmdShortcut::None,
            "plain Cmd+J is not hijacked"
        );
        // Cmd+Shift+K stops the whole fleet (Ctrl-C to all); plain Cmd+K stays free.
        assert_eq!(chars("K", sc), CmdShortcut::InterruptAll);
        assert_eq!(
            chars("k", s),
            CmdShortcut::None,
            "plain Cmd+K is not hijacked"
        );
        // Cmd+Shift+Y opens peek; plain Cmd+Y stays free.
        assert_eq!(chars("Y", sc), CmdShortcut::Peek);
        assert_eq!(
            chars("y", s),
            CmdShortcut::None,
            "plain Cmd+Y is not hijacked"
        );
        // Cmd+G / Cmd+Shift+G: next / previous find match.
        assert_eq!(chars("g", s), CmdShortcut::FindNext);
        assert_eq!(chars("G", sc), CmdShortcut::FindPrev);
        assert_eq!(
            chars("]", s),
            CmdShortcut::None,
            "plain Cmd+] is not a shortcut"
        );
        assert_eq!(
            chars("[", s),
            CmdShortcut::None,
            "plain Cmd+[ stays with the shell"
        );
        // Browser-style Ctrl+Tab / Ctrl+Shift+Tab cycle tabs, independent of the Cmd chord.
        let ctrl = ModifiersState::CONTROL;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let tab = Key::Named(winit::keyboard::NamedKey::Tab);
        assert_eq!(cmd_shortcut(&tab, &ctrl), CmdShortcut::NextTab);
        assert_eq!(cmd_shortcut(&tab, &ctrl_shift), CmdShortcut::PrevTab);
        // A bare Tab (no Ctrl) is NOT captured — it must keep going to the session/shell.
        assert_eq!(
            cmd_shortcut(&tab, &ModifiersState::empty()),
            CmdShortcut::None
        );
        // And Ctrl+Tab still requires a real Tab key, not a character.
        assert_eq!(
            cmd_shortcut(&Key::Character("t".to_string().into()), &ctrl),
            CmdShortcut::None
        );

        // Cmd+number jumps to a tab: 1..9 are 1-based, 0 is the last tab (usize::MAX sentinel).
        assert_eq!(chars("1", s), CmdShortcut::GotoTab(0));
        assert_eq!(chars("5", s), CmdShortcut::GotoTab(4));
        assert_eq!(chars("9", s), CmdShortcut::GotoTab(8));
        assert_eq!(chars("0", s), CmdShortcut::GotoTab(usize::MAX));
        // Cmd+Shift+T reopens the last-closed tab; plain Cmd+t stays NewSession.
        assert_eq!(chars("T", sc), CmdShortcut::ReopenTab);
        assert_eq!(
            chars("t", s),
            CmdShortcut::NewSession,
            "plain Cmd+T unchanged"
        );
        // Shifted digits ("!") are NOT tab jumps — they fall through to None.
        assert_eq!(chars("!", sc), CmdShortcut::None);
        // No Cmd at all: nothing is a shortcut (typing flows through).
        assert_eq!(chars("t", no_cmd), CmdShortcut::None);
        assert_eq!(
            cmd_shortcut(&Key::Named(winit::keyboard::NamedKey::Enter), &s),
            CmdShortcut::None
        );
        // Cmd+C / Cmd+V are copy/paste, handled elsewhere — not a tab shortcut.
        assert_eq!(chars("c", s), CmdShortcut::None);
        assert_eq!(chars("v", s), CmdShortcut::None);
    }

    /// Home/End/forward-Delete/Insert/F-keys map to the standard terminal escape sequences so they
    /// actually reach bash/readline/TUIs, and the keys the app owns (arrows, Tab, PageUp, Escape)
    /// deliberately return None instead of being double-forwarded.
    #[test]
    fn extra_named_seq_maps_navigation_keys_to_escapes() {
        use winit::keyboard::NamedKey as K;
        // The app-level keys we forward.
        assert_eq!(extra_named_seq(&K::Home), Some(&b"\x1b[H"[..]));
        assert_eq!(extra_named_seq(&K::End), Some(&b"\x1b[F"[..]));
        assert_eq!(extra_named_seq(&K::Delete), Some(&b"\x1b[3~"[..]));
        assert_eq!(extra_named_seq(&K::Insert), Some(&b"\x1b[2~"[..]));
        assert_eq!(extra_named_seq(&K::F1), Some(&b"\x1bOP"[..]));
        assert_eq!(extra_named_seq(&K::F4), Some(&b"\x1bOS"[..]));
        assert_eq!(extra_named_seq(&K::F5), Some(&b"\x1b[15~"[..]));
        assert_eq!(extra_named_seq(&K::F12), Some(&b"\x1b[24~"[..]));
        // Keys the app consumes itself must not ALSO be forwarded to the shell.
        assert_eq!(extra_named_seq(&K::ArrowUp), None);
        assert_eq!(extra_named_seq(&K::Tab), None);
        assert_eq!(extra_named_seq(&K::PageUp), None);
        assert_eq!(extra_named_seq(&K::PageDown), None);
        assert_eq!(extra_named_seq(&K::Escape), None);
        assert_eq!(extra_named_seq(&K::Enter), None);
    }

    #[test]
    fn sgr_mouse_encodes_xterm_1006_sequences_byte_for_byte() {
        // Press: ESC [ < Cb ; Cx ; Cy M, 1-based coordinates.
        assert_eq!(sgr_mouse(0, 5, 6, false), b"\x1b[<0;5;6M");
        assert_eq!(sgr_mouse(2, 1, 1, false), b"\x1b[<2;1;1M");
        assert_eq!(sgr_mouse(69, 40, 24, false), b"\x1b[<69;40;24M");
        // Release: trailing lowercase m.
        assert_eq!(sgr_mouse(3, 5, 6, true), b"\x1b[<3;5;6m");
        assert_eq!(sgr_mouse(35, 7, 8, true), b"\x1b[<35;7;8m");
    }

    #[test]
    fn sgr_button_code_maps_buttons_modifiers_and_release() {
        use winit::keyboard::ModifiersState as M;
        let none = M::empty();
        // Plain presses: left=0, middle=1, right=2.
        assert_eq!(
            sgr_button_code(MouseButton::Left, ElementState::Pressed, &none),
            0
        );
        assert_eq!(
            sgr_button_code(MouseButton::Middle, ElementState::Pressed, &none),
            1
        );
        assert_eq!(
            sgr_button_code(MouseButton::Right, ElementState::Pressed, &none),
            2
        );
        // Any release is 3.
        assert_eq!(
            sgr_button_code(MouseButton::Left, ElementState::Released, &none),
            3
        );
        assert_eq!(
            sgr_button_code(MouseButton::Right, ElementState::Released, &none),
            3
        );
        // Modifiers shift(4)/alt(8)/ctrl(16) fold in on top.
        assert_eq!(
            sgr_button_code(MouseButton::Left, ElementState::Pressed, &M::SHIFT),
            4
        );
        assert_eq!(
            sgr_button_code(MouseButton::Right, ElementState::Pressed, &M::ALT),
            10
        );
        assert_eq!(
            sgr_button_code(MouseButton::Left, ElementState::Pressed, &M::CONTROL),
            16
        );
        assert_eq!(
            sgr_button_code(
                MouseButton::Left,
                ElementState::Pressed,
                &(M::SHIFT | M::ALT | M::CONTROL)
            ),
            28
        );
    }

    #[test]
    fn sgr_wheel_and_motion_codes_are_stable() {
        use winit::keyboard::ModifiersState as M;
        let none = M::empty();
        // Wheel up=64, down=65, shift adds 4.
        assert_eq!(sgr_wheel_code(1.0, &none), 64);
        assert_eq!(sgr_wheel_code(-1.0, &none), 65);
        assert_eq!(sgr_wheel_code(1.0, &M::SHIFT), 68);
        // Motion: buttonless=35, left-drag=32, ctrl folds in.
        assert_eq!(sgr_motion_code(false, &none), 35);
        assert_eq!(sgr_motion_code(true, &none), 32);
        assert_eq!(sgr_motion_code(false, &M::CONTROL), 51);
        assert_eq!(sgr_motion_code(true, &M::SHIFT), 36);
    }

    /// Arrows honor the Ctr/Alt modifier encoding so readline gets word/paragraph moves (Ctrl+Left
    /// etc.) instead of plain char moves, while bare arrows stay untouched.
    #[test]
    fn arrow_seq_honors_ctrl_and_alt_modifiers() {
        use winit::keyboard::ModifiersState;
        let ctrl = ModifiersState::CONTROL;
        let alt = ModifiersState::ALT;
        let both = ModifiersState::CONTROL | ModifiersState::ALT;
        let none = ModifiersState::empty();
        // Bare arrows unchanged.
        assert_eq!(arrow_seq(b'D', &none), &b"\x1b[D"[..]);
        assert_eq!(arrow_seq(b'C', &none), &b"\x1b[C"[..]);
        // Ctrl+arrows → word/paragraph moves (xterm 1;5 encoding).
        assert_eq!(arrow_seq(b'D', &ctrl), &b"\x1b[1;5D"[..]);
        assert_eq!(arrow_seq(b'C', &ctrl), &b"\x1b[1;5C"[..]);
        assert_eq!(arrow_seq(b'A', &ctrl), &b"\x1b[1;5A"[..]);
        assert_eq!(arrow_seq(b'B', &ctrl), &b"\x1b[1;5B"[..]);
        // Alt+arrows → object moves (1;3), and Ctrl+Alt → 1;7.
        assert_eq!(arrow_seq(b'D', &alt), &b"\x1b[1;3D"[..]);
        assert_eq!(arrow_seq(b'C', &both), &b"\x1b[1;7C"[..]);
    }

    /// The fleet-search row snippet centers on the match so a long line's hit is on screen.
    #[test]
    fn focus_snippet_centers_on_the_match_and_clips() {
        // A short line already fits whole.
        assert_eq!(focus_snippet("fix here", 0, 40), "fix here");
        // Empty / max-0 are safe.
        assert_eq!(focus_snippet("", 0, 10), "");
        assert_eq!(focus_snippet("abc", 0, 0), "");
        // A long line clips to <= budget and keeps the match near the middle.
        let long = "a".repeat(50) + "HIT" + &"b".repeat(50);
        let s = focus_snippet(&long, 51, 24);
        assert!(s.chars().count() <= 24, "snippet {s:?} exceeds budget");
        assert!(
            s.contains("HIT"),
            "match must be in the centered snippet: {s:?}"
        );
        // The match sits within the window (not at the very edge) when there's room on both sides.
        assert!(s.starts_with('…') && s.ends_with('…'));
        // A line whose match is near the end still contains it and stays in budget.
        let tail_line = "x".repeat(10) + "MATCH";
        let st = focus_snippet(&tail_line, 10, 16);
        assert!(st.contains("MATCH") && st.chars().count() <= 16);
        // A match near the very start stays in budget with a trailing ellipsis.
        let st2 = focus_snippet(&("HIT".to_string() + &"z".repeat(60)), 0, 20);
        assert!(st2.contains("HIT") && st2.chars().count() <= 20);
    }

    /// Fleet search spans multiple sessions and is sorted by tab then line. Build two real emulator
    /// Terms (no PTY needed), seed each with distinct text, and assert the collected matches are
    /// tagged by tab and ordered (tab 1's matches before tab 2's, lines in order within each tab).
    #[test]
    fn fleet_search_crosses_tabs_sorted_by_tab_then_line() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::{Listener, TermSize};

        // Two tabs: tab 0 has "fix" on two lines, tab 1 on one line — out of sorted order at the
        // line level so we can prove the primary sort is BY TAB.
        let size = TermSize { lines: 4, cols: 40 };
        let mut terms: Vec<Arc<FairMutex<Term<Listener>>>> = Vec::new();
        let t0 = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            Listener::default(),
        )));
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t0.lock(), b"fix first\r\nno match\r\nfix third");
        }
        let t1 = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            Listener::default(),
        )));
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t1.lock(), b"only fix here");
        }
        terms.push(t0);
        terms.push(t1);

        let hits = collect_fleet_matches(&terms, "fix");
        // 2 (tab 0) + 1 (tab 1) = 3 matches.
        assert_eq!(hits.len(), 3);
        // Sorted by tab first: both tab-0 hits (lines 0, 2 in line order) then the tab-1 hit.
        assert_eq!(
            hits[0],
            FleetMatch {
                tab: 0,
                line: 0,
                col: 0
            }
        );
        assert_eq!(
            hits[1],
            FleetMatch {
                tab: 0,
                line: 2,
                col: 0
            }
        );
        assert_eq!(
            hits[2],
            FleetMatch {
                tab: 1,
                line: 0,
                col: 5
            }
        );
        // Case-insensitive: uppercase query still matches lowercase text (recompute lowercases it).
        assert_eq!(collect_fleet_matches(&terms, "FIX").len(), 3);
        // A query in no tab matches nothing.
        assert!(collect_fleet_matches(&terms, "zzz").is_empty());
    }

    /// Same-kind queued notifications (a multi-tab broadcast) coalesce into ONE batch, in order,
    /// instead of one popup per tab.
    #[test]
    fn group_notifications_coalesces_same_kind() {
        let pending = vec![
            ("busy".to_string(), 0),
            ("busy".to_string(), 2),
            ("busy".to_string(), 5),
        ];
        let batches = group_notifications(&pending);
        assert_eq!(
            batches.len(),
            1,
            "three simltaneous busy events -> one batch"
        );
        assert_eq!(batches[0].0, "busy");
        assert_eq!(batches[0].1, vec![0, 2, 5]);
    }

    /// Mixed busy and bell events in one frame stay in separate batches so each kind gets its own
    /// (title-accurate) popup, but same-kind events still merge across the alternation.
    #[test]
    fn group_notifications_splits_busy_and_bell() {
        let pending = vec![
            ("busy".to_string(), 1),
            ("bell".to_string(), 2),
            ("busy".to_string(), 3),
        ];
        let batches = group_notifications(&pending);
        assert_eq!(
            batches.len(),
            2,
            "one bucket per kind, even when interleaved"
        );
        assert_eq!(batches[0].0, "busy");
        assert_eq!(batches[0].1, vec![1, 3]);
        assert_eq!(batches[1].0, "bell");
        assert_eq!(batches[1].1, vec![2]);
    }

    /// A single backgrounded tab going busy stays a single-notification event (no spurious merge
    /// or split).
    #[test]
    fn group_notifications_single_stays_single() {
        let pending = vec![("busy".to_string(), 7)];
        let batches = group_notifications(&pending);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], ("busy".to_string(), vec![7]));
    }

    /// `host[:port][/session]` parses into (addr, optional attach-session). A bare host or host:port
    /// means a fresh spawn; `host:port/session` means attach to that existing named session.
    #[test]
    fn parse_remote_attach_modes() {
        // Fresh spawn: plain host, host:port, and blank all yield no attach target.
        assert_eq!(
            parse_remote_attach("build.example.com"),
            ("build.example.com".to_string(), None)
        );
        assert_eq!(
            parse_remote_attach("10.0.0.4:18473"),
            ("10.0.0.4:18473".to_string(), None)
        );
        assert_eq!(parse_remote_attach(""), ("".to_string(), None));
        // Attach-existing: host/session and host:port/session, with whitespace trimmed.
        assert_eq!(
            parse_remote_attach("build.example.com/agent-runs"),
            (
                "build.example.com".to_string(),
                Some("agent-runs".to_string())
            )
        );
        assert_eq!(
            parse_remote_attach("10.0.0.4:18473/agent-runs"),
            ("10.0.0.4:18473".to_string(), Some("agent-runs".to_string()))
        );
        assert_eq!(
            parse_remote_attach(" host / my-session "),
            ("host".to_string(), Some("my-session".to_string()))
        );
        // A trailing slash with an empty session is not an attach (falls back to fresh host) and
        // the stray slash is stripped so it never becomes part of the DNS name.
        assert_eq!(
            parse_remote_attach("build.example.com/"),
            ("build.example.com".to_string(), None)
        );
        assert_eq!(
            parse_remote_attach("build.example.com/ "),
            ("build.example.com".to_string(), None)
        );
        assert_eq!(parse_remote_attach("  "), ("".to_string(), None));
        assert_eq!(
            parse_remote_attach("10.0.0.4:18473/"),
            ("10.0.0.4:18473".to_string(), None)
        );
    }

    /// Peek lands on the first still-down remote pane, skipping live PTYs, and falls back to the
    /// top of the list when the fleet is healthy.
    #[test]
    fn first_down_session_picks_the_down_remote_pane() {
        // Live pty, live ssh, down tunnel, live tmux -> lands on index 2.
        let kinds = ["pty", "ssh", "tunnel", "tmux"];
        let alive = [true, true, false, true];
        assert_eq!(first_down_session(&kinds, &alive), Some(2));
        // A down pty is not a remote pane — ignored, falls back to None.
        let kinds2 = ["pty", "pty"];
        let alive2 = [true, false];
        assert_eq!(first_down_session(&kinds2, &alive2), None);
        // All healthy -> none.
        let kinds3 = ["ssh", "tmux", "tunnel"];
        let alive3 = [true, true, true];
        assert_eq!(first_down_session(&kinds3, &alive3), None);
        // First down remote at index 0 wins even though a later one is also down.
        let kinds4 = ["tmux", "ssh", "tunnel"];
        let alive4 = [false, true, false];
        assert_eq!(first_down_session(&kinds4, &alive4), Some(0));
    }

    /// Fuzzy palette matching: subsequence (not just substring), case-insensitive, blank matches all.
    #[test]
    fn fuzzy_match_is_subsequence_and_case_insensitive() {
        // Subsequence: "crd" spans "cursor codex" out of order.
        assert!(fuzzy_match("crd", "cursor codex"));
        // Case-insensitive.
        assert!(fuzzy_match("FLEET", "fleet grid"));
        assert!(fuzzy_match("fleet", "Fleet Grid"));
        // Contiguous + word-ish matches work naturally.
        assert!(fuzzy_match("bro", "broadcast a line"));
        // Blank query matches everything.
        assert!(fuzzy_match("", "anything"));
        // Out-of-order / missing characters do NOT match.
        assert!(!fuzzy_match("xyz", "qwerty"));
        assert!(!fuzzy_match("crd", "codex")); // r before c? no: c,o,d,e,x → c,d match but r never
    }

    /// The recover kind is its own bucket (distinct from busy/bell), so several panes reconnecting
    /// in the same frame coalesce into ONE "N sessions reconnected" popup.
    #[test]
    fn group_notifications_keeps_recover_its_own_kind() {
        let pending = vec![
            ("busy".to_string(), 1),
            ("recover".to_string(), 2),
            ("recover".to_string(), 3),
        ];
        let batches = group_notifications(&pending);
        let recover = batches.iter().find(|(k, _)| k == "recover").unwrap();
        assert_eq!(recover.1, vec![2, 3]);
    }

    /// Down-edge events from several panes in one frame collapse into a single "down" batch so a
    /// whole-fleet drop is one popup, not N osascript launches (same coalescing as busy/recover).
    #[test]
    fn group_notifications_keeps_down_its_own_kind() {
        let pending = vec![
            ("down".to_string(), 4),
            ("busy".to_string(), 1),
            ("down".to_string(), 7),
        ];
        let batches = group_notifications(&pending);
        let down = batches.iter().find(|(k, _)| k == "down").unwrap();
        assert_eq!(down.1, vec![4, 7]);
        let busy = batches.iter().find(|(k, _)| k == "busy").unwrap();
        assert_eq!(busy.1, vec![1]);
    }

    /// A fleet-wide broadcast produces N busy events; the list should collapse to "all N sessions"
    /// rather than a long comma list.
    #[test]
    fn join_labels_collapses_large_fleet() {
        assert_eq!(
            join_labels(&["a".into(), "b".into(), "c".into()]),
            "a, b, c"
        );
        assert_eq!(
            join_labels(&["w".into(), "x".into(), "y".into(), "z".into()]),
            "all 4 sessions"
        );
    }

    /// Every host gets a stable, in-table color; the same host never changes across calls, and two
    /// different hosts can share a color (fine) but the mapping is deterministic.
    #[test]
    fn host_color_is_deterministic() {
        let a = host_color("build-host");
        assert_eq!(a, host_color("build-host"));
        let b = host_color("10.0.0.4");
        assert_eq!(b, host_color("10.0.0.4"));
        assert_ne!(a, (0, 0, 0), "colors must be visible on black");
    }

    /// ARGB unpacking extracts RGB in the right order, and known engines resolve to their brand
    /// accent while unknown engines fall back to the neutral dim (never black).
    #[test]
    fn engine_accent_unpacks_argb_and_falls_back() {
        assert_eq!(argb_to_rgb(0xff_9a4dff), (0x9a, 0x4d, 0xff));
        // Claude's accent is purple-ish; it must differ from a randomly-picked unknown's fallback.
        let empty = std::collections::BTreeMap::new();
        let cl = engine_accent("claude", &empty);
        assert_eq!(cl, (0x9a, 0x4d, 0xff));
        let unknown = engine_accent("no-such-engine", &empty);
        assert_ne!(
            unknown,
            (0, 0, 0),
            "unknown-engine fallback must still be visible"
        );
        // A `[theme.accents]` override wins over the built-in brand accent.
        let mut accents = std::collections::BTreeMap::new();
        accents.insert("claude".to_string(), (255, 0, 0));
        assert_eq!(engine_accent("claude", &accents), (255, 0, 0));
    }

    /// Cmd+click word expansion picks the whole token, not the shell quoting around it.
    #[test]
    fn click_word_expands_to_token() {
        let line = "see https://example.com/foo in the log (src/main.rs)";
        assert_eq!(expand_click_word(line, 25), "https://example.com/foo");
        // The substring is a byte index into the line; find it by locating the token start.
        let url_start = line.find("https").unwrap();
        assert_eq!(
            expand_click_word(line, url_start + 5),
            "https://example.com/foo"
        );
        // A path inside parens expands to the path, stopping at ')'.
        let p = line.find("src/main.rs").unwrap();
        assert_eq!(expand_click_word(line, p), "src/main.rs");
        // A click on a boundary returns an empty token.
        assert_eq!(expand_click_word("   ", 1), "");
    }

    /// A single word on its own line is returned whole.
    #[test]
    fn click_word_single_token() {
        assert_eq!(expand_click_word("README.md", 3), "README.md");
    }

    /// Broadcast formatting: a non-blank line is fanned out with a trailing newline; a blank line
    /// sends nothing (never broadcast a bare newline).
    #[test]
    fn broadcast_bytes_sends_line_but_not_blank() {
        assert_eq!(broadcast_bytes("git pull"), b"git pull\n");
        assert_eq!(
            broadcast_bytes("  "),
            b"  \n",
            "whitespace-only is still a real line"
        );
        assert!(
            broadcast_bytes("").is_empty(),
            "blank must not broadcast a bare newline"
        );
    }

    /// Find-history MRU dedups (a re-run moves to the front), caps at the max, and ignores blanks.
    #[test]
    fn prepend_capped_dedups_caps_and_skips_blank() {
        let mut h: Vec<String> = vec![];
        // Blank is ignored.
        prepend_capped(&mut h, "", 4);
        assert!(h.is_empty());
        // New runs go to the front, newest first.
        prepend_capped(&mut h, "zzz", 4);
        prepend_capped(&mut h, "rust", 4);
        prepend_capped(&mut h, "claude", 4);
        assert_eq!(h, &["claude", "rust", "zzz"]);
        // Re-running an existing query moves it to the front (dedup).
        prepend_capped(&mut h, "rust", 4);
        assert_eq!(h, &["rust", "claude", "zzz"]);
        // Cap is honored: the oldest drops off when a 5th unique query arrives.
        prepend_capped(&mut h, "codex", 4);
        prepend_capped(&mut h, "tmux", 4);
        assert_eq!(h.len(), 4);
        assert_eq!(h[0], "tmux");
        assert!(
            !h.iter().any(|x| x == "zzz"),
            "oldest entry should be dropped"
        );
    }

    /// Broadcast-history recall walks older/newer and wraps around both ends; a cold start lands on
    /// the newest line for Shift+Up and the oldest for Shift+Down.
    #[test]
    fn recall_broadcast_wraps_and_cold_starts() {
        let n = 3;
        // Cold + Up -> newest (index 2), cold + Down -> oldest (index 0).
        assert_eq!(recall_index(n, -1, None), 2);
        assert_eq!(recall_index(n, 1, None), 0);
        // Stepping older from the newest wraps to the oldest.
        assert_eq!(recall_index(n, -1, Some(2)), 1);
        assert_eq!(recall_index(n, -1, Some(0)), 2);
        // Stepping newer from the oldest wraps to the newest.
        assert_eq!(recall_index(n, 1, Some(0)), 1);
        assert_eq!(recall_index(n, 1, Some(2)), 0);
    }

    /// Mouse hit-testing in the fleet grid mirrors the renderer's tile layout: it picks the tile
    /// under a window pixel, and rejects header/gutter, out-of-column, clipped-row, and past-the-end
    /// points — so click-to-select can't land on a neighbor or a non-existent session.
    #[test]
    fn grid_tile_at_matches_render_layout() {
        let (x0, y0, tw, th, cols, n, height) =
            (8usize, 30usize, 100usize, 72usize, 3usize, 8usize, 500usize);
        // Top-left of the first tile.
        assert_eq!(
            grid_tile_at(8, 30, x0, y0, tw, th, cols, n, height),
            Some(0)
        );
        // Bottom-right inside the first tile still maps to tile 0.
        assert_eq!(
            grid_tile_at(8 + 99, 30 + 71, x0, y0, tw, th, cols, n, height),
            Some(0)
        );
        // Second column (stride tw+8), same row.
        assert_eq!(
            grid_tile_at(8 + 108, 30, x0, y0, tw, th, cols, n, height),
            Some(1)
        );
        // Second row (stride th+8), first column -> index 3.
        assert_eq!(
            grid_tile_at(8, 30 + 80, x0, y0, tw, th, cols, n, height),
            Some(3)
        );
        // Right of the last column -> None.
        assert_eq!(
            grid_tile_at(8 + 3 * 108, 30, x0, y0, tw, th, cols, n, height),
            None
        );
        // Header / left gutter -> None.
        assert_eq!(grid_tile_at(4, 30, x0, y0, tw, th, cols, n, height), None);
        assert_eq!(grid_tile_at(8, 20, x0, y0, tw, th, cols, n, height), None);
        // A row whose tile bottom would exceed the window height (clipped in render) -> None.
        let _ = height; // (covered below via an explicit deep row)
        assert_eq!(
            grid_tile_at(8, 30 + 5 * 80, x0, y0, tw, th, cols, n, height),
            None
        );
        // Past the end of the session list (idx 8 with n=8) -> None.
        assert_eq!(
            grid_tile_at(8 + 2 * 108, 30 + 2 * 80, x0, y0, tw, th, cols, n, height),
            None
        );
        // Degenerate: zero columns or zero width -> None (no panic).
        assert_eq!(grid_tile_at(8, 30, x0, y0, 0, th, 0, n, height), None);
    }

    /// The list viewport never hides the selected row, and rides the bottom edge once the list
    /// outgrows the window — so a fleet/palette/broadcast with more entries than fit on screen stays
    /// fully reachable by Up/Down (regression for the 20-max fleet list swallowing later sessions).
    #[test]
    fn scroll_top_keeps_selection_visible_and_slides() {
        // Fits: no scroll.
        assert_eq!(scroll_top(5, 3, 20), 0);
        // Larger than the window: selection rides the bottom edge.
        assert_eq!(scroll_top(30, 0, 20), 0);
        assert_eq!(scroll_top(30, 25, 20), 6);
        assert_eq!(scroll_top(30, 29, 20), 10);
        // Never over-scrolls past the end (total == window).
        assert_eq!(scroll_top(20, 19, 20), 0);
        // Empty / total == 1.
        assert_eq!(scroll_top(0, 0, 20), 0);
        assert_eq!(scroll_top(1, 0, 20), 0);
    }

    /// Host tally groups by machine in first-seen order with live/total counts, so the fleet
    /// summary's ``host · 2/2 live`` snapshot is correct across a multi-machine farm.
    #[test]
    fn host_tally_groups_by_machine() {
        let tabs = [
            ("build02", true),
            ("edge1", false),
            ("build02", true),
            ("edge1", false),
            ("build02", false),
        ];
        let tally = host_tally(tabs.iter().copied());
        assert_eq!(
            tally,
            vec![("build02", 2, 3), ("edge1", 0, 2)],
            "engine@host grouping with partial-live counts"
        );
        // Empty fleet → no hosts.
        assert!(host_tally(std::iter::empty()).is_empty());
    }

    /// Status accents follow a strict precedence so a dark pane is never mistaken for busy, and the
    /// mapping stays scannable: down=red, busy=amber, quiet=blue, reconnecting=green, nothing
    /// neutral. Recovering means the pane came back, so it tints even when otherwise quiet.
    #[test]
    fn status_accent_precedence() {
        // Down always wins over busy / quiet / recovering — a dead pane reads as dead.
        assert_eq!(status_accent(true, true, true, true), Some(CHROME_ERR));
        assert_eq!(status_accent(true, false, false, false), Some(CHROME_ERR));
        // Busy beats quiet, and recovering alone names itself.
        assert_eq!(status_accent(false, true, true, false), Some(CHROME_BUSY));
        assert_eq!(status_accent(false, true, false, false), Some(CHROME_BUSY));
        assert_eq!(status_accent(false, false, true, false), Some(CHROME_QUIET));
        assert_eq!(
            status_accent(false, false, false, true),
            Some(CHROME_RECOVER)
        );
        // Fully idle local session → neutral.
        assert_eq!(status_accent(false, false, false, false), None);
    }

    /// The fleet-grid `n` triage jump wraps around and prefers a down pane anywhere over a busy one,
    /// so a war-room diver hops pane-to-pane through a large fleet's trouble spots.
    #[test]
    fn next_trouble_index_wraps_and_prioritizes_down() {
        let down = [false, true, false, false];
        let busy = [true, false, false, false];
        // Down at index 1 found even though a busy pane sits earlier in the wrap from index 3.
        assert_eq!(next_trouble_index(&down, &busy, 3), Some(1));
        // Single pane that is down: a wrap lands back on it (the only index) → itself.
        assert_eq!(next_trouble_index(&[true], &[true], 0), Some(0));
        // Down at 0, busy nowhere, start 2 → wraps to 0.
        assert_eq!(
            next_trouble_index(&[true, false, false], &[false, false, false], 2),
            Some(0)
        );
        // No down anywhere → falls back to the nearest busy.
        assert_eq!(
            next_trouble_index(&[false, false, false], &[false, true, false], 2),
            Some(1)
        );
        // Nothing eligible anywhere → None.
        assert_eq!(
            next_trouble_index(&[false, false], &[false, false], 0),
            None
        );
        // Empty fleet → None (never a panic).
        assert_eq!(next_trouble_index(&[], &[], 0), None);
    }

    /// The fleet-grid `N` backward jump walks opposite the forward one, still wrapping and still
    /// preferring a down pane anywhere over a busy one.
    #[test]
    fn prev_trouble_index_walks_backward_and_prioritizes_down() {
        let down = [true, false, false, false];
        let busy = [false, false, true, false];
        // Backward from index 2 (busy) → the previous down is at 0, not the nearer busy at 2's own
        // slot, and it wraps past index 0.
        assert_eq!(prev_trouble_index(&down, &busy, 2), Some(0));
        // Backward from index 1 → the previous busy is at 2 (wrapping), no down before it.
        assert_eq!(
            prev_trouble_index(&[false, false, false], &[false, false, true], 1),
            Some(2)
        );
        // Backward from 0 → wraps to the last down at 3.
        assert_eq!(
            prev_trouble_index(
                &[false, false, false, true],
                &[false, false, false, false],
                0
            ),
            Some(3)
        );
        // Nothing eligible anywhere → None.
        assert_eq!(
            prev_trouble_index(&[false, false], &[false, false], 1),
            None
        );
        // Empty fleet → None (never a panic).
        assert_eq!(prev_trouble_index(&[], &[], 0), None);
    }

    /// Idle-age formatting stays compact and readable at every scale — seconds, minutes, hours,
    /// days — never a raw millisecond dump.
    #[test]
    fn fmt_duration_readable_units() {
        let d = |s: u64| std::time::Duration::from_secs(s);
        assert_eq!(fmt_duration(d(0)), "0s");
        assert_eq!(fmt_duration(d(59)), "59s");
        assert_eq!(fmt_duration(d(60)), "1m");
        assert_eq!(fmt_duration(d(3599)), "59m");
        assert_eq!(fmt_duration(d(3600)), "1h");
        assert_eq!(fmt_duration(d(23 * 3600)), "23h");
        assert_eq!(fmt_duration(d(24 * 3600)), "1d");
    }

    /// `clip_dots` character-clips (not byte-clips) and always signals a cut with "…", never grows
    /// text past the bound, and survives multi-byte glyphs.
    #[test]
    fn clip_dots_truncates_on_char_boundaries() {
        // Under the bound: returned verbatim, no ellipsis.
        assert_eq!(
            clip_dots("tunnel connect refused", 40),
            "tunnel connect refused"
        );
        assert_eq!(clip_dots("", 5), "");
        // At the bound exactly: nothing cut, no ellipsis.
        assert_eq!(clip_dots("abcde", 5), "abcde");
        // Over the bound: clipped to max chars plus an ellipsis (so output > max by one glyph).
        assert_eq!(clip_dots("abcdef", 5), "abcde…");
        // A clipped multi-byte reason keeps each char whole (Han + space) without splitting bytes.
        assert_eq!(clip_dots("主机 refused—retrying", 3), "主机 …");
        // Zero bound collapses to just the ellipsis.
        assert_eq!(clip_dots("anything", 0), "…");
    }

    /// The copy-mode-search "no match" flash names the missy query so a diver can see what failed,
    /// trims whitespace, stays generic for a blank query, and clips a very long query.
    #[test]
    fn copy_no_match_flash_names_the_query() {
        assert_eq!(copy_no_match_flash("fix"), "copy: no match /fix");
        assert_eq!(
            copy_no_match_flash("  fix  "),
            "copy: no match /fix",
            "query text is trimmed"
        );
        assert_eq!(
            copy_no_match_flash("   "),
            "copy: no match",
            "blank query falls back to the generic message"
        );
        let long = "x".repeat(100);
        assert_eq!(
            copy_no_match_flash(&long),
            format!("copy: no match /{}…", "x".repeat(40)),
            "over-long query is clipped with an ellipsis"
        );
    }

    /// `fmt_reconnect_summary` reports the count and clips the still-down host+reason detail so a
    /// fleet-wide reconnect toast stays readable instead of dumping every failure line.
    #[test]
    fn reconnect_summary_reports_hosts_still_down() {
        assert_eq!(fmt_reconnect_summary(2, &[]), "reconnect-all: 2 reached");
        assert_eq!(
            fmt_reconnect_summary(1, &[("build02".to_string(), "refused".to_string())]),
            "reconnect-all: 1 reached, 1 still down — build02: refused"
        );
        // A long list is clipped to the 48-char toast budget while still saying how many are left.
        let big: Vec<(String, String)> = (0..8)
            .map(|i| {
                (
                    format!("host-{i}"),
                    "tunnel timeout while retrying".to_string(),
                )
            })
            .collect();
        let s = fmt_reconnect_summary(0, &big);
        assert!(s.starts_with("reconnect-all: 0 reached, 8 still down — "));
        assert!(s.ends_with('…'));
    }

    /// `prefix+H` pages by host: from a three-host fleet it steps to the first tab of the next
    /// distinct host (in first-occurrence order), not the adjacent pane.
    #[test]
    fn next_host_pages_by_distinct_host_in_first_occurrence_order() {
        // Tab order: a0 a1 (host-a), b0 (host-b), c0 c1 (host-c).
        let hosts = ["host-a", "host-a", "host-b", "host-c", "host-c"];
        // Active on host-a -> next host in first-occurrence order is host-b, first tab = index 2.
        assert_eq!(next_host_index(&hosts, 0), Some(2));
        // Active on index 1 (still host-a) -> same next host.
        assert_eq!(next_host_index(&hosts, 1), Some(2));
        // Active on host-b -> next is host-c, first tab = index 3.
        assert_eq!(next_host_index(&hosts, 2), Some(3));
    }

    #[test]
    fn next_host_wraps_to_first_distinct_host() {
        let hosts = ["host-a", "host-b", "host-c"];
        // Active on the LAST distinct host (host-c) -> wrap to the first host, its first tab.
        assert_eq!(next_host_index(&hosts, 2), Some(0));
    }

    #[test]
    fn next_host_from_later_tab_still_picks_next_not_adjacent() {
        let hosts = ["host-a", "host-b", "host-b", "host-c"];
        // Active on host-b's second tab (index 2) -> next distinct host is host-c (index 3),
        // NOT host-b's adjacent behavior blocking on same-host tabs.
        assert_eq!(next_host_index(&hosts, 2), Some(3));
    }

    #[test]
    fn next_host_is_none_for_single_or_empty_fleet() {
        assert_eq!(next_host_index(&[], 0), None);
        assert_eq!(next_host_index(&["only-host", "only-host"], 0), None);
    }

    /// Re-anchoring focus after a batch close: the active tab is never itself closed, but a closed
    /// tab BELOW it shifts focus down one slot per such close; a closed tab above it does not move
    /// it. Exhaustive over all active positions and close subsets.
    #[test]
    fn reanchor_active_after_batch_is_shifted_only_by_below_closes() {
        let n = 5usize;
        for active in 0..n {
            // Enumerate every non-empty subset of the other 4 tabs being closed.
            let others: Vec<usize> = (0..n).filter(|&i| i != active).collect();
            for mask in 1usize..(1 << others.len()) {
                let closed: Vec<usize> = others
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, &i)| i)
                    .collect();
                let expected =
                    active.saturating_sub(closed.iter().filter(|&&i| i < active).count());
                assert_eq!(
                    reanchor_active_after_batch(active, &closed),
                    expected,
                    "active={active} closed={closed:?}"
                );
            }
        }
        // Sanity: empty close set leaves the active index untouched.
        assert_eq!(reanchor_active_after_batch(3, &[]), 3);
    }

    /// `Cmd`/`Alt`-click word expansion: returns the contiguous run of non-boundary characters
    /// containing the clicked column. Boundaries are whitespace and `()"\'<>[]`. Purely local, so a
    /// click in the middle of a URL/path expands exactly that token.
    #[test]
    fn expand_click_word_expands_contiguous_token() {
        // Middle of a URL.
        assert_eq!(
            expand_click_word("open https://a.com/path now", 8),
            "https://a.com/path"
        );
        // A path token joined by non-boundaries (dashes, dots, slashes stay in the token).
        assert_eq!(
            expand_click_word("cd /Users/me/project-x/", 12),
            "/Users/me/project-x/"
        );
        // Leading/trailing boundary stops the token exactly.
        // Clicking on a boundary column falls back to the adjacent left token.
        assert_eq!(expand_click_word("hello world", 5), "hello");
        // Click on the token itself (the 'b' at index 7) expands to that token.
        assert_eq!(expand_click_word("(go to b) ...", 7), "b");
        // Click at the start / past-the-end column is clamped, not a panic.
        assert_eq!(expand_click_word("abc", 0), "abc");
        assert_eq!(expand_click_word("abc", 99), "abc");
        // A single-character token.
        assert_eq!(expand_click_word("[x]", 1), "x");
    }

    /// `prefix+H` next-host paging: jumps to the first tab of the next DISTINCT host (cycling),
    /// preferring a tab after the current one and wrapping to the front only when none exists.
    #[test]
    fn next_host_index_pages_distinct_hosts_and_wraps() {
        // Distinct hosts, first tab after active.
        assert_eq!(next_host_index(&["a", "b", "c"], 0), Some(1));
        // Active on the last distinct host wraps back to the first host.
        assert_eq!(next_host_index(&["a", "b", "c"], 2), Some(0));
        // Interleaved / repeated hosts: still pages by distinct host, and skips ahead on repeats.
        // host order a,b,b,c; from active a (idx0) -> first tab of b is index 1.
        assert_eq!(next_host_index(&["a", "b", "b", "c"], 0), Some(1));
        // From the second b (idx2) -> c (idx3).
        assert_eq!(next_host_index(&["a", "b", "b", "c"], 2), Some(3));
        // From c (last) wraps to the first tab of a.
        assert_eq!(next_host_index(&["a", "b", "b", "c"], 3), Some(0));
        // One distinct host (even if repeated) has nothing to page to.
        assert_eq!(next_host_index(&["x", "x", "x"], 0), None);
        // Empty tab set has nowhere to go.
        assert_eq!(next_host_index(&[], 0), None);
    }
    /// A tab move (swap) must keep every tab-parallel vector aligned with its session identity: if
    /// slots a/b hold per-session flags (pin, mute, busy, badge...), swapping tabs a/b has to swap
    /// those flags with them so they attach to the same session, not the same slot. This is the
    /// regression that left pin/mute/busy/badges on the wrong session after prefix-move.
    #[test]
    fn moving_a_tab_keeps_parallel_state_aligned_with_the_session() {
        // Three parallel vectors, each indexed by slot, whose VALUES are the session identity that
        // was originally at that slot (pin, mute, quiet, badge flags all tag a session).
        // Column 0 = session 0, column 1 = session 1, column 2 = session 2.
        let mut pinned: Vec<usize> = vec![0, 1, 2];
        let mut muted: Vec<usize> = vec![0, 1, 2];
        let mut was_down: Vec<bool> = vec![false, true, false];

        // Move (swap) slot 0 with slot 1 — like `move_tab(1)` on the active tab.
        swap_slot(&mut pinned, 0, 1);
        swap_slot(&mut muted, 0, 1);
        swap_slot(&mut was_down, 0, 1);

        // Whichever slot session 1 (the one originally muted/down at slot 1) lands in must keep its
        // flags: session 1 is muted and down; session 0 is pinned; session 2 is untouched.
        assert_eq!(
            pinned,
            vec![1, 0, 2],
            "pinned flag must follow session 1 to its new slot"
        );
        assert_eq!(muted, vec![1, 0, 2], "mute flag must follow session 1");
        assert_eq!(
            was_down,
            vec![true, false, false],
            "busy/down flag must follow session 1"
        );
        // All three stay aligned slot-for-slot (no flag slid onto another session).
        for i in 0..3 {
            assert_eq!(
                pinned[i] == 1,
                muted[i] == 1,
                "alignment broken at slot {i}"
            );
        }
    }

    /// Fleet-grid bulk targets: marks win when any are set; otherwise (for non-destructive
    /// reconnect/interrupt) the fallback mask applies; close (None fallback) targets nothing on an
    /// empty mark set so a stray `X` can never nuke the fleet.
    #[test]
    fn grid_targets_resolves_marks_then_fallback() {
        let marks = [false, true, false, true, false];
        let fallback = [true, true, true, false, false];
        // Marks win over any fallback.
        assert_eq!(grid_targets(&marks, Some(&fallback)), vec![1, 3]);
        // No marks -> fallback mask applies.
        assert_eq!(
            grid_targets(&[false, false, false, false, false], Some(&fallback)),
            vec![0, 1, 2]
        );
        // No marks + no fallback (close) -> nothing.
        assert_eq!(
            grid_targets(&[false, false, false, false, false], None),
            Vec::<usize>::new()
        );
        // Length mismatches are safe: marks index beyond fallback's shorter length are still taken.
        assert_eq!(
            grid_targets(&[true, true, true], Some(&[false, false])),
            vec![0, 1, 2]
        );
    }

    /// The busy-nudge coalescing rule: nudge only when there is fresh output AND the tab has not
    /// already been notified AND it is not still inside its recovery window (it just got a
    /// `recover` toast for the same reconnect). Guards the one-notification-per-reconnect invariant.
    #[test]
    fn busy_nudge_coalesces_with_recovery() {
        // Fresh output, never notified, not recovering -> nudge.
        assert!(should_busy_nudge(true, false, false));
        // Already notified this settle-window -> no duplicate nudge.
        assert!(!should_busy_nudge(true, true, false));
        // Fresh output but still inside the recovery window -> suppressed (the recover toast covers it).
        assert!(!should_busy_nudge(true, false, true));
        // No fresh output -> nothing regardless.
        assert!(!should_busy_nudge(false, false, false));
        // Already notified AND recovering -> definitely nothing.
        assert!(!should_busy_nudge(true, true, true));
    }

    /// A drag-reorder (remove/insert) must apply the same relocation to every parallel vector, so a
    /// session dragged from `from` to `to` takes its flags along and neighbors shift consistently.
    #[test]
    fn drag_reorder_keeps_parallel_state_aligned_with_the_session() {
        // Parallel per-slot flags tagged by original session identity.
        let mut pinned: Vec<usize> = (0..4).collect();
        let mut muted: Vec<usize> = (0..4).collect();
        let mut busy: Vec<bool> = vec![false, true, false, true];

        // Drag session at slot 0 to final slot 2 (same remove/insert as `move_tab_from_to(0, 2)`).
        move_slot(&mut pinned, 0, 2);
        move_slot(&mut muted, 0, 2);
        move_slot(&mut busy, 0, 2);

        // Session 0 moved to index 2; sessions 1,2 shifted left; session 3 stays at the end.
        assert_eq!(pinned, vec![1, 2, 0, 3]);
        assert_eq!(muted, vec![1, 2, 0, 3]);
        // Session 1 (busy at old slot 1) is now at index 0; session 3 (busy) stays at index 3.
        assert_eq!(busy, vec![true, false, false, true]);
        // Alignment: identical pinned/muted patterns per column.
        assert_eq!(pinned, muted);

        // Out-of-range relocations are safe no-ops (never panic on stale lengths).
        let mut v = (0..3).collect::<Vec<_>>();
        move_slot(&mut v, 9, 0); // from out of range -> untouched
        assert_eq!(v, vec![0, 1, 2]);
        let mut u = (0..3).collect::<Vec<_>>();
        move_slot(&mut u, 1, 99); // to beyond length -> appends at the end
        assert_eq!(u, vec![0, 2, 1]);
    }
    /// Per-host aggregation keeps alive/total AND the agent mix split by machine, in first-seen
    /// host and engine order, with an empty host normalized to `local` — the data behind the
    /// host-overview rows (prefix+.).
    #[test]
    fn host_engine_breakdown_groups_by_machine_with_agent_mix() {
        let tabs = [
            ("", true, "claude"),
            ("build02", true, "claude"),
            ("build02", false, "codex"),
            ("build02", true, "claude"),
            ("edge1", true, "codex"),
        ];
        let out = host_engine_breakdown(tabs.iter().copied());
        // First-seen host order, local empty host normalized.
        assert_eq!(out.len(), 3);
        // local: 1 alive / 1 session, one claude.
        assert_eq!(out[0], ("local".into(), 1, 1, vec![("claude".into(), 1)]));
        // build02: 2 alive / 3 sessions, claude×2 + codex×1.
        assert_eq!(out[1].0, "build02");
        assert_eq!((out[1].1, out[1].2), (2, 3));
        assert_eq!(
            out[1].3,
            vec![("claude".to_string(), 2), ("codex".to_string(), 1)]
        );
        // edge1: 1/1, one codex.
        assert_eq!(out[2], ("edge1".into(), 1, 1, vec![("codex".into(), 1)]));
        // Empty iter yields nothing.
        assert!(host_engine_breakdown(std::iter::empty()).is_empty());
    }

    /// The hosted mix renders compactly: a lone agent is just its name; repeats fold to a ×count.
    #[test]
    fn format_engine_mix_is_compact() {
        assert_eq!(format_engine_mix(&[("claude".into(), 1)]), "claude");
        assert_eq!(format_engine_mix(&[]), "");
        assert_eq!(
            format_engine_mix(&[("claude".into(), 2), ("codex".into(), 1)]),
            "claude\u{00d7}2, codex"
        );
        assert_eq!(format_engine_mix(&[("codex".into(), 3)]), "codex\u{00d7}3");
    }
    /// The copied fleet summary's host line carries the same status + agent mix as the on-screen
    /// host overview, ready to paste into a report, with a local empty host normalized.
    #[test]
    fn fleet_host_line_matches_host_overview_rows() {
        // Fully alive host with a mixed fleet.
        assert_eq!(
            fleet_host_line(
                "build02",
                2,
                2,
                &[("claude".into(), 2), ("codex".into(), 1)]
            ),
            "● build02 · live · claude\u{00d7}2, codex"
        );
        // Partly down host.
        assert_eq!(
            fleet_host_line("edge1", 1, 3, &[("codex".into(), 2)]),
            "◐ edge1 · 1/3 live · codex\u{00d7}2"
        );
        // Fully down host: dimmed-down marker, no live fraction.
        assert_eq!(
            fleet_host_line("edge1", 0, 2, &[("claude".into(), 1)]),
            "○ edge1 · down · claude"
        );
        // Empty host normalized to "local".
        assert_eq!(
            fleet_host_line("", 1, 1, &[("claude".into(), 1)]),
            "● local · live · claude"
        );
    }
    /// The host-overview drill-in must list exactly the sessions on the selected host, in tab order,
    /// with the local/empty host normalized — the data the `→` sub-view navigates.
    #[test]
    fn session_indices_for_host_lists_that_hosts_sessions_in_tab_order() {
        let tabs = [
            (0, ""), // local claude
            (1, "build02"),
            (2, "build02"),
            (3, ""), // local codex
            (4, "edge1"),
        ];
        // Empty host maps to "local".
        assert_eq!(
            session_indices_for_host(tabs.into_iter(), "local"),
            vec![0, 3]
        );
        // A specific remote host, in tab order.
        assert_eq!(
            session_indices_for_host(tabs.into_iter(), "build02"),
            vec![1, 2]
        );
        assert_eq!(session_indices_for_host(tabs.into_iter(), "edge1"), vec![4]);
        // A host with no sessions is empty.
        assert!(session_indices_for_host(tabs.into_iter(), "nope").is_empty());
        assert!(session_indices_for_host(std::iter::empty(), "local").is_empty());
    }
    /// Every element type the tab-parallel vectors use (bool, usize, u64, Instant, Option<Instant>)
    /// must survive the swap/remove-insert relocation with its session, not drift to another slot.
    /// Guards the move/drag path against a newly-routed vector silently misaligning.
    #[test]
    fn parallel_swap_and_reorder_keep_every_element_type_aligned() {
        let t0 = std::time::Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(1);
        let t2 = t0 + std::time::Duration::from_millis(2);
        // All "vectors" share a session identity via their slot; values tag the original session.
        let mut muted: Vec<bool> = vec![false, true, false];
        let mut detect_len: Vec<usize> = vec![0, 1, 2];
        let mut content_sig: Vec<u64> = vec![10, 11, 12];
        let mut last_output: Vec<std::time::Instant> = vec![t0, t1, t2];
        let mut recover_until: Vec<Option<std::time::Instant>> = vec![None, Some(t1), None];

        // Swap slots 0 and 1 (like a prefix move of the active tab).
        swap_slot(&mut muted, 0, 1);
        swap_slot(&mut detect_len, 0, 1);
        swap_slot(&mut content_sig, 0, 1);
        swap_slot(&mut last_output, 0, 1);
        swap_slot(&mut recover_until, 0, 1);
        // Session 1 (originally muted, sig 11, t1) moved to slot 0 with all its flags.
        assert_eq!(muted, vec![true, false, false]);
        assert_eq!(detect_len, vec![1, 0, 2]);
        assert_eq!(content_sig, vec![11, 10, 12]);
        assert_eq!(last_output, vec![t1, t0, t2]);
        assert_eq!(recover_until, vec![Some(t1), None, None]);

        // Reorder (remove slot 0 -> insert at 2) like a drag; all stay aligned.
        move_slot(&mut muted, 0, 2);
        move_slot(&mut detect_len, 0, 2);
        move_slot(&mut content_sig, 0, 2);
        move_slot(&mut last_output, 0, 2);
        move_slot(&mut recover_until, 0, 2);
        assert_eq!(muted, vec![false, false, true]);
        assert_eq!(detect_len, vec![0, 2, 1]);
        assert_eq!(content_sig, vec![10, 12, 11]);
        assert_eq!(last_output, vec![t0, t2, t1]);
        assert_eq!(recover_until, vec![None, None, Some(t1)]);
    }
}
