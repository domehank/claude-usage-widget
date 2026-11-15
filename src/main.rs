//! Claude usage widget: two thin bars embedded in the Windows taskbar showing Claude's
//! 5-hour and weekly usage. Hover for exact numbers and reset times, drag left/right to move
//! it (the position is remembered), right-click for "Reset position" / "Exit".
//!
//! Token counts in the tooltip come from Claude Code's session logs (see tokens.rs).
//!
//! Data: ~/.claude/usage.json, written by the Claude Code status line (statusline/usage-statusline.ps1) on
//! every refresh. The widget watches that folder and redraws only when the numbers change.
//!
//! The widget is a child window of the taskbar (Shell_TrayWnd), so the taskbar can never cover
//! it. If Explorer restarts, the taskbar takes the widget with it; it is recreated on the
//! "TaskbarCreated" broadcast, or at the latest by the 15-second housekeeping timer.

#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn, static_mut_refs)]

use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use std::sync::Mutex;

mod tokens;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus as gp;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_FILE_CHANGED: u32 = WM_APP + 1;
const WM_APP_TOKENS: u32 = WM_APP + 2; // a token count finished
const TIMER_HOUSEKEEPING: usize = 1; // every 15 s: Explorer restart, taskbar size, reset times
const TIMER_DEBOUNCE: usize = 2; // collapses bursts of folder change notifications
const TIMER_RECREATE: usize = 3; // retries creating the widget while the taskbar is missing
const CMD_RESET_POSITION: i32 = 1;
const CMD_EXIT: i32 = 2;

struct Limit {
    key: &'static str,
    short: &'static str,
    long: &'static str,
    with_day: bool,
    secs: i64, // length of the window
}
const LIMITS: [Limit; 2] = [
    Limit { key: "five_hour", short: "5h", long: "5-hour", with_day: false, secs: 5 * 3600 },
    Limit { key: "seven_day", short: "W", long: "Weekly", with_day: true, secs: 7 * 86_400 },
];

struct Theme {
    light: bool, // Windows taskbar/system theme is light
    fg: u32,
    dim: u32,
    track: u32,
}

struct App {
    hinst: HINSTANCE,
    owner: HWND,  // hidden top-level window: timers, broadcasts, menu owner
    widget: HWND, // child of the taskbar
    tooltip: HWND,
    taskbar: HWND,
    data_file: PathBuf,
    pos_file: PathBuf,
    raw: String,
    usage: Value,
    state: String,
    offset: Option<i32>, // px from the taskbar's right edge to the widget's right edge
    scale: f32,
    bounds: (i32, i32, i32, i32),
    drag: Option<(i32, i32)>, // (cursor x at start, offset at start)
    theme: Theme,
    quitting: bool,
    tokens: [Option<u64>; 2], // per limit, counted from the session logs
}

static mut APP: Option<App> = None;

