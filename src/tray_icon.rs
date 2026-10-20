use std::sync::{Mutex, MutexGuard};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::native_interop::WM_APP_TRAY;

const APP_TRAY_ICON_ID: u32 = 1;
const LEGACY_CODEX_TRAY_ICON_ID: u32 = 2;
const LEGACY_ANTIGRAVITY_TRAY_ICON_ID: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayOperation {
    Add,
    Update,
    Remove(u32),
}

#[derive(Default)]
struct TrayLifecycle {
    stopped: bool,
}

impl TrayLifecycle {
    fn sync(&self, visible: bool, mut notify: impl FnMut(TrayOperation) -> bool) {
        if self.stopped {
            return;
        }
        notify(TrayOperation::Remove(LEGACY_CODEX_TRAY_ICON_ID));
        notify(TrayOperation::Remove(LEGACY_ANTIGRAVITY_TRAY_ICON_ID));
        if visible {
            // Modify the existing registration; add only on first use or after
            // the shell has lost it. Never add on every successful refresh.
            if !notify(TrayOperation::Update) {
                notify(TrayOperation::Add);
            }
        } else {
            notify(TrayOperation::Remove(APP_TRAY_ICON_ID));
        }
    }

    fn shutdown(&mut self, mut notify: impl FnMut(TrayOperation) -> bool) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        for id in [
            APP_TRAY_ICON_ID,
            LEGACY_CODEX_TRAY_ICON_ID,
            LEGACY_ANTIGRAVITY_TRAY_ICON_ID,
        ] {
            notify(TrayOperation::Remove(id));
        }
    }
}

// Serialize UI refreshes with watchdog cleanup so a pending refresh cannot
// re-register an icon between its deletion and process::exit.
static LIFECYCLE: Mutex<TrayLifecycle> = Mutex::new(TrayLifecycle { stopped: false });

fn lock_lifecycle() -> MutexGuard<'static, TrayLifecycle> {
    LIFECYCLE.lock().unwrap_or_else(|error| error.into_inner())
}

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 70;

/// Actions the tray message handler can request from the main window.
pub enum TrayAction {
    None,
    ToggleWidget,
    ShowContextMenu,
}

/// Identifies the provider that originated a notification. All notifications
/// now use the one application tray icon rather than provider-specific icons.
#[derive(Clone, Copy)]
pub enum TrayIconKind {
    Claude,
    Codex,
    Antigravity,
}

pub struct TrayIconData {
    pub tooltip: String,
}

/// Load the same embedded icon used by the executable and desktop shortcut.
pub fn create_icon() -> HICON {
    load_embedded_app_icon()
}

fn load_embedded_app_icon() -> HICON {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return HICON::default();
        }

        let mut small_icon = HICON::default();
        let mut large_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            return HICON::default();
        }

        if !small_icon.is_invalid() {
            if !large_icon.is_invalid() {
                let _ = DestroyIcon(large_icon);
            }
            small_icon
        } else {
            large_icon
        }
    }
}

/// Show a Windows balloon notification from the single application tray icon.
pub fn notify_balloon(hwnd: HWND, _kind: TrayIconKind, title: &str, message: &str) {
    let lifecycle = lock_lifecycle();
    if lifecycle.stopped {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = APP_TRAY_ICON_ID;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(title, &mut nid.szInfoTitle);
        copy_wide(message, &mut nid.szInfo);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn copy_wide<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut len = wide.len().min(N - 1);
    if len > 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
        len -= 1;
    }
    buf[..len].copy_from_slice(&wide[..len]);
    buf[len] = 0;
}

fn add(hwnd: HWND, tooltip: &str) -> bool {
    let hicon = create_icon();
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = APP_TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = hicon;
        copy_wide(tooltip, &mut nid.szTip);
        let added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        diagnose::log(format!("tray: NIM_ADD hwnd={hwnd:?} success={added}"));
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
        added
    }
}

fn update(hwnd: HWND, tooltip: &str) -> bool {
    let hicon = create_icon();
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = APP_TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = hicon;
        copy_wide(tooltip, &mut nid.szTip);
        let updated = Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
        if !updated {
            diagnose::log(format!(
                "tray: NIM_MODIFY hwnd={hwnd:?} failed, attempting registration"
            ));
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
        updated
    }
}

