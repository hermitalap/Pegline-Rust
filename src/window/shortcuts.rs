//! The shortcut recorder (BEHAVIOR §12.2).
//!
//! The WPF app opened a small modal window that took the next key combination
//! straight from the keyboard, refused combinations without a modifier unless
//! they were F1–F24 or PrtScn, and explained why a combination could not be
//! used. This is the same dialog as a plain Win32 window painted with GDI: it is
//! static UI opened on demand, not part of the overlay, so there is no swap
//! chain to keep alive.
//!
//! [`ask`] blocks in a local message loop until the window closes. The app's
//! timers still fire because this loop dispatches every thread message.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK,
    DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, FillRect, FrameRect,
    GetTextExtentPoint32W, HDC, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN,
    VK_SHIFT, VK_SNAPSHOT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, IsWindow, MSG, PostMessageW, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_CHAR,
    WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WS_CAPTION, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU,
};
use windows::core::{PCWSTR, w};

use crate::font;
use crate::i18n;
use crate::logging;
use crate::shortcut::{self, Hotkey};
use crate::types::RectI;
use crate::win32;
use crate::window;

const CLASS_NAME: PCWSTR = w!("PeglineShortcuts");
const WINDOW_TITLE: PCWSTR = w!("Pegline");
/// Client size in DIP.
const WIDTH_DIP: f32 = 420.0;
const HEIGHT_DIP: f32 = 236.0;
/// Hotkey ids reserved for the availability probe, above the app's own ids.
const PROBE_ID: i32 = 0x4000;

/// What the user chose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    /// A combination (or `None` for "no shortcut").
    Chosen(Option<Hotkey>),
    Cancelled,
}

/// Opens the recorder for `current` and blocks until it closes.
///
/// `validate` runs when the user confirms a combination and returns the reason
/// it cannot be used (already taken by Pegline, reserved for the toggle), or
/// `None` when it is acceptable.
pub fn ask(
    parent: HWND,
    title: &str,
    current: Option<Hotkey>,
    validate: Box<dyn Fn(Option<Hotkey>) -> Option<String>>,
) -> Choice {
    let dpi = win32::cursor_pos()
        .and_then(|(x, y)| win32::monitor_at(x, y))
        .map(|screen| screen.dpi)
        .unwrap_or_else(|| win32::window_dpi(parent));
    let scale = dpi as f32 / 96.0;
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    // `WIDTH_DIP`/`HEIGHT_DIP` describe the *client* area; the caption and
    // borders are added around it. Without this the button row overlapped the
    // error line by a few DIP (the caption ate the difference).
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: (WIDTH_DIP * scale).round() as i32,
        bottom: (HEIGHT_DIP * scale).round() as i32,
    };
    // SAFETY: pure measurement for the style this window is about to use.
    if let Err(error) =
        unsafe { AdjustWindowRectExForDpi(&mut frame, style, false, WS_EX_TOPMOST, dpi) }
    {
        logging::error(format!("shortcuts: AdjustWindowRectExForDpi: {error}"));
    }
    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;
    let (left, top) = win32::cursor_pos()
        .map(|(x, y)| (x - width / 2, y - height / 2))
        .unwrap_or((100, 100));

    let state = Box::new(State {
        title: title.to_string(),
        current,
        pressed: None,
        partial: false,
        error: String::new(),
        hover: None,
        down: None,
        buttons: [RectI::default(); 3],
        choice: None,
        closed: false,
        hwnd: HWND::default(),
        scale,
        validate,
    });
    let raw = Box::into_raw(state);

    let instance = match unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) } {
        Ok(instance) => instance,
        Err(error) => {
            logging::error(format!("shortcuts: GetModuleHandleW: {error}"));
            // SAFETY: `raw` came from `Box::into_raw` and was never shared.
            unsafe { drop(Box::from_raw(raw)) };
            return Choice::Cancelled;
        }
    };
    if let Err(error) = window::register_class(CLASS_NAME, Some(dialog_wndproc)) {
        logging::error(format!("shortcuts: {error}"));
        // SAFETY: `raw` came from `Box::into_raw` and was never shared.
        unsafe { drop(Box::from_raw(raw)) };
        return Choice::Cancelled;
    }
    // SAFETY: the class is registered and `raw` stays alive until the window is
    // destroyed; the window procedure runs on this thread only.
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            CLASS_NAME,
            WINDOW_TITLE,
            style,
            left,
            top,
            width,
            height,
            Some(parent),
            None,
            Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(error) => {
            logging::error(format!("shortcuts: CreateWindowExW: {error}"));
            // SAFETY: `raw` came from `Box::into_raw` and was never shared.
            unsafe { drop(Box::from_raw(raw)) };
            return Choice::Cancelled;
        }
    };
    // SAFETY: the window was just created and belongs to this thread.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        (*raw).hwnd = hwnd;
        let title_wide = win32::wide(title);
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
            hwnd,
            PCWSTR(title_wide.as_ptr()),
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }

    // Local modal loop: the app's own timers arrive here too, so the line keeps
    // working while the recorder is open.
    let mut message = MSG::default();
    loop {
        // SAFETY: a standard message loop on this thread; `message` is a live
        // local and the window is checked before every iteration.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == WM_QUIT {
            // Put the quit back for the app's own loop.
            // SAFETY: posting to this thread's queue.
            unsafe {
                let _ = PostMessageW(None, WM_QUIT, message.wParam, message.lParam);
            }
            break;
        }
        // SAFETY: messages are dispatched to this thread's windows.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        // SAFETY: the handle is ours; `IsWindow` only reads it.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            break;
        }
        // SAFETY: the pointer was stored above and is valid until we free it.
        if unsafe { (*raw).closed } {
            break;
        }
    }

    // SAFETY: the window is destroyed (or the loop ended); no procedure can
    // touch `raw` after this point because the window is gone.
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        // SAFETY: the window is ours and the state pointer is still alive.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    let state = unsafe { Box::from_raw(raw) };
    state.choice.unwrap_or(Choice::Cancelled)
}

