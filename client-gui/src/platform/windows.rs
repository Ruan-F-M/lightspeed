//! # LightSpeed GUI - Windows Platform
//!
//! Real tray-icon backend using `tray_icon`, admin check via `net session`,
//! and capture check via `sc query npcap`.
//!
//! ## Why the tray does not run in the frame loop
//!
//! Windows sends no `WM_PAINT` to a hidden or minimized window, so eframe
//! stops calling `App::ui` the moment the window is hidden to the tray. Tray
//! events polled from the frame loop were therefore never read while hidden:
//! "Show window" and a double-click did nothing, and Quit left the process
//! running. The tray events are now read on their own threads, which show the
//! window through Win32 directly; the frame loop only receives the actions
//! that need the app (Connect / Disconnect) once it is painting again.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::app::TrayState;
use crate::platform::{Platform, QuitFlag, TrayAction, TrayHandle};
use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    TrayIcon, TrayIconBuilder, TrayIconEvent,
};

// ── Tray menu item IDs ──────────────────────────────────────────────────────

const MENU_SHOW: &str = "show";
const MENU_CONNECT: &str = "connect";
const MENU_DISCONNECT: &str = "disconnect";
const MENU_QUIT: &str = "quit";

/// Set once the tray icon exists; read by `has_system_tray`.
static TRAY_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// SAFETY: `TrayIcon` uses `Rc<RefCell<…>>` internally on Windows, which is
/// SAFETY: `WindowsTray` is only ever created and accessed on the main
/// (egui) thread - the same thread that runs the Windows message loop.
/// It is never sent across threads, so `Rc<RefCell<…>>` internals from
/// the `tray_icon` crate cannot cause data races in practice.
// SAFETY: WindowsTray contains winapi HANDLEs which are Send-safe (HANDLEs
// can be used from any thread in the Windows API model).
unsafe impl Send for WindowsTray {}

/// Windows system-tray icon with a context menu (Show, Connect, Disconnect, Quit).
///
/// Created via [`WindowsPlatform::new_tray`].  Runs on the egui UI thread and
/// communicates back to the app via [`TrayHandle::poll_events`].
pub struct WindowsTray {
    icon: TrayIcon,
    /// Connect / Disconnect picked in the tray menu, forwarded by the menu
    /// thread. Drained by the frame loop.
    actions: Receiver<TrayAction>,
    last_state: Cell<TrayState>,
}

/// The tray menu item ids, moved into the menu thread.
struct MenuIds {
    show: MenuId,
    connect: MenuId,
    disconnect: MenuId,
    quit: MenuId,
}

impl WindowsTray {
    pub fn new(quit: QuitFlag) -> Option<Self> {
        let item_show = MenuItem::with_id(MENU_SHOW, "Show window", true, None);
        let item_connect = MenuItem::with_id(MENU_CONNECT, "Connect", true, None);
        let item_disconnect = MenuItem::with_id(MENU_DISCONNECT, "Disconnect", true, None);
        let item_quit = MenuItem::with_id(MENU_QUIT, "Quit", true, None);

        let id_show = item_show.id().clone();
        let id_connect = item_connect.id().clone();
        let id_disconnect = item_disconnect.id().clone();
        let id_quit = item_quit.id().clone();

        let menu = Menu::new();
        let _ = menu.append(&item_show);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item_connect);
        let _ = menu.append(&item_disconnect);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item_quit);

        // Start gray (disconnected) - will update on first frame via tray state machine.
        let Some(icon) = lightning_icon(160, 160, 160) else {
            tracing::warn!("Tray icon image unavailable; continuing without a system tray");
            return None;
        };

        let icon = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("\u{26a1} LightSpeed \u{2014} disconnected")
            .with_icon(icon)
            .build()
        {
            Ok(icon) => icon,
            Err(e) => {
                tracing::warn!("Failed to create tray icon ({e}); continuing without a tray");
                return None;
            }
        };

        let (tx, actions) = channel();
        spawn_menu_thread(
            MenuIds {
                show: id_show,
                connect: id_connect,
                disconnect: id_disconnect,
                quit: id_quit,
            },
            quit,
            tx,
        );
        spawn_icon_thread();

        Some(WindowsTray {
            icon,
            actions,
            last_state: Cell::new(TrayState::Disconnected),
        })
    }
}

