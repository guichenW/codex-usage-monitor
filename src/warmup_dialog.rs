use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    IsDlgButtonChecked, BST_CHECKED, EM_SETCUEBANNER, EM_SETLIMITTEXT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::codex_warmup::{self, Settings};
use crate::localization::LanguageId;
use crate::native_interop::wide_str;

const DIALOG: usize = 221;
const ENABLED: i32 = 1201;
const COMMON: i32 = 1203;
const APPLY_WEEK: i32 = 1204;
const MODEL: i32 = 1207;
const STATUS: i32 = 1208;
const DAY_FIRST: i32 = 1220;
static ACTIVE_DIALOG: AtomicIsize = AtomicIsize::new(0);

struct Context {
    settings: Settings,
    chinese: bool,
    summary: String,
    result: Option<Settings>,
}

pub fn menu_label(language: LanguageId) -> &'static str {
    if chinese(language) {
        "Codex 定时预热…"
    } else {
        "Codex scheduled warm-up..."
    }
}

fn chinese(language: LanguageId) -> bool {
    matches!(
        language,
        LanguageId::SimplifiedChinese | LanguageId::TraditionalChinese
    )
}

pub fn show(
    settings: Settings,
    language: LanguageId,
    summary: String,
) -> Result<Option<Settings>, String> {
    let active = ACTIVE_DIALOG.load(Ordering::Relaxed);
    if active != 0 {
        unsafe {
            let _ = SetForegroundWindow(HWND(active as *mut _));
        }
        return Ok(None);
    }
    let mut context = Context {
        settings,
        chinese: chinese(language),
        summary,
        result: None,
    };
    let result = unsafe {
        let module = GetModuleHandleW(PCWSTR::null())
            .map_err(|_| "Unable to open the warm-up settings.".to_owned())?;
        DialogBoxParamW(
            HINSTANCE(module.0),
            PCWSTR(DIALOG as *const u16),
            HWND::default(),
            Some(dialog_proc),
            LPARAM(&mut context as *mut _ as isize),
        )
    };
    if result == -1 {
        return Err("Unable to open the warm-up settings.".to_owned());
    }
    Ok(context.result)
}

unsafe fn set_text(dialog: HWND, id: i32, text: &str) {
    let value = wide_str(text);
    let _ = SetDlgItemTextW(dialog, id, PCWSTR(value.as_ptr()));
}

unsafe fn text(dialog: HWND, id: i32) -> String {
    let mut buffer = [0u16; 2049];
    let count = GetDlgItemTextW(dialog, id, &mut buffer) as usize;
    String::from_utf16_lossy(&buffer[..count])
}

unsafe fn collect(dialog: HWND) -> Result<Settings, &'static str> {
    let mut settings = Settings {
        enabled: IsDlgButtonChecked(dialog, ENABLED) == BST_CHECKED.0,
        model: text(dialog, MODEL),
        ..Default::default()
    };
    for (index, day) in settings.days.iter_mut().enumerate() {
        *day = codex_warmup::parse_times(&text(dialog, DAY_FIRST + index as i32))?;
    }
    settings.validated()
}

unsafe fn apply_week(dialog: HWND) -> Result<(), &'static str> {
    let value = codex_warmup::parse_times(&text(dialog, COMMON))?.join(", ");
    for index in 0..7 {
        set_text(dialog, DAY_FIRST + index, &value);
    }
    set_text(dialog, COMMON, &value);
    Ok(())
}

unsafe fn validation_error(dialog: HWND, chinese: bool, error: &str) {
    let message = wide_str(&if chinese {
        format!("请检查时刻和模型名称。时间示例：07:00, 13:00, 19:00；每天最多 24 个时刻，模型可留空。\n\n{error}")
    } else {
        error.to_owned()
    });
    let title = wide_str(if chinese {
        "定时预热设置"
    } else {
        "Scheduled warm-up"
    });
    MessageBoxW(
        dialog,
        PCWSTR(message.as_ptr()),
        PCWSTR(title.as_ptr()),
        MB_OK | MB_ICONWARNING,
    );
}

