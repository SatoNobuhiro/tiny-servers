#![allow(static_mut_refs)]
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::common::{log_system, SharedLog};
use crate::{ftp, httpd, tftp};

// Static control styles (not exported by windows-sys under our features)
const SS_RIGHT: u32 = 2;
const SS_CENTER: u32 = 1;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn get_window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

unsafe fn create_child(
    class: &str,
    text: &str,
    style: u32,
    x: i32, y: i32, w: i32, h: i32,
    parent: HWND,
    id: i32,
) -> HWND {
    let cls = wide(class);
    let txt = wide(text);
    CreateWindowExW(
        0,
        cls.as_ptr(),
        txt.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x, y, w, h,
        parent,
        id as isize as HMENU,
        GetModuleHandleW(ptr::null()),
        ptr::null(),
    )
}

unsafe fn create_child_ex(
    ex_style: u32,
    class: &str,
    text: &str,
    style: u32,
    x: i32, y: i32, w: i32, h: i32,
    parent: HWND,
    id: i32,
) -> HWND {
    let cls = wide(class);
    let txt = wide(text);
    CreateWindowExW(
        ex_style,
        cls.as_ptr(),
        txt.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x, y, w, h,
        parent,
        id as isize as HMENU,
        GetModuleHandleW(ptr::null()),
        ptr::null(),
    )
}

unsafe fn set_font(hwnd: HWND, font: HFONT) {
    SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1);
}

unsafe fn draw_edit_border(hwnd: HWND) {
    let hdc = GetWindowDC(hwnd);
    if hdc.is_null() { return; }
    let mut rc: RECT = std::mem::zeroed();
    GetWindowRect(hwnd, &mut rc);
    rc.right -= rc.left;
    rc.bottom -= rc.top;
    rc.left = 0;
    rc.top = 0;
    let pen = CreatePen(PS_SOLID as i32, 1, 0x00ACACAC);
    let old_pen = SelectObject(hdc, pen);
    let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    Rectangle(hdc, rc.left, rc.top, rc.right, rc.bottom);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    DeleteObject(pen);
    ReleaseDC(hwnd, hdc);
}

unsafe extern "system" fn edit_border_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
    _uid: usize, _data: usize,
) -> LRESULT {
    let result = DefSubclassProc(hwnd, msg, wparam, lparam);
    if msg == WM_PAINT || msg == WM_NCPAINT {
        draw_edit_border(hwnd);
    }
    result
}

// ---------------------------------------------------------------------------
// Control IDs
// ---------------------------------------------------------------------------

const ID_COMBO_BIND: i32 = 100;
const ID_TAB_CTRL: i32 = 101;

// Tab control messages / constants
const TCM_FIRST: u32 = 0x1300;
const TCM_GETCURSEL: u32 = TCM_FIRST + 11;
const TCM_INSERTITEMW: u32 = TCM_FIRST + 62;
const TCM_SETITEMW: u32 = TCM_FIRST + 61;
const TCN_SELCHANGE: u32 = (-551i32) as u32;
const TCIF_TEXT: u32 = 0x0001;

#[repr(C)]
struct TabItem {
    mask: u32,
    dw_state: u32,
    dw_state_mask: u32,
    psz_text: *mut u16,
    cch_text_max: i32,
    i_image: i32,
    l_param: LPARAM,
}

const ID_FTP_ROOT: i32 = 200;
const ID_FTP_BROWSE: i32 = 201;
const ID_FTP_PORT: i32 = 202;
const ID_FTP_USER: i32 = 203;
const ID_FTP_PASS: i32 = 204;
const ID_FTP_START: i32 = 205;
const ID_FTP_ERROR: i32 = 210;

const ID_TFTP_ROOT: i32 = 300;
const ID_TFTP_BROWSE: i32 = 301;
const ID_TFTP_PORT: i32 = 302;
const ID_TFTP_START: i32 = 303;
const ID_TFTP_ERROR: i32 = 306;

const ID_HTTP_ROOT: i32 = 400;
const ID_HTTP_BROWSE: i32 = 401;
const ID_HTTP_PORT: i32 = 402;
const ID_HTTP_START: i32 = 403;
const ID_HTTP_ERROR: i32 = 406;

const ID_CHECK_SCROLL: i32 = 502;
const ID_BTN_CLEAR: i32 = 503;
const ID_LISTVIEW: i32 = 504;
const ID_BTN_SAVE: i32 = 505;

// ---------------------------------------------------------------------------
// Server handle
// ---------------------------------------------------------------------------

struct ServerHandle {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    _thread: std::thread::JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab { Ftp, Tftp, Http }

struct AppState {
    #[allow(dead_code)]
    font: HFONT,
    logs: SharedLog,
    active_tab: Tab,
    auto_scroll: bool,
    last_log_count: usize,
    bind_addr: String,
    available_ips: Vec<(String, String)>,

