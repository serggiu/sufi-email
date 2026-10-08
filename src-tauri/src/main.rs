#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod crypto;
#[cfg(test)]
mod fake_imap;
mod mail;
mod store;
mod theme;
#[cfg(test)]
mod account_tests;
#[cfg(test)]
mod imap_integration_tests;
#[cfg(test)]
mod crypto_tests;
#[cfg(test)]
mod store_tests;

use account::{AccountConfig, AccountInfo, Config};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Emitter;
use tauri::Manager;
use tauri::State;

/// Envelope shape for the system tray icon: the embedded white envelope's
/// alpha channel is the mask; the fill color comes from the active Omarchy
/// theme (see [`tray_icon`]).
const TRAY_ICON_SHAPE: &[u8] = include_bytes!("../icons/tray-gray.png");

/// How often the background watcher re-issues the IDLE command (seconds).
/// The imap crate's default is 29 minutes (RFC 2177's inactivity bound),
/// which is far too slow to notice a dead connection after a network drop:
/// the watcher would sit blocked in the dormant IDLE wait on the stale
/// socket and never reconnect to catch up on mail that arrived meanwhile.
/// Re-issuing every 120s is a trivial amount of traffic, but each re-issue's
/// DONE/IDLE round trip surfaces a dead link within a couple of minutes.
const IDLE_KEEPALIVE_SECS: u64 = 120;

/// How many times the resize handler re-applies a remembered window size
/// before giving up. Wayland can drop a resize requested before the window is
/// mapped and then report a decorations-offset size, so a couple of retries
/// after the surface is mapped are needed for the size to stick.
const WINDOW_RESTORE_ATTEMPTS: u8 = 5;

/// Parse a "#rrggbb" hex color into (r, g, b).
fn hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Repaint the embedded envelope shape with the given RGB, keeping its
/// alpha (the anti-aliased outline), and re-encode it as a PNG for the tray
/// host. This is how the tray icon follows the active Omarchy theme.
fn tint_envelope(r: u8, g: u8, b: u8) -> tauri::image::Image<'static> {
    use image::GenericImageView;
    use image::ImageEncoder;
    let img = image::load_from_memory(TRAY_ICON_SHAPE).expect("embedded tray icon decodes");
    let (w, h) = img.dimensions();
    let mut out = image::RgbaImage::new(w, h);
    for (x, y, px) in img.pixels() {
        out.put_pixel(x, y, image::Rgba([r, g, b, px[3]]));
    }
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(out.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .expect("encode tray icon");
    tauri::image::Image::from_bytes(&buf).expect("encoded tray icon")
}

/// The RGB tint the tray envelope should carry for the given unread state:
/// the active theme's accent when mail is waiting, its foreground otherwise.
/// Falls back to the built-in blue / near-white when no Omarchy theme is
/// staged (mirrors the defaults used by [`tray_icon`]).
fn tray_tint(unread: u32) -> (u8, u8, u8) {
    let theme = theme::read_theme_colors();
    if unread > 0 {
        theme
            .as_ref()
            .and_then(|t| hex_rgb(&t.accent))
            .unwrap_or((0x81, 0xa2, 0xc1)) // built-in blue
    } else {
        theme
            .as_ref()
            .and_then(|t| hex_rgb(&t.foreground))
            .unwrap_or((0xec, 0xec, 0xef)) // built-in near-white
    }
}

/// The tray envelope icon: filled with the active theme's foreground when
/// there is no unread mail (matching the bar's other tinted icons, e.g.
/// Slack/Telegram) and the theme's accent when mail is waiting — so the
/// icon follows the desktop theme while keeping the blue unread cue. Falls
/// back to the built-in near-white/blue when no Omarchy theme is staged.
fn tray_icon(unread: u32) -> tauri::image::Image<'static> {
    let (r, g, b) = tray_tint(unread);
    tint_envelope(r, g, b)
}

/// Sum of unread messages across every configured account (from the cache).
fn total_unread(app: &tauri::AppHandle) -> u32 {
    let cfg = { app.state::<AppState>().config.lock().unwrap().clone() };
    cfg.accounts
        .iter()
        .filter_map(|acc| {
            store::Store::open(acc)
                .ok()
                .and_then(|s| s.load_folders().ok())
                .map(|folders| folders.iter().map(|f| f.unread).sum::<u32>())
        })
        .sum()
}

/// Tooltip shown on the tray icon (the StatusNotifierItem Title, which the
/// tray host displays).
fn tray_tooltip(unread: u32) -> String {
    match unread {
        0 => "Sufi Email - no new messages".to_string(),
        1 => "Sufi Email - 1 new message".to_string(),
        n => format!("Sufi Email - {n} new messages"),
    }
}

/// The visual state the tray icon is currently asked to show. Repaints are
/// gated on this, so routine mail events that leave the unread count and the
/// theme tint unchanged stop rewriting the icon PNG and issuing DBus property
/// updates on every folder refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrayVisual {
    unread: u32,
    /// Envelope tint: the theme's accent when unread > 0, its foreground
    /// otherwise.
    tint: (u8, u8, u8),
}

/// Whether it is safe to call the libappindicator tooltip setter on the
/// current tray icon.
///
/// libayatana-appindicator (0.6.x, and still upstream through master) has a
/// use-after-free: when the StatusNotifierWatcher (the omarchy-shell tray
/// host) is absent for more than ~100 ms it creates an XEmbed fallback
/// GtkStatusIcon and connects its "tooltip"-changed handler with that icon as
/// `data` (`fallback()` in app-indicator.c). When the watcher comes back,
/// `unfallback()` unrefs the icon but never disconnects that handler, so the
/// next `app_indicator_set_tooltip_full()` dereferences the freed icon and
/// segfaults — observed here as SIGSEGV in `gtk_status_icon_set_tooltip_markup`
/// on 4 of the last 5 crashes.
///
/// The SNI ToolTip can only be updated through that same C call, and omarchy's
/// tray shows the item's ToolTip on hover (Quickshell refreshes it only on
/// the NewToolTip signal), so the app cannot simply stop updating it. Instead
/// it only ever makes that call against an AppIndicator that has not been
/// through a fallback cycle: never while the watcher is absent, and the icon
/// is rebuilt (a fresh AppIndicator) after any absence that could have armed
/// the bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraySafety {
    /// No watcher on the bus — do not call the tooltip setter.
    WatcherAbsent,
    /// The watcher is (back) up but the current icon may be poisoned; it must
    /// be rebuilt before tooltips resume.
    RebuildPending,
    /// The watcher is up and the current icon was built after the last
    /// absence — safe to update the tooltip.
    Clean,
}

impl TraySafety {
    fn to_u8(self) -> u8 {
        match self {
            TraySafety::WatcherAbsent => 0,
            TraySafety::RebuildPending => 1,
            TraySafety::Clean => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => TraySafety::RebuildPending,
            2 => TraySafety::Clean,
            _ => TraySafety::WatcherAbsent,
        }
    }

    /// Whether a StatusNotifier host owns the watcher name, i.e. the tray
    /// icon has somewhere to appear. Only [`TraySafety::WatcherAbsent`] means
    /// no host; the other two both imply the watcher is up. Used to decide
    /// whether hiding the window on close (close-to-tray) is safe: with no
    /// host there is no icon to restore the window from.
    fn host_present(self) -> bool {
        !matches!(self, TraySafety::WatcherAbsent)
    }
}

/// Shared, thread-safe state coordinating tray-icon rebuilds (see
/// [`TraySafety`]) and change-gated repaints.
pub(crate) struct TrayGuard {
    safety: AtomicU8,
    /// The visual the current tray icon fully shows (tooltip included). None
    /// while the icon is fresh or predates the last watcher change.
    shown: Mutex<Option<TrayVisual>>,
}

impl TrayGuard {
    fn new() -> Self {
        Self {
            // Assume no watcher until the monitor thread proves otherwise, so
            // the startup repaint never races a mid-startup fallback.
            safety: AtomicU8::new(TraySafety::WatcherAbsent.to_u8()),
            shown: Mutex::new(None),
        }
    }

    fn safety(&self) -> TraySafety {
        TraySafety::from_u8(self.safety.load(Ordering::Acquire))
    }

    fn set_safety(&self, safety: TraySafety) {
        self.safety.store(safety.to_u8(), Ordering::Release);
    }
}

/// Whether the current tray icon still needs a repaint for `visual`: its
/// recorded "fully shown" state differs from what we are asked to show. The
/// state is only recorded once a repaint applied the tooltip as well, so a
/// tooltip that had to be skipped while the watcher was absent always leaves
/// the icon "not fully shown" and is repainted once tooltips are safe again.
fn needs_tray_repaint(shown: Option<TrayVisual>, visual: TrayVisual) -> bool {
    shown != Some(visual)
}

/// The libappindicator tooltip setter may only run while the tray icon is
/// known to have been built after the last watcher change and never to have
/// been through a fallback cycle — exactly [`TraySafety::Clean`] (see the
/// `TraySafety` docs for why).
fn tooltip_safe(safety: TraySafety) -> bool {
    matches!(safety, TraySafety::Clean)
}

/// Set the tray icon (grey envelope with no unread mail, blue otherwise)
/// and its tooltip, which follows the unread count. Both the StatusNotifierItem
/// Title and ToolTip are updated (the vendored tray-icon patches make
/// set_title/set_tooltip work on Linux); tray hosts like omarchy prefer the
/// ToolTip.
///
/// Change-gated on [`TrayVisual`], and the tooltip setter is only reached
/// while [`TraySafety::Clean`] (see [`TrayGuard`]). Safe to call from any
/// thread: tauri marshals the tray calls onto the main thread.
pub(crate) fn update_tray_icon(app: &tauri::AppHandle) {
    let tray_guard = app.state::<AppState>().tray_guard.clone();

    let unread = total_unread(app);
    let visual = TrayVisual {
        unread,
        tint: tray_tint(unread),
    };

    // The current icon already fully shows this state.
    if !needs_tray_repaint(*tray_guard.shown.lock().unwrap(), visual) {
        return;
    }

    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };

    let tooltip = tray_tooltip(unread);
    let _ = tray.set_title(Some(tooltip.clone()));
    let _ = tray.set_icon(Some(tray_icon(unread)));

    // The one call that can crash on a poisoned indicator (see TraySafety).
    if tooltip_safe(tray_guard.safety()) {
        let _ = tray.set_tooltip(Some(tooltip));
        // Only remember the state once the tooltip landed, so a later
        // transition to Clean repaints a tooltip that had to be skipped.
        *tray_guard.shown.lock().unwrap() = Some(visual);
    }
}

/// Whether we are running under the Hyprland compositor (e.g. Omarchy).
/// Hyprland never draws title bars itself — windows run undecorated and are
/// moved/resized with its SUPER+drag bindings — so an app-drawn title bar
/// sticks out against every other window there. Detected from the
/// environment Hyprland injects into every client.
fn is_hyprland() -> bool {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok()
        || std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_lowercase().contains("hyprland"))
            .unwrap_or(false)
}

