//! Single-instance lock and a Win32 notify icon.
//! Close X hides the window; the tray can Show, toggle vcam/record, or Exit.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, IsIconic, KillTimer,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    ShowWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU,
    MB_ICONWARNING, MB_OK, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

use crate::icon::TrayTint;
use crate::preview::quality_by_id;
use crate::shared::{HostCmd, Shared};
use tokio::sync::mpsc::UnboundedSender;

const SHOW_NAME: PCWSTR = w!("Local\\PocketCam.show");
const MUTEX_NAME: PCWSTR = w!("Local\\PocketCam.single");
const WM_TRAY: u32 = WM_APP + 1;
const WM_TRAY_REFRESH: u32 = WM_APP + 2;
const WM_TRAY_MENU: u32 = WM_APP + 3;
const WM_TOGGLE_VCAM: u32 = WM_APP + 4;
const WM_TOGGLE_RECORD: u32 = WM_APP + 5;
const WM_TRAY_BALLOON: u32 = WM_APP + 6;
const TRAY_TIMER: usize = 1;

const ID_SHOW: usize = 1;
const ID_EXIT: usize = 2;
const ID_VCAM: usize = 3;
const ID_RECORD: usize = 4;

static SHOW_H: AtomicIsize = AtomicIsize::new(0);
static EXIT_H: AtomicIsize = AtomicIsize::new(0);
static THEME_H: AtomicIsize = AtomicIsize::new(0);
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);
static LAST_TINT: AtomicU8 = AtomicU8::new(255);
static EXIT_REQ: AtomicBool = AtomicBool::new(false);
static TRAY_SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
static TRAY_CMDS: Mutex<Option<UnboundedSender<HostCmd>>> = Mutex::new(None);
static LAST_NOTICE: Mutex<Option<String>> = Mutex::new(None);
static BALLOON: Mutex<Option<(String, bool)>> = Mutex::new(None);

pub struct Host {
    mutex: HANDLE,
    show: HANDLE,
    exit: HANDLE,
    theme: HANDLE,
    tray: Mutex<Option<JoinHandle<()>>>,
}