    ftp_handle: Option<ServerHandle>,
    ftp_running: Arc<AtomicBool>,
    tftp_handle: Option<ServerHandle>,
    tftp_running: Arc<AtomicBool>,
    http_handle: Option<ServerHandle>,
    http_running: Arc<AtomicBool>,

    // Controls
    combo_bind: HWND,
    tab_ctrl: HWND,
    listview: HWND,
    check_scroll: HWND,

    ftp_lbl_root: HWND, ftp_root: HWND, ftp_browse: HWND,
    ftp_lbl_port: HWND, ftp_port: HWND,
    ftp_lbl_user: HWND, ftp_user: HWND,
    ftp_lbl_pass: HWND, ftp_pass: HWND,
    ftp_start: HWND, ftp_error: HWND,

    tftp_lbl_root: HWND, tftp_root: HWND, tftp_browse: HWND,
    tftp_lbl_port: HWND, tftp_port: HWND,
    tftp_start: HWND, tftp_error: HWND,

    http_lbl_root: HWND, http_root: HWND, http_browse: HWND,
    http_lbl_port: HWND, http_port: HWND,
    http_start: HWND, http_error: HWND,
}

static mut APP: Option<AppState> = None;

fn app() -> &'static mut AppState {
    unsafe { APP.as_mut().expect("AppState not initialized") }
}

fn try_app() -> Option<&'static mut AppState> {
    unsafe { APP.as_mut() }
}

// Null handle shorthand
const NH: HWND = ptr::null_mut();

// ---------------------------------------------------------------------------
// Control creation
// ---------------------------------------------------------------------------

