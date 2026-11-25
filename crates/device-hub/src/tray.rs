//! タスクトレイ常駐 (G HUB の置き換えとして常時起動するため)。
//!
//! - 窓の × はアプリを終了せず窓を隠すだけ (トレイに残る)。
//! - トレイアイコン: 左クリック = 窓を表示 / 右クリック = メニュー (表示・終了)。
//! - 多重起動防止: 2 個目の起動は既存インスタンスの窓を前面に出して終わる。
//! - `--minimized` で起動するとトレイだけで始まる (ログオン時の自動起動用)。
//!
//! GPUI のメッセージループはメインスレッドの全ウィンドウにディスパッチするので、
//! メインスレッドに作った非表示ウィンドウの wndproc がそのまま動く。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static MAIN: AtomicIsize = AtomicIsize::new(0);
static TRAY: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// トレイメニューの「終了」が押された — 窓の close を本当に通す。
pub static QUITTING: AtomicBool = AtomicBool::new(false);

const WM_TRAY: u32 = WM_APP + 1;
const WM_SHOW_MAIN: u32 = WM_APP + 2;
const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;
const CLASS: &str = "DeviceHubTrayWnd";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 起動引数に `--minimized` / `--background` があるか。
pub fn start_minimized() -> bool {
    std::env::args().any(|a| a == "--minimized" || a == "--background")
}

/// 1 個目のインスタンスなら true。2 個目なら既存の窓を出すよう頼んで false。
pub fn single_instance() -> bool {
    unsafe {
        let name = wide("Local\\DeviceHubSingleInstance");
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if !h.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let class = wide(CLASS);
            let w = FindWindowW(class.as_ptr(), std::ptr::null());
            if !w.is_null() {
                PostMessageW(w, WM_SHOW_MAIN, 0, 0);
            }
            return false;
        }
        // ミューテックスはプロセス終了まで保持 (リークさせる)。
        true
    }
}

pub fn show_main() {
    let h = MAIN.load(Ordering::Relaxed) as HWND;
    if h.is_null() {
        return;
    }
    unsafe {
        ShowWindow(h, if IsIconic(h) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(h);
    }
}

pub fn hide_main() {
    let h = MAIN.load(Ordering::Relaxed) as HWND;
    if !h.is_null() {
        unsafe {
            ShowWindow(h, SW_HIDE);
        }
    }
}

/// 窓が隠れている (トレイ格納中) か。
pub fn main_hidden() -> bool {
    let h = MAIN.load(Ordering::Relaxed) as HWND;
    !h.is_null() && unsafe { IsWindowVisible(h) } == 0
}

fn nid(tray: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = tray;
    n.uID = 1;
    n
}

fn add_icon(tray: HWND) {
    let mut n = nid(tray);
    n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    unsafe {
        // 実行ファイルのリソース ID 1 (build.rs が埋め込むアプリアイコン)。
        n.hIcon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as _);
        if n.hIcon.is_null() {
            n.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        }
    }
    let tip = wide("Device HUB");
    n.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &n);
    }
}

/// トレイアイコンを消す (終了時)。
pub fn remove() {
    let t = TRAY.swap(0, Ordering::Relaxed) as HWND;
    if !t.is_null() {
        let n = nid(t);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &n);
        }
    }
}

fn menu(tray: HWND) {
    unsafe {
        let m = CreatePopupMenu();
        let show = wide("Device HUB を表示");
        let quit = wide("終了");
        AppendMenuW(m, MF_STRING, ID_SHOW, show.as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, ID_QUIT, quit.as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // 前面化しないとメニュー外クリックで閉じない (既知の Win32 の癖)。
        SetForegroundWindow(tray);
        TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, tray, std::ptr::null());
        PostMessageW(tray, WM_NULL, 0, 0);
        DestroyMenu(m);
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (l & 0xFFFF) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(),
                WM_RBUTTONUP | WM_CONTEXTMENU => menu(h),
                _ => {}
            }
            0
        }
        WM_SHOW_MAIN => {
            show_main();
            0
        }
        WM_COMMAND => {
            match w & 0xFFFF {
                ID_SHOW => show_main(),
                ID_QUIT => {
                    QUITTING.store(true, Ordering::Relaxed);
                    remove();
                    let m = MAIN.load(Ordering::Relaxed) as HWND;
                    if !m.is_null() {
                        PostMessageW(m, WM_CLOSE, 0, 0);
                    }
                }
                _ => {}
            }
            0
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // エクスプローラー再起動でトレイが消えた — 付け直す。
            add_icon(h);
            0
        }
        _ => DefWindowProcW(h, msg, w, l),
    }
}

/// メイン窓 (HWND) を登録してトレイアイコンを出す。メインスレッドから 1 回呼ぶ。
pub fn install(main_hwnd: isize) {
    MAIN.store(main_hwnd, Ordering::Relaxed);
    if TRAY.load(Ordering::Relaxed) != 0 {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let title = wide("Device HUB tray");
        let t = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(), // 非表示のトップレベル (message-only だと TaskbarCreated が届かない)
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if t.is_null() {
            return;
        }
        let tc = wide("TaskbarCreated");
        TASKBAR_CREATED.store(RegisterWindowMessageW(tc.as_ptr()), Ordering::Relaxed);
        TRAY.store(t as isize, Ordering::Relaxed);
        add_icon(t);
    }
}
