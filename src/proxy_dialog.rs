use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    CheckRadioButton, IsDlgButtonChecked, BST_CHECKED, EM_SETLIMITTEXT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::localization::LanguageId;
use crate::native_interop::wide_str;
use crate::proxy::{ProxyMode, ProxySettings};

const DIALOG_ID: usize = 220;
const ENVIRONMENT: i32 = 1101;
const CUSTOM: i32 = 1102;
const DIRECT: i32 = 1103;
const ADDRESS_LABEL: i32 = 1104;
const ADDRESS: i32 = 1105;
const HELP: i32 = 1106;
static ACTIVE_DIALOG: AtomicIsize = AtomicIsize::new(0);

struct DialogState {
    settings: ProxySettings,
    chinese: bool,
    result: Option<ProxySettings>,
}

pub fn menu_label(language: LanguageId) -> &'static str {
    if is_chinese(language) {
        "代理设置…"
    } else {
        "Proxy settings..."
    }
}

fn is_chinese(language: LanguageId) -> bool {
    matches!(
        language,
        LanguageId::SimplifiedChinese | LanguageId::TraditionalChinese
    )
}

pub fn show(
    settings: ProxySettings,
    language: LanguageId,
) -> Result<Option<ProxySettings>, String> {
    let active = ACTIVE_DIALOG.load(Ordering::Relaxed);
    if active != 0 {
        unsafe {
            let _ = SetForegroundWindow(HWND(active as *mut _));
        }
        return Ok(None);
    }
    let mut state = DialogState {
        settings,
        chinese: is_chinese(language),
        result: None,
    };
    let result = unsafe {
        let module = GetModuleHandleW(PCWSTR::null())
            .map_err(|_| "Unable to open proxy settings.".to_owned())?;
        // The widget is a child of Explorer's taskbar. Use an independent owner
        // to avoid disabling Explorer while this modal settings dialog is open.
        DialogBoxParamW(
            HINSTANCE(module.0),
            PCWSTR(DIALOG_ID as *const u16),
            HWND::default(),
            Some(dialog_proc),
            LPARAM(&mut state as *mut _ as isize),
        )
    };
    if result == -1 {
        return Err("Unable to open proxy settings.".to_owned());
    }
    Ok(state.result)
}

unsafe fn set_text(dialog: HWND, id: i32, text: &str) {
    let wide = wide_str(text);
    let _ = SetDlgItemTextW(dialog, id, PCWSTR(wide.as_ptr()));
}

unsafe fn update_controls(dialog: HWND) {
    if let Ok(edit) = GetDlgItem(dialog, ADDRESS) {
        let _ = EnableWindow(edit, IsDlgButtonChecked(dialog, CUSTOM) == BST_CHECKED.0);
    }
}

unsafe fn collect_settings(dialog: HWND) -> Result<ProxySettings, &'static str> {
    let mode = if IsDlgButtonChecked(dialog, CUSTOM) == BST_CHECKED.0 {
        ProxyMode::Custom
    } else if IsDlgButtonChecked(dialog, DIRECT) == BST_CHECKED.0 {
        ProxyMode::Direct
    } else {
        ProxyMode::Environment
    };
    let mut address = [0u16; 2049];
    let count = GetDlgItemTextW(dialog, ADDRESS, &mut address) as usize;
    ProxySettings {
        mode,
        url: String::from_utf16_lossy(&address[..count]),
    }
    .validated()
}

