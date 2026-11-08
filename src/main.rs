// RecycleTray (Rust port)
// Native Win32 tray app that shows the Recycle Bin status and allows opening/emptying it.
// Build: cargo build --release
// Expects light.png, dark.png, light_empty.png, dark_empty.png next to the EXE.

#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::ffi::c_void;
use std::iter::once;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAYICON: u32 = WM_USER + 1;
const ID_TRAY_TIMER: usize = 1001;
const ID_MENU_OPEN: usize = 2001;
const ID_MENU_EMPTY: usize = 2002;
const ID_MENU_EXIT: usize = 2003;
const ID_MENU_CLEANMGR: usize = 2004;
const ID_MENU_AUTOSTART: usize = 2005;

const RUN_KEY_PATH: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const AUTOSTART_VALUE_NAME: PCWSTR = w!("RecycleTray");

// Tray icons: one for a filled and one for an empty Recycle Bin (theme dependent).
// The window procedure always runs on the main thread, so thread-local state is enough.
thread_local! {
    static ICONS: Cell<(HICON, HICON)> = Cell::new((HICON::default(), HICON::default()));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

/// Copies `s` into a fixed-size UTF-16 buffer (truncating, always null-terminated).
fn copy_wide(dst: &mut [u16], s: &str) {
    let mut i = 0;
    for c in s.encode_utf16() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Loads a PNG from the EXE folder (not the working directory, which differs with
/// Autostart) and turns it into an HICON of the system small-icon size.
fn load_icon_from_png(bytes: &[u8]) -> Option<HICON> {
    let cx = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let cy = unsafe { GetSystemMetrics(SM_CYSMICON) };
    if cx <= 0 || cy <= 0 {
        return None;
    }

    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img
        .resize_exact(cx as u32, cy as u32, image::imageops::FilterType::CatmullRom)
        .to_rgba8();

    // RGBA -> BGRA (what GDI expects for a 32 bpp bitmap)
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    // The monochrome mask is ignored for 32 bpp icons with alpha, but must be valid.
    let mask_stride = ((cx as usize + 15) / 16) * 2;
    let mask = vec![0u8; mask_stride * cy as usize];

    unsafe {
        let hbm_color = CreateBitmap(cx, cy, 1, 32, Some(bgra.as_ptr() as *const c_void));
        let hbm_mask = CreateBitmap(cx, cy, 1, 1, Some(mask.as_ptr() as *const c_void));

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let icon = CreateIconIndirect(&info).ok();

        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        icon
    }
}

fn is_light_theme() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        let res = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            0,
            KEY_READ,
            &mut hkey,
        );
        if res != ERROR_SUCCESS {
            return false;
        }
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let res = RegQueryValueExW(
            hkey,
            w!("SystemUsesLightTheme"),
            None,
            None,
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        res == ERROR_SUCCESS && data == 1
    }
}

fn load_theme_icons() {
    let light = is_light_theme();

    let full_bytes: &[u8] = if light {
        &include_bytes!("../assets/light.png")[..]
    } else {
        &include_bytes!("../assets/dark.png")[..]
    };
    let empty_bytes: &[u8] = if light {
        &include_bytes!("../assets/light_empty.png")[..]
    } else {
        &include_bytes!("../assets/dark_empty.png")[..]
    };

    let full = load_icon_from_png(full_bytes)
        // fallback to the system application icon
        .or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).ok() })
        .unwrap_or_default();

    let empty = load_icon_from_png(empty_bytes)
        // fallback: same icon as for a filled bin
        .or_else(|| unsafe { CopyIcon(full).ok() })
        .unwrap_or_default();

    let (old_full, old_empty) = ICONS.with(|c| c.replace((full, empty)));
    unsafe {
        if !old_full.is_invalid() {
            let _ = DestroyIcon(old_full);
        }
        if !old_empty.is_invalid() {
            let _ = DestroyIcon(old_empty);
        }
    }
}

fn destroy_icons() {
    let (full, empty) = ICONS.with(|c| c.replace((HICON::default(), HICON::default())));
    unsafe {
        if !full.is_invalid() {
            let _ = DestroyIcon(full);
        }
        if !empty.is_invalid() {
            let _ = DestroyIcon(empty);
        }
    }
}

