#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use temp_cleaner::app::TempCleanerApp;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 760.0])
            .with_min_inner_size([760.0, 560.0])
            .with_title("Temp 清理工具"),
        centered: true,
        ..Default::default()
    };

    let result = eframe::run_native(
        "TempCleaner",
        options,
        Box::new(|cc| Ok(Box::new(TempCleanerApp::new(cc)))),
    );

    if let Err(error) = &result {
        show_fatal_error(&format!(
            "无法启动 Temp 清理工具界面：\n{error}\n\n请确认系统支持 OpenGL，并检查程序文件是否完整。"
        ));
    }
    result
}

fn show_fatal_error(message: &str) {
    let text: Vec<u16> = OsStr::new(message)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let title: Vec<u16> = OsStr::new("Temp 清理工具")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: both pointers refer to valid NUL-terminated UTF-16 buffers that
    // live for the duration of the call. A null owner window is documented.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