impl Host {
    /// `None` = another PocketCam is running and was asked to show its window.
    pub fn claim() -> Result<Option<Self>> {
        unsafe {
            let mutex = CreateMutexW(None, true, MUTEX_NAME).context("CreateMutexW")?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                for _ in 0..20 {
                    if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, SHOW_NAME) {
                        let _ = SetEvent(ev);
                        let _ = CloseHandle(ev);
                        tracing::info!("PocketCam already running — asked it to show");
                        return Ok(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                tracing::warn!("PocketCam already running, but could not signal it");
                already_running_box();
                return Ok(None);
            }
            let show = CreateEventW(None, false, false, SHOW_NAME).context("CreateEventW show")?;
            let exit = CreateEventW(None, false, false, None).context("CreateEventW exit")?;
            let theme = CreateEventW(None, false, false, None).context("CreateEventW theme")?;
            SHOW_H.store(handle_to_isize(show), Ordering::SeqCst);
            EXIT_H.store(handle_to_isize(exit), Ordering::SeqCst);
            THEME_H.store(handle_to_isize(theme), Ordering::SeqCst);
            let host = Self {
                mutex,
                show,
                exit,
                theme,
                tray: Mutex::new(None),
            };
            host.spawn_tray()?;
            Ok(Some(host))
        }
    }

    pub fn bind_shared(&self, shared: Arc<Shared>, cmds: UnboundedSender<HostCmd>) {
        *TRAY_SHARED.lock() = Some(shared);
        *TRAY_CMDS.lock() = Some(cmds);
        poke_tray();
    }

    fn spawn_tray(&self) -> Result<()> {
        let handle = thread::Builder::new()
            .name("pocketcam-tray".into())
            .spawn(tray_thread)
            .context("spawn tray")?;
        *self.tray.lock() = Some(handle);
        Ok(())
    }

    pub fn take_show(&self) -> bool {
        wait_zero(self.show)
    }

    pub fn take_exit(&self) -> bool {
        wait_zero(self.exit)
    }

    pub fn take_theme(&self) -> bool {
        wait_zero(self.theme)
    }

    pub fn request_toggle_vcam(&self) {
        if TRAY_HWND.load(Ordering::SeqCst) == 0 {
            apply_toggle_vcam();
            return;
        }
        post_tray(WM_TOGGLE_VCAM);
    }

    pub fn request_toggle_record(&self) {
        if TRAY_HWND.load(Ordering::SeqCst) == 0 {
            apply_toggle_record();
            return;
        }
        post_tray(WM_TOGGLE_RECORD);
    }

    pub fn take_notice(&self) -> Option<String> {
        LAST_NOTICE.lock().take()
    }

    pub fn request_exit(&self) {
        exit_app();
    }

    /// Balloon when the preview window is hidden. Safe from the UI thread.
    pub fn balloon(&self, msg: &str) {
        request_balloon(msg, false);
    }

    pub fn exit_requested() -> bool {
        EXIT_REQ.load(Ordering::SeqCst)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        *TRAY_SHARED.lock() = None;
        *TRAY_CMDS.lock() = None;
        let hwnd = TRAY_HWND.swap(0, Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(HWND(hwnd as *mut core::ffi::c_void), WM_DESTROY, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(h) = self.tray.lock().take() {
            let _ = h.join();
        }
        SHOW_H.store(0, Ordering::SeqCst);
        EXIT_H.store(0, Ordering::SeqCst);
        THEME_H.store(0, Ordering::SeqCst);
        unsafe {
            let _ = CloseHandle(self.show);
            let _ = CloseHandle(self.exit);
            let _ = CloseHandle(self.theme);
            let _ = CloseHandle(self.mutex);
        }
    }
}

fn wait_zero(h: HANDLE) -> bool {
    unsafe { WaitForSingleObject(h, 0) == WAIT_OBJECT_0 }
}

fn handle_to_isize(h: HANDLE) -> isize {
    h.0 as isize
}

fn isize_to_handle(v: isize) -> HANDLE {
    HANDLE(v as *mut core::ffi::c_void)
}

fn signal(which: &AtomicIsize) {
    let v = which.load(Ordering::SeqCst);
    if v == 0 {
        return;
    }
    unsafe {
        let _ = SetEvent(isize_to_handle(v));
    }
}

fn poke_tray() {
    post_tray(WM_TRAY_REFRESH);
}

fn post_tray(msg: u32) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            HWND(hwnd as *mut core::ffi::c_void),
            msg,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn tray_thread() {
    if let Err(e) = unsafe { tray_loop() } {
        tracing::error!("tray: {e:#}");
    }
}

unsafe fn tray_loop() -> Result<()> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let hinst = GetModuleHandleW(None)?;
    let class = w!("PocketCamTrayClass");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(tray_wndproc),
        hInstance: hinst.into(),
        lpszClassName: class,
        ..Default::default()
    };
    let atom = RegisterClassW(&wc);
    if atom == 0 {
        let err = GetLastError();
        if err != windows::Win32::Foundation::WIN32_ERROR(1410) {
            bail!("RegisterClassW {err:?}");
        }
    }
    // A real (hidden) window — HWND_MESSAGE cannot own TrackPopupMenu, so
    // Show/Exit used to return 0 and do nothing.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class,
        w!("PocketCamTray"),
        WS_OVERLAPPED,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        None,
        None,
        hinst,
        None,
    )?;
    TRAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    LAST_TINT.store(255, Ordering::SeqCst);
    let snap = TraySnap::capture();
    let icon = crate::icon::hicon_hex(32, snap.tint.hex())?;
    TRAY_ICON.store(icon.0 as isize, Ordering::SeqCst);
    LAST_TINT.store(snap.tint as u8, Ordering::SeqCst);
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    write_tip(&mut nid, &snap.tip);
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        bail!("Shell_NotifyIconW NIM_ADD");
    }
    let _ = SetTimer(hwnd, TRAY_TIMER, 500, None);
    tracing::info!("tray icon added");
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = KillTimer(hwnd, TRAY_TIMER);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let old = TRAY_ICON.swap(0, Ordering::SeqCst);
    if old != 0 {
        crate::icon::destroy_icon(windows::Win32::UI::WindowsAndMessaging::HICON(
            old as *mut core::ffi::c_void,
        ));
    }
    let _ = DestroyWindow(hwnd);
    Ok(())
}

