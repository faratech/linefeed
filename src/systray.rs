//! Custom Windows system tray implementation using Win32 APIs directly.
//! Replaces the tray-icon crate for full control over behavior.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DispatchMessageW, EnumWindows, FindWindowW, GetClassNameW, GetCursorPos,
    GetMessageW, GetWindowTextW, GetWindowThreadProcessId, HICON, InsertMenuW, IsIconic,
    IsWindowVisible, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow,
    SetMenuDefaultItem, ShowWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_NULL, WM_RBUTTONUP, WM_USER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::icon_data;

const WM_TRAYICON: u32 = WM_USER + 1;
const WM_SHOW_WINDOW: u32 = WM_USER + 2; // Custom message to show window
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
// "TaskbarCreated" broadcast id: Explorer sends it after (re)starting, at which
// point every tray icon must be re-added or it is gone for good.
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

struct FindWindowState {
    found: HWND,
    pid: u32,
}

unsafe extern "system" fn enum_find_main_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut FindWindowState) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != state.pid {
        return BOOL(1); // keep enumerating
    }
    // Skip our own tray message window.
    let mut class_buf = [0u16; 64];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) }.max(0) as usize;
    if String::from_utf16_lossy(&class_buf[..class_len]) == "Linefeed_TrayClass" {
        return BOOL(1);
    }
    let mut title_buf = [0u16; 64];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) }.max(0) as usize;
    if String::from_utf16_lossy(&title_buf[..title_len]) == "Linefeed" {
        state.found = hwnd;
        return BOOL(0); // stop enumerating
    }
    BOOL(1)
}

/// Find this process's main window. Restricting the search to our own process
/// id is essential: a bare title match ("Linefeed") could hide/show/flash a
/// foreign window, e.g. an Explorer window open on a folder named Linefeed.
fn find_main_window() -> Option<HWND> {
    let mut state = FindWindowState {
        found: HWND::default(),
        pid: unsafe { GetCurrentProcessId() },
    };
    unsafe {
        // Returns Err when the callback stops enumeration early - not an error.
        let _ = EnumWindows(
            Some(enum_find_main_window),
            LPARAM(&mut state as *mut FindWindowState as isize),
        );
    }
    if state.found.is_invalid() {
        None
    } else {
        Some(state.found)
    }
}

pub fn show_window() {
    WINDOW_HIDDEN.store(false, Ordering::SeqCst);
    RESTORE_REQUESTED.store(true, Ordering::SeqCst);
    if let Some(hwnd) = find_main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            // Restore only when actually minimized: an unconditional
            // SW_RESTORE would also collapse a maximized window back to its
            // normal size.
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
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

/// Whether the main window is actually visible on screen (WS_VISIBLE set).
/// Detects out-of-band re-shows - anything that calls ShowWindow on us while
/// we believe the window is hidden in the tray - so the GUI can resync
/// instead of presenting a live window that never repaints.
pub fn is_window_visible() -> bool {
    find_main_window().is_some_and(|hwnd| unsafe { IsWindowVisible(hwnd) }.as_bool())
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
        if let Ok(hwnd) = FindWindowW(None, w!("Linefeed Tray"))
            && !hwnd.is_invalid()
        {
            // Send our custom show message
            SendMessageW(hwnd, WM_SHOW_WINDOW, Some(WPARAM(0)), Some(LPARAM(0)));
            tracing::info!("Sent activate message to existing instance");
        }
    }
}

pub fn should_exit() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Flash the taskbar to alert the user of a notification
pub fn flash_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx,
    };

    if let Some(hwnd) = find_main_window() {
        unsafe {
            let flash_info = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            let _ = FlashWindowEx(&flash_info);
        }
        tracing::debug!("Flashed taskbar for notification");
    }
}

/// Alert the user of a notification. While hidden to the tray there is no
/// taskbar button, so FlashWindowEx shows nothing at all - use a tray balloon
/// instead; otherwise flash the taskbar as before.
pub fn notify(title: &str, body: &str) {
    if is_window_hidden() && TRAY_ICON_ADDED.load(Ordering::SeqCst) {
        show_balloon(title, body);
    } else {
        flash_window();
    }
}

