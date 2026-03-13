#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod common;
mod ftp;
mod httpd;
mod tftp;

use std::ptr;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const TIMER_LOG_UPDATE: usize = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn make_hicon() -> HICON {
    let size: i32 = 32;
    let bg = [0xD2u8, 0x76, 0x19, 0xFF];
    let fg = [0xFF, 0xFF, 0xFF, 0xFF];

    let mut pixels = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        let bmp_y = size - 1 - y;
        for x in 0..size {
            let top_bar = y >= 5 && y <= 9 && x >= 6 && x <= 25;
            let stem = y >= 5 && y <= 26 && x >= 13 && x <= 18;
            let color = if top_bar || stem { &fg } else { &bg };
            let offset = ((bmp_y * size + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(color);
        }
    }

    let mask = vec![0u8; (((size + 31) / 32) * 4 * size) as usize];

    unsafe {
        CreateIcon(
            GetModuleHandleW(ptr::null()),
            size,
            size,
            1,
            32,
            mask.as_ptr(),
            pixels.as_ptr(),
        )
    }
}

const WM_CTLCOLORSTATIC: u32 = 0x0138;

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLORSTATIC => {
            let result = app::on_ctlcolor_static(wparam, lparam);
            if result != 0 { result } else { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_COMMAND => {
            app::on_command(hwnd, wparam, lparam);
            0
        }
        WM_DRAWITEM => {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER => {
            if wparam == TIMER_LOG_UPDATE {
                app::on_timer(hwnd);
            }
            0
        }
        WM_NOTIFY => {
            app::on_notify(lparam);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SIZE => {
            app::on_size(hwnd, lparam);
            0
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = 520;
            mmi.ptMinTrackSize.y = 440;
            0
        }
        WM_DESTROY => {
            app::on_destroy();
            KillTimer(hwnd, TIMER_LOG_UPDATE);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    if let Err(e) = run_app() {
        show_error(&format!("Failed to start Tiny Servers:\n\n{}", e));
    }
}

fn run_app() -> Result<(), String> {
    unsafe {
        let class_name = wide("TinyServersClass");
        let hinstance = GetModuleHandleW(ptr::null());

        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES | ICC_TAB_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let icon = make_hicon();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: icon,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE as usize + 1) as HBRUSH,
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: icon,
        };

        if RegisterClassExW(&wc) == 0 {
            return Err("RegisterClassExW failed".into());
        }

        let title = wide("Tiny Servers");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            660,
            580,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        );

        if hwnd.is_null() {
            return Err("CreateWindowExW failed".into());
        }

        app::on_create(hwnd);

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        SetTimer(hwnd, TIMER_LOG_UPDATE, 200, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    Ok(())
}

fn show_error(msg: &str) {
    let text = wide(msg);
    let caption = wide("Tiny Servers - Error");
    unsafe {
        MessageBoxW(ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_OK | MB_ICONERROR);
    }
}