pub unsafe fn on_create(hwnd: HWND) {
    let font = CreateFontW(
        -14, 0, 0, 0,
        FW_NORMAL as i32, 0, 0, 0,
        DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0,
        wide("Segoe UI").as_ptr(),
    );

    let available_ips = detect_all_ips();
    let default_bind = detect_default_ip();

    let state = AppState {
        font,
        logs: Arc::new(Mutex::new(Vec::new())),
        active_tab: Tab::Ftp,
        auto_scroll: true,
        last_log_count: 0,
        bind_addr: default_bind.clone(),
        available_ips: available_ips.clone(),

        ftp_handle: None, ftp_running: Arc::new(AtomicBool::new(false)),
        tftp_handle: None, tftp_running: Arc::new(AtomicBool::new(false)),
        http_handle: None, http_running: Arc::new(AtomicBool::new(false)),

        combo_bind: NH, tab_ctrl: NH,
        listview: NH, check_scroll: NH,
        ftp_lbl_root: NH, ftp_root: NH, ftp_browse: NH,
        ftp_lbl_port: NH, ftp_port: NH,
        ftp_lbl_user: NH, ftp_user: NH,
        ftp_lbl_pass: NH, ftp_pass: NH,
        ftp_start: NH, ftp_error: NH,
        tftp_lbl_root: NH, tftp_root: NH, tftp_browse: NH,
        tftp_lbl_port: NH, tftp_port: NH,
        tftp_start: NH, tftp_error: NH,
        http_lbl_root: NH, http_root: NH, http_browse: NH,
        http_lbl_port: NH, http_port: NH,
        http_start: NH, http_error: NH,
    };

    APP = Some(state);
    let a = app();

    // --- Bind address ---
    let lx = 10; let ly = 10;
    create_child("STATIC", "Bind Address:", SS_RIGHT, lx, ly + 3, 90, 20, hwnd, 500);
    a.combo_bind = create_child("COMBOBOX", "",
        CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP,
        lx + 95, ly, 300, 200, hwnd, ID_COMBO_BIND);

    for (_, label) in &available_ips {
        let w = wide(label);
        SendMessageW(a.combo_bind, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    let default_idx = available_ips.iter().position(|(v, _)| *v == default_bind).unwrap_or(0);
    SendMessageW(a.combo_bind, CB_SETCURSEL, default_idx, 0);

    // --- Tab control ---
    let ty = 40;
    a.tab_ctrl = create_child("SysTabControl32", "",
        WS_CLIPSIBLINGS | WS_TABSTOP, 10, ty, 630, 225, hwnd, ID_TAB_CTRL);

    let tab_labels = ["FTP (Stopped)", "TFTP (Stopped)", "HTTP (Stopped)"];
    for (i, label) in tab_labels.iter().enumerate() {
        let mut wtext = wide(label);
        let mut item: TabItem = std::mem::zeroed();
        item.mask = TCIF_TEXT;
        item.psz_text = wtext.as_mut_ptr();
        SendMessageW(a.tab_ctrl, TCM_INSERTITEMW, i, &item as *const _ as LPARAM);
    }

    // --- Tab content (y=80) ---
    let cy = 80;
    let lbl_x = 20;
    let lbl_w = 85;
    let inp_x = 110;

    // FTP
    a.ftp_lbl_root = create_child("STATIC", "Public Folder:", SS_RIGHT, lbl_x, cy + 3, lbl_w, 20, hwnd, 206);
    a.ftp_root = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy, 300, 22, hwnd, ID_FTP_ROOT);
    a.ftp_browse = create_child("BUTTON", "Browse...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 415, cy, 75, 22, hwnd, ID_FTP_BROWSE);

    a.ftp_lbl_port = create_child("STATIC", "Port:", SS_RIGHT, lbl_x, cy + 33, lbl_w, 20, hwnd, 207);
    a.ftp_port = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "21",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy + 30, 80, 22, hwnd, ID_FTP_PORT);

    a.ftp_lbl_user = create_child("STATIC", "Username:", SS_RIGHT, lbl_x, cy + 63, lbl_w, 20, hwnd, 208);
    a.ftp_user = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy + 60, 160, 22, hwnd, ID_FTP_USER);

    a.ftp_lbl_pass = create_child("STATIC", "Password:", SS_RIGHT, lbl_x, cy + 93, lbl_w, 20, hwnd, 209);
    a.ftp_pass = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "",
        ES_AUTOHSCROLL as u32 | ES_PASSWORD as u32 | WS_TABSTOP,
        inp_x, cy + 90, 160, 22, hwnd, ID_FTP_PASS);

    a.ftp_start = create_child("BUTTON", "Start FTP",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 110, cy + 122, 200, 36, hwnd, ID_FTP_START);
    a.ftp_error = create_child("STATIC", "", SS_CENTER, lbl_x, cy + 162, 470, 18, hwnd, ID_FTP_ERROR);

    // TFTP
    a.tftp_lbl_root = create_child("STATIC", "Public Folder:", SS_RIGHT, lbl_x, cy + 3, lbl_w, 20, hwnd, 304);
    a.tftp_root = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy, 300, 22, hwnd, ID_TFTP_ROOT);
    a.tftp_browse = create_child("BUTTON", "Browse...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 415, cy, 75, 22, hwnd, ID_TFTP_BROWSE);

    a.tftp_lbl_port = create_child("STATIC", "Port:", SS_RIGHT, lbl_x, cy + 33, lbl_w, 20, hwnd, 305);
    a.tftp_port = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "69",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy + 30, 80, 22, hwnd, ID_TFTP_PORT);

    a.tftp_start = create_child("BUTTON", "Start TFTP",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 110, cy + 72, 200, 36, hwnd, ID_TFTP_START);
    a.tftp_error = create_child("STATIC", "", SS_CENTER, lbl_x, cy + 112, 470, 18, hwnd, ID_TFTP_ERROR);

    // HTTP
    a.http_lbl_root = create_child("STATIC", "Public Folder:", SS_RIGHT, lbl_x, cy + 3, lbl_w, 20, hwnd, 404);
    a.http_root = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy, 300, 22, hwnd, ID_HTTP_ROOT);
    a.http_browse = create_child("BUTTON", "Browse...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 415, cy, 75, 22, hwnd, ID_HTTP_BROWSE);

    a.http_lbl_port = create_child("STATIC", "Port:", SS_RIGHT, lbl_x, cy + 33, lbl_w, 20, hwnd, 405);
    a.http_port = create_child_ex(WS_EX_CLIENTEDGE, "EDIT", "80",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP, inp_x, cy + 30, 80, 22, hwnd, ID_HTTP_PORT);

    a.http_start = create_child("BUTTON", "Start HTTP",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 110, cy + 72, 200, 36, hwnd, ID_HTTP_START);
    a.http_error = create_child("STATIC", "", SS_CENTER, lbl_x, cy + 112, 470, 18, hwnd, ID_HTTP_ERROR);

    // --- Log area ---
    let log_y = 270;
    create_child("STATIC", "Access Log", 0, 10, log_y, 80, 20, hwnd, 501);
    a.check_scroll = create_child("BUTTON", "Auto-scroll",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP, 100, log_y, 100, 20, hwnd, ID_CHECK_SCROLL);
    SendMessageW(a.check_scroll, BM_SETCHECK, BST_CHECKED as usize, 0);
    create_child("BUTTON", "Clear",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 210, log_y, 60, 22, hwnd, ID_BTN_CLEAR);
    let btn_save = create_child("BUTTON", "Save",
        BS_PUSHBUTTON as u32 | WS_TABSTOP, 280, log_y, 60, 22, hwnd, ID_BTN_SAVE);

    // ListView
    let lv_cls = wide("SysListView32");
    let lv_txt = wide("");
    a.listview = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        lv_cls.as_ptr(),
        lv_txt.as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_VSCROLL | LVS_REPORT | LVS_SINGLESEL | LVS_NOSORTHEADER,
        10, log_y + 26, 620, 240,
        hwnd, ID_LISTVIEW as isize as HMENU,
        GetModuleHandleW(ptr::null()), ptr::null(),
    );
    SendMessageW(a.listview, LVM_SETEXTENDEDLISTVIEWSTYLE, 0,
        (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM);

    let columns = ["DateTime", "Type", "Server", "LocalIP", "LocalPort", "RemoteIP", "RemotePort", "Message"];
    let widths: [i32; 8] = [130, 50, 50, 100, 60, 100, 70, 250];
    for (i, (name, w)) in columns.iter().zip(widths.iter()).enumerate() {
        let wname = wide(name);
        let mut col: LVCOLUMNW = std::mem::zeroed();
        col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
        col.fmt = LVCFMT_LEFT;
        col.cx = *w;
        col.pszText = wname.as_ptr() as *mut u16;
        SendMessageW(a.listview, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
    }

    // Apply font
    let all: &[HWND] = &[
        a.combo_bind, a.tab_ctrl,
        a.ftp_lbl_root, a.ftp_root, a.ftp_browse,
        a.ftp_lbl_port, a.ftp_port,
        a.ftp_lbl_user, a.ftp_user,
        a.ftp_lbl_pass, a.ftp_pass,
        a.ftp_start, a.ftp_error,
        a.tftp_lbl_root, a.tftp_root, a.tftp_browse,
        a.tftp_lbl_port, a.tftp_port,
        a.tftp_start, a.tftp_error,
        a.http_lbl_root, a.http_root, a.http_browse,
        a.http_lbl_port, a.http_port,
        a.http_start, a.http_error,
        a.check_scroll, btn_save, a.listview,
    ];
    for &ctrl in all {
        set_font(ctrl, font);
    }

    // Apply border subclass to edit controls
    for &ctrl in &[a.ftp_root, a.ftp_port, a.ftp_user, a.ftp_pass,
                    a.tftp_root, a.tftp_port, a.http_root, a.http_port] {
        SetWindowSubclass(ctrl, Some(edit_border_proc), 1, 0);
    }

    show_tab(Tab::Ftp);
    update_tab_buttons();
}

// ---------------------------------------------------------------------------
// Tab visibility
// ---------------------------------------------------------------------------

unsafe fn show_tab(tab: Tab) {
    let a = app();
    a.active_tab = tab;

    let ftp_ctrls: &[HWND] = &[
        a.ftp_lbl_root, a.ftp_root, a.ftp_browse,
        a.ftp_lbl_port, a.ftp_port,
        a.ftp_lbl_user, a.ftp_user,
        a.ftp_lbl_pass, a.ftp_pass,
        a.ftp_start, a.ftp_error,
    ];
    let tftp_ctrls: &[HWND] = &[
        a.tftp_lbl_root, a.tftp_root, a.tftp_browse,
        a.tftp_lbl_port, a.tftp_port,
        a.tftp_start, a.tftp_error,
    ];
    let http_ctrls: &[HWND] = &[
        a.http_lbl_root, a.http_root, a.http_browse,
        a.http_lbl_port, a.http_port,
        a.http_start, a.http_error,
    ];

    let show_hide = |ctrls: &[HWND], visible: bool| {
        let cmd = if visible { SW_SHOW } else { SW_HIDE };
        for &h in ctrls {
            if !h.is_null() { ShowWindow(h, cmd); }
        }
    };

    show_hide(ftp_ctrls, tab == Tab::Ftp);
    show_hide(tftp_ctrls, tab == Tab::Tftp);
    show_hide(http_ctrls, tab == Tab::Http);

    update_tab_buttons();
}

unsafe fn update_tab_buttons() {
    let a = app();
    let labels = [
        if a.ftp_running.load(Ordering::Relaxed) { "FTP (Running)" } else { "FTP (Stopped)" },
        if a.tftp_running.load(Ordering::Relaxed) { "TFTP (Running)" } else { "TFTP (Stopped)" },
        if a.http_running.load(Ordering::Relaxed) { "HTTP (Running)" } else { "HTTP (Stopped)" },
    ];
    for (i, label) in labels.iter().enumerate() {
        let mut wtext = wide(label);
        let mut item: TabItem = std::mem::zeroed();
        item.mask = TCIF_TEXT;
        item.psz_text = wtext.as_mut_ptr();
        SendMessageW(a.tab_ctrl, TCM_SETITEMW, i, &item as *const _ as LPARAM);
    }
}

// ---------------------------------------------------------------------------
// WM_COMMAND
// ---------------------------------------------------------------------------

pub unsafe fn on_command(_hwnd: HWND, wparam: WPARAM, _lparam: LPARAM) {
    if try_app().is_none() { return; }
    let id = (wparam & 0xFFFF) as i32;
    let code = ((wparam >> 16) & 0xFFFF) as u32;

    match id {
        ID_COMBO_BIND if code == CBN_SELCHANGE => {
            let a = app();
            let sel = SendMessageW(a.combo_bind, CB_GETCURSEL, 0, 0) as usize;
            if sel < a.available_ips.len() {
                a.bind_addr = a.available_ips[sel].0.clone();
            }
        }
        ID_FTP_BROWSE => browse_folder(app().ftp_root),
        ID_TFTP_BROWSE => browse_folder(app().tftp_root),
        ID_HTTP_BROWSE => browse_folder(app().http_root),

        ID_FTP_START => toggle_server(Tab::Ftp),
        ID_TFTP_START => toggle_server(Tab::Tftp),
        ID_HTTP_START => toggle_server(Tab::Http),

        ID_CHECK_SCROLL => {
            let a = app();
            a.auto_scroll = SendMessageW(a.check_scroll, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
        }
        ID_BTN_CLEAR => {
            let a = app();
            if let Ok(mut logs) = a.logs.lock() { logs.clear(); }
            a.last_log_count = 0;
            SendMessageW(a.listview, LVM_DELETEALLITEMS, 0, 0);
        }
        ID_BTN_SAVE => {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("TSV", &["tsv"])
                .add_filter("All", &["*"])
                .save_file()
            {
                let a = app();
                if let Ok(logs) = a.logs.lock() {
                    let mut out = String::from("DateTime\tType\tServer\tLocalIP\tLocalPort\tRemoteIP\tRemotePort\tMessage\n");
                    for e in logs.iter() {
                        out.push_str(&format!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                            e.timestamp, e.kind, e.server,
                            e.local_ip, e.local_port,
                            e.remote_ip, e.remote_port, e.message
                        ));
                    }
                    let _ = std::fs::write(path, out);
                }
            }
        }
        _ => {}
    }
}

fn browse_folder(edit_hwnd: HWND) {
    if let Some(path) = rfd::FileDialog::new().pick_folder() {
        let w = wide(&path.to_string_lossy());
        unsafe { SetWindowTextW(edit_hwnd, w.as_ptr()); }
    }
}

// ---------------------------------------------------------------------------
// Server start/stop
// ---------------------------------------------------------------------------

unsafe fn toggle_server(tab: Tab) {
    match tab {
        Tab::Ftp => {
            if app().ftp_running.load(Ordering::Relaxed) { stop_ftp(); } else { start_ftp(); }
        }
        Tab::Tftp => {
            if app().tftp_running.load(Ordering::Relaxed) { stop_tftp(); } else { start_tftp(); }
        }
        Tab::Http => {
            if app().http_running.load(Ordering::Relaxed) { stop_http(); } else { start_http(); }
        }
    }
    refresh_ui();
}

unsafe fn refresh_ui() {
    update_start_button_text();
    update_tab_buttons();
    update_controls_enabled();
}

unsafe fn set_error_text(hwnd: HWND, msg: &str) {
    SetWindowTextW(hwnd, wide(msg).as_ptr());
}

unsafe fn start_ftp() {
    let a = app();
    set_error_text(a.ftp_error, "");

    let port: u16 = match get_window_text(a.ftp_port).trim().parse() {
        Ok(p) => p,
        Err(_) => { set_error_text(a.ftp_error, "Invalid port number"); return; }
    };
    let root = match validate_root(&get_window_text(a.ftp_root), a.ftp_error) {
        Some(r) => r, None => return,
    };
    let bind_str = if a.bind_addr.trim().is_empty() { "0.0.0.0" } else { a.bind_addr.trim() };
    let config = ftp::ServerConfig {
        root_dir: root, port,
        bind_addr: match format!("{}:{}", bind_str, port).parse() {
            Ok(addr) => addr,
            Err(_) => { set_error_text(a.ftp_error, "Invalid bind address"); return; }
        },
        username: non_empty(&get_window_text(a.ftp_user)),
        password: non_empty(&get_window_text(a.ftp_pass)),
    };

    let (tx, rx) = tokio::sync::watch::channel(false);
    let logs = a.logs.clone();
    let running = a.ftp_running.clone();
    running.store(true, Ordering::Relaxed);
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(ftp::run(config, logs, rx));
        running.store(false, Ordering::Relaxed);
        rt.shutdown_timeout(std::time::Duration::from_secs(3));
    });
    a.ftp_handle = Some(ServerHandle { shutdown_tx: tx, _thread: thread });
}