fn app() -> &'static mut App {
    // Single UI thread; the watcher thread only posts messages.
    unsafe { APP.as_mut().unwrap() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn argb(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn load_theme() -> Theme {
    let mut light: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("SystemUsesLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut light as *mut u32 as *mut _,
            &mut size,
        );
    }
    if light == 1 {
        Theme { light: true, fg: argb(25, 25, 25), dim: argb(95, 95, 95), track: argb(200, 200, 205) }
    } else {
        Theme { light: false, fg: argb(235, 235, 235), dim: argb(160, 160, 160), track: argb(70, 72, 80) }
    }
}

/// Matches the right-click menu to the system theme. Windows has no public API for dark menus;
/// this uses uxtheme.dll's SetPreferredAppMode (ordinal 135) and FlushMenuThemes (136), which
/// are undocumented but stable since Windows 10 1903 and widely used. Missing: menus stay light.
unsafe fn style_menus(light: bool) {
    let ux = LoadLibraryW(wide("uxtheme.dll").as_ptr());
    if ux.is_null() {
        return;
    }
    if let Some(f) = GetProcAddress(ux, 135 as windows_sys::core::PCSTR) {
        let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
        set_mode(if light { 3 } else { 2 }); // ForceLight / ForceDark
    }
    if let Some(f) = GetProcAddress(ux, 136 as windows_sys::core::PCSTR) {
        let flush: extern "system" fn() = std::mem::transmute(f);
        flush();
    }
}

/// Matches the hover tooltip to the system theme: Windows' built-in dark tooltip style in dark
/// mode, the normal one in light mode.
unsafe fn style_tooltip(tooltip: HWND, light: bool) {
    if !tooltip.is_null() {
        let style = wide(if light { "Explorer" } else { "DarkMode_Explorer" });
        SetWindowTheme(tooltip, style.as_ptr(), null());
    }
}

// ---- data ---------------------------------------------------------------------------------

struct Window {
    pct: f64,
    reset: Option<i64>,
    expired: bool,
}

fn limit_window(usage: &Value, key: &str) -> Option<Window> {
    let o = usage.get(key)?;
    let mut pct = o.get("used_percentage")?.as_f64()?;
    let reset = o.get("resets_at").and_then(Value::as_f64).map(|r| r as i64);
    let expired = reset.is_some_and(|r| r < now());
    if expired {
        pct = 0.0;
    }
    Some(Window { pct, reset, expired })
}

/// What is drawn; the widget repaints only when this changes.
fn draw_state(usage: &Value) -> String {
    LIMITS
        .iter()
        .map(|l| limit_window(usage, l.key).map_or("-".into(), |w| format!("{}", w.pct as i32)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Re-reads usage.json; returns true if its content changed.
fn load_usage() -> bool {
    let a = app();
    let Ok(raw) = std::fs::read_to_string(&a.data_file) else { return false };
    if raw == a.raw {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')) else { return false };
    a.raw = raw;
    a.usage = v;
    true
}

fn local_time(unix: i64) -> SYSTEMTIME {
    unsafe {
        let ticks = (unix as u64 + 11_644_473_600) * 10_000_000;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (zeroed(), zeroed());
        FileTimeToSystemTime(&ft, &mut utc);
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local);
        local
    }
}

fn fmt_time(unix: i64, with_day: bool) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let t = local_time(unix);
    let hm = format!("{:02}:{:02}", t.wHour, t.wMinute);
    if with_day { format!("{} {}", DAYS[t.wDayOfWeek as usize % 7], hm) } else { hm }
}

fn tooltip_text(usage: &Value, toks: &[Option<u64>; 2]) -> String {
    let mut lines: Vec<String> = LIMITS
        .iter()
        .zip(toks)
        .map(|(l, tok)| match limit_window(usage, l.key) {
            None => format!("{}: no data yet", l.long),
            Some(w) if w.expired => format!("{}: reset since last update", l.long),
            Some(w) => {
                let reset = w.reset.map_or(String::new(), |r| format!(", resets {}", fmt_time(r, l.with_day)));
                let tok = tok.map_or(String::new(), |n| {
                    // Estimated max: tokens so far ÷ share of the limit used. Too noisy below 10%,
                    // where the whole-number percentage is off by up to a few tens of percent.
                    let max = if w.pct >= 10.0 {
                        format!(" / ~{}", tokens::format((n as f64 * 100.0 / w.pct) as u64))
                    } else {
                        String::new()
                    };
                    format!(", {}{} tokens", tokens::format(n), max)
                });
                format!("{}: {}% used{}{}", l.long, w.pct as i32, tok, reset)
            }
        })
        .collect();
    if let Some(u) = usage.get("updated").and_then(Value::as_f64) {
        lines.push(format!("Updated {} (from Claude Code)", fmt_time(u as i64, true)));
    }
    if toks.iter().any(Option::is_some) {
        lines.push("Tokens: this PC's Claude Code sessions, without cache reads".into());
        lines.push("~Max: estimated as tokens ÷ % used, shown from 10%".into());
    }
    lines.join("\r\n")
}

/// Reloads the data and repaints only if the file or what is shown changed.
unsafe fn refresh(force: bool) {
    let changed = load_usage();
    let a = app();
    let state = draw_state(&a.usage);
    if changed {
        count_tokens();
    }
    if force || changed || state != a.state {
        a.state = state;
        update_tooltip();
        if !a.widget.is_null() {
            paint(a.widget);
        }
    }
}

// ---- token counts -------------------------------------------------------------------------

static COUNTER: Mutex<Option<tokens::Counter>> = Mutex::new(None);
static COUNTED: Mutex<[Option<u64>; 2]> = Mutex::new([None, None]);

/// Recounts the tokens of each current limit window on a worker thread (the first count reads a
/// week of logs); the result comes back as WM_APP_TOKENS.
fn count_tokens() {
    let a = app();
    let starts: Vec<Option<i64>> = LIMITS
        .iter()
        .map(|l| limit_window(&a.usage, l.key).filter(|w| !w.expired).and_then(|w| w.reset).map(|r| r - l.secs))
        .collect();
    let Some(since) = starts.iter().flatten().min().copied() else { return };
    let projects = a.data_file.with_file_name("projects");
    let owner = a.owner as isize;
    std::thread::spawn(move || {
        let mut guard = COUNTER.lock().unwrap();
        let counter = guard.get_or_insert_with(tokens::Counter::default);
        counter.update(&projects, since);
        let totals = [0, 1].map(|i| starts[i].map(|s| counter.total(s)));
        *COUNTED.lock().unwrap() = totals;
        unsafe { PostMessageW(owner as HWND, WM_APP_TOKENS, 0, 0) };
    });
}

// ---- position -----------------------------------------------------------------------------

fn load_offset(path: &PathBuf) -> Option<i32> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(s.trim_start_matches('\u{feff}')).ok()?;
    v.get("offset")?.as_f64().map(|o| o as i32)
}

unsafe fn rect_of(h: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r);
    r
}

/// Default spot: just left of the tray icons.
unsafe fn default_offset(tb: &RECT, scale: f32) -> i32 {
    let mut off = (420.0 * scale) as i32;
    let tray = FindWindowExW(app().taskbar, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    if !tray.is_null() {
        let n = rect_of(tray);
        if n.right > n.left && n.left > tb.left + (tb.right - tb.left) / 2 {
            off = tb.right - n.left + (12.0 * scale) as i32;
        }
    }
    off
}

fn current_offset() -> i32 {
    let a = app();
    unsafe { a.offset.unwrap_or_else(|| default_offset(&rect_of(a.taskbar), a.scale)) }
}

/// Positions the widget inside the taskbar (child coordinates are relative to the taskbar).
unsafe fn place(force: bool) {
    let a = app();
    if a.widget.is_null() {
        return;
    }
    let t = rect_of(a.taskbar);
    let h = t.bottom - t.top;
    if h <= 0 || h > 200 {
        return; // vertical or odd taskbar
    }
    a.scale = h as f32 / 48.0; // the Windows 11 taskbar is 48 px tall at 100% scaling
    let (w, fh) = ((120.0 * a.scale) as i32, (36.0 * a.scale) as i32);
    let off = a.offset.unwrap_or_else(|| default_offset(&t, a.scale));
    let b = ((t.right - t.left) - off - w, (h - fh) / 2, w, fh);
    if !force && b == a.bounds {
        return;
    }
    a.bounds = b;
    // HWND_TOP among the taskbar's children, so it sits above the taskbar's own content
    SetWindowPos(a.widget, HWND_TOP, b.0, b.1, b.2, b.3, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    paint(a.widget);
}

// ---- widget lifetime ----------------------------------------------------------------------

unsafe fn create_widget() -> bool {
    let a = app();
    let tb = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    if tb.is_null() {
        return false;
    }
    a.taskbar = tb;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        wide("ClaudeUsageWidget").as_ptr(),
        null(),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
        0, 0, 10, 10,
        tb,
        null_mut(),
        a.hinst,
        null(),
    );
    if hwnd.is_null() {
        return false;
    }
    a.widget = hwnd;
    a.bounds = (0, 0, 0, 0);

    // Tooltip, owned by our own top-level window (not the taskbar, which is another process)
    a.tooltip = CreateWindowExW(
        WS_EX_TOPMOST,
        TOOLTIPS_CLASSW,
        null(),
        WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        a.owner,
        null_mut(),
        a.hinst,
        null(),
    );
    let mut text = wide("");
    let ti = tool_info(&mut text);
    SendMessageW(a.tooltip, TTM_ADDTOOLW, 0, &ti as *const _ as LPARAM);
    SendMessageW(a.tooltip, TTM_SETMAXTIPWIDTH, 0, 800);
    style_tooltip(a.tooltip, a.theme.light);
    style_menus(a.theme.light);

    place(true);
    refresh(true);
    true
}

fn tool_info(text: &mut Vec<u16>) -> TTTOOLINFOW {
    let a = app();
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: a.widget,
        uId: a.widget as usize,
        lpszText: text.as_mut_ptr(),
        ..unsafe { zeroed() }
    }
}

unsafe fn update_tooltip() {
    let a = app();
    if a.tooltip.is_null() {
        return;
    }
    let mut text = wide(&tooltip_text(&a.usage, &a.tokens));
    let ti = tool_info(&mut text);
    SendMessageW(a.tooltip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
}

unsafe fn destroy_widget() {
    let a = app();
    if !a.tooltip.is_null() {
        DestroyWindow(a.tooltip);
        a.tooltip = null_mut();
    }
    if !a.widget.is_null() {
        let w = a.widget;
        a.widget = null_mut();
        if IsWindow(w) != 0 {
            DestroyWindow(w);
        }
    }
}

/// Recreates the widget if Explorer restarted or the widget got detached.
unsafe fn ensure_widget() {
    let a = app();
    let ok = !a.widget.is_null()
        && IsWindow(a.widget) != 0
        && IsWindow(a.taskbar) != 0
        && GetParent(a.widget) == a.taskbar;
    if !ok {
        destroy_widget();
        if !create_widget() {
            SetTimer(app().owner, TIMER_RECREATE, 2000, None);
        }
    }
}

// ---- painting -----------------------------------------------------------------------------

const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x000E_200B; // GDI+ PixelFormat32bppPARGB

/// Draws the widget into a 32-bit ARGB bitmap and hands it to the layered window.
/// The background is alpha 1: invisible, but it still catches the mouse over the whole widget
/// (fully transparent pixels would let clicks fall through to the taskbar).
unsafe fn paint(hwnd: HWND) {
    let a = app();
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    if w <= 0 || h <= 0 {
        return;
    }
    let screen = GetDC(null_mut());
    let mem = CreateCompatibleDC(screen);
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    let mut bits = null_mut();
    let bmp = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    let old = SelectObject(mem, bmp as HGDIOBJ);
    let mut canvas = null_mut();
    gp::GdipCreateBitmapFromScan0(w, h, w * 4, PIXEL_FORMAT_32BPP_PARGB, bits as *const u8, &mut canvas);
    let mut g = null_mut();
    gp::GdipGetImageGraphicsContext(canvas as *mut gp::GpImage, &mut g);
    gp::GdipGraphicsClear(g, 0x0100_0000);
    gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
    gp::GdipSetTextRenderingHint(g, gp::TextRenderingHintAntiAliasGridFit);
    let sc = a.scale;
    let mut family = null_mut();
    gp::GdipCreateFontFamilyFromName(wide("Segoe UI").as_ptr(), null_mut(), &mut family);
    let (mut small, mut font) = (null_mut(), null_mut());
    gp::GdipCreateFont(family, 11.0 * sc, gp::FontStyleRegular, gp::UnitPixel, &mut small);
    gp::GdipCreateFont(family, 11.0 * sc, gp::FontStyleRegular, gp::UnitPixel, &mut font);
    let (mut dim, mut fg) = (null_mut(), null_mut());
    gp::GdipCreateSolidFill(a.theme.dim, &mut dim);
    gp::GdipCreateSolidFill(a.theme.fg, &mut fg);

    let text = |s: &str, x: f32, cy: f32, font: *mut gp::GpFont, brush: *mut gp::GpSolidFill| {
        let s: Vec<u16> = s.encode_utf16().collect();
        let layout = gp::RectF { X: 0.0, Y: 0.0, Width: 1000.0, Height: 1000.0 };
        let mut bb: gp::RectF = zeroed();
        gp::GdipMeasureString(g, s.as_ptr(), s.len() as i32, font, &layout, null(), &mut bb, null_mut(), null_mut());
        let at = gp::RectF { X: x, Y: cy - bb.Height / 2.0, Width: 0.0, Height: 0.0 };
        gp::GdipDrawString(g, s.as_ptr(), s.len() as i32, font, &at, null(), brush as *const gp::GpBrush);
    };
    let line = |color: u32, x1: f32, x2: f32, y: f32| {
        let mut pen = null_mut();
        gp::GdipCreatePen1(color, 3.0 * sc, gp::UnitPixel, &mut pen);
        gp::GdipSetPenStartCap(pen, gp::LineCapRound);
        gp::GdipSetPenEndCap(pen, gp::LineCapRound);
        gp::GdipDrawLine(g, pen, x1, y, x2, y);
        gp::GdipDeletePen(pen);
    };

    for (i, l) in LIMITS.iter().enumerate() {
        let win = limit_window(&a.usage, l.key);
        let cy = h as f32 * (0.3 + 0.4 * i as f32); // 5h on the top row, weekly below
        let x = 4.0 * sc;
        text(l.short, x, cy, small, dim);
        let (bx, bw) = (x + 22.0 * sc, 50.0 * sc);
        line(a.theme.track, bx, bx + bw, cy);
        if let Some(w) = &win {
            if w.pct > 0.0 {
                let color = if w.pct >= 90.0 {
                    argb(235, 87, 87)
                } else if w.pct >= 75.0 {
                    argb(242, 153, 74)
                } else {
                    argb(96, 165, 250)
                };
                line(color, bx, bx + (bw * w.pct.min(100.0) as f32 / 100.0).max(0.5), cy);
            }
        }
        let pct = win.map_or("\u{2013}".to_string(), |w| format!("{}%", w.pct as i32));
        text(&pct, bx + bw + 8.0 * sc, cy, font, fg);
    }

    gp::GdipDeleteBrush(dim as *mut gp::GpBrush);
    gp::GdipDeleteBrush(fg as *mut gp::GpBrush);
    gp::GdipDeleteFont(font);
    gp::GdipDeleteFont(small);
    gp::GdipDeleteFontFamily(family);
    gp::GdipDeleteGraphics(g);
    gp::GdipDisposeImage(canvas as *mut gp::GpImage);

    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(hwnd, screen, null(), &size, mem, &src, 0, &blend, ULW_ALPHA);
    SelectObject(mem, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(mem);
    ReleaseDC(null_mut(), screen);
}

// ---- window procedures --------------------------------------------------------------------

fn cursor_x() -> i32 {
    let mut p: POINT = unsafe { zeroed() };
    unsafe { GetCursorPos(&mut p) };
    p.x
}

unsafe fn show_menu() {
    let a = app();
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, CMD_RESET_POSITION as usize, wide("Reset position").as_ptr());
    AppendMenuW(menu, MF_STRING, CMD_EXIT as usize, wide("Exit").as_ptr());
    let mut p: POINT = zeroed();
    GetCursorPos(&mut p);
    SetForegroundWindow(a.owner); // so the menu closes when clicking elsewhere
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, 0, a.owner, null());
    PostMessageW(a.owner, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match cmd {
        CMD_RESET_POSITION => {
            a.offset = None;
            let _ = std::fs::remove_file(&a.pos_file);
            place(true);
        }
        CMD_EXIT => {
            a.quitting = true;
            destroy_widget();
            PostQuitMessage(0);
        }
        _ => {}
    }
}

extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                // Layered windows are redrawn with paint() directly; just validate here
                ValidateRect(hwnd, null());
                0
            }
            WM_ERASEBKGND => 1,
            WM_LBUTTONDOWN => {
                let start = current_offset();
                app().drag = Some((cursor_x(), start));
                SetCapture(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                if let Some((x0, off0)) = app().drag {
                    app().offset = Some(off0 - (cursor_x() - x0));
                    place(false);
                }
                0
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if let Some((_, off0)) = app().drag.take() {
                    if msg == WM_LBUTTONUP {
                        ReleaseCapture();
                    }
                    let a = app();
                    // Save only when the widget actually moved, so a plain click changes nothing
                    if let Some(off) = a.offset.filter(|&o| o != off0) {
                        let _ = std::fs::write(&a.pos_file, format!("{{\"offset\": {off}}}"));
                    }
                }
                0
            }
            WM_RBUTTONUP => {
                show_menu();
                0
            }
            WM_DESTROY => {
                // Destroyed together with the taskbar when Explorer restarts: come back later
                let a = app();
                if a.widget == hwnd {
                    a.widget = null_mut();
                }
                if !a.quitting {
                    SetTimer(a.owner, TIMER_RECREATE, 2000, None);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

static mut TASKBAR_CREATED: u32 = 0;

extern "system" fn owner_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == TASKBAR_CREATED && msg != 0 {
            ensure_widget(); // Explorer restarted: attach to the new taskbar right away
            return 0;
        }
        match msg {
            WM_APP_TOKENS => {
                app().tokens = *COUNTED.lock().unwrap();
                update_tooltip();
                0
            }
            WM_APP_FILE_CHANGED => {
                SetTimer(hwnd, TIMER_DEBOUNCE, 200, None);
                0
            }
            WM_TIMER => {
                match wp {
                    TIMER_DEBOUNCE => {
                        KillTimer(hwnd, TIMER_DEBOUNCE);
                        refresh(false);
                    }
                    TIMER_RECREATE => {
                        KillTimer(hwnd, TIMER_RECREATE);
                        if !app().quitting {
                            ensure_widget();
                        }
                    }
                    TIMER_HOUSEKEEPING => {
                        ensure_widget();
                        if app().drag.is_none() {
                            place(false);
                        }
                        refresh(false);
                    }
                    _ => {}
                }
                0
            }
            WM_SETTINGCHANGE | WM_DISPLAYCHANGE => {
                // Theme switch (light/dark) or display change
                let a = app();
                a.theme = load_theme();
                style_tooltip(a.tooltip, a.theme.light);
    style_menus(a.theme.light);
                place(true);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---- folder watcher -----------------------------------------------------------------------

/// Posts WM_APP_FILE_CHANGED to the owner window whenever a file in ~/.claude is written or
/// renamed (the status line writes a temp file and renames it to usage.json).
fn start_watcher(dir: PathBuf, owner: HWND) {
    let owner = owner as isize;
    std::thread::spawn(move || unsafe {
        let d = wide(&dir.to_string_lossy());
        let h = FindFirstChangeNotificationW(d.as_ptr(), 0, FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME);
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        while WaitForSingleObject(h, INFINITE) == WAIT_OBJECT_0 {
            PostMessageW(owner as HWND, WM_APP_FILE_CHANGED, 0, 0);
            if FindNextChangeNotification(h) == 0 {
                break;
            }
        }
    });
}

// ---- main ---------------------------------------------------------------------------------

fn main() {
    unsafe {
        // Single instance (same name as the old PowerShell widget, so they never run together)
        CreateMutexW(null(), 1, wide(r"Local\ClaudeUsageWidget").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
        InitCommonControlsEx(&icc);
        let mut token = 0usize;
        let input = gp::GdiplusStartupInput { GdiplusVersion: 1, ..zeroed() };
        gp::GdiplusStartup(&mut token, &input, null_mut());

        let hinst = GetModuleHandleW(null()) as HINSTANCE;
        let mut wc: WNDCLASSW = zeroed();
        wc.hInstance = hinst;
        wc.lpfnWndProc = Some(owner_proc);
        let owner_class = wide("ClaudeUsageOwner");
        wc.lpszClassName = owner_class.as_ptr();
        RegisterClassW(&wc);
        wc.lpfnWndProc = Some(widget_proc);
        wc.hCursor = LoadCursorW(null_mut(), IDC_SIZEWE);
        let widget_class = wide("ClaudeUsageWidget");
        wc.lpszClassName = widget_class.as_ptr();
        RegisterClassW(&wc);

        // Hidden top-level window (not message-only, so it receives the TaskbarCreated broadcast)
        let owner = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            owner_class.as_ptr(),
            wide("Claude usage widget").as_ptr(),
            WS_POPUP,
            0, 0, 0, 0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        TASKBAR_CREATED = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());

        let dir = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".claude");
        let pos_file = dir.join("usage-widget.json");
        APP = Some(App {
            hinst,
            owner,
            widget: null_mut(),
            tooltip: null_mut(),
            taskbar: null_mut(),
            data_file: dir.join("usage.json"),
            offset: load_offset(&pos_file),
            pos_file,
            raw: String::new(),
            usage: Value::Null,
            state: String::new(),
            scale: 1.0,
            bounds: (0, 0, 0, 0),
            drag: None,
            theme: load_theme(),
            quitting: false,
            tokens: [None, None],
        });
        load_usage();
        count_tokens();
        app().state = draw_state(&app().usage);

        if !create_widget() {
            SetTimer(owner, TIMER_RECREATE, 2000, None); // taskbar not up yet (early at login)
        }
        SetTimer(owner, TIMER_HOUSEKEEPING, 15_000, None);
        start_watcher(dir, owner);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        gp::GdiplusShutdown(token);
    }
}
