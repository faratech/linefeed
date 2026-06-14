//! Custom Windows system tray implementation using Win32 APIs directly.
//! Replaces the tray-icon crate for full control over behavior.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW,
    InsertMenuW, PostMessageW, PostQuitMessage, RegisterClassExW, SetForegroundWindow,
    SetMenuDefaultItem, ShowWindow, TrackPopupMenu, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_RESTORE, SW_SHOW,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_USER, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};

use crate::icon_data;

const WM_TRAYICON: u32 = WM_USER + 1;
const WM_SHOW_WINDOW: u32 = WM_USER + 2;  // Custom message to show window
const ID_SHOW: u16 = 1;
const ID_QUIT: u16 = 2;

static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);
static RESTORE_REQUESTED: AtomicBool = AtomicBool::new(false);
static MSG_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static TRAY_ICON_ADDED: AtomicBool = AtomicBool::new(false);

// Store NID fields we need for cleanup (avoiding Send issues with raw pointers)
static NID_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
// Handle of the icon we created, so we can DestroyIcon it on teardown.
static TRAY_HICON: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

fn find_main_window() -> Option<HWND> {
    unsafe {
        match FindWindowW(None, w!("Linefeed")) {
            Ok(hwnd) if !hwnd.is_invalid() => Some(hwnd),
            _ => None,
        }
    }
}

pub fn show_window() {
    WINDOW_HIDDEN.store(false, Ordering::SeqCst);
    RESTORE_REQUESTED.store(true, Ordering::SeqCst);
    if let Some(hwnd) = find_main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        tracing::info!("Window shown via Win32");
    } else {
        tracing::warn!("Could not find window to show");
    }
}

pub fn hide_window() {
    WINDOW_HIDDEN.store(true, Ordering::SeqCst);
    if let Some(hwnd) = find_main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        tracing::info!("Window hidden to tray");
    }
}

pub fn is_window_hidden() -> bool {
    WINDOW_HIDDEN.load(Ordering::SeqCst)
}

pub fn clear_hidden() {
    WINDOW_HIDDEN.store(false, Ordering::SeqCst);
}

pub fn take_restore_request() -> bool {
    RESTORE_REQUESTED.swap(false, Ordering::SeqCst)
}

pub fn is_active() -> bool {
    TRAY_ACTIVE.load(Ordering::SeqCst)
}

/// Send a message to an existing instance to show its window
/// This is used when a second instance detects the first is already running
pub fn activate_existing_instance() {
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    unsafe {
        // Find the tray message window of the existing instance
        if let Ok(hwnd) = FindWindowW(None, w!("Linefeed Tray")) {
            if !hwnd.is_invalid() {
                // Send our custom show message
                SendMessageW(hwnd, WM_SHOW_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
                tracing::info!("Sent activate message to existing instance");
            }
        }
    }
}

pub fn should_exit() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Flash the taskbar to alert the user of a notification
pub fn flash_window() {
    use windows::Win32::UI::WindowsAndMessaging::{FlashWindowEx, FLASHWINFO, FLASHW_ALL, FLASHW_TIMERNOFG};

    if let Some(hwnd) = find_main_window() {
        unsafe {
            let mut flash_info = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            let _ = FlashWindowEx(&mut flash_info);
        }
        tracing::debug!("Flashed taskbar for notification");
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            let event = lparam.0 as u32;
            match event {
                x if x == WM_LBUTTONDBLCLK => {
                    show_window();
                }
                x if x == WM_RBUTTONUP => {
                    // Show context menu
                    unsafe { show_context_menu(hwnd) };
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SHOW_WINDOW => {
            // Received from another instance trying to activate us
            show_window();
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd = (wparam.0 & 0xFFFF) as u16;
            match cmd {
                ID_SHOW => show_window(),
                ID_QUIT => {
                    EXIT_REQUESTED.store(true, Ordering::SeqCst);
                    show_window(); // Show so egui can process close
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    unsafe {
        // Don't unwrap inside this extern "system" callback path: a panic would
        // unwind across the FFI boundary (UB). Bail out gracefully on failure.
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };

        // Add menu items
        let show_text: Vec<u16> = "Show Linefeed\0".encode_utf16().collect();
        let quit_text: Vec<u16> = "Quit\0".encode_utf16().collect();

        let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_STRING, ID_SHOW as usize, PCWSTR(show_text.as_ptr()));
        let _ = InsertMenuW(menu, 1, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
        let _ = InsertMenuW(menu, 2, MF_BYPOSITION | MF_STRING, ID_QUIT as usize, PCWSTR(quit_text.as_ptr()));

        // Make "Show" the default (bold) - third param: 1 = by position
        let _ = SetMenuDefaultItem(menu, 0, 1);

        // Get cursor position
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        // Required for menu to work properly
        let _ = SetForegroundWindow(hwnd);

        // Show menu
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );

        let _ = DestroyMenu(menu);
    }
}

fn create_icon() -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::UI::WindowsAndMessaging::CreateIcon;

    let width = icon_data::ICON_WIDTH as i32;
    let height = icon_data::ICON_HEIGHT as i32;

    // Convert RGBA to BGRA (Windows format) and separate into color and mask
    let mut bgra_data = icon_data::ICON_RGBA.to_vec();
    for chunk in bgra_data.chunks_exact_mut(4) {
        chunk.swap(0, 2); // Swap R and B
    }

    // Create AND mask (1 bit per pixel, 0 = opaque, 1 = transparent)
    // For a 32x32 icon, we need 32 * 32 / 8 = 128 bytes
    let mask_size = ((width as usize + 7) / 8) * height as usize;
    let mut and_mask = vec![0u8; mask_size];

    // Set mask based on alpha channel (alpha < 128 = transparent)
    for y in 0..height as usize {
        for x in 0..width as usize {
            let alpha = bgra_data[(y * width as usize + x) * 4 + 3];
            if alpha < 128 {
                // Set bit to 1 (transparent)
                let byte_idx = y * ((width as usize + 7) / 8) + x / 8;
                let bit_idx = 7 - (x % 8);
                and_mask[byte_idx] |= 1 << bit_idx;
            }
        }
    }

    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let hinstance = HINSTANCE(instance.0);

        let icon = CreateIcon(
            Some(hinstance),
            width,
            height,
            1,  // planes
            32, // bits per pixel
            and_mask.as_ptr(),
            bgra_data.as_ptr(),
        );

        match icon {
            Ok(h) if !h.is_invalid() => {
                tracing::debug!("Created tray icon {}x{}", width, height);
                Some(h)
            }
            _ => {
                tracing::warn!("Failed to create tray icon from resource");
                None
            }
        }
    }
}

fn create_message_window() -> Option<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let hinstance = HINSTANCE(instance.0);

        let class_name = w!("Linefeed_TrayClass");

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            tracing::warn!("Failed to register tray window class");
            return None;
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Linefeed Tray"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinstance),
            None,
        );

        match hwnd {
            Ok(h) if !h.is_invalid() => Some(h),
            _ => {
                tracing::warn!("Failed to create tray message window");
                None
            }
        }
    }
}