// ---------------------------------------------------------------------------
// Recycle Bin
// ---------------------------------------------------------------------------

/// Returns (number of items, total size in bytes) of everything in the Recycle Bin.
fn query_recycle_bin() -> (u64, u64) {
    unsafe {
        let mut info = SHQUERYRBINFO::default();
        info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
        if SHQueryRecycleBinW(PCWSTR::null(), &mut info).is_ok() {
            // copy out of the (possibly packed) struct
            let items = info.i64NumItems;
            let size = info.i64Size;
            (items.max(0) as u64, size.max(0) as u64)
        } else {
            (0, 0)
        }
    }
}

fn empty_recycle_bin_no_confirm() {
    unsafe {
        // Errors (e.g. bin already empty) are irrelevant here.
        let _ = SHEmptyRecycleBinW(None, PCWSTR::null(), SHERB_NOCONFIRMATION);
    }
}

fn run_disk_cleanup_as_admin() {
    unsafe {
        // "runas" verb triggers the UAC elevation prompt
        ShellExecuteW(
            None,
            w!("runas"),
            w!("cleanmgr.exe"),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn open_recycle_bin_in_explorer() {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            w!("shell:RecycleBinFolder"),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// Adds the tray icon (if not yet present) and updates icon + tooltip.
// The icon is always visible: "empty" icon for an empty bin, "full" icon otherwise.
fn update_tray_icon(hwnd: HWND, item_count: u64, total_bytes: u64) {
    let (full, empty) = ICONS.with(|c| c.get());

    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = if item_count > 0 { full } else { empty };

    let tip = if item_count > 0 {
        let mut buf = [0u16; 32];
        let size_str = unsafe {
            StrFormatByteSizeW(total_bytes as i64, &mut buf);
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        };
        let size_str = if size_str.is_empty() { "?".to_string() } else { size_str };
        format!("{} item(s) in the Recycle Bin ({})", item_count, size_str)
    } else {
        "Recycle Bin is empty".to_string()
    };
    copy_wide(&mut nid.szTip, &tip);

    unsafe {
        // NIM_ADD fails harmlessly if the icon already exists, then NIM_MODIFY updates it
        // (this also re-adds the icon after an Explorer restart).
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn refresh_tray(hwnd: HWND) {
    let (items, bytes) = query_recycle_bin();
    update_tray_icon(hwnd, items, bytes);
}

// ---------------------------------------------------------------------------
// Autostart (current user only: HKCU\...\Run)
// ---------------------------------------------------------------------------

fn quoted_exe_path() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", path.display()))
}

fn read_autostart_value() -> Option<String> {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY_PATH, 0, KEY_QUERY_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let res = RegQueryValueExW(
            hkey,
            AUTOSTART_VALUE_NAME,
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if res != ERROR_SUCCESS || ty != REG_SZ {
            return None;
        }
        let n = (size as usize / 2).min(buf.len());
        Some(
            String::from_utf16_lossy(&buf[..n])
                .trim_end_matches('\0')
                .to_string(),
        )
    }
}

/// Only counts as enabled if it points to this very EXE
/// (an outdated path after moving the EXE does not count).
fn is_autostart_enabled() -> bool {
    match (quoted_exe_path(), read_autostart_value()) {
        (Some(expected), Some(value)) => value.to_lowercase() == expected.to_lowercase(),
        _ => false,
    }
}

fn set_autostart(enable: bool) {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY_PATH, 0, KEY_SET_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return;
        }
        if enable {
            if let Some(path) = quoted_exe_path() {
                let value = wide(&path);
                let bytes =
                    std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
                let _ = RegSetValueExW(hkey, AUTOSTART_VALUE_NAME, 0, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(hkey, AUTOSTART_VALUE_NAME);
        }
        let _ = RegCloseKey(hkey);
    }
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

/// Draws a check mark or a cross into a bitmap sized for the menu check column.
fn create_menu_mark_bitmap(check: bool) -> HBITMAP {
    unsafe {
        let cx = GetSystemMetrics(SM_CXMENUCHECK);
        let cy = GetSystemMetrics(SM_CYMENUCHECK);
        let hdc_screen = GetDC(None);
        let hdc = CreateCompatibleDC(hdc_screen);
        let bmp = CreateCompatibleBitmap(hdc_screen, cx, cy);
        let old_bmp = SelectObject(hdc, HGDIOBJ(bmp.0));

        let rc = windows::Win32::Foundation::RECT { left: 0, top: 0, right: cx, bottom: cy };
        FillRect(hdc, &rc, GetSysColorBrush(COLOR_MENU));

        let thickness = (cx / 8).max(2);
        let color = if check { rgb(0, 150, 0) } else { rgb(200, 0, 0) };
        let pen = CreatePen(PS_SOLID, thickness, color);
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));

        if check {
            let _ = MoveToEx(hdc, cx * 20 / 100, cy * 55 / 100, None);
            let _ = LineTo(hdc, cx * 42 / 100, cy * 75 / 100);
            let _ = LineTo(hdc, cx * 80 / 100, cy * 28 / 100);
        } else {
            let m = cx * 25 / 100;
            let _ = MoveToEx(hdc, m, m, None);
            let _ = LineTo(hdc, cx - m, cy - m);
            let _ = MoveToEx(hdc, cx - m, m, None);
            let _ = LineTo(hdc, m, cy - m);
        }

        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        SelectObject(hdc, old_bmp);
        let _ = DeleteDC(hdc);
        ReleaseDC(None, hdc_screen);
        bmp
    }
}

fn rgb(r: u8, g: u8, b: u8) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut pt);

        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, ID_MENU_OPEN, w!("Open"));
        let _ = AppendMenuW(menu, MF_STRING, ID_MENU_CLEANMGR, w!("Disk Cleanup (Administrator)"));
        let _ = AppendMenuW(menu, MF_STRING, ID_MENU_EMPTY, w!("Empty Trash Bin"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let state = if is_autostart_enabled() { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | state, ID_MENU_AUTOSTART, w!("Autostart"));
        let bmp_check = create_menu_mark_bitmap(true);
        let bmp_cross = create_menu_mark_bitmap(false);
        let _ = SetMenuItemBitmaps(
            menu,
            ID_MENU_AUTOSTART as u32,
            MF_BYCOMMAND,
            bmp_cross,
            bmp_check,
        );
        let _ = AppendMenuW(menu, MF_STRING, ID_MENU_EXIT, w!("Exit"));

        // Set foreground window so the menu disappears correctly
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

        let _ = DestroyMenu(menu);
        let _ = DeleteObject(HGDIOBJ(bmp_check.0));
        let _ = DeleteObject(HGDIOBJ(bmp_cross.0));
    }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam.0 == ID_TRAY_TIMER {
                refresh_tray(hwnd);
            }
            LRESULT(0)
        }

        WM_TRAYICON => {
            match lparam.0 as u32 {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_context_menu(hwnd),
                WM_LBUTTONUP => open_recycle_bin_in_explorer(),
                _ => {}
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            match (wparam.0 & 0xFFFF) as usize {
                ID_MENU_OPEN => open_recycle_bin_in_explorer(),
                ID_MENU_AUTOSTART => set_autostart(!is_autostart_enabled()),
                ID_MENU_CLEANMGR => run_disk_cleanup_as_admin(),
                ID_MENU_EMPTY => {
                    empty_recycle_bin_no_confirm();
                    refresh_tray(hwnd); // immediately update
                }
                ID_MENU_EXIT => {
                    // WM_DESTROY removes the tray icon and quits
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            // theme or settings changed: reload icons according to current theme
            load_theme_icons();
            refresh_tray(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            let _ = KillTimer(hwnd, ID_TRAY_TIMER);

            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);

            destroy_icons();
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> windows::core::Result<()> {
    unsafe {
        // load icons (filled / empty) according to the current theme
        load_theme_icons();

        let hinstance = GetModuleHandleW(None)?.into();
        let class_name = w!("RecycleTrayHiddenWindow");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Hidden window (never shown)
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("RecycleTray"),
            WINDOW_STYLE(0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            hinstance,
            None,
        );
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                destroy_icons();
                return Err(e);
            }
        };

        // add tray icon with the state matching the current Recycle Bin content
        refresh_tray(hwnd);

        // poll the Recycle Bin every second
        SetTimer(hwnd, ID_TRAY_TIMER, 1000, None);

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        destroy_icons();
    }
    Ok(())
}