/// Realign the client-side title bar's buttons with the desktop's configured
/// layout.
///
/// On Wayland tao gives every window its own title bar — a stock
/// `GtkHeaderBar` — and pins its button layout to the literal
/// `"menu:minimize,maximize,close"`. That overrides the desktop's
/// `gtk-decoration-layout` setting (on Fedora/GNOME it is
/// `"menu:minimize,close"`), so the window sprouts a Maximize button the rest
/// of the desktop does not show and looks out of place.
///
/// Clearing the header bar's layout hands the choice back to GTK, which then
/// reads `gtk-decoration-layout` — the same setting every other GTK app
/// honours — and keeps following it if the user rearranges the buttons. It is
/// a no-op on X11, where GTK leaves decorations to the window manager (which
/// already follows the setting), and wherever tao installed no title bar.
#[cfg(target_os = "linux")]
fn adopt_system_decoration_layout(window: &tauri::WebviewWindow) {
    use gtk::prelude::*;

    let Ok(gtk_window) = window.gtk_window() else {
        return;
    };
    let Some(titlebar) = gtk_window.titlebar() else {
        return;
    };
    match find_header_bar(&titlebar) {
        Some(header) => {
            // A NULL layout sets decoration-layout-set = FALSE, which makes the
            // header bar fall back to the gtk-decoration-layout setting (see
            // gtk_header_bar_set_decoration_layout).
            header.set_decoration_layout(None);
            log::info!("title bar now follows the system decoration layout");
        }
        None => log::debug!("no client-side title bar to adjust"),
    }
}

/// The first `GtkHeaderBar` in `widget`'s subtree (inclusive), if any. tao
/// wraps its header bar in a `GtkEventBox`, so it is reached by descending
/// through the title bar's children rather than assumed to be the direct
/// child.
#[cfg(target_os = "linux")]
fn find_header_bar(widget: &gtk::Widget) -> Option<gtk::HeaderBar> {
    use gtk::prelude::*;

    if let Some(header) = widget.downcast_ref::<gtk::HeaderBar>() {
        return Some(header.clone());
    }
    widget
        .downcast_ref::<gtk::Container>()?
        .children()
        .iter()
        .find_map(find_header_bar)
}

/// Whether the shared library the Linux tray backend dlopens is present.
///
/// `libappindicator-sys` `dlopen`s one of these sonames and **panics** from a
/// `Lazy` initializer when none is found — which aborts the whole app during
/// startup. Some runtimes ship WebKitGTK/GTK but no appindicator library (for
/// example the Flatpak `org.gnome.Platform` runtime), so probe for it first
/// and run tray-less there instead of crashing. The order mirrors
/// `libappindicator-sys`: the Ayatana fork first, then the legacy library;
/// the `.so.1` soname first, then the unversioned backcompat name.
fn appindicator_available() -> bool {
    const SONAMES: [&str; 4] = [
        "libayatana-appindicator3.so.1",
        "libappindicator3.so.1",
        "libayatana-appindicator3.so",
        "libappindicator3.so",
    ];
    for name in SONAMES {
        if let Ok(lib) = unsafe { libloading::Library::new(name) } {
            // Keep it loaded so the tray backend reuses this mapping rather
            // than reloading (and so a later probe cannot unload it mid-use).
            std::mem::forget(lib);
            return true;
        }
    }
    false
}

/// Create the tray icon with its menu and wire "Open Sufi Email" / "Quit",
/// then start the monitor that keeps the icon safe from libayatana-
/// appindicator's tooltip use-after-free (see [`TraySafety`]).
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    build_tray(app)?;
    let tray_guard = app.state::<AppState>().tray_guard.clone();
    spawn_tray_watcher_monitor(app.clone(), tray_guard);
    Ok(())
}

/// (Re)create the tray icon with its menu. Each call builds a fresh menu and
/// a fresh libappindicator object — a new object is the only way to drop a
/// tooltip handler left dangling by an earlier fallback cycle (see
/// [`TraySafety`]). Must run on the main thread.
///
/// Note: on Linux the tray backend (libappindicator) does not deliver icon
/// click events — the host shows the menu on click, so "Open Sufi Email" is
/// the reliable way to open the window there. The left-click handler covers
/// platforms/hosts that do report clicks.
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open Sufi Email", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let tray = TrayIconBuilder::with_id("main-tray")
        .icon(tray_icon(total_unread(app)))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    // The Linux backend ignores set_tooltip; the host shows the StatusNotifier
    // title instead, so set both. Safe here: this icon is brand new.
    let _ = tray.set_tooltip(Some("Sufi Email"));
    let _ = tray.set_title(Some("Sufi Email"));
    Ok(())
}

/// Remove the current tray icon and build a fresh one, then repaint it with
/// the latest state. Must run on the main thread; see [`TraySafety`].
fn rebuild_tray(app: &tauri::AppHandle) {
    app.remove_tray_by_id("main-tray");
    if let Err(e) = build_tray(app) {
        log::error!("failed to rebuild tray icon: {e}");
    }
    let tray_guard = &app.state::<AppState>().tray_guard;
    tray_guard.set_safety(TraySafety::Clean);
    force_repaint(app, tray_guard);
}

/// Force `update_tray_icon` to repaint even when the visual is unchanged.
/// Safe from any thread: the actual tray calls are marshalled to the main
/// thread by tauri.
fn force_repaint(app: &tauri::AppHandle, tray_guard: &TrayGuard) {
    *tray_guard.shown.lock().unwrap() = None;
    update_tray_icon(app);
}

/// True while something owns the StatusNotifierWatcher name on the session
/// bus (the omarchy-shell tray host).
fn watcher_present(conn: &zbus::blocking::Connection) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &("org.kde.StatusNotifierWatcher",),
    )
    .map(|reply| reply.body().deserialize::<bool>().unwrap_or(false))
    .unwrap_or(false)
}

/// What the tray watcher monitor must do after sampling the bus, decided by
/// the pure function [`watcher_action`]. Kept separate from the monitor loop
/// so the safety invariants can be unit-tested (see `main_tests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherAction {
    /// The watcher is (or just went) absent: tooltip updates must stop.
    Suppress,
    /// The watcher came back after an absence: the current AppIndicator may
    /// carry a dangling tooltip handler, so tooltips stay locked until the
    /// icon is rebuilt (see [`rebuild_tray`]).
    RebuildPending,
    /// First sighting of the watcher and it is up: the icon created during
    /// setup predates this monitor, is assumed clean, so tooltips unlock and
    /// the pending repaint is flushed without a rebuild.
    UnlockClean,
    /// Nothing changed; keep the previous state.
    Nothing,
}

/// Pure decision table behind the tray watcher monitor: given the last known
/// watcher state `watcher_up` (`None` = not sampled yet) and a fresh
/// `present` sample, what must happen next?
fn watcher_action(watcher_up: Option<bool>, present: bool) -> WatcherAction {
    match (watcher_up, present) {
        (None, true) => WatcherAction::UnlockClean,
        (None, false) => WatcherAction::Suppress,
        (Some(false), true) => WatcherAction::RebuildPending,
        (Some(true), false) => WatcherAction::Suppress,
        _ => WatcherAction::Nothing,
    }
}