fn remove_id(hwnd: HWND, id: u32) -> bool {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = id;
        Shell_NotifyIconW(NIM_DELETE, &nid).as_bool()
    }
}

pub fn sync(hwnd: HWND, icon: Option<&TrayIconData>) {
    let lifecycle = lock_lifecycle();
    let tooltip = icon.map(|icon| icon.tooltip.as_str()).unwrap_or_default();
    lifecycle.sync(icon.is_some(), |operation| match operation {
        TrayOperation::Add => add(hwnd, tooltip),
        TrayOperation::Update => update(hwnd, tooltip),
        TrayOperation::Remove(id) => remove_id(hwnd, id),
    });
}

pub fn remove_all(hwnd: HWND) {
    let mut lifecycle = lock_lifecycle();
    lifecycle.shutdown(|operation| match operation {
        TrayOperation::Remove(id) => {
            let removed = remove_id(hwnd, id);
            diagnose::log(format!(
                "tray: NIM_DELETE hwnd={hwnd:?} id={id} success={removed}"
            ));
            removed
        }
        _ => unreachable!("shutdown only removes tray icons"),
    });
}

/// Interpret a tray callback message and return the action to take.
pub fn handle_message(lparam: LPARAM) -> TrayAction {
    let mouse_msg = lparam.0 as u32;
    match mouse_msg {
        WM_LBUTTONUP => TrayAction::ToggleWidget,
        WM_RBUTTONUP => TrayAction::ShowContextMenu,
        _ => TrayAction::None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeShell {
        registered: bool,
        successful_adds: usize,
        operations: Vec<TrayOperation>,
    }

    impl FakeShell {
        fn notify(&mut self, operation: TrayOperation) -> bool {
            self.operations.push(operation);
            match operation {
                TrayOperation::Add => {
                    if self.registered {
                        return false;
                    }
                    self.registered = true;
                    self.successful_adds += 1;
                    true
                }
                TrayOperation::Update => self.registered,
                TrayOperation::Remove(APP_TRAY_ICON_ID) => {
                    std::mem::replace(&mut self.registered, false)
                }
                TrayOperation::Remove(_) => false,
            }
        }
    }

    #[test]
    fn repeated_refreshes_keep_one_registration() {
        let lifecycle = TrayLifecycle::default();
        let mut shell = FakeShell::default();
        for _ in 0..10 {
            lifecycle.sync(true, |operation| shell.notify(operation));
        }
        assert!(shell.registered);
        assert_eq!(shell.successful_adds, 1);
        assert_eq!(
            shell
                .operations
                .iter()
                .filter(|op| **op == TrayOperation::Add)
                .count(),
            1
        );
    }

    #[test]
    fn shell_restart_restores_the_registration() {
        let lifecycle = TrayLifecycle::default();
        let mut shell = FakeShell::default();
        lifecycle.sync(true, |operation| shell.notify(operation));
        shell.registered = false;
        lifecycle.sync(true, |operation| shell.notify(operation));
        assert!(shell.registered);
        assert_eq!(shell.successful_adds, 2);
    }

    #[test]
    fn shutdown_deletes_icons_and_blocks_pending_refreshes() {
        let mut lifecycle = TrayLifecycle::default();
        let mut shell = FakeShell::default();
        lifecycle.sync(true, |operation| shell.notify(operation));
        shell.operations.clear();
        lifecycle.shutdown(|operation| shell.notify(operation));
        assert!(!shell.registered);
        assert_eq!(
            shell.operations,
            vec![
                TrayOperation::Remove(APP_TRAY_ICON_ID),
                TrayOperation::Remove(LEGACY_CODEX_TRAY_ICON_ID),
                TrayOperation::Remove(LEGACY_ANTIGRAVITY_TRAY_ICON_ID),
            ]
        );
        let cleanup = shell.operations.clone();
        lifecycle.sync(true, |operation| shell.notify(operation));
        lifecycle.shutdown(|operation| shell.notify(operation));
        assert_eq!(shell.operations, cleanup);
        assert_eq!(shell.successful_adds, 1);
    }

    #[test]
    fn failed_registration_is_retried_on_the_next_refresh() {
        let lifecycle = TrayLifecycle::default();
        lifecycle.sync(true, |_| false);
        let mut shell = FakeShell::default();
        lifecycle.sync(true, |operation| shell.notify(operation));
        assert!(shell.registered);
        assert_eq!(shell.successful_adds, 1);
    }
}