unsafe fn refresh_tray(hwnd: HWND, force_icon: bool) {
    let snap = TraySnap::capture();
    let tint = snap.tint as u8;
    let mut flags = NIF_TIP;
    let mut icon = windows::Win32::UI::WindowsAndMessaging::HICON::default();
    if force_icon || tint != LAST_TINT.load(Ordering::SeqCst) {
        if let Ok(h) = crate::icon::hicon_hex(32, snap.tint.hex()) {
            icon = h;
            flags |= NIF_ICON;
            LAST_TINT.store(tint, Ordering::SeqCst);
            let old = TRAY_ICON.swap(h.0 as isize, Ordering::SeqCst);
            if old != 0 && old != h.0 as isize {
                crate::icon::destroy_icon(windows::Win32::UI::WindowsAndMessaging::HICON(
                    old as *mut core::ffi::c_void,
                ));
            }
        }
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: flags,
        hIcon: icon,
        ..Default::default()
    };
    write_tip(&mut nid, &snap.tip);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

fn write_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let mut encoded: Vec<u16> = tip.encode_utf16().collect();
    encoded.push(0);
    let n = encoded.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&encoded[..n]);
    if n < nid.szTip.len() {
        nid.szTip[n..].fill(0);
    }
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            let mouse = lparam.0 as u32;
            if mouse == WM_LBUTTONUP || mouse == WM_LBUTTONDBLCLK {
                show_app_window();
            } else if mouse == WM_RBUTTONUP || mouse == WM_CONTEXTMENU {
                // Do not TrackPopupMenu from the shell callback — clicks are eaten.
                let _ = PostMessageW(hwnd, WM_TRAY_MENU, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_TRAY_BALLOON => {
            show_balloon(hwnd);
            LRESULT(0)
        }
        WM_TRAY_MENU => {
            popup_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            dispatch_cmd((wparam.0 as u32 & 0xffff) as usize);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TRAY_TIMER {
                refresh_tray(hwnd, false);
            }
            LRESULT(0)
        }
        WM_TRAY_REFRESH => {
            refresh_tray(hwnd, false);
            LRESULT(0)
        }
        WM_TOGGLE_VCAM => {
            apply_toggle_vcam();
            LRESULT(0)
        }
        WM_TOGGLE_RECORD => {
            apply_toggle_record();
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            signal(&THEME_H);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn dispatch_cmd(id: usize) {
    match id {
        ID_SHOW => show_app_window(),
        ID_EXIT => exit_app(),
        ID_VCAM => apply_toggle_vcam(),
        ID_RECORD => apply_toggle_record(),
        _ => {}
    }
}

fn apply_toggle_vcam() {
    let Some(shared) = TRAY_SHARED.lock().clone() else {
        return;
    };
    let notice = shared.toggle_vcam();
    tracing::info!("{notice}");
    *LAST_NOTICE.lock() = Some(notice.clone());
    if !shared.preview.window_shown() {
        request_balloon(&notice, notice.contains("failed"));
    }
    if let Some(cmds) = TRAY_CMDS.lock().as_ref() {
        let _ = cmds.send(HostCmd::CaptureLock);
    }
    poke_tray();
}

fn apply_toggle_record() {
    let Some(shared) = TRAY_SHARED.lock().clone() else {
        return;
    };
    let notice = shared.toggle_record();
    tracing::info!("{notice}");
    *LAST_NOTICE.lock() = Some(notice.clone());
    if !shared.preview.window_shown() {
        request_balloon(&notice, notice.contains("failed"));
    }
    if let Some(cmds) = TRAY_CMDS.lock().as_ref() {
        let _ = cmds.send(HostCmd::CaptureLock);
    }
    poke_tray();
}

fn app_hwnd() -> HWND {
    unsafe { FindWindowW(None, w!("PocketCam")).unwrap_or_default() }
}

fn show_app_window() {
    if let Some(shared) = TRAY_SHARED.lock().clone() {
        shared.preview.set_window_shown(true);
    }
    signal(&SHOW_H);
    unsafe {
        let hwnd = app_hwnd();
        if hwnd.0.is_null() {
            tracing::warn!("tray show: PocketCam window not found");
            return;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
    }
}

fn exit_app() {
    EXIT_REQ.store(true, Ordering::SeqCst);
    signal(&EXIT_H);
    if let Some(shared) = TRAY_SHARED.lock().clone() {
        if shared.record.is_on() {
            let _ = shared.record.stop();
            shared.preview.record_on.store(false, Ordering::Relaxed);
        }
        if shared.vcam.is_on() {
            shared.vcam.stop();
            shared.preview.vcam_on.store(false, Ordering::Relaxed);
        }
    }
    unsafe {
        let hwnd = app_hwnd();
        if hwnd.0.is_null() {
            tracing::warn!("tray exit: PocketCam window not found");
            std::process::exit(0);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

unsafe fn popup_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let snap = TraySnap::capture();
    append_text(menu, MF_STRING, ID_SHOW, "Show window");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_text(
        menu,
        MF_STRING | MF_GRAYED | MF_DISABLED,
        0,
        &snap.status_line,
    );
    let mut vcam_flags = MF_STRING;
    if snap.vcam {
        vcam_flags |= MF_CHECKED;
    }
    append_text(menu, vcam_flags, ID_VCAM, "Virtual camera");
    let mut rec_flags = MF_STRING;
    if snap.rec {
        rec_flags |= MF_CHECKED;
    }
    append_text(menu, rec_flags, ID_RECORD, "Record");
    if let Some(file) = &snap.rec_file {
        append_text(
            menu,
            MF_STRING | MF_GRAYED | MF_DISABLED,
            0,
            file,
        );
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_text(menu, MF_STRING, ID_EXIT, "Exit");

    let mut pt = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

fn append_text(menu: HMENU, flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS, id: usize, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe { AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr())) };
}

struct TraySnap {
    vcam: bool,
    rec: bool,
    rec_file: Option<String>,
    status_line: String,
    tip: String,
    tint: TrayTint,
}

impl TraySnap {
    fn capture() -> Self {
        let Some(shared) = TRAY_SHARED.lock().clone() else {
            return Self {
                vcam: false,
                rec: false,
                rec_file: None,
                status_line: "Starting…".into(),
                tip: "PocketCam".into(),
                tint: TrayTint::Idle,
            };
        };
        let vcam = shared.vcam.is_on();
        let rec = shared.record.is_on();
        let rec_file = shared.record.last_path().and_then(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
        });
        let (q_label, max_fps, fps, live) = {
            let stats = shared.stats.lock();
            let spec = quality_by_id(&stats.selected_quality)
                .unwrap_or_else(|| shared.preview.camera_quality());
            let stalled = stats
                .last_frame
                .map(|t| t.elapsed() > Duration::from_millis(1500))
                .unwrap_or(true);
            let fps = if stalled { 0.0 } else { stats.fps };
            let live = stats.last_frame.is_some() && !stalled;
            (format!("{}p {}", spec.height, spec.fps), spec.fps, fps, live)
        };
        let status_line = if live {
            format!("{q_label}  ·  {fps:.0} fps")
        } else if vcam || rec {
            format!("{q_label}  ·  waiting")
        } else {
            format!("{q_label}  ·  idle")
        };
        let mut tip = String::from("PocketCam");
        if live {
            tip.push_str(&format!(" · {fps:.0}/{max_fps} fps"));
        }
        if vcam {
            tip.push_str(" · vcam");
        }
        if rec {
            tip.push_str(" · rec");
        }
        let tint = if vcam || rec {
            TrayTint::from_fps(fps, max_fps)
        } else {
            TrayTint::Idle
        };
        Self {
            vcam,
            rec,
            rec_file: rec_file.filter(|_| rec),
            status_line,
            tip,
            tint,
        }
    }
}

fn request_balloon(msg: &str, warn: bool) {
    *BALLOON.lock() = Some((msg.to_string(), warn));
    post_tray(WM_TRAY_BALLOON);
}

fn already_running_box() {
    let title: Vec<u16> = "PocketCam".encode_utf16().chain(std::iter::once(0)).collect();
    let body: Vec<u16> = "PocketCam is already running, but the window could not be shown. Use the PocketCam icon in the notification area, then Show window."
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            windows::core::PCWSTR(body.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONWARNING,
        );
    }
}

unsafe fn show_balloon(hwnd: HWND) {
    let Some((msg, warn)) = BALLOON.lock().take() else {
        return;
    };
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: if warn { NIIF_WARNING } else { NIIF_INFO },
        ..Default::default()
    };
    write_utf16(&mut nid.szInfoTitle, "PocketCam");
    write_utf16(&mut nid.szInfo, &msg);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

fn write_utf16(dest: &mut [u16], s: &str) {
    let mut encoded: Vec<u16> = s.encode_utf16().collect();
    encoded.push(0);
    let n = encoded.len().min(dest.len());
    dest[..n].copy_from_slice(&encoded[..n]);
    if n < dest.len() {
        dest[n..].fill(0);
    }
}