/// Watch the StatusNotifierWatcher and keep the tray icon safe from
/// libayatana-appindicator's tooltip use-after-free (see [`TraySafety`]).
///
/// Polling cadence: the bug is only armed when the watcher stays absent long
/// enough (~100 ms) for the library to create its XEmbed fallback icon and
/// then comes back, freeing the icon behind the still-connected tooltip
/// handler. Polling every 50 ms guarantees any such absence is observed as an
/// "absent" sample, which suppresses tooltip updates until the icon has been
/// rebuilt after the watcher returns.
fn spawn_tray_watcher_monitor(app: tauri::AppHandle, tray_guard: Arc<TrayGuard>) {
    std::thread::spawn(move || {
        // The session bus may not be reachable yet at login; keep retrying.
        let conn = loop {
            match zbus::blocking::Connection::session() {
                Ok(conn) => break conn,
                Err(e) => {
                    log::warn!("tray watcher monitor: session bus unavailable: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        };

        // None = not sampled yet, Some(present) = last known state.
        let mut watcher_up: Option<bool> = None;
        loop {
            let present = watcher_present(&conn);
            match watcher_action(watcher_up, present) {
                WatcherAction::UnlockClean => {
                    // First sighting while up: the icon was just created
                    // during setup; if the watcher is already up it was up
                    // when the icon was made (no fallback cycle possible), so
                    // unlock tooltips and flush what the pre-watcher repaint
                    // had to skip.
                    watcher_up = Some(true);
                    tray_guard.set_safety(TraySafety::Clean);
                    force_repaint(&app, &tray_guard);
                }
                WatcherAction::Suppress => {
                    // The watcher went away (or was never seen): tooltips are
                    // unsafe until it is back and the icon has been rebuilt.
                    watcher_up = Some(present);
                    tray_guard.set_safety(TraySafety::WatcherAbsent);
                }
                WatcherAction::RebuildPending => {
                    // The watcher came back after an absence: the current
                    // icon may be poisoned — replace it before any tooltip
                    // update can run.
                    watcher_up = Some(true);
                    tray_guard.set_safety(TraySafety::RebuildPending);
                    let app2 = app.clone();
                    if let Err(e) = app.run_on_main_thread(move || rebuild_tray(&app2)) {
                        log::error!("tray watcher monitor: could not schedule rebuild: {e}");
                    }
                }
                WatcherAction::Nothing => {}
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

/// Show and focus the main window (used by the tray's left-click and the
/// "Open Sufi Email" menu item).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// The window operations the size helpers need. Tauri's `Window` (delivered to
/// window events) and `WebviewWindow` (returned by `get_webview_window`) expose
/// the same methods but share no trait, so bridge them here to keep one code
/// path for both.
trait WindowGeometry {
    fn geo_logical_inner_size(&self) -> Option<(u32, u32)>;
    fn geo_is_maximized(&self) -> bool;
    fn geo_is_fullscreen(&self) -> bool;
    fn geo_monitor_bounds(&self) -> Option<(u32, u32)>;
    fn geo_set_size(&self, width: u32, height: u32) -> tauri::Result<()>;
}

macro_rules! impl_window_geometry {
    ($t:ty) => {
        impl WindowGeometry for $t {
            fn geo_logical_inner_size(&self) -> Option<(u32, u32)> {
                let scale = self.scale_factor().unwrap_or(1.0);
                let logical = self.inner_size().ok()?.to_logical::<u32>(scale);
                Some((logical.width, logical.height))
            }
            fn geo_is_maximized(&self) -> bool {
                self.is_maximized().unwrap_or(false)
            }
            fn geo_is_fullscreen(&self) -> bool {
                self.is_fullscreen().unwrap_or(false)
            }
            fn geo_monitor_bounds(&self) -> Option<(u32, u32)> {
                let monitor = self.current_monitor().ok().flatten()?;
                let logical = monitor.size().to_logical::<u32>(monitor.scale_factor());
                Some((logical.width, logical.height))
            }
            fn geo_set_size(&self, width: u32, height: u32) -> tauri::Result<()> {
                self.set_size(tauri::LogicalSize::new(width as f64, height as f64))
            }
        }
    };
}
impl_window_geometry!(tauri::Window);
impl_window_geometry!(tauri::WebviewWindow);

/// A remembered window size that has not appeared on screen yet. `desired` is
/// the size we want to end up at; `last_request` is what we most recently
/// asked `set_size` for, used to cancel any constant offset between the
/// requested and the resulting size (Wayland/CSD can add a decorations delta,
/// so asking for `desired` yields `desired + offset`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingWindowRestore {
    desired: (u32, u32),
    last_request: (u32, u32),
    attempts: u8,
    /// Give up after this instant, so a startup restore that never resolves
    /// can't keep fighting a resize the user makes later.
    deadline: std::time::Instant,
}

/// The window's current size in logical pixels, or `None` while it is
/// maximized/fullscreen. Those sizes describe the screen, not a size the user
/// chose, so they are not remembered (the last normal size is kept instead).
fn savable_window_size<W: WindowGeometry>(window: &W) -> Option<(u32, u32)> {
    if window.geo_is_maximized() || window.geo_is_fullscreen() {
        return None;
    }
    window.geo_logical_inner_size()
}

/// Resize a window to a remembered size, clamped to the current monitor so a
/// size saved on a larger display never opens bigger than the screen it
/// reappears on.
fn apply_saved_window_size<W: WindowGeometry>(window: &W, width: u32, height: u32) {
    let (mut width, mut height) = (width, height);
    if let Some((monitor_width, monitor_height)) = window.geo_monitor_bounds() {
        width = width.min(monitor_width);
        height = height.min(monitor_height);
    }
    if let Err(e) = window.geo_set_size(width, height) {
        log::warn!("window: set_size({width}x{height}) failed: {e}");
    }
}

/// The next `set_size` request that cancels whatever constant offset sits
/// between the size we asked for (`last_request`) and the size we actually got
/// (`actual`), aiming at `desired`. Pure so the correction is unit-testable.
/// Clamped to a sane minimum so a wild report can't produce a degenerate
/// window.
fn corrected_request(
    desired: (u32, u32),
    last_request: (u32, u32),
    actual: (u32, u32),
) -> (u32, u32) {
    let dx = actual.0 as i64 - last_request.0 as i64;
    let dy = actual.1 as i64 - last_request.1 as i64;
    (
        (desired.0 as i64 - dx).max(200) as u32,
        (desired.1 as i64 - dy).max(200) as u32,
    )
}

/// What the pending-restore state machine wants done with an observed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreStep {
    /// No restore in progress: persist the size normally.
    Idle,
    /// The size matched the one being restored: persist normally.
    Confirmed,
    /// Ask the window for this size and keep the saved value for now.
    Request((u32, u32)),
    /// Ran out of attempts or time; give up on the desired size.
    GaveUp((u32, u32)),
}

/// Advance the pending-restore state machine for one observed window size.
/// Pure (no window access, no locking) so the confirm/retry/give-up behaviour
/// is unit-testable; returns the new pending state and the action to take.
fn restore_step(
    pending: Option<PendingWindowRestore>,
    current: (u32, u32),
    now: std::time::Instant,
) -> (Option<PendingWindowRestore>, RestoreStep) {
    match pending {
        None => (None, RestoreStep::Idle),
        Some(p) if current == p.desired => (None, RestoreStep::Confirmed),
        Some(p) if p.attempts > 0 && now <= p.deadline => {
            let request = corrected_request(p.desired, p.last_request, current);
            let next = PendingWindowRestore {
                desired: p.desired,
                last_request: request,
                attempts: p.attempts - 1,
                deadline: p.deadline,
            };
            (Some(next), RestoreStep::Request(request))
        }
        Some(p) => (None, RestoreStep::GaveUp(p.desired)),
    }
}

/// Block until a size arrives, then keep absorbing sizes until the stream has
/// been quiet for `quiet`, returning the last one seen. `None` once the sender
/// is gone. This is the debounce behind the window-size writer, split out so
/// it can be tested without a real window.
fn coalesce_sizes(
    rx: &std::sync::mpsc::Receiver<(u32, u32)>,
    quiet: Duration,
) -> Option<(u32, u32)> {
    let (mut width, mut height) = rx.recv().ok()?;
    loop {
        match rx.recv_timeout(quiet) {
            Ok((w, h)) => {
                width = w;
                height = h;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Some((width, height))
}

/// Persist the window size into `config.toml`. No-op when the value is
/// unchanged, so the debounced resize writer does not rewrite the file on
/// every quiet cycle.
fn save_window_size(app: &tauri::AppHandle, width: u32, height: u32) {
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    let next = Some(account::WindowSize { width, height });
    if cfg.window == next {
        return;
    }
    cfg.window = next;
    if let Err(e) = cfg.save() {
        log::warn!("failed to save window size: {e}");
    }
}


pub struct AppState {
    pub config: Mutex<Config>,
    /// Accounts that currently have a live watcher thread (IDLE or slow
    /// poll), keyed by account name and mapped to the watcher's stop flag.
    /// Used to avoid spawning duplicate watchers when new accounts are
    /// added at runtime, and so delete_account can stop a watcher before
    /// removing the account's local cache.
    pub idle_watchers: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Coordinates tray-icon rebuilds around libayatana-appindicator's
    /// tooltip use-after-free (see `TraySafety`) and gates tray repaints.
    pub(crate) tray_guard: Arc<TrayGuard>,
    /// A remembered window size that has not been confirmed on screen yet.
    /// While set, resize events re-apply the target and are not persisted, so
    /// the compositor's decorations-offset startup size cannot overwrite the
    /// saved one.
    pub(crate) pending_window_restore: Mutex<Option<PendingWindowRestore>>,
}

/// Fire the silent new-mail toast through the OS notification daemon.
/// Visual only: no sound hint is set (the freedesktop spec is silent by
/// default; `.silent()` makes that explicit on platforms that support it).
/// Shows the sender and subject when available; `extra` is the number of
/// additional new messages beyond the one described.
///
/// Runs on a plain detached thread on purpose: notify-rust's zbus blocking
/// path must not run on a tokio worker thread. With the tokio feature
/// (pulled in transitively by the dialog plugin's xdg-portal backend),
/// zbus::block_on drives its own static runtime, and Runtime::block_on
/// panics with "Cannot start a runtime from within a runtime" on a thread
/// already inside tauri's async runtime — which is exactly where the
/// notification plugin's internal spawn used to run `show()`, silently
/// killing every new-mail toast.
fn fire_new_mail_notification(from: &str, subject: &str, extra: usize) {
    let title = if from.is_empty() {
        "New email".to_string()
    } else {
        format!("New email from {from}")
    };
    let mut body = if subject.is_empty() {
        "You have a new email.".to_string()
    } else {
        subject.to_string()
    };
    if extra > 0 {
        body.push_str(&format!("  (+{extra} more)"));
    }
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new()
            .appname("sufi-email")
            .summary(&title)
            .body(&body)
            .show()
        {
            log::warn!("new-mail notification failed: {e}");
        }
    });
}

/// Accounts for the UI — a sanitized view without the sealed passwords.
#[tauri::command]
fn get_accounts(state: State<AppState>) -> Vec<AccountInfo> {
    state
        .config
        .lock()
        .unwrap()
        .accounts
        .iter()
        .map(AccountInfo::from)
        .collect()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccount {
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub username: String,
    pub password: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_username: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,
    #[serde(default)]
    pub smtp_starttls: bool,
}

impl From<NewAccount> for AccountConfig {
    fn from(n: NewAccount) -> Self {
        // Passwords are sealed (AES-256-GCM) before ever touching the disk.
        let password = crypto::seal_password(&n.password).unwrap_or_default();
        let smtp_password = n
            .smtp_password
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| crypto::seal_password(p).unwrap_or_default());
        Self {
            name: n.name,
            email: n.email,
            imap_host: n.imap_host,
            imap_port: n.imap_port,
            smtp_host: n.smtp_host,
            smtp_port: n.smtp_port,
            username: n.username,
            password,
            smtp_username: n.smtp_username,
            smtp_password,
            smtp_starttls: n.smtp_starttls,
        }
    }
}

/// Validate + save a new account. Tests IMAP login and SMTP auth first so
/// broken credentials are never persisted.
#[tauri::command]
async fn add_account(
    account: NewAccount,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let acc: AccountConfig = account.into();

    mail::test_imap(&acc).await?;
    mail::test_smtp(&acc).await?;

    let mut cfg = state.config.lock().unwrap();
    if cfg.accounts.iter().any(|a| a.name == acc.name) {
        return Err(format!("an account named '{}' already exists", acc.name));
    }
    cfg.accounts.push(acc);
    let result = cfg.save();
    drop(cfg);
    result?;

    // Give the new account its own background watcher right away.
    spawn_missing_idle_watchers(&app);
    Ok(())
}

#[tauri::command]
async fn delete_account(
    account: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    let removed = cfg.accounts.iter().find(|a| a.name == account).cloned();
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.name != account);
    if cfg.accounts.len() == before {
        return Err(format!("no account named '{account}'"));
    }
    let result = cfg.save();
    drop(cfg);
    result?;

    // Stop the account's background watcher (if any) and deregister it
    // BEFORE removing the cache. Otherwise a watcher mid-cycle could
    // re-create the local database after it was deleted (the watcher
    // re-checks the config and its stop flag at every opportunity and
    // exits within one IDLE keepalive interval at most).
    if let Some(stop) = state.idle_watchers.lock().unwrap().remove(&account) {
        stop.store(true, Ordering::Release);
    }

    // Remove the account's local mail cache (best effort).
    if let Some(acc) = removed {
        store::Store::remove_account_db(&acc);
    }
    Ok(())
}

/// Serve the cached folder list instantly; refresh from the server in the
/// background and push the fresh list over the channel when it arrives.
#[tauri::command]
async fn list_folders(
    account: String,
    on_refresh: tauri::ipc::Channel<(Vec<mail::Folder>, bool)>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<mail::Folder>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // 1. Cached list, returned immediately (works offline).
    let cached = {
        let store = store::Store::open(&acc)?;
        store.load_folders()?
    };

    // 2. Server refresh in the background; UI gets the result via channel.
    let acc2 = acc.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = match mail::list_folders(&acc2).await {
            Ok(fresh) => {
                if let Ok(mut store) = store::Store::open(&acc2) {
                    let _ = store.store_folders(&fresh);
                }
                // Tray icon follows unread mail (blue when anything is
                // unread, grey otherwise).
                update_tray_icon(&app2);
                // A successful round trip proves we are online — push any
                // offline read/unread changes to the server.
                let app3 = app2.clone();
                tauri::async_runtime::spawn_blocking(move || flush_pending_flags(&app3));
                Ok(fresh)
            }
            Err(e) => {
                log::warn!("folder refresh failed (offline?): {e}");
                Err(e)
            }
        };
        if let Err(e) = on_refresh.send(match result {
            Ok(fresh) => (fresh, true),
            Err(_) => (Vec::new(), false),
        }) {
            log::warn!("folder refresh channel send failed: {e}");
        }
    });

    Ok(cached)
}

#[tauri::command]
async fn list_messages(
    account: String,
    folder: String,
    on_batch: tauri::ipc::Channel<Vec<mail::MessageSummary>>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // 1. Instantly serve whatever we have cached locally — bounded to the
    //    newest 200 (the same window the server stream uses), so large
    //    folders (e.g. Trash with thousands of rows) show a navigable list
    //    instead of every cached message.
    {
        let store = store::Store::open(&acc)?;
        // load_summaries already hides rows under an in-flight optimistic
        // delete; the pending-delete filter is belt and braces here.
        let mut cached = store.load_summaries(&folder)?;
        if cached.len() > 200 {
            cached.sort_by(|a, b| b.date.cmp(&a.date));
            cached.truncate(200);
        }
        if !cached.is_empty() {
            on_batch.send(cached).map_err(|e| e.to_string())?;
        }
    }

    // 2. Refresh from the server, streaming batches; each batch is also
    //    persisted so the cache stays warm for the next launch. The final
    //    on_reconcile callback receives the server's full UID set — capture
    //    it so we can diff it against the notified set afterwards.
    let acc2 = acc.clone();
    let folder2 = folder.clone();
    let store_channel = on_batch.clone();
    let acc3 = acc.clone();
    let folder3 = folder.clone();
    let result = mail::list_messages_streamed(
        &acc,
        &folder,
        move |mut batch, mut bodies| {
            if let Ok(mut s) = store::Store::open(&acc2) {
                // Messages with an in-flight optimistic delete are still on
                // the server mid-move; drop them from the outgoing batch so
                // the UI doesn't flash them back for a moment.
                if let Ok(pending) = s.pending_delete_uids(&folder2) {
                    if !pending.is_empty() {
                        let mut kept = Vec::with_capacity(batch.len());
                        let mut kept_bodies = Vec::with_capacity(bodies.len());
                        for (m, b) in batch.drain(..).zip(bodies.drain(..)) {
                            if !pending.contains(&m.uid) {
                                kept.push(m);
                                kept_bodies.push(b);
                            }
                        }
                        batch = kept;
                        bodies = kept_bodies;
                    }
                }
                if let Err(e) = s.upsert_summaries(&folder2, &batch) {
                    log::warn!("cache upsert failed: {e}");
                }
                // Warm the body cache: the full bodies were downloaded to
                // build the snippets, so store them and make opening any
                // listed message a cache hit.
                if let Err(e) = s.store_bodies(&folder2, &bodies) {
                    log::warn!("body cache write failed: {e}");
                }
                // Apply locally-pending seen states to the outgoing batch so
                // the UI doesn't flash a just-marked message back to unread
                // while the server STORE is still in flight.
                if let Ok(pending) = s.pending_seen_map(&folder2) {
                    for m in batch.iter_mut() {
                        if let Some(seen) = pending.get(&m.uid) {
                            m.seen = *seen;
                        }
                    }
                }
            }
            let _ = store_channel.send(batch);
        },
        move |server_uids| {
            // Remove cache rows for UIDs that vanished from the server.
            if let Ok(mut s) = store::Store::open(&acc3) {
                match s.remove_uids_not_in(&folder3, &server_uids) {
                    Ok(n) if n > 0 => log::info!(
                        "reconciled {folder3}: removed {n} stale cache row(s)"
                    ),
                    Ok(_) => {}
                    Err(e) => log::warn!("reconcile failed: {e}"),
                }
                // Pending offline flag changes for vanished messages have
                // nothing left to sync.
                if let Err(e) = s.remove_pending_uids_not_in(&folder3, &server_uids) {
                    log::warn!("pending-flag reconcile failed: {e}");
                }
                // The server delete for an optimistically-deleted message
                // finished: its UID is gone, so drop the shield.
                if let Err(e) = s.clear_pending_deletes_not_in(&folder3, &server_uids) {
                    log::warn!("pending-delete reconcile failed: {e}");
                }
            }
        },
    )
    .await;

    // Report connectivity so the ~offline tag tracks the real state.
    match &result {
        // No new-mail notification here: the background IDLE watcher is the
        // notifier (it watches INBOX, where new mail arrives). Detecting
        // "new" UIDs in an arbitrary fetched folder would fire toasts for
        // messages merely moved into it (e.g. an unread message deleted to
        // Trash), which is not new mail.
        Ok(()) => Ok(()),
        Err(e) => {
            // Serve-from-cache already happened; report offline instead of
            // failing so the UI can show the tag.
            log::warn!("message refresh failed (offline?): {e}");
            Err(e.clone())
        }
    }
}

#[tauri::command]
async fn mark_message(
    account: String,
    folder: String,
    uid: u32,
    seen: bool,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<u32, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    // Apply to the local cache first: offline, reading a message still
    // marks it read locally, and the change is queued for the server.
    let mut store = store::Store::open(&acc)?;
    store.set_seen(&folder, uid, seen)?;
    // Record the change as pending BEFORE the server call: a list refresh
    // during the SMTP/IMAP round trip would otherwise overwrite it with
    // the server's still-stale flag and flash the message back to unread.
    store.upsert_pending_flag(&folder, uid, seen)?;

    match mail::set_seen(&acc, &folder, uid, seen).await {
        Ok(server_unread) => {
            // Synced: nothing pending for this message anymore, and the
            // badge reflects the server's authoritative count.
            store.remove_pending_flag(&folder, uid)?;
            store.set_folder_unread(&folder, server_unread)?;
            Ok(server_unread)
        }
        Err(e) => {
            // Offline: queue the change; the poller flushes the queue once
            // a connection is back. Badge gets a best-effort local count,
            // corrected by the next online folder refresh.
            log::warn!("mark {folder}/{uid} seen={seen} failed (queued): {e}");
            store.upsert_pending_flag(&folder, uid, seen)?;
            let local_unread = store
                .load_summaries(&folder)?
                .iter()
                .filter(|m| !m.seen)
                .count() as u32;
            store.set_folder_unread(&folder, local_unread)?;
            Ok(local_unread)
        }
    }
    .map(|unread| {
        // The store's folder unread count was just updated above, so
        // repaint the tray icon right away — reading the last unread
        // message should gray the icon without waiting for a folder
        // refresh.
        update_tray_icon(&app);
        unread
    })
}

#[tauri::command]
async fn fetch_message(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<mail::MessageBody, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // Serve from cache when we already have the body AND its recipients
    // (the cache warm-up path stores bodies without recipients; the first
    // server fetch fills them in for Reply All).
    {
        let store = store::Store::open(&acc)?;
        if let Some((text, html)) = store.load_body(&folder, uid)? {
            if let Some((to, cc)) = store.load_recipients(&folder, uid)? {
                return Ok(mail::MessageBody { uid, text, html, to, cc });
            }
        }
    }

    // Not cached (or recipients unknown): fetch the full message, then
    // persist both the body and the recipients.
    let fetched = mail::fetch_message_full(&acc, &folder, uid).await?;
    let mut store = store::Store::open(&acc)?;
    store.store_body(
        &folder,
        uid,
        fetched.body.text.as_deref(),
        fetched.body.html.as_deref(),
        &fetched.attachments,
    )?;
    store.store_recipients(&folder, uid, &fetched.body.to, &fetched.body.cc)?;
    Ok(fetched.body)
}

/// Optimistic move, part 1: remove the message from the UI + shield the
/// cached row right away (fast local call), mirroring delete. The server
/// move runs in the background via [`move_message_server`] and relocates
/// the row to the destination.
#[tauri::command]
async fn move_message_local(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let mut store = store::Store::open(&acc)?;
    store.mark_pending_delete(&folder, uid)
}

/// Optimistic move, part 2: the server-side move, run in the background.
/// On success the cached row is relocated to the destination (so it shows
/// up there immediately) and recorded as already notified — a server move
/// looks like a brand-new UID to the IDLE watcher, which would otherwise
/// toast "new email" for a message the user just moved.
#[tauri::command]
async fn move_message_server(
    account: String,
    folder: String,
    uid: u32,
    dest_folder: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    let result: Result<Option<u32>, String> =
        mail::move_message(&acc, &folder, uid, &dest_folder).await;

    if let Ok(new_uid) = &result {
        if let Ok(mut store) = store::Store::open(&acc) {
            if let Some(new_uid) = new_uid {
                let _ = store.relocate_message(&folder, uid, &dest_folder, *new_uid);
                let _ = store.mark_notified(&dest_folder, &[*new_uid]);
            }
            let _ = store.delete_message(&folder, uid);
            let _ = store.clear_pending_delete(&folder, uid);
        }
    } else {
        // Failure: unshield the row so the next folder sync re-syncs it
        // (the message never left the source folder on the server).
        if let Ok(mut store) = store::Store::open(&acc) {
            let _ = store.clear_pending_delete(&folder, uid);
        }
    }

    // Refresh the UI: a user sitting in the destination folder sees the
    // moved message appear there right away.
    if result.is_ok() {
        let _ = app.emit("mail-refresh", ());
    }
    result.map(|_| ())
}

/// Optimistic delete, part 1: remove the message from the local cache
/// immediately (UI + store). Returns fast; the server call follows in the
/// background via [`delete_message_server`].
#[tauri::command]
async fn delete_message_local(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let mut store = store::Store::open(&acc)?;
    // Keep the row (hidden from lists by the pending-delete marker) so the
    // background delete_message_server can relocate it into Trash's cache
    // with the copy's new UID — deleting it here would leave Trash without
    // the message until the next server sync.
    store.mark_pending_delete(&folder, uid)
}

/// Optimistic delete, part 2: the actual server-side delete (move to Trash,
/// or \Deleted + expunge when there is no Trash). Runs in the background;
/// a failure surfaces in the UI and the message reappears on the next
/// folder sync (the reconcile re-adds UIDs still on the server). On success
/// the cached row is relocated into Trash so the deleted message is already
/// there when the user opens it.
#[tauri::command]
async fn delete_message_server(
    account: String,
    folder: String,
    uid: u32,
    trash_folder: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    let result: Result<Option<u32>, String> = (|| async {
        match trash_folder.clone() {
            // Move to Trash when the server has one (or the UI found one).
            // move_message returns the copy's new UID in Trash.
            Some(trash) if trash != folder => {
                mail::move_message(&acc, &folder, uid, &trash).await
            }
            // No Trash folder: fall back to plain IMAP delete (\Deleted + expunge).
            _ => {
                mail::delete_message(&acc, &folder, uid).await?;
                Ok(None)
            }
        }
    })()
    .await;

    if let Ok(mut store) = store::Store::open(&acc) {
        match &result {
            // Moved to Trash: re-point the cached row to the copy's new
            // UID so the deleted message shows up in Trash immediately.
            Ok(Some(new_uid)) => {
                if let Some(trash) = trash_folder.as_deref() {
                    let _ = store.relocate_message(&folder, uid, trash, *new_uid);
                }
                let _ = store.delete_message(&folder, uid);
            }
            // Permanent delete (no Trash): drop the cached row.
            Ok(None) => {
                let _ = store.delete_message(&folder, uid);
            }
            // Failure: keep the row (still shielded by pending_delete until
            // the next folder sync re-syncs it — the existing rollback).
            Err(_) => {}
        }
        // The delete is settled either way: success means the message is gone
        // from the server; failure means the next sync re-adds it (rollback).
        // Either way it must no longer be shielded from refreshes.
        let _ = store.clear_pending_delete(&folder, uid);
    }

    // Refresh the UI (a user sitting in Trash sees the deleted message
    // appear there right away instead of on the next manual refresh).
    if result.is_ok() {
        let _ = app.emit("mail-refresh", ());
    }
    result.map(|_| ())
}

#[derive(serde::Serialize)]
struct AttachmentInfo {
    filename: String,
    #[serde(rename = "contentType")]
    content_type: String,
    size: i64,
    part_id: String,
    #[serde(rename = "contentId")]
    content_id: Option<String>,
}

#[tauri::command]
async fn list_attachments(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<Vec<AttachmentInfo>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let store = store::Store::open(&acc)?;
    Ok(store
        .load_attachments(&folder, uid)?
        .into_iter()
        .map(|a| AttachmentInfo {
            filename: a.filename,
            content_type: a.content_type,
            size: a.size,
            part_id: a.part_id,
            content_id: a.content_id,
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct SendArgs {
    account: String,
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    subject: String,
    body: String,
    #[serde(default)]
    attachments: Vec<String>,
    /// Message-ID being replied to (sets In-Reply-To, keeps threads linked).
    #[serde(default)]
    in_reply_to: Option<String>,
    /// The thread's References chain (space-joined message-ids).
    #[serde(default)]
    references: Option<String>,
}

#[tauri::command]
async fn send_email(args: SendArgs, state: State<'_, AppState>) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == args.account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{}'", args.account))?;
    mail::send_email(
        &acc,
        args.to,
        args.cc,
        &args.subject,
        &args.body,
        args.attachments,
        args.in_reply_to,
        args.references,
    )
    .await
}

/// Search every account's local cache (all folders) for messages matching
/// all of the query's whitespace-separated terms. Pure local search: subject,
/// sender and plain-text body. Newest first, capped at 200.
#[tauri::command]
async fn search_messages(
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<store::SearchResult>, String> {
    let accounts = state.config.lock().unwrap().accounts.clone();
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_string())
        .filter(|t| !t.is_empty())
        .collect();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out = Vec::new();
        for acc in &accounts {
            if let Ok(store) = store::Store::open(acc) {
                if let Ok(hits) = store.search_all(&terms) {
                    out.extend(hits.into_iter().map(|hit| store::SearchResult {
                        account: acc.name.clone(),
                        hit,
                    }));
                }
            }
        }
        out.sort_by(|a, b| b.hit.date.cmp(&a.hit.date));
        out.truncate(200);
        Ok(out)
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// All cached messages of one conversation, newest first.
#[tauri::command]
async fn get_thread(
    account: String,
    folder: String,
    thread_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<mail::MessageSummary>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let store = store::Store::open(&acc)?;
    store.load_thread(&folder, &thread_id)
}

/// Fetch attachment parts and return them base64-encoded, for inline image
/// thumbnails and cid: rewriting in the message view. All requested parts of
/// one message are fetched with a single round-trip, and each part is cached
/// on first use so later views (and offline views) don't hit the server.
#[tauri::command]
async fn get_attachments_data(
    account: String,
    folder: String,
    uid: u32,
    part_ids: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let part_indexes: Vec<usize> = part_ids
        .iter()
        .map(|p| p.parse().map_err(|_| format!("invalid part id '{p}'")))
        .collect::<Result<_, _>>()?;

    // Serve everything we already have from the local cache.
    let mut store = store::Store::open(&acc)?;
    let mut cached = store.load_attachments_data(&folder, uid, &part_ids)?;

    // One round-trip for every part we're still missing.
    let miss_indexes: Vec<usize> = cached
        .iter()
        .enumerate()
        .filter_map(|(i, c)| if c.is_none() { Some(i) } else { None })
        .collect();
    if !miss_indexes.is_empty() {
        let miss_positions: Vec<usize> = miss_indexes.iter().map(|&i| part_indexes[i]).collect();
        let acc2 = acc.clone();
        let folder2 = folder.clone();
        let fetched = tauri::async_runtime::spawn_blocking(move || {
            mail::fetch_attachment_parts(&acc2, &folder2, uid, &miss_positions)
        })
        .await
        .map_err(|e| format!("join error: {e}"))??;
        let mut to_cache = Vec::with_capacity(fetched.len());
        for (k, data) in fetched.into_iter().enumerate() {
            cached[miss_indexes[k]] = Some(data.clone());
            to_cache.push((part_ids[miss_indexes[k]].clone(), data));
        }
        store.store_attachments_data(&folder, uid, &to_cache)?;
    }

    Ok(cached
        .into_iter()
        .map(|c| c.map(|d| crypto::base64_encode(&d)).unwrap_or_default())
        .collect())
}

/// Fetch one attachment part by its position in the attachments iterator
/// and write it to the user-chosen destination.
#[tauri::command]
async fn save_attachment(
    account: String,
    folder: String,
    uid: u32,
    part_id: String,
    dest_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use std::io::Write;

    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let part_index: usize = part_id.parse().map_err(|_| "invalid part id")?;

    // Prefer the cached copy (already fetched for a thumbnail preview) so
    // saving doesn't re-download the whole message.
    let mut store = store::Store::open(&acc)?;
    if let Some(data) = store.load_attachment_data(&folder, uid, &part_id)? {
        let mut file = open_private_file(&dest_path)
            .map_err(|e| format!("create {}: {e}", dest_path))?;
        file.write_all(&data).map_err(|e| format!("write: {e}"))?;
        return Ok(());
    }

    // Not cached: fetch + extract the requested part off the UI thread.
    let acc2 = acc.clone();
    let folder2 = folder.clone();
    let data = tauri::async_runtime::spawn_blocking(move || {
        mail::fetch_attachment_part(&acc2, &folder2, uid, part_index)
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    store.store_attachment_data(&folder, uid, &part_id, &data)?;

    let mut file = open_private_file(&dest_path)
        .map_err(|e| format!("create {}: {e}", dest_path))?;
    file.write_all(&data).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Open a file for writing with owner-only permissions (0o600), so saved
/// message attachments aren't world-readable by default.
fn open_private_file(path: &str) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(path)
    }
}

/// Push queued offline flag changes to the server. Called whenever the app
/// proves it is online (a successful inbox poll, a successful folder
/// refresh). Runs on a worker thread; each pending change gets its own
/// connection, and only successful syncs are dropped from the queue.
fn flush_pending_flags(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    let accounts = st.config.lock().unwrap().accounts.clone();
    drop(st);
    for acc in &accounts {
        // The account may have been deleted while we were connecting;
        // never push changes for (or re-create the cache of) an account
        // that no longer exists.
        if !account_configured(app, &acc.name) {
            continue;
        }
        let Ok(mut store) = store::Store::open(acc) else { continue };
        let pending = match store.pending_flags() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("pending-flag read failed for {}: {e}", acc.email);
                continue;
            }
        };
        for (folder, uid, seen) in pending {
            match mail::set_seen_blocking(acc, &folder, uid, seen) {
                Ok(_) => {
                    log::info!("synced offline flag {folder}/{uid} seen={seen}");
                    if let Err(e) = store.remove_pending_flag(&folder, uid) {
                        log::warn!("pending-flag removal failed: {e}");
                    }
                }
                Err(e) => log::debug!("pending flag {folder}/{uid} still unsynced: {e}"),
            }
        }
    }
}

/// Find the folder that plays the role of the inbox: prefer the server's
/// declared \Inbox special-use, then a name match on "INBOX", then any
/// folder containing "inbox". Returns None when the folder list is
/// unreachable (offline) or no inbox-like folder exists.
fn inbox_folder_name(acc: &AccountConfig) -> Option<String> {
    let folders = match mail::list_folders_blocking(acc) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("inbox poll: folder list failed for {}: {e}", acc.email);
            return None;
        }
    };
    folders
        .iter()
        .find(|f| f.special_use.as_deref() == Some("inbox"))
        .map(|f| f.name.clone())
        .or_else(|| {
            folders
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
                .map(|f| f.name.clone())
        })
        .or_else(|| {
            folders
                .iter()
                .find(|f| f.name.to_lowercase().contains("inbox"))
                .map(|f| f.name.clone())
        })
}

/// One poll cycle: check every account's inbox for new mail and notify.
/// Runs on the poller thread; never touches the UI directly. Emits
/// `mail-refresh` afterwards so the frontend reloads folders + messages in
/// Diff the server UID set against what we've already notified about and
/// return the genuinely-new UIDs (recording them so they won't re-fire).
fn diff_new_uids(acc: &AccountConfig, inbox: &str, uids: &[u32]) -> Vec<u32> {
    match store::Store::open(acc).and_then(|mut s| s.new_uids_since_last_sync(inbox, uids)) {
        Ok(new) => new,
        Err(e) => {
            log::warn!("new-mail check failed for {}: {e}", acc.email);
            Vec::new()
        }
    }
}

/// Split "new" UIDs into genuinely new mail vs. moves: a message moved
/// into Inbox from another folder keeps its Message-ID, so a UID whose
/// Message-ID is already known in the cache (any folder) is a move, not
/// new mail. Purely a function of the fetched ids and the `known` test, so
/// it is unit-testable without IMAP.
fn partition_genuine_vs_moved(
    new_uids: &[u32],
    message_ids: &[Option<String>],
    known: &dyn Fn(&str) -> bool,
) -> (Vec<u32>, Vec<u32>) {
    let mut genuine = Vec::new();
    let mut moved = Vec::new();
    for (i, &uid) in new_uids.iter().enumerate() {
        match message_ids.get(i).and_then(|m| m.as_deref()) {
            Some(mid) if known(mid) => moved.push(uid),
            _ => genuine.push(uid),
        }
    }
    (genuine, moved)
}

/// Of the UIDs that look new, drop the ones that are actually moves into
/// this folder (their Message-ID already exists in the cache). Returns the
/// genuinely new UIDs; the moved ones are marked notified so they don't
/// re-toast later. Uses the live session for one batched header fetch.
fn filter_moved_new_uids(
    acc: &AccountConfig,
    inbox: &str,
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    new_uids: Vec<u32>,
) -> Vec<u32> {
    if new_uids.is_empty() {
        return new_uids;
    }
    // Keep the fetch list sorted so the batched header fetch is compact;
    // fetch_message_ids aligns results by UID, so order never matters.
    let mut sorted = new_uids.clone();
    sorted.sort_unstable();
    let ids = match mail::fetch_message_ids(session, &sorted) {
        Ok(ids) => ids,
        Err(e) => {
            log::warn!("new-mail header fetch failed: {e}; notifying as new");
            return new_uids;
        }
    };
    let Ok(mut store) = store::Store::open(acc) else {
        return new_uids;
    };
    let (genuine, moved) = partition_genuine_vs_moved(&sorted, &ids, &|mid| {
        store.message_id_known(mid).unwrap_or(false)
    });
    if !moved.is_empty() {
        log::info!(
            "{} message(s) in {inbox} already known in the cache (moved or previously synced), no toast",
            moved.len()
        );
        let _ = store.mark_notified(inbox, &moved);
    }
    genuine
}

/// Sender display + subject of the newest of `new_uids`, read from the
/// local cache (used by the slow-poll fallback, which has no live session).
fn cached_preview(acc: &AccountConfig, inbox: &str, new_uids: &[u32]) -> (String, String, usize) {
    let Ok(store) = store::Store::open(acc) else {
        return (String::new(), String::new(), 0);
    };
    let Ok(all) = store.load_summaries(inbox) else {
        return (String::new(), String::new(), 0);
    };
    let set: std::collections::HashSet<u32> = new_uids.iter().copied().collect();
    let mut hits: Vec<&crate::mail::MessageSummary> =
        all.iter().filter(|m| set.contains(&m.uid)).collect();
    hits.sort_by(|a, b| b.uid.cmp(&a.uid));
    match hits.first() {
        Some(m) => (m.from.clone(), m.subject.clone(), hits.len().saturating_sub(1)),
        None => (String::new(), String::new(), 0),
    }
}

/// Spawn one IDLE watcher thread per account that doesn't have one yet.
/// Called at startup and periodically (accounts can be added at runtime).
fn spawn_missing_idle_watchers(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    let accounts = st.config.lock().unwrap().accounts.clone();
    let mut watchers = st.idle_watchers.lock().unwrap();
    for acc in &accounts {
        if !watchers.contains_key(&acc.name) {
            let stop = Arc::new(AtomicBool::new(false));
            watchers.insert(acc.name.clone(), stop.clone());
            let app2 = app.clone();
            let acc2 = acc.clone();
            log::info!("starting IDLE watcher for {}", acc.email);
            tauri::async_runtime::spawn_blocking(move || {
                idle_watch_account(&app2, &acc2, stop);
            });
        }
    }
}

/// Result of one IDLE connection attempt.
enum IdleOutcome {
    /// The server does not advertise IDLE; fall back to slow polling.
    NoIdle,
    /// The watcher was told to stop (account deleted) mid-cycle.
    Stopped,
}

/// True while the named account is still present in the config.
fn account_configured(app: &tauri::AppHandle, name: &str) -> bool {
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .accounts
        .iter()
        .any(|a| a.name == name)
}

/// Unregister a watcher thread, but only if the stop flag registered under
/// the account name is the one this thread holds: a re-added account gets
/// a fresh flag, so a winding-down old watcher must not unregister its
/// replacement.
fn deregister_watcher(app: &tauri::AppHandle, acc: &AccountConfig, stop: &Arc<AtomicBool>) {
    let st = app.state::<AppState>();
    let mut watchers = st.idle_watchers.lock().unwrap();
    if watchers
        .get(&acc.name)
        .is_some_and(|f| Arc::ptr_eq(f, stop))
    {
        watchers.remove(&acc.name);
    }
}

/// Per-account background watcher: prefers the RFC 2177 IDLE push (one
/// dormant connection per account, instant new-mail notification with no
/// polling), falling back to a slow poll loop for servers without IDLE.
/// Reconnects with backoff on connection loss; exits if the account is
/// removed.
fn idle_watch_account(app: &tauri::AppHandle, acc: &AccountConfig, stop: Arc<AtomicBool>) {
    let mut backoff = 5u64;
    loop {
        // Stop if the account was deleted (or a stop was requested) while
        // we were away.
        let st = app.state::<AppState>();
        let still_there = {
            let cfg = st.config.lock().unwrap();
            cfg.accounts.iter().any(|a| a.name == acc.name)
        };
        if stop.load(Ordering::Acquire) || !still_there {
            log::info!("background watcher for {} exiting (account removed)", acc.email);
            deregister_watcher(app, acc, &stop);
            return;
        }
        match idle_cycle(app, acc, &stop) {
            Ok(IdleOutcome::NoIdle) => {
                log::info!(
                    "{} has no IDLE support; falling back to slow polling",
                    acc.email
                );
                slow_poll_account(app, acc, stop);
                return;
            }
            Ok(IdleOutcome::Stopped) => {
                // delete_account asked us to stop mid-cycle.
                deregister_watcher(app, acc, &stop);
                return;
            }
            Err(e) => {
                log::warn!("background watcher for {} lost: {e}; retrying in {backoff}s", acc.email);
                // Tell the frontend the link is down so it starts its
                // recovery polling — this is the reliable offline signal
                // (the webview's window "offline" event is not dependable
                // on Linux/WebKitGTK). Idempotent on the JS side.
                let _ = app.emit("connection-lost", ());
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// Fallback for servers without IDLE: a modest interval check (new mail +
/// offline-flag sync) that keeps notifications working without hammering
/// the server. The frontend's manual Refresh stays the primary trigger.
fn slow_poll_account(app: &tauri::AppHandle, acc: &AccountConfig, stop: Arc<AtomicBool>) {
    loop {
        // Sleep in one-minute slices so a deleted account stops the poller
        // promptly instead of lingering for a full 15-minute interval.
        for _ in 0..15 {
            if stop.load(Ordering::Acquire) || !account_configured(app, &acc.name) {
                deregister_watcher(app, acc, &stop);
                return;
            }
            std::thread::sleep(Duration::from_secs(60));
        }
        let Some(inbox) = inbox_folder_name(acc) else {
            // Folder listing failed — the server is unreachable.
            let _ = app.emit("connection-lost", ());
            continue;
        };
        match mail::list_folder_uids(acc, &inbox) {
            Ok(uids) => {
                let new = diff_new_uids(acc, &inbox, &uids);
                if !new.is_empty() {
                    log::info!(
                        "new mail: {} message(s) for {} ({inbox})",
                        new.len(),
                        acc.email
                    );
                    // No live session here: sync the cache first so the
                    // sender/subject can come from the stored summaries.
                    warm_inbox_cache(acc, &inbox);
                    let (from, subject, extra) = cached_preview(acc, &inbox, &new);
                    fire_new_mail_notification(&from, &subject, extra);
                }
                flush_pending_flags(app);
                // Keep the tray icon truthful from the backend: store the
                // server's authoritative unread count and repaint (the
                // frontend's folder-refresh path can lag while the UI
                // thinks it is offline).
                if let Ok(unseen) = mail::folder_unseen_count(acc, &inbox) {
                    if let Ok(mut store) = store::Store::open(acc) {
                        let _ = store.set_folder_unread(&inbox, unseen);
                    }
                }
                update_tray_icon(app);
                let _ = app.emit("mail-refresh", ());
            }
            Err(e) => {
                log::warn!("slow poll for {} failed: {e}", acc.email);
                let _ = app.emit("connection-lost", ());
            }
        }
    }
}

/// Full INBOX sync triggered by new-mail detection: fetch + cache the
/// folder's summaries and bodies so the message list is warm. Switching to
/// INBOX (or refreshing it) then shows the new mail instantly from cache
/// instead of streaming from the server first — closing the gap between
/// the toast and the message appearing.
fn warm_inbox_cache(acc: &AccountConfig, inbox: &str) {
    let acc2 = acc.clone();
    let inbox2 = inbox.to_string();
    if let Err(e) = mail::list_messages_streamed_blocking(
        acc,
        inbox,
        move |batch, bodies| {
            if let Ok(mut s) = store::Store::open(&acc2) {
                let _ = s.upsert_summaries(&inbox2, &batch);
                let _ = s.store_bodies(&inbox2, &bodies);
            }
        },
        |_server_uids| {},
    ) {
        log::warn!("inbox warm refresh failed for {}: {e}", acc.email);
    }
}

/// One IDLE connection lifetime: connect, select the inbox, verify the
/// server supports IDLE, then idle until the connection drops. Returns
/// `NoIdle` (fall back to slow polling) or an error to trigger a reconnect.
fn idle_cycle(
    app: &tauri::AppHandle,
    acc: &AccountConfig,
    stop: &AtomicBool,
) -> Result<IdleOutcome, String> {
    let Some(inbox) = inbox_folder_name(acc) else {
        return Err("no inbox folder found".into());
    };
    let mut session = mail::imap_session_pub(acc)?;
    session
        .select(&inbox)
        .map_err(|e| format!("SELECT {inbox} failed: {e}"))?;
    let caps = session
        .capabilities()
        .map_err(|e| format!("CAPABILITY failed: {e}"))?;
    if !caps.has_str("IDLE") {
        return Ok(IdleOutcome::NoIdle);
    }
    // We are connected: flush any offline flag changes queued while we
    // were away (the reconnect itself is the "back online" signal).
    flush_pending_flags(app);
    log::info!("background watcher for {} watching {inbox} (IDLE)", acc.email);

    // Catch up on anything that arrived while the connection was down:
    // IDLE only pushes FUTURE changes, so without this diff a message that
    // landed during a disconnect would stay invisible until a manual
    // refresh. Best-effort: a failed catch-up must not abort the watch.
    if let Err(e) = process_inbox_change(app, acc, &inbox, &mut session) {
        log::warn!("inbox catch-up after reconnect failed: {e}");
    }

    loop {
        use imap::extensions::idle::WaitOutcome;
        // Exit promptly when the account is deleted: the IDLE wait below
        // can block for up to IDLE_KEEPALIVE_SECS, so check at every
        // opportunity between waits.
        if stop.load(Ordering::Acquire) {
            return Ok(IdleOutcome::Stopped);
        }
        // Bind the outcome first so the temporary IDLE handle (which borrows
        // the session mutably) is dropped before we touch the session again.
        // The keepalive timeout is far shorter than the crate default (29
        // minutes): every expiry re-issues IDLE, which keeps the connection
        // fresh AND surfaces a dead link — the re-issue's DONE/IDLE round
        // trip fails on a connection that died while we were offline.
        // Without this, a network drop leaves the watcher blocked in the
        // dormant IDLE wait and it never reconnects to catch up on mail.
        let outcome = {
            let mut idle = session.idle();
            idle.timeout(Duration::from_secs(IDLE_KEEPALIVE_SECS));
            idle.wait_while(imap::extensions::idle::stop_on_any)
        };
        match outcome {
            Ok(WaitOutcome::MailboxChanged) => {
                // The server pushed a mailbox change — most likely new mail.
                // Reuse this session (the IDLE borrow has ended) to diff the
                // UIDs and react instantly.
                if let Err(e) = process_inbox_change(app, acc, &inbox, &mut session) {
                    log::warn!("inbox change handling failed: {e}");
                }
                // Re-enter IDLE.
            }
            Ok(WaitOutcome::TimedOut) => {
                // keepalive re-issued the IDLE; keep waiting.
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One mailbox-change pass: diff the current UID set, drop moved messages,
/// notify about genuinely new mail, warm the cache, and poke the frontend.
/// Shared by the IDLE push handler and the post-reconnect catch-up.
fn process_inbox_change(
    app: &tauri::AppHandle,
    acc: &AccountConfig,
    inbox: &str,
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
) -> Result<(), String> {
    // The account can be deleted while an IDLE wait is outstanding (the
    // wait only returns on timeout or a mailbox change). Bail before any
    // store access so a deleted account's cache is never re-created.
    if !account_configured(app, &acc.name) {
        return Ok(());
    }
    let uids = session
        .uid_search("ALL")
        .map_err(|e| format!("UID SEARCH failed: {e}"))?;
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable();
    let new = diff_new_uids(acc, inbox, &uids);
    // A message moved into Inbox from another folder (by any client) shows
    // up as a brand-new UID here; drop those whose Message-ID is already
    // cached so they don't toast.
    let new = filter_moved_new_uids(acc, inbox, session, new);
    if !new.is_empty() {
        log::info!(
            "new mail: {} message(s) for {} ({inbox})",
            new.len(),
            acc.email
        );
        // Sender + subject from the ENVELOPE on this very session —
        // instant, no full sync needed.
        let newest = *new.iter().max().unwrap_or(&0);
        let (from, subject) =
            mail::fetch_envelope_preview(session, newest).unwrap_or_default();
        fire_new_mail_notification(&from, &subject, new.len() - 1);
        // Warm the INBOX cache so the message is already in the list when
        // the user opens/refreshes it — the toast should not beat the
        // message.
        warm_inbox_cache(acc, inbox);
        // Keep the tray icon truthful from the backend itself: refresh the
        // inbox's cached unread count from this very session and repaint.
        // The frontend normally does this via its folder refresh, but that
        // path is skipped while the UI thinks it is offline — exactly when
        // a reconnect catch-up like this one runs. Only done when new mail
        // arrived so quiet change pings (e.g. Dovecot's "Still here") don't
        // pay for a SEARCH round trip.
        if let Ok(unseen) = session.search("UNSEEN") {
            if let Ok(mut store) = store::Store::open(acc) {
                let _ = store.set_folder_unread(inbox, unseen.len() as u32);
            }
        }
        update_tray_icon(app);
    }
    flush_pending_flags(app);
    let _ = app.emit("mail-refresh", ());
    Ok(())
}

#[tauri::command]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let mut config = Config::load_or_default();
    config.migrate_plaintext_passwords();
    // The system tray's StatusNotifierItem Title comes from the glib
    // application name (libappindicator reads g_get_application_name()), and
    // the tray host shows that as the tooltip — set it before the tray is
    // built so the initial hover text is right; update_tray_icon refreshes
    // it with the unread count afterwards.
    glib::set_application_name("Sufi Email");

    // Backfill content hashes + remove duplicates in existing caches.
    for acc in &config.accounts {
        match store::Store::open(acc) {
            Ok(mut s) => {
                if let Err(e) = s.backfill_hashes() {
                    log::warn!("hash backfill failed for {}: {e}", acc.email);
                }
            }
            Err(e) => log::warn!("store open failed for {}: {e}", acc.email),
        }
        match store::dedupe_existing(acc) {
            Ok(n) if n > 0 => log::info!("removed {n} duplicate message(s) from {}", acc.email),
            Ok(_) => {}
            Err(e) => log::warn!("dedupe failed for {}: {e}", acc.email),
        }
    }

    // Window-size persistence. Resize events arrive in a burst while the user
    // drags, so a small worker coalesces them and writes config.toml once the
    // drag stops. mpsc::Sender is Send but not Sync and on_window_event
    // requires Sync, so the sender is wrapped in a Mutex (never contended —
    // the handler only runs on the main thread).
    let (window_size_tx, window_size_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let window_size_tx = Mutex::new(window_size_tx);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            config: Mutex::new(config),
            idle_watchers: Mutex::new(HashMap::new()),
            tray_guard: Arc::new(TrayGuard::new()),
            pending_window_restore: Mutex::new(None),
        })
        .setup(move |app| {
            // Reopen at the remembered size. The window is created hidden
            // (tauri.conf.json `visible: false`) and revealed at the end of
            // setup, so it never flashes at the default size — or with a title
            // bar that Hyprland is about to hide (see below).
            let main_window = app.get_webview_window("main");
            let saved_size = app.state::<AppState>().config.lock().unwrap().window;
            if let (Some(window), Some(size)) = (main_window.as_ref(), saved_size) {
                // Wayland can drop a resize requested before the window is
                // mapped and then report a decorations-offset size. Mark the
                // restore pending: the resize handler re-applies it until it
                // sticks and refuses to persist anything until then.
                *app
                    .state::<AppState>()
                    .pending_window_restore
                    .lock()
                    .unwrap() = Some(PendingWindowRestore {
                    desired: (size.width, size.height),
                    last_request: (size.width, size.height),
                    attempts: WINDOW_RESTORE_ATTEMPTS,
                    deadline: std::time::Instant::now() + Duration::from_secs(5),
                });
                apply_saved_window_size(window, size.width, size.height);
            }

            // Debounced window-size saver: keep the last size received and
            // persist it once the stream of resize events goes quiet.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    while let Some((width, height)) =
                        coalesce_sizes(&window_size_rx, Duration::from_millis(400))
                    {
                        save_window_size(&handle, width, height);
                    }
                });
            }

            spawn_missing_idle_watchers(app.handle());
            // Follow the active Omarchy theme: repaint the UI live when the
            // theme changes (the watcher emits system-theme-changed).
            {
                let app2 = app.handle().clone();
                tauri::async_runtime::spawn_blocking(move || theme::watch(app2));
            }
            if let Some(colors) = theme::read_theme_colors() {
                log::info!("system theme: {} ({})", colors.mode, colors.background);
            }
            // Hyprland/Omarchy convention: windows have no title bars. The
            // compositor draws only borders, so hide the client-side title
            // bar there; other desktops keep it (the title bar is expected
            // on ordinary X11/Wayland desktops).
            if is_hyprland() {
                if let Some(window) = main_window.as_ref() {
                    match window.set_decorations(false) {
                        Ok(()) => log::info!("hidden window decorations (Hyprland)"),
                        Err(e) => log::warn!("could not hide window decorations: {e}"),
                    }
                }
            } else {
                // Everywhere else the title bar stays, but on Wayland tao
                // hardcodes its button layout; realign it with the desktop's
                // configured one so the buttons match other apps (no stray
                // Maximize on Fedora/GNOME).
                #[cfg(target_os = "linux")]
                if let Some(window) = main_window.as_ref() {
                    adopt_system_decoration_layout(window);
                }
            }
            // The Linux tray backend dlopens an appindicator library and
            // panics if none is found, taking the app down at startup. That
            // happens in sandboxed runtimes that ship WebKitGTK/GTK but not
            // appindicator (e.g. the Flatpak GNOME runtime), so only build the
            // tray when the library is actually there; otherwise run without
            // it (closing the window then quits, see the close handler).
            if appindicator_available() {
                setup_tray(app.handle())?;
                update_tray_icon(app.handle());
            } else {
                log::warn!(
                    "no appindicator library found — running without a tray icon"
                );
            }
            // Reveal the window. If the compositor ignored the pre-map resize,
            // the resize handler re-applies the remembered size once the
            // surface is mapped.
            if let Some(window) = main_window.as_ref() {
                let _ = window.show();
            }
            Ok(())
        })
        // Remember the window size across resizes, and ignore the sizes seen
        // while maximized/fullscreen (see savable_window_size).
        //
        // Close-to-tray: clicking the window's close button (or Super+W)
        // only hides the window; the app keeps running in the background
        // with the tray icon. Quit from the tray menu is the real exit.
        //
        // Only do this when a tray host is actually on the bus: on a desktop
        // with no StatusNotifierWatcher (e.g. Fedora Workstation's default
        // GNOME, which ships no AppIndicator extension) the icon is invisible,
        // so hiding the window would strand the running app with no way back.
        // There we fall through to a normal close, which quits. The signal is
        // the same watcher state the tray code tracks: WatcherAbsent means no
        // host. The watcher monitor samples within milliseconds of startup, so
        // this reflects reality by the time a user can click close.
        .on_window_event(move |window, event| match event {
            tauri::WindowEvent::Resized(_) => {
                let Some(current) = savable_window_size(window) else {
                    return;
                };
                let state = window.state::<AppState>();
                let step = {
                    let mut pending = state.pending_window_restore.lock().unwrap();
                    let (next, step) =
                        restore_step(*pending, current, std::time::Instant::now());
                    *pending = next;
                    step
                };
                match step {
                    RestoreStep::Request((width, height)) => {
                        apply_saved_window_size(window, width, height);
                        return;
                    }
                    RestoreStep::GaveUp((width, height)) => {
                        log::warn!(
                            "window: could not restore {width}x{height} (stuck at {}x{})",
                            current.0,
                            current.1
                        );
                    }
                    RestoreStep::Idle | RestoreStep::Confirmed => {}
                }
                let _ = window_size_tx.lock().unwrap().send(current);
            }
            tauri::WindowEvent::CloseRequested { api, .. } => {
                let state = window.state::<AppState>();
                // Don't persist a startup size we never managed to correct.
                let restoring = state.pending_window_restore.lock().unwrap().is_some();
                if !restoring {
                    // Flush synchronously on the way out: the app may only be
                    // hidden to the tray (or, with no tray host, actually quit),
                    // so the debounced writer can't be relied on for the last
                    // value.
                    if let Some((width, height)) = savable_window_size(window) {
                        save_window_size(window.app_handle(), width, height);
                    }
                }
                let has_tray_host = state.tray_guard.safety().host_present();
                if has_tray_host {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_accounts,
            add_account,
            delete_account,
            list_folders,
            list_messages,
            mark_message,
            fetch_message,
            list_attachments,
            save_attachment,
            move_message_local,
            move_message_server,
            delete_message_local,
            delete_message_server,
            get_thread,
            get_attachments_data,
            send_email,
            theme::get_system_theme,
            search_messages
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod main_tests {
    #[test]
    fn partition_genuine_vs_moved() {
        let known = |mid: &str| mid == "<known@x>" || mid == "<known2@x>";
        let uids = vec![1, 2, 3, 4];
        let ids = vec![
            Some("<known@x>".into()),   // moved
            None,                        // no Message-ID -> treat as new
            Some("<fresh@x>".into()),   // genuinely new
            Some("<known2@x>".into()),  // moved
        ];
        let (genuine, moved) = super::partition_genuine_vs_moved(&uids, &ids, &known);
        assert_eq!(genuine, vec![2, 3]);
        assert_eq!(moved, vec![1, 4]);
    }

    #[test]
    fn open_private_file_creates_owner_only_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("saved-attachment.bin");
        let file = super::open_private_file(path.to_str().unwrap()).expect("create file");
        drop(file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "saved attachments must be owner-only");
        }
    }

    #[test]
    fn hex_rgb_parses_and_rejects() {
        assert_eq!(super::hex_rgb("#81a1c1"), Some((0x81, 0xa1, 0xc1)));
        assert_eq!(super::hex_rgb("81a1c1"), Some((0x81, 0xa1, 0xc1)));
        assert_eq!(super::hex_rgb("#f0f"), None); // shorthand not supported
        assert_eq!(super::hex_rgb("nope"), None);
        assert_eq!(super::hex_rgb(""), None);
    }

    #[test]
    fn tint_envelope_repaints_the_shape_and_keeps_alpha() {
        let img = super::tint_envelope(0x81, 0xa1, 0xc1);
        assert_eq!(img.width(), 256);
        assert_eq!(img.height(), 256);
        let rgba = img.rgba();
        // The opaque envelope area is filled with the requested color.
        let mut opaque = 0usize;
        let mut colored_ok = true;
        for px in rgba.chunks_exact(4) {
            if px[3] > 0 {
                opaque += 1;
                if px[0] != 0x81 || px[1] != 0xa1 || px[2] != 0xc1 {
                    colored_ok = false;
                }
            }
        }
        assert!(opaque > 0, "envelope shape present");
        assert!(colored_ok, "every opaque pixel carries the requested RGB");
    }

    #[test]
    fn hyprland_detection_reads_the_client_environment() {
        let prev_sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE");
        let prev_desk = std::env::var_os("XDG_CURRENT_DESKTOP");

        // Hyprland sets an instance signature for every client.
        std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", "efb50993");
        std::env::remove_var("XDG_CURRENT_DESKTOP");
        assert!(super::is_hyprland());

        // XDG_CURRENT_DESKTOP fallback.
        std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE");
        std::env::set_var("XDG_CURRENT_DESKTOP", "Hyprland");
        assert!(super::is_hyprland());

        // Neither set: not Hyprland (title bar stays).
        std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE");
        std::env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
        assert!(!super::is_hyprland());

        match prev_sig {
            Some(v) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", v),
            None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
        }
        match prev_desk {
            Some(v) => std::env::set_var("XDG_CURRENT_DESKTOP", v),
            None => std::env::remove_var("XDG_CURRENT_DESKTOP"),
        }
    }
    // ---------------------------------------------------------------------
    // Tray watcher / tooltip-safety logic.
    //
    // The crash this fix guards (libayatana-appindicator's tooltip
    // use-after-free) happens inside a dlopened C library and needs a live
    // session bus plus a tray host to reproduce, so no end-to-end test is
    // possible here. These tests pin the pure decision logic instead: when
    // tooltip updates may run, when the icon must be rebuilt, and when a
    // repaint can be skipped. See the TraySafety docs in main.rs for the bug
    // mechanics.
    // ---------------------------------------------------------------------

    fn tray_visual(unread: u32) -> super::TrayVisual {
        super::TrayVisual {
            unread,
            tint: (1, 2, 3),
        }
    }

    #[test]
    fn watcher_action_table() {
        use super::WatcherAction;
        // Not sampled yet: the first sighting decides — watcher up unlocks
        // without a rebuild (the setup icon predates the monitor), down
        // suppresses.
        assert_eq!(super::watcher_action(None, true), WatcherAction::UnlockClean);
        assert_eq!(super::watcher_action(None, false), WatcherAction::Suppress);
        // Steady states change nothing.
        assert_eq!(super::watcher_action(Some(true), true), WatcherAction::Nothing);
        assert_eq!(super::watcher_action(Some(false), false), WatcherAction::Nothing);
        // The watcher left: suppress tooltips (the library may be about to
        // create its fallback icon behind our back).
        assert_eq!(super::watcher_action(Some(true), false), WatcherAction::Suppress);
        // The core invariant: a return after an absence demands a rebuild of
        // the (possibly poisoned) icon before tooltips may resume. Losing
        // this arm would regress the crash.
        assert_eq!(super::watcher_action(Some(false), true), WatcherAction::RebuildPending);
    }

    #[test]
    fn watcher_action_unlock_only_before_any_absence() {
        // A long uninterrupted presence must never ask for a rebuild, and
        // only the very first sample may unlock tooltips.
        let mut state: Option<bool> = None;
        let (mut unlocks, mut rebuilds) = (0, 0);
        for present in [true; 200] {
            match super::watcher_action(state, present) {
                super::WatcherAction::UnlockClean => unlocks += 1,
                super::WatcherAction::RebuildPending => rebuilds += 1,
                _ => {}
            }
            state = Some(present);
        }
        assert_eq!(unlocks, 1);
        assert_eq!(rebuilds, 0);
    }

    #[test]
    fn watcher_action_each_loss_and_return_demands_one_rebuild() {
        // Churn the watcher (absent long enough to arm the bug, then back)
        // and check each return produces exactly one rebuild request, with
        // tooltips suppressed for the whole absence.
        let samples = [true, true, false, false, true, false, true];
        let mut state: Option<bool> = None;
        let mut events = Vec::new();
        for &present in &samples {
            events.push(super::watcher_action(state, present));
            state = Some(present);
        }
        assert_eq!(
            events,
            vec![
                super::WatcherAction::UnlockClean,   // first sample, watcher up
                super::WatcherAction::Nothing,       // still up
                super::WatcherAction::Suppress,      // left
                super::WatcherAction::Nothing,       // still away
                super::WatcherAction::RebuildPending, // back -> rebuild
                super::WatcherAction::Suppress,      // left again
                super::WatcherAction::RebuildPending, // back -> rebuild again
            ]
        );
    }

    #[test]
    fn tooltip_setter_safe_only_while_clean() {
        use super::TraySafety;
        assert!(!super::tooltip_safe(TraySafety::WatcherAbsent));
        assert!(!super::tooltip_safe(TraySafety::RebuildPending));
        assert!(super::tooltip_safe(TraySafety::Clean));
    }

    #[test]
    fn tray_host_present_tracks_the_watcher() {
        use super::TraySafety;
        // No watcher -> no tray host -> close must not hide to tray.
        assert!(!TraySafety::WatcherAbsent.host_present());
        // Watcher up (whether or not the icon had to be rebuilt) -> a host is
        // there to restore the window from, so hide-to-tray stays enabled.
        assert!(TraySafety::RebuildPending.host_present());
        assert!(TraySafety::Clean.host_present());
    }

    #[test]
    fn tray_safety_encodes_conservatively() {
        use super::TraySafety;
        for s in [
            TraySafety::WatcherAbsent,
            TraySafety::RebuildPending,
            TraySafety::Clean,
        ] {
            assert_eq!(super::TraySafety::from_u8(s.to_u8()), s);
        }
        // Unknown bytes must decode to the most restrictive state.
        assert_eq!(super::TraySafety::from_u8(255), TraySafety::WatcherAbsent);
    }

    #[test]
    fn tray_repaint_gated_on_full_visual() {
        let v = tray_visual(3);
        // A fresh icon (nothing fully shown yet) always needs a repaint.
        assert!(super::needs_tray_repaint(None, v));
        // Fully shown (tooltip included) -> skip.
        assert!(!super::needs_tray_repaint(Some(v), v));
        // Unread count changed -> repaint.
        assert!(super::needs_tray_repaint(Some(tray_visual(0)), v));
        // Same unread but the theme tint changed -> repaint (icon recolor).
        let recolored = super::TrayVisual { unread: 3, tint: (9, 9, 9) };
        assert!(super::needs_tray_repaint(Some(recolored), v));
    }

    // ---------------------------------------------------------------------
    // Window-size restore correction (Wayland/CSD decorations offset)
    // ---------------------------------------------------------------------

    #[test]
    fn window_restore_correction_cancels_a_constant_offset() {
        // Asked for 1105x874 but the window came back 52x99 bigger: the next
        // request backs off by exactly that offset...
        let desired = (1105u32, 874u32);
        let request = super::corrected_request(desired, desired, (1157, 973));
        assert_eq!(request, (1053, 775));
        // ...so re-requesting lands on the desired size (one-step convergence).
        assert_eq!((request.0 + 52, request.1 + 99), desired);
    }

    #[test]
    fn window_restore_correction_is_a_noop_without_offset() {
        assert_eq!(
            super::corrected_request((800, 600), (800, 600), (800, 600)),
            (800, 600)
        );
    }

    #[test]
    fn window_restore_correction_clamps_to_a_sane_minimum() {
        assert_eq!(
            super::corrected_request((800, 600), (800, 600), (5000, 5000)),
            (200, 200)
        );
    }

    // ---------------------------------------------------------------------
    // Pending-restore state machine and resize debounce
    // ---------------------------------------------------------------------

    fn pending(desired: (u32, u32), attempts: u8) -> super::PendingWindowRestore {
        super::PendingWindowRestore {
            desired,
            last_request: desired,
            attempts,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
        }
    }

    #[test]
    fn restore_step_is_idle_without_pending() {
        use super::RestoreStep;
        let (next, step) = super::restore_step(None, (1000, 700), std::time::Instant::now());
        assert_eq!(step, RestoreStep::Idle);
        assert!(next.is_none());
    }

    #[test]
    fn restore_step_confirms_when_the_size_matches() {
        use super::RestoreStep;
        let desired = (1105u32, 874u32);
        let (next, step) =
            super::restore_step(Some(pending(desired, 5)), desired, std::time::Instant::now());
        assert_eq!(step, RestoreStep::Confirmed);
        assert!(next.is_none());
    }

    #[test]
    fn restore_step_retries_with_the_corrected_request_then_confirms() {
        use super::RestoreStep;
        let desired = (1105u32, 874u32);
        // The window came back 52x99 bigger.
        let (next, step) = super::restore_step(
            Some(pending(desired, 5)),
            (1157, 973),
            std::time::Instant::now(),
        );
        assert_eq!(step, RestoreStep::Request((1053, 775)));
        let next = next.expect("still pending");
        assert_eq!(next.attempts, 4);
        // The window honors the correction: asking for 1053x775 with the same
        // +52/+99 offset comes back as exactly the desired 1105x874, which
        // confirms the restore.
        let (after, step) =
            super::restore_step(Some(next), desired, std::time::Instant::now());
        assert_eq!(step, RestoreStep::Confirmed);
        assert!(after.is_none());
    }

    #[test]
    fn restore_step_gives_up_after_attempts_or_deadline() {
        use super::RestoreStep;
        let now = std::time::Instant::now();
        // Attempts exhausted.
        assert_eq!(
            super::restore_step(Some(pending((900, 700), 0)), (1280, 800), now).1,
            RestoreStep::GaveUp((900, 700))
        );
        // Deadline passed.
        let mut expired = pending((900, 700), 5);
        expired.deadline = now - std::time::Duration::from_secs(1);
        let (next, step) = super::restore_step(Some(expired), (1280, 800), now);
        assert_eq!(step, RestoreStep::GaveUp((900, 700)));
        assert!(next.is_none());
    }

    #[test]
    fn coalesce_returns_the_last_size_of_a_burst() {
        let (tx, rx) = std::sync::mpsc::channel();
        for size in [(1, 1), (2, 2), (3, 3)] {
            tx.send(size).unwrap();
        }
        drop(tx);
        assert_eq!(
            super::coalesce_sizes(&rx, std::time::Duration::from_millis(50)),
            Some((3, 3))
        );
    }

    #[test]
    fn coalesce_returns_none_when_the_channel_is_closed() {
        let (tx, rx) = std::sync::mpsc::channel::<(u32, u32)>();
        drop(tx);
        assert_eq!(
            super::coalesce_sizes(&rx, std::time::Duration::from_millis(50)),
            None
        );
    }

    #[test]
    fn coalesce_ends_a_batch_at_the_quiet_gap() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((10, 10)).unwrap();
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = tx2.send((20, 20));
        });
        let quiet = std::time::Duration::from_millis(50);
        // The first batch ends at the quiet gap...
        assert_eq!(super::coalesce_sizes(&rx, quiet), Some((10, 10)));
        // ...and the later size is its own batch.
        assert_eq!(super::coalesce_sizes(&rx, quiet), Some((20, 20)));
    }
}


#[cfg(test)]
mod manual_send_tests {
    use tauri::async_runtime::block_on;

    /// Manual check: sends a real email through the configured SMTP account
    /// (to itself). Run with: cargo test -- --ignored manual_send
    #[test]
    #[ignore]
    fn manual_send_test_email() {
        let cfg = crate::account::Config::load_or_default();
        let acc = cfg.accounts.first().expect("no configured account");
        let to = acc.email.clone();
        block_on(crate::mail::send_email(
            acc,
            vec![to.clone()],
            Vec::new(), // no Cc
            "sufi-email send test",
            "hello from the manual send test",
            Vec::new(),
            None,
            None,
        ))
        .expect("send failed");
        println!("sent to {to}");
    }
}

#[cfg(test)]
mod manual_inbox_tests {
    use tauri::async_runtime::block_on;

    /// List the newest INBOX messages on the server with subject + flags.
    #[test]
    #[ignore]
    fn manual_inbox() {
        let cfg = crate::account::Config::load_or_default();
        let acc = cfg.accounts.first().expect("no account");
        block_on(async {
            let acc = acc.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let mut s = crate::mail::imap_session_pub(&acc).map_err(|e| e.to_string())?;
                let m = s.select("INBOX").map_err(|e| e.to_string())?;
                let top = m.exists.max(1);
                let from = top.saturating_sub(10);
                let f = s
                    .fetch(&format!("{from}:{top}"), "(UID ENVELOPE)")
                    .map_err(|e| e.to_string())?;
                for x in f.iter() {
                    let uid = x.uid.unwrap_or(0);
                    let subj = x
                        .envelope()
                        .and_then(|e| e.subject.as_ref())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .unwrap_or_default();
                    println!("uid {uid}: {subj}");
                }
                Ok::<(), String>(())
            })
            .await
        })
        .expect("failed");
    }
}
