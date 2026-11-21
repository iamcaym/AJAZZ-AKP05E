//! Значок в области уведомлений. ЛКМ показывает окно, ПКМ открывает меню
//! с пунктом «Закрыть». Значок живёт в своём потоке со своим окном сообщений.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, HICON, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

use crate::shared::Shared;

const WINDOW_CLASS: PCWSTR = w!("AkpDeck.Tray");
/// Сообщение от значка: в `lparam` событие мыши.
const WM_TRAY: u32 = WM_APP + 1;
/// Просьба показать окно от второго запущенного экземпляра.
const WM_TRAY_SHOW: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;
const MENU_QUIT: usize = 1;

static SHARED: OnceLock<Arc<Mutex<Shared>>> = OnceLock::new();
static ICON: OnceLock<isize> = OnceLock::new();
/// Сообщение «TaskbarCreated»: после перезапуска проводника значок добавляется заново.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Запускает поток значка. `rgba` - квадратная картинка `size` x `size`.
pub fn start(shared: Arc<Mutex<Shared>>, rgba: Vec<u8>, size: u32) -> Result<()> {
    SHARED.set(shared).map_err(|_| anyhow!("трей уже запущен"))?;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let created = unsafe { create(&rgba, size) };
            let ok = created.is_ok();
            let _ = ready_tx.send(created);
            if ok {
                unsafe { message_loop() };
            }
        })?;
    ready_rx.recv()?.map_err(|e| anyhow!("значок в трее не создан: {e}"))
}

/// Убирает значок и завершает поток трея.
pub fn stop() {
    if let Some(window) = find_window() {
        unsafe {
            let _ = SendMessageW(window, WM_CLOSE, None, None);
        }
    }
}

/// Просит уже запущенный экземпляр показать окно. `false`, если трея нет.
pub fn show_existing() -> bool {
    match find_window() {
        Some(window) => unsafe { PostMessageW(Some(window), WM_TRAY_SHOW, WPARAM(0), LPARAM(0)).is_ok() },
        None => false,
    }
}

fn find_window() -> Option<HWND> {
    unsafe { FindWindowW(WINDOW_CLASS, None) }
        .ok()
        .filter(|window| !window.is_invalid())
}

unsafe fn create(rgba: &[u8], size: u32) -> windows::core::Result<()> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    RegisterClassW(&class);
    let window = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        WINDOW_CLASS,
        w!("AKP Deck"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance.into()),
        None,
    )?;

    let bgra: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    let mask = vec![0u8; (size.div_ceil(16) * 2 * size) as usize];
    let icon = CreateIcon(
        Some(instance.into()),
        size as i32,
        size as i32,
        1,
        32,
        mask.as_ptr(),
        bgra.as_ptr(),
    )?;
    let _ = ICON.set(icon.0 as isize);
    TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
    add_icon(window);
    Ok(())
}

unsafe fn message_loop() {
    let mut message = MSG::default();
    while GetMessageW(&mut message, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: ICON_ID,
        ..Default::default()
    }
}

unsafe fn add_icon(window: HWND) {
    let mut data = icon_data(window);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = HICON(ICON.get().copied().unwrap_or(0) as *mut _);
    let tip: Vec<u16> = "AKP Deck".encode_utf16().collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    let _ = Shell_NotifyIconW(NIM_ADD, &data);
}

unsafe fn show_menu(window: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Закрыть"));
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    // Без этого меню не закрывается по клику мимо.
    let _ = SetForegroundWindow(window);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, cursor.x, cursor.y, None, window, None);
    let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

/// Передаёт просьбу окну и будит его, даже если оно скрыто.
fn request(show: bool, quit: bool) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let ctx = {
        let mut state = match shared.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.tray_show |= show;
        state.tray_quit |= quit;
        state.egui_ctx.clone()
    };
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => request(true, false),
                WM_RBUTTONUP => show_menu(window),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY_SHOW => {
            request(true, false);
            LRESULT(0)
        }
        WM_COMMAND => {
            if wparam.0 & 0xffff == MENU_QUIT {
                request(false, true);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(window));
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ if message != 0 && message == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            add_icon(window);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}