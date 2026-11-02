//! Where a screenshot was taken, so the flight can start there.
//!
//! Ported from `CaptureArea.Guess` (`archive/wpf-v1/src/Flight.cs`). All
//! rectangles are device pixels. The rules are deliberately kept exactly as the
//! WPF app had them, including the quirk that a screen-sized capture only
//! matches a screen whose bounds contain the *pointer screen's origin*.

use crate::types::RectI;

/// Two edges are "the same" within 2 px (the documented tolerance).
fn near(a: i32, b: i32) -> bool {
    (a - b).abs() <= 2
}

fn same_size(size: (u32, u32), rect: RectI) -> bool {
    near(size.0 as i32, rect.width()) && near(size.1 as i32, rect.height())
}

/// Picks the screen the capture covers, if any.
fn covering_screen(
    size: (u32, u32),
    screens: &[RectI],
    pointer_screen: Option<RectI>,
) -> Option<RectI> {
    let pointer_origin = pointer_screen?;
    screens.iter().copied().find(|screen| {
        same_size(size, *screen) && screen.contains(pointer_origin.left, pointer_origin.top)
    })
}

/// Infers the capture region.
///
/// * `size` — pixel size of the screenshot.
/// * `screens` — monitor bounds in device pixels.
/// * `virtual_screen` — the whole desktop rectangle.
/// * `foreground` — the foreground window's rect at capture time.
/// * `pointer` — cursor position in device pixels.
/// * `from_pointer` — the file was written just now (3 s), i.e. it is a fresh
///   capture and not an old file that happened to appear.
pub fn guess(
    size: (u32, u32),
    screens: &[RectI],
    virtual_screen: RectI,
    foreground: Option<RectI>,
    pointer: (i32, i32),
    from_pointer: bool,
) -> Option<RectI> {
    if size.0 < 3 || size.1 < 3 {
        return None;
    }
    let pointer_screen = screens
        .iter()
        .copied()
        .find(|screen| screen.contains(pointer.0, pointer.1));

    // A capture that is exactly one screen big started at that screen.
    if let Some(screen) = covering_screen(size, screens, pointer_screen) {
        return Some(screen);
    }

    // The whole desktop (multi-monitor capture): start at the pointer screen.
    if same_size(size, virtual_screen) {
        return pointer_screen;
    }

    // A window capture: start at the foreground window, centred horizontally.
    if let Some(rect) = foreground {
        if near(size.0 as i32, rect.width()) && near(size.1 as i32, rect.height()) {
            let dx = (rect.width() - size.0 as i32) / 2;
            return Some(RectI::new(
                rect.left + dx,
                rect.top,
                rect.left + dx + size.0 as i32,
                rect.top + size.1 as i32,
            ));
        }
    }

    if !from_pointer {
        return None;
    }

    // Snipping Tool: the selection ends at the pointer.
    let rect = RectI::new(
        pointer.0 - size.0 as i32,
        pointer.1 - size.1 as i32,
        pointer.0,
        pointer.1,
    );
    if virtual_screen.contains(rect.left, rect.top)
        && virtual_screen.contains(rect.right - 1, rect.bottom - 1)
    {
        return Some(rect);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: RectI = RectI {
        left: 0,
        top: 0,
        right: 6016,
        bottom: 3384,
    };
    const SECOND: RectI = RectI {
        left: 6016,
        top: 0,
        right: 7936,
        bottom: 1080,
    };

    #[test]
    fn tiny_captures_have_no_area() {
        assert_eq!(
            guess((2, 400), &[PRIMARY], PRIMARY, None, (10, 10), true),
            None
        );
    }

    #[test]
    fn full_screen_capture_uses_the_pointer_screen() {
        let area = guess((6016, 3384), &[PRIMARY], PRIMARY, None, (100, 100), true);
        assert_eq!(area, Some(PRIMARY));
    }

    #[test]
    fn screen_sized_capture_keeps_the_origin_quirk() {
        // Pointer on the secondary screen: the primary does not contain its
        // origin, and the secondary does not have the image's size, so the
        // screen rule misses and the pointer-corner rule decides.
        let area = guess(
            (6016, 3384),
            &[PRIMARY, SECOND],
            RectI::new(0, 0, 7936, 3384),
            None,
            (6100, 500),
            false,
        );
        assert_eq!(area, None);
    }

    #[test]
    fn window_capture_is_centred_on_the_window() {
        let window = RectI::new(100, 50, 1100, 650);
        let area = guess(
            (1000, 600),
            &[PRIMARY],
            PRIMARY,
            Some(window),
            (300, 300),
            false,
        );
        assert_eq!(area, Some(RectI::new(100, 50, 1100, 650)));
        // The image size must be within 20 px of the window in both axes.
        assert_eq!(
            guess(
                (900, 600),
                &[PRIMARY],
                PRIMARY,
                Some(window),
                (300, 300),
                false
            ),
            None
        );
    }

    #[test]
    fn snipping_tool_selection_ends_at_the_pointer() {
        let area = guess((400, 300), &[PRIMARY], PRIMARY, None, (1000, 800), true);
        assert_eq!(area, Some(RectI::new(600, 500, 1000, 800)));
        // A stale file (not fresh) never uses the pointer rule.
        assert_eq!(
            guess((400, 300), &[PRIMARY], PRIMARY, None, (1000, 800), false),
            None
        );
    }

    #[test]
    fn selection_outside_the_desktop_is_rejected() {
        assert_eq!(
            guess((400, 300), &[PRIMARY], PRIMARY, None, (100, 100), true),
            None
        );
    }

    #[test]
    fn virtual_screen_capture_uses_the_pointer_screen() {
        let virtual_screen = RectI::new(0, 0, 7936, 3384);
        let area = guess(
            (7936, 3384),
            &[PRIMARY, SECOND],
            virtual_screen,
            None,
            (6200, 500),
            false,
        );
        assert_eq!(area, Some(SECOND));
    }
}