/// Read tray menu events off the frame loop.
///
/// Every item brings the window back except Quit: Connect and Disconnect need
/// the app, and the app only runs while the window is painting.
fn spawn_menu_thread(ids: MenuIds, quit: QuitFlag, actions: Sender<TrayAction>) {
    let spawned = std::thread::Builder::new()
        .name("ls-tray-menu".into())
        .spawn(move || {
            while let Ok(event) = MenuEvent::receiver().recv() {
                tracing::debug!("Tray menu event: {:?}", event.id);
                if event.id == ids.show {
                    native::show_main_window();
                } else if event.id == ids.connect {
                    let _ = actions.send(TrayAction::Connect);
                    native::show_main_window();
                } else if event.id == ids.disconnect {
                    let _ = actions.send(TrayAction::Disconnect);
                    native::show_main_window();
                } else if event.id == ids.quit {
                    // Tray Quit must terminate the process, not hide the
                    // window. A painting frame loop sees the flag at once; the
                    // quit watchdog in app.rs covers a hidden window.
                    tracing::info!("Tray Quit picked");
                    quit.store(true, Ordering::Relaxed);
                    native::repaint_main_window();
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start the tray menu thread: {e}");
    }
}

/// Read tray icon events off the frame loop: a double-click shows the window.
///
/// The thread also keeps the event queue drained. The icon reports every
/// mouse move over it, and nothing read that queue while the window was
/// hidden.
fn spawn_icon_thread() {
    let spawned = std::thread::Builder::new()
        .name("ls-tray-icon".into())
        .spawn(move || {
            while let Ok(event) = TrayIconEvent::receiver().recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    native::show_main_window();
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start the tray icon thread: {e}");
    }
}

impl TrayHandle for WindowsTray {
    fn poll_events(&self, _ctx: &egui::Context) -> Vec<TrayAction> {
        // Show and Quit are handled on the tray threads; only the actions
        // that need the engine arrive here.
        self.actions.try_iter().collect()
    }

    fn set_state(&self, state: TrayState, rtt_ms: f64) {
        if self.last_state.get() == state {
            return;
        }
        self.last_state.set(state);

        let (r, g, b): (u8, u8, u8) = match state {
            TrayState::Disconnected => (160, 160, 160),
            TrayState::Connected => (255, 200, 60),
            TrayState::Optimizing => (80, 210, 120),
            TrayState::Error => (220, 80, 80),
        };
        let tooltip: String = match state {
            TrayState::Disconnected => "\u{26a1} LightSpeed \u{2014} disconnected".into(),
            TrayState::Connected => {
                format!(
                    "\u{26a1} LightSpeed \u{2014} connected \u{00b7} RTT {:.0}ms",
                    rtt_ms
                )
            }
            TrayState::Optimizing => "\u{26a1} LightSpeed \u{2014} optimizing".into(),
            TrayState::Error => "\u{26a1} LightSpeed \u{2014} error".into(),
        };

        if let Some(icon) = lightning_icon(r, g, b) {
            let _ = self.icon.set_icon(Some(icon));
        }
        let _ = self.icon.set_tooltip(Some(&tooltip));
    }
}

/// Windows [`Platform`] backend.
///
/// Uses `tray_icon` for the system tray, admin check via `net session`, and
/// Npcap detection via `sc query`.
pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    type Tray = WindowsTray;

    fn new_tray(quit: QuitFlag) -> Option<Self::Tray> {
        let tray = WindowsTray::new(quit);
        TRAY_AVAILABLE.store(tray.is_some(), Ordering::SeqCst);
        if tray.is_none() {
            tracing::warn!("System tray unavailable; closing the window will exit the app");
        }
        tray
    }

    /// Whether this process can raise an interface (needs the driver).
    ///
    /// Reads the process token rather than shelling out to `net session`,
    /// which also fails when the Server service is merely stopped - reporting a
    /// limited user on a machine where the process is in fact elevated.
    fn is_admin() -> bool {
        // Declared locally: pulling in `windows-sys` for one token query would
        // add a dependency with a dozen features for three calls.
        #[link(name = "advapi32")]
        extern "system" {
            fn OpenProcessToken(
                process: *mut core::ffi::c_void,
                desired_access: u32,
                token: *mut *mut core::ffi::c_void,
            ) -> i32;
            fn GetTokenInformation(
                token: *mut core::ffi::c_void,
                class: i32,
                info: *mut core::ffi::c_void,
                len: u32,
                returned: *mut u32,
            ) -> i32;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> *mut core::ffi::c_void;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }

        const TOKEN_QUERY: u32 = 0x0008;
        const TOKEN_ELEVATION_CLASS: i32 = 20;

        // SAFETY: `token` is only read after a successful `OpenProcessToken`
        // and is closed on every path; the buffer passed to
        // `GetTokenInformation` is exactly the size of the struct it fills.
        unsafe {
            let mut token: *mut core::ffi::c_void = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevated: u32 = 0;
            let mut returned: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TOKEN_ELEVATION_CLASS,
                &mut elevated as *mut u32 as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
                &mut returned,
            );
            CloseHandle(token);
            ok != 0 && elevated != 0
        }
    }

    fn is_capture_available() -> bool {
        crate::platform::silent_command("sc")
            .args(["query", "npcap"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn setup_fonts(_ctx: &egui::Context) {
        // egui's bundled default fonts already include a monochrome Noto
        // Emoji, which renders the UI emojis consistently on every platform.
        // Color-emoji fonts cannot be rasterized by egui, so nothing else is
        // loaded here.
    }

    fn has_system_tray() -> bool {
        TRAY_AVAILABLE.load(Ordering::SeqCst)
    }

    /// Relaunch elevated, but only when that would actually change anything.
    ///
    /// The previous version always tore the process down and respawned it, so
    /// pressing the button on an already-elevated install restarted the app as
    /// admin at itself - a pointless flash of the window and a dropped session.
    /// When this process is already elevated there is nothing to raise, so it
    /// says so and does nothing.
    fn relaunch_as_admin() {
        if Self::is_admin() {
            tracing::info!("already elevated; not relaunching");
            return;
        }
        let exe = std::env::current_exe()
            .unwrap_or_default()
            .display()
            .to_string();
        let script = format!("Start-Process '{}' -Verb RunAs", exe.replace('\'', "''"));
        let _ = crate::platform::silent_command("powershell")
            .args(["-WindowStyle", "Hidden", "-Command", &script])
            .spawn();
        std::process::exit(0);
    }
}

// ── Main window control ──────────────────────────────────────────────────────

/// Show, restore and repaint the main window from any thread.
///
/// `egui::ViewportCommand`s cannot do this: they are applied at the end of a
/// frame, and a hidden window gets no frames.
mod native {
    use std::ffi::c_void;

    type Hwnd = *mut c_void;

    #[link(name = "user32")]
    extern "system" {
        fn FindWindowExW(
            parent: Hwnd,
            child_after: Hwnd,
            class: *const u16,
            title: *const u16,
        ) -> Hwnd;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
        fn GetClassNameW(hwnd: Hwnd, buffer: *mut u16, capacity: i32) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn ShowWindowAsync(hwnd: Hwnd, command: i32) -> i32;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn InvalidateRect(hwnd: Hwnd, rect: *const c_void, erase: i32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    /// Window class of every Win32 dialog, including `MessageBoxW`.
    const DIALOG_CLASS: &str = "#32770";

    /// This process's main window, visible or not.
    ///
    /// Matched by title among the top-level windows this process owns. The
    /// crash dialog carries the same title, so dialogs are skipped.
    fn find_main_window() -> Option<Hwnd> {
        let title: Vec<u16> = crate::platform::WINDOW_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: plain kernel32 call with no arguments.
        let own_process = unsafe { GetCurrentProcessId() };
        let mut hwnd: Hwnd = std::ptr::null_mut();
        loop {
            // SAFETY: `title` is NUL-terminated and outlives the call; a null
            // parent and class enumerate top-level windows of any class, and
            // `hwnd` is null or a handle this function returned.
            hwnd = unsafe {
                FindWindowExW(std::ptr::null_mut(), hwnd, std::ptr::null(), title.as_ptr())
            };
            if hwnd.is_null() {
                return None;
            }
            let mut process: u32 = 0;
            // SAFETY: `hwnd` was just returned by `FindWindowExW` and
            // `process` is a valid out pointer.
            unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
            if process == own_process && !is_dialog(hwnd) {
                return Some(hwnd);
            }
        }
    }

    fn is_dialog(hwnd: Hwnd) -> bool {
        let mut buffer = [0u16; 64];
        // SAFETY: the buffer is 64 UTF-16 units long and that capacity is
        // what is passed.
        let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        let len = usize::try_from(len).unwrap_or(0).min(buffer.len());
        String::from_utf16_lossy(&buffer[..len]) == DIALOG_CLASS
    }

    /// Un-hide (or un-minimize) the main window and bring it to the front.
    pub fn show_main_window() {
        let Some(hwnd) = find_main_window() else {
            tracing::warn!("tray: main window not found, cannot show it");
            return;
        };
        // SAFETY: `hwnd` is a live top-level window of this process. The
        // async variant posts to the UI thread, so a busy frame loop cannot
        // block the tray thread.
        unsafe {
            let command = if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            ShowWindowAsync(hwnd, command);
            SetForegroundWindow(hwnd);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }

    /// Ask a visible main window for a frame without changing its state.
    pub fn repaint_main_window() {
        if let Some(hwnd) = find_main_window() {
            // SAFETY: `hwnd` is a live top-level window of this process.
            unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
        }
    }
}

// ── Icon generation ──────────────────────────────────────────────────────────

fn lightning_icon(r: u8, g: u8, b: u8) -> Option<tray_icon::Icon> {
    const SIZE: usize = 32;
    const LOGO_PNG: &[u8] = include_bytes!("../../../web/assets/brand/icon-256.png");
    let decoded = match image::load_from_memory(LOGO_PNG) {
        Ok(decoded) => decoded
            .resize_exact(
                SIZE as u32,
                SIZE as u32,
                image::imageops::FilterType::Lanczos3,
            )
            .to_rgba8(),
        Err(e) => {
            tracing::warn!("could not decode tray logo: {e}");
            return None;
        }
    };
    let mut rgba = decoded.into_raw();
    // Per-state signal: an 8px corner square in the state colour
    // (grey / amber / green / red). The mark itself is the same artwork the
    // window, taskbar and About surfaces use.
    const DOT: usize = 8;
    for y in (SIZE - DOT)..SIZE {
        for x in (SIZE - DOT)..SIZE {
            let idx = (y * SIZE + x) * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    match tray_icon::Icon::from_rgba(rgba, SIZE as u32, SIZE as u32) {
        Ok(icon) => Some(icon),
        Err(e) => {
            tracing::warn!("Failed to build tray icon from RGBA data: {e}");
            None
        }
    }
}