unsafe fn stop_ftp() {
    let a = app();
    if let Some(h) = a.ftp_handle.take() {
        let _ = h.shutdown_tx.send(true);
        let port: u16 = get_window_text(a.ftp_port).trim().parse().unwrap_or(0);
        log_system(&a.logs, "FTP", &a.bind_addr, port, "Server stopped");
    }
    a.ftp_running.store(false, Ordering::Relaxed);
}

unsafe fn start_tftp() {
    let a = app();
    set_error_text(a.tftp_error, "");

    let port: u16 = match get_window_text(a.tftp_port).trim().parse() {
        Ok(p) => p,
        Err(_) => { set_error_text(a.tftp_error, "Invalid port number"); return; }
    };
    let root = match validate_root(&get_window_text(a.tftp_root), a.tftp_error) {
        Some(r) => r, None => return,
    };
    let bind_str = if a.bind_addr.trim().is_empty() { "0.0.0.0".to_string() } else { a.bind_addr.trim().to_string() };
    let config = tftp::TftpConfig { root_dir: root, port, bind_addr: bind_str };

    let (tx, rx) = tokio::sync::watch::channel(false);
    let logs = a.logs.clone();
    let running = a.tftp_running.clone();
    running.store(true, Ordering::Relaxed);
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(tftp::run(config, logs, rx));
        running.store(false, Ordering::Relaxed);
        rt.shutdown_timeout(std::time::Duration::from_secs(3));
    });
    a.tftp_handle = Some(ServerHandle { shutdown_tx: tx, _thread: thread });
}