pub fn create_tray_icon() -> bool {
    let msg_hwnd = match create_message_window() {
        Some(h) => h,
        None => return false,
    };

    MSG_HWND.store(msg_hwnd.0 as *mut _, Ordering::SeqCst);
    NID_HWND.store(msg_hwnd.0 as *mut _, Ordering::SeqCst);

    // Create the icon - if it fails, use a default
    let hicon = create_icon();
    if let Some(icon) = hicon {
        // Remember the handle so destroy_tray_icon can free it (HICON is Copy).
        TRAY_HICON.store(icon.0 as *mut _, Ordering::SeqCst);
    }

    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: msg_hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: hicon.unwrap_or_default(),
            ..Default::default()
        };

        // Set tooltip. Copy the array out by value first: NOTIFYICONDATAW is
        // packed on some targets (e.g. i686), so taking a reference to the
        // szTip field directly is unaligned (E0793).
        let mut sztip = nid.szTip;
        let tip = "Linefeed";
        for (i, c) in tip.encode_utf16().enumerate() {
            if i < sztip.len() - 1 {
                sztip[i] = c;
            }
        }
        nid.szTip = sztip;

        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            TRAY_ICON_ADDED.store(true, Ordering::SeqCst);
            TRAY_ACTIVE.store(true, Ordering::SeqCst);
            tracing::info!("System tray icon created");
            true
        } else {
            tracing::warn!("Failed to add tray icon");
            false
        }
    }
}

pub fn setup_event_handler() {
    // Start message pump thread for tray events
    std::thread::spawn(|| {
        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

pub fn destroy_tray_icon() {
    if TRAY_ICON_ADDED.load(Ordering::SeqCst) {
        let hwnd_ptr = NID_HWND.load(Ordering::SeqCst);
        if !hwnd_ptr.is_null() {
            unsafe {
                let nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: HWND(hwnd_ptr as *mut _),
                    uID: 1,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }
        TRAY_ICON_ADDED.store(false, Ordering::SeqCst);
    }

    // Free the icon resource we created, after it has been removed from the tray.
    let icon_ptr = TRAY_HICON.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !icon_ptr.is_null() {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
            let _ = DestroyIcon(HICON(icon_ptr as *mut _));
        }
    }

    let hwnd_ptr = MSG_HWND.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !hwnd_ptr.is_null() {
        unsafe {
            let hwnd = HWND(hwnd_ptr as *mut _);
            let _ = PostMessageW(Some(hwnd), WM_DESTROY, WPARAM(0), LPARAM(0));
            let _ = DestroyWindow(hwnd);
        }
    }

    TRAY_ACTIVE.store(false, Ordering::SeqCst);
}