unsafe extern "system" fn dialog_proc(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if message == WM_INITDIALOG {
        let context = &mut *(lparam.0 as *mut Context);
        SetWindowLongPtrW(dialog, GWLP_USERDATA, lparam.0);
        ACTIVE_DIALOG.store(dialog.0 as isize, Ordering::Relaxed);
        SendDlgItemMessageW(
            dialog,
            ENABLED,
            BM_SETCHECK,
            WPARAM(if context.settings.enabled {
                BST_CHECKED.0 as usize
            } else {
                0
            }),
            LPARAM(0),
        );
        let common = if context
            .settings
            .days
            .iter()
            .all(|day| day == &context.settings.days[0])
        {
            context.settings.days[0].join(", ")
        } else {
            "07:00, 13:00, 19:00".into()
        };
        set_text(dialog, COMMON, &common);
        set_text(dialog, MODEL, &context.settings.model);
        set_text(dialog, STATUS, &context.summary);
        for index in 0..7 {
            set_text(
                dialog,
                DAY_FIRST + index,
                &context.settings.days[index as usize].join(", "),
            );
        }
        for id in [COMMON, MODEL, 1220, 1221, 1222, 1223, 1224, 1225, 1226] {
            SendDlgItemMessageW(
                dialog,
                id,
                EM_SETLIMITTEXT,
                WPARAM(if id == MODEL { 128 } else { 2048 }),
                LPARAM(0),
            );
        }
        let cue = wide_str(if context.chinese {
            "留空自动选择，优先轻量型号"
        } else {
            "Automatic, preferring a lightweight model"
        });
        SendDlgItemMessageW(
            dialog,
            MODEL,
            EM_SETCUEBANNER,
            WPARAM(0),
            LPARAM(cue.as_ptr() as isize),
        );
        if context.chinese {
            let title = wide_str("Codex Usage — 每周定时预热");
            let _ = SetWindowTextW(dialog, PCWSTR(title.as_ptr()));
            for (id, label) in [
                (ENABLED, "启用定时预热（按电脑本地时间）"),
                (1202, "每天统一设置："),
                (APPLY_WEEK, "应用到整周"),
                (1205, "多个时刻用逗号分隔，留空表示当天不触发。"),
                (1206, "模型："),
                (1209, "到点发送真实模型请求，会消耗用量。错过的时段不补发。"),
                (IDOK.0, "保存"),
                (IDCANCEL.0, "取消"),
            ] {
                set_text(dialog, id, label);
            }
            for (index, day) in ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
                .into_iter()
                .enumerate()
            {
                set_text(dialog, 1210 + index as i32, day);
            }
        }
        return 1;
    }
    match message {
        WM_COMMAND => {
            let pointer = GetWindowLongPtrW(dialog, GWLP_USERDATA) as *mut Context;
            if pointer.is_null() {
                return 0;
            }
            let context = &mut *pointer;
            match (wparam.0 & 0xffff) as i32 {
                APPLY_WEEK => {
                    if let Err(error) = apply_week(dialog) {
                        validation_error(dialog, context.chinese, error);
                    }
                    1
                }
                id if id == IDOK.0 => {
                    match collect(dialog) {
                        Ok(settings) => {
                            context.result = Some(settings);
                            let _ = EndDialog(dialog, IDOK.0 as isize);
                        }
                        Err(error) => validation_error(dialog, context.chinese, error),
                    }
                    1
                }
                id if id == IDCANCEL.0 => {
                    let _ = EndDialog(dialog, IDCANCEL.0 as isize);
                    1
                }
                _ => 0,
            }
        }
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

    #[test]
    fn native_weekly_editor_loads_copies_and_validates_all_days() {
        unsafe {
            let module = GetModuleHandleW(PCWSTR::null()).unwrap();
            for chinese in [false, true] {
                let mut context = Context {
                    settings: Settings::default(),
                    chinese,
                    summary: "No attempts yet".into(),
                    result: None,
                };
                let dialog = CreateDialogParamW(
                    HINSTANCE(module.0),
                    PCWSTR(DIALOG as *const u16),
                    HWND::default(),
                    Some(dialog_proc),
                    LPARAM(&mut context as *mut _ as isize),
                )
                .unwrap();
                assert!(!collect(dialog).unwrap().enabled);
                assert_eq!(text(dialog, DAY_FIRST), "07:00, 13:00, 19:00");
                set_text(dialog, DAY_FIRST, "");
                assert!(collect(dialog).unwrap().days[0].is_empty());
                set_text(dialog, COMMON, "8:05，20:10");
                apply_week(dialog).unwrap();
                let settings = collect(dialog).unwrap();
                assert!(settings
                    .days
                    .iter()
                    .all(|day| day == &vec!["08:05", "20:10"]));
                set_text(dialog, DAY_FIRST, "25:00");
                assert!(collect(dialog).is_err());
                assert!(context.result.is_none());
                DestroyWindow(dialog).unwrap();
            }
        }
    }
}