unsafe fn stop_tftp() {
    let a = app();
    if let Some(h) = a.tftp_handle.take() {
        let _ = h.shutdown_tx.send(true);
        let port: u16 = get_window_text(a.tftp_port).trim().parse().unwrap_or(0);
        log_system(&a.logs, "TFTP", &a.bind_addr, port, "Server stopped");
    }
    a.tftp_running.store(false, Ordering::Relaxed);
}

unsafe fn start_http() {
    let a = app();
    set_error_text(a.http_error, "");

    let port: u16 = match get_window_text(a.http_port).trim().parse() {
        Ok(p) => p,
        Err(_) => { set_error_text(a.http_error, "Invalid port number"); return; }
    };
    let root = match validate_root(&get_window_text(a.http_root), a.http_error) {
        Some(r) => r, None => return,
    };
    let bind_str = if a.bind_addr.trim().is_empty() { "0.0.0.0".to_string() } else { a.bind_addr.trim().to_string() };
    let config = httpd::HttpConfig { root_dir: root, port, bind_addr: bind_str };

    let (tx, rx) = tokio::sync::watch::channel(false);
    let logs = a.logs.clone();
    let running = a.http_running.clone();
    running.store(true, Ordering::Relaxed);
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(httpd::run(config, logs, rx));
        running.store(false, Ordering::Relaxed);
        rt.shutdown_timeout(std::time::Duration::from_secs(3));
    });
    a.http_handle = Some(ServerHandle { shutdown_tx: tx, _thread: thread });
}