unsafe extern "system" fn dialog_proc(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if message == WM_INITDIALOG {
        let state = &mut *(lparam.0 as *mut DialogState);
        SetWindowLongPtrW(dialog, GWLP_USERDATA, lparam.0);
        ACTIVE_DIALOG.store(dialog.0 as isize, Ordering::Relaxed);
        let selected = match state.settings.mode {
            ProxyMode::Environment => ENVIRONMENT,
            ProxyMode::Custom => CUSTOM,
            ProxyMode::Direct => DIRECT,
        };
        let _ = CheckRadioButton(dialog, ENVIRONMENT, DIRECT, selected);
        SendDlgItemMessageW(dialog, ADDRESS, EM_SETLIMITTEXT, WPARAM(2048), LPARAM(0));
        set_text(dialog, ADDRESS, &state.settings.url);
        if state.chinese {
            let title = wide_str("Codex Usage — 代理设置");
            let _ = SetWindowTextW(dialog, PCWSTR(title.as_ptr()));
            for (id, text) in [
                (ENVIRONMENT, "跟随环境变量（兼容原启动脚本）"),
                (CUSTOM, "使用自定义 HTTP 代理"),
                (DIRECT, "直接连接（不使用代理）"),
                (ADDRESS_LABEL, "代理地址："),
                (
                    HELP,
                    "用于额度查询和更新下载。HTTP 代理也支持 HTTPS 请求；保存后生效。",
                ),
                (IDOK.0, "保存"),
                (IDCANCEL.0, "取消"),
            ] {
                set_text(dialog, id, text);
            }
        }
        update_controls(dialog);
        return 1;
    }
    match message {
        WM_COMMAND => match (wparam.0 & 0xffff) as i32 {
            ENVIRONMENT | CUSTOM | DIRECT => {
                update_controls(dialog);
                1
            }
            id if id == IDOK.0 => {
                let pointer = GetWindowLongPtrW(dialog, GWLP_USERDATA) as *mut DialogState;
                if pointer.is_null() {
                    return 0;
                }
                let state = &mut *pointer;
                match collect_settings(dialog) {
                    Ok(settings) => {
                        state.result = Some(settings);
                        let _ = EndDialog(dialog, IDOK.0 as isize);
                    }
                    Err(_) => {
                        let message = wide_str(if state.chinese {
                            "请输入有效的 HTTP 代理地址，例如 http://127.0.0.1:7897。端口范围为 1–65535。"
                        } else {
                            "Enter a valid HTTP proxy address, such as http://127.0.0.1:7897. Port must be 1-65535."
                        });
                        let title = wide_str(if state.chinese {
                            "代理设置"
                        } else {
                            "Proxy settings"
                        });
                        MessageBoxW(
                            dialog,
                            PCWSTR(message.as_ptr()),
                            PCWSTR(title.as_ptr()),
                            MB_OK | MB_ICONWARNING,
                        );
                    }
                }
                1
            }
            id if id == IDCANCEL.0 => {
                let _ = EndDialog(dialog, IDCANCEL.0 as isize);
                1
            }
            _ => 0,
        },
        WM_CLOSE => {
            let _ = EndDialog(dialog, IDCANCEL.0 as isize);
            1
        }
        WM_DESTROY => {
            ACTIVE_DIALOG.store(0, Ordering::Relaxed);
            0
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

    #[test]
    fn native_dialog_loads_settings_and_validates_custom_addresses() {
        unsafe {
            let module = GetModuleHandleW(PCWSTR::null()).unwrap();
            for chinese in [false, true] {
                let mut state = DialogState {
                    settings: ProxySettings::default(),
                    chinese,
                    result: None,
                };
                let dialog = CreateDialogParamW(
                    HINSTANCE(module.0),
                    PCWSTR(DIALOG_ID as *const u16),
                    HWND::default(),
                    Some(dialog_proc),
                    LPARAM(&mut state as *mut _ as isize),
                )
                .unwrap();
                assert_eq!(IsDlgButtonChecked(dialog, ENVIRONMENT), BST_CHECKED.0);
                let edit = GetDlgItem(dialog, ADDRESS).unwrap();
                assert!(!IsWindowEnabled(edit).as_bool());
                let _ = CheckRadioButton(dialog, ENVIRONMENT, DIRECT, CUSTOM);
                update_controls(dialog);
                assert!(IsWindowEnabled(edit).as_bool());
                assert_eq!(collect_settings(dialog).unwrap().mode, ProxyMode::Custom);
                assert_eq!(
                    collect_settings(dialog).unwrap().url,
                    crate::proxy::DEFAULT_URL
                );
                set_text(dialog, ADDRESS, "http://localhost:99999");
                assert!(collect_settings(dialog).is_err());
                assert!(state.result.is_none());
                DestroyWindow(dialog).unwrap();
            }
        }
    }
}