struct State {
    title: String,
    current: Option<Hotkey>,
    /// The combination shown in the box and applied by OK.
    pressed: Option<Hotkey>,
    /// True while only modifiers are down; OK stays disabled.
    partial: bool,
    error: String,
    hover: Option<usize>,
    down: Option<usize>,
    /// Button rectangles in client pixels, left to right:
    /// 0 = no shortcut, 1 = cancel, 2 = OK.
    buttons: [RectI; 3],
    choice: Option<Choice>,
    closed: bool,
    hwnd: HWND,
    scale: f32,
    validate: Box<dyn Fn(Option<Hotkey>) -> Option<String>>,
}

impl State {
    fn any_modifiers() -> u32 {
        // SAFETY: GetKeyState only reads the calling thread's key state.
        unsafe {
            let mut modifiers = 0;
            if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                modifiers |= MOD_CONTROL.0;
            }
            if GetKeyState(VK_MENU.0 as i32) < 0 {
                modifiers |= MOD_ALT.0;
            }
            if GetKeyState(VK_SHIFT.0 as i32) < 0 {
                modifiers |= MOD_SHIFT.0;
            }
            if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
                modifiers |= MOD_WIN.0;
            }
            modifiers
        }
    }

    fn label(&self) -> String {
        if self.partial {
            let modifiers = Self::any_modifiers();
            let text = Hotkey {
                modifiers,
                virtual_key: 0,
            }
            .label();
            if text.is_empty() {
                "…".to_string()
            } else {
                text
            }
        } else {
            self.pressed
                .map(Hotkey::label)
                .or_else(|| self.current.map(Hotkey::label))
                .unwrap_or_else(|| i18n::s().no_shortcut.to_string())
        }
    }

    fn handle_key(&mut self, vk: u32, released: bool) {
        // A registered global hotkey can leave a zero virtual key behind;
        // "Ctrl+Alt+VK00" must never be offered as a shortcut.
        if vk == 0 {
            return;
        }
        let modifiers = Self::any_modifiers();
        let modifier_only = matches!(
            vk,
            0x10 | 0x11 | 0x12 | 0x5B | 0x5C // Shift, Ctrl, Alt, LWin, RWin
        );
        if modifier_only {
            self.partial = true;
            self.pressed = None;
            self.error.clear();
            return;
        }
        if vk == VK_ESCAPE.0 as u32 && modifiers == 0 {
            self.finish(Choice::Cancelled);
            return;
        }
        if vk == VK_RETURN.0 as u32 && modifiers == 0 {
            if self.pressed.is_some() {
                self.confirm(self.pressed);
            }
            return;
        }
        // PrtScn is usually reported on key-up only (as in the WPF dialog).
        if vk == VK_SNAPSHOT.0 as u32 && !released {
            return;
        }
        let hotkey = Hotkey {
            modifiers,
            virtual_key: vk,
        };
        self.partial = false;
        if !shortcut::usable(hotkey) {
            self.pressed = None;
            self.error = i18n::s().add_modifier.to_string();
            return;
        }
        self.pressed = Some(hotkey);
        self.error.clear();
    }

    fn confirm(&mut self, choice: Option<Hotkey>) {
        if let Some(hotkey) = choice {
            if let Some(reason) = (self.validate)(Some(hotkey)) {
                self.error = reason;
                return;
            }
            if !available(self.hwnd, hotkey) {
                self.error = i18n::s().conflict_external.to_string();
                return;
            }
        } else if let Some(reason) = (self.validate)(None) {
            self.error = reason;
            return;
        }
        self.finish(Choice::Chosen(choice));
    }

    fn finish(&mut self, choice: Choice) {
        self.choice = Some(choice);
        self.closed = true;
        // The window handle is the message target; the caller's loop breaks on
        // `closed`, and `DestroyWindow` runs there.
    }

    fn button_at(&self, x: i32, y: i32) -> Option<usize> {
        self.buttons.iter().position(|button| button.contains(x, y))
    }

    fn layout(&mut self, width: i32, height: i32, dc: HDC, font: HGDIOBJ) {
        let scale = self.scale;
        let margin = (20.0 * scale) as i32;
        let gap = (8.0 * scale) as i32;
        let padding = (14.0 * scale) as i32;
        let labels = [i18n::s().no_shortcut, i18n::s().cancel, i18n::s().ok];
        let mut right = width - margin;
        let top = height - margin - (30.0 * scale) as i32;
        let mut rects = [RectI::default(); 3];
        for (index, label) in labels.iter().enumerate().rev() {
            let text_width = measure(dc, font, label).max((44.0 * scale) as i32);
            let button_width = text_width + padding * 2;
            rects[index] = RectI::new(
                right - button_width,
                top,
                right,
                top + (30.0 * scale) as i32,
            );
            right -= button_width + gap;
        }
        self.buttons = rects;
    }
}