unsafe fn stop_http() {
    let a = app();
    if let Some(h) = a.http_handle.take() {
        let _ = h.shutdown_tx.send(true);
        let port: u16 = get_window_text(a.http_port).trim().parse().unwrap_or(0);
        log_system(&a.logs, "HTTP", &a.bind_addr, port, "Server stopped");
    }
    a.http_running.store(false, Ordering::Relaxed);
}

unsafe fn update_start_button_text() {
    let a = app();
    let ft = if a.ftp_running.load(Ordering::Relaxed) { "Stop FTP" } else { "Start FTP" };
    let tt = if a.tftp_running.load(Ordering::Relaxed) { "Stop TFTP" } else { "Start TFTP" };
    let ht = if a.http_running.load(Ordering::Relaxed) { "Stop HTTP" } else { "Start HTTP" };
    SetWindowTextW(a.ftp_start, wide(ft).as_ptr());
    SetWindowTextW(a.tftp_start, wide(tt).as_ptr());
    SetWindowTextW(a.http_start, wide(ht).as_ptr());
}

unsafe fn update_controls_enabled() {
    let a = app();
    let ftp_r = a.ftp_running.load(Ordering::Relaxed);
    let tftp_r = a.tftp_running.load(Ordering::Relaxed);
    let http_r = a.http_running.load(Ordering::Relaxed);
    let any = ftp_r || tftp_r || http_r;

    let b = |v: bool| -> BOOL { if v { 1 } else { 0 } };

    EnableWindow(a.combo_bind, b(!any));
    EnableWindow(a.ftp_root, b(!ftp_r));
    EnableWindow(a.ftp_browse, b(!ftp_r));
    EnableWindow(a.ftp_port, b(!ftp_r));
    EnableWindow(a.ftp_user, b(!ftp_r));
    EnableWindow(a.ftp_pass, b(!ftp_r));
    EnableWindow(a.tftp_root, b(!tftp_r));
    EnableWindow(a.tftp_browse, b(!tftp_r));
    EnableWindow(a.tftp_port, b(!tftp_r));
    EnableWindow(a.http_root, b(!http_r));
    EnableWindow(a.http_browse, b(!http_r));
    EnableWindow(a.http_port, b(!http_r));
}

// ---------------------------------------------------------------------------
// WM_TIMER
// ---------------------------------------------------------------------------