/// Show a tray balloon notification (NIM_MODIFY + NIF_INFO).
fn show_balloon(title: &str, body: &str) {
    let hwnd_ptr = NID_HWND.load(Ordering::SeqCst);
    if hwnd_ptr.is_null() {
        return;
    }
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(hwnd_ptr as *mut _),
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_INFO,
            ..Default::default()
        };
        // Copy the arrays out by value: NOTIFYICONDATAW is packed on some
        // targets, so referencing the fields directly is unaligned (E0793).
        let mut info = nid.szInfo;
        for (i, c) in body.encode_utf16().enumerate() {
            if i < info.len() - 1 {
                info[i] = c;
            }
        }
        nid.szInfo = info;
        let mut info_title = nid.szInfoTitle;
        for (i, c) in title.encode_utf16().enumerate() {
            if i < info_title.len() - 1 {
                info_title[i] = c;
            }
        }
        nid.szInfoTitle = info_title;

        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
    tracing::debug!("Showed tray balloon notification");
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
        _ => {
            // Explorer (re)started: the notification area is brand new and our
            // icon no longer exists there. Re-add it, otherwise a tray-hidden
            // app becomes completely unreachable (no icon, no taskbar button).
            let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::SeqCst);
            if taskbar_created != 0 && msg == taskbar_created {
                // Explorer may have been unavailable for every startup retry,
                // so recovery must not depend on already being active.
                let added = add_icon_to_shell();
                TRAY_ICON_ADDED.store(added, Ordering::SeqCst);
                TRAY_ACTIVE.store(added, Ordering::SeqCst);
                tracing::info!("Explorer started/restarted; tray icon added: {}", added);
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
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

        let _ = InsertMenuW(
            menu,
            0,
            MF_BYPOSITION | MF_STRING,
            ID_SHOW as usize,
            PCWSTR(show_text.as_ptr()),
        );
        let _ = InsertMenuW(menu, 1, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
        let _ = InsertMenuW(
            menu,
            2,
            MF_BYPOSITION | MF_STRING,
            ID_QUIT as usize,
            PCWSTR(quit_text.as_ptr()),
        );

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

        // Required after TrackPopupMenu (see its documentation): without this
        // the next menu open flashes and immediately dismisses itself.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

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
    let mask_size = (width as usize).div_ceil(8) * height as usize;
    let mut and_mask = vec![0u8; mask_size];

    // Set mask based on alpha channel (alpha < 128 = transparent)
    for y in 0..height as usize {
        for x in 0..width as usize {
            let alpha = bgra_data[(y * width as usize + x) * 4 + 3];
            if alpha < 128 {
                // Set bit to 1 (transparent)
                let byte_idx = y * (width as usize).div_ceil(8) + x / 8;
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

/// Add the tray icon to the shell using the stored window/icon handles.
/// Used both at startup and to re-add after an Explorer restart.
fn add_icon_to_shell() -> bool {
    let hwnd_ptr = NID_HWND.load(Ordering::SeqCst);
    if hwnd_ptr.is_null() {
        return false;
    }
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(hwnd_ptr as *mut _),
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: HICON(TRAY_HICON.load(Ordering::SeqCst)),
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

        Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
    }
}

/// Set up the tray icon on a dedicated thread that owns the message window and
/// pumps its messages. Running the pump off the egui thread means tray events
/// (including TrackPopupMenu's modal message loop) never stall rendering or
/// message draining. Window messages are delivered to the creating thread, so
/// the window must be created on the pump thread itself.
pub fn create_tray_icon() {
    std::thread::spawn(|| {
        let msg_hwnd = match create_message_window() {
            Some(h) => h,
            None => return,
        };

        MSG_HWND.store(msg_hwnd.0 as *mut _, Ordering::SeqCst);
        NID_HWND.store(msg_hwnd.0 as *mut _, Ordering::SeqCst);

        // Learn Explorer's "TaskbarCreated" broadcast id so the icon can be
        // re-added when Explorer restarts.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        TASKBAR_CREATED_MSG.store(taskbar_created, Ordering::SeqCst);

        // Create the icon - if it fails, a default (blank) icon is used.
        if let Some(icon) = create_icon() {
            // Remember the handle so destroy_tray_icon can free it (HICON is Copy).
            TRAY_HICON.store(icon.0 as *mut _, Ordering::SeqCst);
        }

        // The shell may not be ready during session startup; retry briefly.
        let mut added = false;
        for attempt in 0u64..3 {
            if add_icon_to_shell() {
                added = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
        }
        if added {
            TRAY_ICON_ADDED.store(true, Ordering::SeqCst);
            TRAY_ACTIVE.store(true, Ordering::SeqCst);
            tracing::info!("System tray icon created");
        } else {
            tracing::warn!("Failed to add tray icon");
        }

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
            // The window is owned by the pump thread (DestroyWindow only works
            // from the owning thread), so post WM_DESTROY: the wnd_proc calls
            // PostQuitMessage, which ends the pump thread; the OS reclaims the
            // window at process exit.
            let hwnd = HWND(hwnd_ptr as *mut _);
            let _ = PostMessageW(Some(hwnd), WM_DESTROY, WPARAM(0), LPARAM(0));
        }
    }

    TRAY_ACTIVE.store(false, Ordering::SeqCst);
}