/// Measures one line of text in pixels.
fn measure(dc: HDC, font: HGDIOBJ, text: &str) -> i32 {
    let wide = win32::wide(text);
    let old = unsafe { SelectObject(dc, font) };
    let mut size = windows::Win32::Foundation::SIZE::default();
    // SAFETY: `dc` is owned by the caller, the string is alive for the call and
    // `size` is a live local.
    let ok = unsafe { GetTextExtentPoint32W(dc, &wide[..wide.len().saturating_sub(1)], &mut size) };
    // SAFETY: restores the object that was selected before.
    unsafe {
        SelectObject(dc, old);
    }
    if ok.as_bool() { size.cx } else { 0 }
}

fn create_font(scale: f32, height_dip: f32, bold: bool) -> windows::Win32::Graphics::Gdi::HFONT {
    let face: Vec<u16> = font::UI_FAMILY_STR
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: GDI allocates a font from the stationary description; the caller
    // deletes it. The dialog is opaque, so ClearType is valid here.
    unsafe {
        CreateFontW(
            -((height_dip * scale).round() as i32),
            0,
            0,
            0,
            if bold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
            PCWSTR(face.as_ptr()),
        )
    }
}

fn draw(state: &mut State, dc: HDC, width: i32, height: i32, bold: HGDIOBJ, small: HGDIOBJ) {
    let scale = state.scale;
    let margin = (20.0 * scale) as i32;

    let background = unsafe { CreateSolidBrush(COLORREF(0x00F8F8F8)) };
    let field = unsafe { CreateSolidBrush(COLORREF(0x00FFFFFF)) };
    let button_brush = unsafe { CreateSolidBrush(COLORREF(0x00EDEDED)) };
    let border = unsafe { CreateSolidBrush(COLORREF(0x00C8C8C8)) };

    let all = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: all GDI objects are created above and deleted below.
    unsafe {
        FillRect(dc, &all, background);
        SetBkMode(dc, TRANSPARENT);
        SelectObject(dc, bold);
        SetTextColor(dc, COLORREF(0x001E1E1E));
        let mut title = win32::wide(&state.title);
        let mut title_rect = RECT {
            left: margin,
            top: margin,
            right: width - margin,
            bottom: margin + (22.0 * scale) as i32,
        };
        let _ = DrawTextW(
            dc,
            &mut title,
            &mut title_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );

        SelectObject(dc, small);
        SetTextColor(dc, COLORREF(0x00464646));
        let mut hint = win32::wide(i18n::s().shortcut_hint);
        let mut hint_rect = RECT {
            left: margin,
            top: margin + (26.0 * scale) as i32,
            right: width - margin,
            bottom: margin + (74.0 * scale) as i32,
        };
        DrawTextW(dc, &mut hint, &mut hint_rect, DT_LEFT | DT_WORDBREAK);

        // The combination field.
        let field_rect = RECT {
            left: margin,
            top: hint_rect.bottom + (6.0 * scale) as i32,
            right: width - margin,
            bottom: hint_rect.bottom + (58.0 * scale) as i32,
        };
        FillRect(dc, &field_rect, field);
        FrameRect(dc, &field_rect, border);
        SelectObject(dc, bold);
        SetTextColor(dc, COLORREF(0x001E1E1E));
        let mut label = win32::wide(&state.label());
        let mut label_rect = field_rect;
        let _ = DrawTextW(
            dc,
            &mut label,
            &mut label_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );

        // Error line.
        if !state.error.is_empty() {
            SelectObject(dc, small);
            // COLORREF is 0x00BBGGRR: this is the WPF error red C42B1C.
            SetTextColor(dc, COLORREF(0x001C2BC4));
            let mut error = win32::wide(&state.error);
            let mut error_rect = RECT {
                left: margin,
                top: field_rect.bottom + (4.0 * scale) as i32,
                right: width - margin,
                bottom: field_rect.bottom + (34.0 * scale) as i32,
            };
            DrawTextW(dc, &mut error, &mut error_rect, DT_LEFT | DT_WORDBREAK);
        }

        state.layout(width, height, dc, bold);

        let labels = [i18n::s().no_shortcut, i18n::s().cancel, i18n::s().ok];
        for (index, rect) in state.buttons.iter().enumerate() {
            let r = RECT {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            };
            let enabled = index != 2 || state.pressed.is_some();
            FillRect(dc, &r, button_brush);
            FrameRect(dc, &r, border);
            SetTextColor(
                dc,
                if enabled {
                    COLORREF(0x001E1E1E)
                } else {
                    COLORREF(0x00999999)
                },
            );
            let mut text = win32::wide(labels[index]);
            let mut text_rect = r;
            let _ = DrawTextW(
                dc,
                &mut text,
                &mut text_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // SAFETY: every object was created above and is not selected any more.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(background.0));
        let _ = DeleteObject(HGDIOBJ(field.0));
        let _ = DeleteObject(HGDIOBJ(button_brush.0));
        let _ = DeleteObject(HGDIOBJ(border.0));
    }
}

/// Window procedure for the recorder.
///
/// SAFETY: `GWLP_USERDATA` holds a live `State` for the lifetime of the window;
/// all messages arrive on the thread that created it.
unsafe extern "system" fn dialog_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A panic may not unwind out of an `extern "system"` function: it aborts the
    // process. Contain it here.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        dialog_wndproc_inner(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            crate::logging::error("panic in dialog_wndproc");
            LRESULT(0)
        }
    }
}