pub unsafe fn on_timer(_hwnd: HWND) {
    if try_app().is_none() { return; }
    check_servers();

    let a = app();
    let entries: Vec<crate::common::LogEntry>;
    {
        let logs = match a.logs.lock() { Ok(l) => l, Err(_) => return };
        let new_count = logs.len();
        if new_count <= a.last_log_count { return; }
        entries = logs[a.last_log_count..].to_vec();
        a.last_log_count = new_count;
    }

    let a = app();
    for entry in &entries {
        let fields: [String; 8] = [
            entry.timestamp.clone(),
            entry.kind.to_string(),
            entry.server.to_string(),
            entry.local_ip.clone(),
            entry.local_port.to_string(),
            entry.remote_ip.clone(),
            entry.remote_port.clone(),
            entry.message.clone(),
        ];

        let wtext0 = wide(&fields[0]);
        let mut item: LVITEMW = std::mem::zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = SendMessageW(a.listview, LVM_GETITEMCOUNT, 0, 0) as i32;
        item.iSubItem = 0;
        item.pszText = wtext0.as_ptr() as *mut u16;
        let idx = SendMessageW(a.listview, LVM_INSERTITEMW, 0, &item as *const _ as LPARAM);

        for col in 1..8 {
            let wt = wide(&fields[col]);
            let mut sub: LVITEMW = std::mem::zeroed();
            sub.mask = LVIF_TEXT;
            sub.iItem = idx as i32;
            sub.iSubItem = col as i32;
            sub.pszText = wt.as_ptr() as *mut u16;
            SendMessageW(a.listview, LVM_SETITEMTEXTW, idx as WPARAM, &sub as *const _ as LPARAM);
        }
    }

    if a.auto_scroll && !entries.is_empty() {
        let count = SendMessageW(a.listview, LVM_GETITEMCOUNT, 0, 0);
        if count > 0 {
            SendMessageW(a.listview, LVM_ENSUREVISIBLE, (count - 1) as WPARAM, 0);
        }
    }
}

unsafe fn check_servers() {
    let a = app();
    let mut changed = false;
    if a.ftp_handle.is_some() && !a.ftp_running.load(Ordering::Relaxed) {
        a.ftp_handle = None; changed = true;
    }
    if a.tftp_handle.is_some() && !a.tftp_running.load(Ordering::Relaxed) {
        a.tftp_handle = None; changed = true;
    }
    if a.http_handle.is_some() && !a.http_running.load(Ordering::Relaxed) {
        a.http_handle = None; changed = true;
    }
    if changed { refresh_ui(); }
}

// ---------------------------------------------------------------------------
// WM_NOTIFY / WM_SIZE / WM_DESTROY
// ---------------------------------------------------------------------------

pub unsafe fn on_notify(lparam: LPARAM) {
    if try_app().is_none() { return; }
    let nmhdr = &*(lparam as *const NMHDR);
    if nmhdr.code == TCN_SELCHANGE {
        let a = app();
        let sel = SendMessageW(a.tab_ctrl, TCM_GETCURSEL, 0, 0);
        let tab = match sel {
            0 => Tab::Ftp,
            1 => Tab::Tftp,
            2 => Tab::Http,
            _ => return,
        };
        show_tab(tab);
    }
}

pub unsafe fn on_size(_hwnd: HWND, lparam: LPARAM) {
    let a = match try_app() { Some(a) => a, None => return };
    let w = (lparam & 0xFFFF) as i32;
    let h = ((lparam >> 16) & 0xFFFF) as i32;

    let margin = 10;
    MoveWindow(a.tab_ctrl, margin, 40, w - margin * 2, 225, 1);

    let log_header_y = 270;
    let lv_top = log_header_y + 26;
    let lv_h = h - lv_top - margin;
    if lv_h > 0 {
        MoveWindow(a.listview, margin, lv_top, w - margin * 2, lv_h, 1);
    }

    let inp_x = 110;
    let browse_w = 75;
    let edit_w = w - inp_x - browse_w - margin * 2 - 5;
    if edit_w > 100 {
        let cy = 80;
        MoveWindow(a.ftp_root, inp_x, cy, edit_w, 22, 1);
        MoveWindow(a.ftp_browse, inp_x + edit_w + 5, cy, browse_w, 22, 1);
        MoveWindow(a.tftp_root, inp_x, cy, edit_w, 22, 1);
        MoveWindow(a.tftp_browse, inp_x + edit_w + 5, cy, browse_w, 22, 1);
        MoveWindow(a.http_root, inp_x, cy, edit_w, 22, 1);
        MoveWindow(a.http_browse, inp_x + edit_w + 5, cy, browse_w, 22, 1);
    }
}

pub unsafe fn on_ctlcolor_static(wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let a = match try_app() { Some(a) => a, None => return 0 };
    let id = GetDlgCtrlID(lparam as HWND);
    if id >= 200 && id < 500 {
        let hdc = wparam as HDC;
        let theme = OpenThemeData(a.tab_ctrl, wide("Tab").as_ptr());
        if theme != 0 {
            // Map this control's origin to tab control coordinates
            let mut pt = POINT { x: 0, y: 0 };
            MapWindowPoints(lparam as HWND, a.tab_ctrl, &mut pt, 1);
            // Offset DC so the full pane is drawn aligned to the tab control,
            // showing only the inner portion at this control's position (no border)
            let mut old_org: POINT = std::mem::zeroed();
            SetWindowOrgEx(hdc, pt.x, pt.y, &mut old_org);
            let mut tab_rc: RECT = std::mem::zeroed();
            GetClientRect(a.tab_ctrl, &mut tab_rc);
            DrawThemeBackground(theme, hdc, 9 /* TABP_PANE */, 0, &tab_rc, ptr::null());
            SetWindowOrgEx(hdc, old_org.x, old_org.y, ptr::null_mut());
            CloseThemeData(theme);
        }
        SetBkMode(hdc, TRANSPARENT as i32);
        GetStockObject(NULL_BRUSH) as LRESULT
    } else {
        0
    }
}

pub fn on_destroy() {
    unsafe {
        if try_app().is_none() { return; }
        if app().ftp_running.load(Ordering::Relaxed) { stop_ftp(); }
        if app().tftp_running.load(Ordering::Relaxed) { stop_tftp(); }
        if app().http_running.load(Ordering::Relaxed) { stop_http(); }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn validate_root(root_dir: &str, error_hwnd: HWND) -> Option<PathBuf> {
    if root_dir.is_empty() {
        set_error_text(error_hwnd, "Please select a public folder");
        return None;
    }
    let root = PathBuf::from(root_dir);
    if !root.is_dir() {
        set_error_text(error_hwnd, "The specified folder does not exist");
        return None;
    }
    Some(root)
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn detect_default_ip() -> String {
    let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s, Err(_) => return "0.0.0.0".to_string(),
    };
    if socket.connect("8.8.8.8:80").is_err() { return "0.0.0.0".to_string(); }
    match socket.local_addr() {
        Ok(addr) => addr.ip().to_string(),
        Err(_) => "0.0.0.0".to_string(),
    }
}

fn detect_all_ips() -> Vec<(String, String)> {
    let mut ips = vec![("0.0.0.0".to_string(), "0.0.0.0 (All interfaces)".to_string())];
    let default_ip = detect_default_ip();
    if default_ip != "0.0.0.0" {
        ips.push((default_ip.clone(), format!("{} (Default)", default_ip)));
    }
    #[cfg(windows)]
    for ip in get_adapter_addresses() {
        let s = ip.to_string();
        if !ips.iter().any(|(v, _)| v == &s) {
            let label = if ip.is_loopback() { format!("{} (Localhost)", s) } else { s.clone() };
            ips.push((s, label));
        }
    }
    ips
}

#[cfg(windows)]
fn get_adapter_addresses() -> Vec<std::net::Ipv4Addr> {
    use std::net::Ipv4Addr;

    const AF_INET: u32 = 2;
    const FLAGS: u32 = 0x0002 | 0x0004 | 0x0008;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    #[repr(C)]
    struct SockaddrIn { sin_family: u16, sin_port: u16, sin_addr: [u8; 4], sin_zero: [u8; 8] }
    #[repr(C)]
    struct SocketAddress { lp_sockaddr: *const SockaddrIn, i_sockaddr_length: i32 }
    #[repr(C)]
    struct UnicastAddress { _header: u64, next: *const UnicastAddress, address: SocketAddress }
    #[repr(C)]
    struct AdapterAddresses { _header: u64, next: *const AdapterAddresses, _adapter_name: *const u8, first_unicast_address: *const UnicastAddress }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetAdaptersAddresses(family: u32, flags: u32, reserved: *mut u8, addresses: *mut u8, size: *mut u32) -> u32;
    }

    let mut result = Vec::new();
    let mut size: u32 = 0;
    unsafe {
        if GetAdaptersAddresses(AF_INET, FLAGS, ptr::null_mut(), ptr::null_mut(), &mut size) != ERROR_BUFFER_OVERFLOW {
            return result;
        }
    }
    let mut buffer = vec![0u8; size as usize];
    unsafe {
        if GetAdaptersAddresses(AF_INET, FLAGS, ptr::null_mut(), buffer.as_mut_ptr(), &mut size) != 0 {
            return result;
        }
        let mut adapter = buffer.as_ptr() as *const AdapterAddresses;
        while !adapter.is_null() {
            let mut unicast = (*adapter).first_unicast_address;
            while !unicast.is_null() {
                let sa = (*unicast).address.lp_sockaddr;
                if !sa.is_null() && (*sa).sin_family == AF_INET as u16 {
                    let o = (*sa).sin_addr;
                    let ip = Ipv4Addr::new(o[0], o[1], o[2], o[3]);
                    if !result.contains(&ip) { result.push(ip); }
                }
                unicast = (*unicast).next;
            }
            adapter = (*adapter).next;
        }
    }
    result
}