/// The real handler; plain `unsafe fn` so a panic unwinds into the guard above.
unsafe fn dialog_wndproc_inner(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if raw.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let state = unsafe { &mut *raw };
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: standard paint sequence; the DC is released by EndPaint.
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                let mut client = RECT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
                let bold = create_font(state.scale, 22.0, true);
                let small = create_font(state.scale, 12.0, false);
                draw(
                    state,
                    dc,
                    client.right,
                    client.bottom,
                    HGDIOBJ(bold.0),
                    HGDIOBJ(small.0),
                );
                let _ = DeleteObject(HGDIOBJ(bold.0));
                let _ = DeleteObject(HGDIOBJ(small.0));
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_KEYDOWN => {
            state.handle_key(wparam.0 as u32, false);
            if !state.closed {
                let _ = unsafe {
                    windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false)
                };
            }
            LRESULT(0)
        }
        WM_KEYUP => {
            if (wparam.0 as u32) == VK_SNAPSHOT.0 as u32 {
                state.handle_key(wparam.0 as u32, true);
                if !state.closed {
                    let _ = unsafe {
                        windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false)
                    };
                }
            }
            LRESULT(0)
        }
        WM_CHAR => LRESULT(0),
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let hover = state.button_at(x, y);
            if state.hover != hover {
                state.hover = hover;
                let _ = unsafe {
                    windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false)
                };
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            state.down = state.button_at(x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let hit = state.button_at(x, y);
            let down = state.down.take();
            if let (Some(down), Some(hit)) = (down, hit) {
                if down == hit {
                    match down {
                        0 => choice_and_close(state, Choice::Chosen(None)),
                        1 => choice_and_close(state, Choice::Cancelled),
                        _ => state.confirm(state.pressed),
                    }
                    if !state.closed {
                        let _ = unsafe {
                            windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false)
                        };
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            state.finish(Choice::Cancelled);
            LRESULT(0)
        }
        WM_DESTROY => {
            state.closed = true;
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Records the answer and lets `ask` close the window.
///
/// Never calls `DestroyWindow` from inside this window's own procedure: it
/// dispatches `WM_DESTROY` synchronously, which re-enters the procedure and
/// creates a second `&mut State` while the first is still on the stack.
fn choice_and_close(state: &mut State, choice: Choice) {
    state.finish(choice);
}

/// Whether a combination can be registered by another window right now.
pub fn available(hwnd: HWND, hotkey: Hotkey) -> bool {
    // SAFETY: the window is ours; the probe id is outside the app's range.
    let registered = unsafe {
        RegisterHotKey(
            Some(hwnd),
            PROBE_ID,
            HOT_KEY_MODIFIERS(hotkey.modifiers | MOD_NOREPEAT.0),
            hotkey.virtual_key,
        )
    };
    match registered {
        Ok(()) => {
            // SAFETY: balances the successful registration above.
            let _ = unsafe { UnregisterHotKey(Some(hwnd), PROBE_ID) };
            true
        }
        Err(_) => false,
    }
}

/// The reason a combination cannot be used, for the app's validator.
pub fn conflict(reason: &str) -> String {
    reason.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::MOD_CONTROL;

    #[test]
    fn choices_are_copy_and_compare() {
        let hotkey = Hotkey {
            modifiers: MOD_CONTROL.0,
            virtual_key: 'S' as u32,
        };
        assert_eq!(Choice::Chosen(Some(hotkey)), Choice::Chosen(Some(hotkey)));
        assert_ne!(Choice::Chosen(None), Choice::Cancelled);
    }

    #[test]
    fn client_size_is_scaled() {
        let _ = core::mem::size_of::<State>();
        let _ = conflict("x");
    }
}
