//! The Windows overlay surface: a layered, topmost, non-activating window owned by the overlay's
//! thread, drawn with `UpdateLayeredWindow` from premultiplied pixels.

#![expect(
    unsafe_code,
    reason = "raw Win32 window calls; each block says why it is sound"
)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::time::Instant;

use ::windows::Win32::Foundation::{
    COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_TIMEOUT, WPARAM,
};
use ::windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, HDC, SelectObject,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, MA_NOACTIVATE, MSG,
    MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT, RegisterClassW,
    SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ShowWindow,
    SystemParametersInfoW, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WM_APP,
    WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use ::windows::core::{HSTRING, w};

use crate::host::{HostError, OVERLAY_TITLE, OverlayEvent, OverlayFrame};

/// Posted by an [`OverlayWaker`].
const WM_WAKE: u32 = WM_APP + 1;

thread_local! {
    /// What the window procedure saw, for `wait` to hand on.
    static EVENTS: RefCell<VecDeque<OverlayEvent>> = const { RefCell::new(VecDeque::new()) };
    /// The window's physical pixels per point, for turning a press into points.
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

fn failed(cause: impl std::fmt::Display) -> HostError {
    HostError::Unsupported(format!(
        "The status overlay could not open its window: {cause}."
    ))
}

/// Where the overlay goes on Windows: the top of the primary monitor's work area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayPlan;

impl OverlayPlan {
    /// Opens the hidden window, `width` × `height` points; the calling thread then owns it and
    /// must keep calling [`OverlaySurface::wait`], which runs its messages.
    ///
    /// # Errors
    /// Why Windows gave no window; the overlay then stays off.
    pub fn open(
        self,
        width: f32,
        height: f32,
        takes_clicks: bool,
    ) -> Result<OverlaySurface, HostError> {
        // SAFETY: changes only this thread's awareness, so its window gets real pixels.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // SAFETY: the handle of this program's own module.
        let instance = unsafe { GetModuleHandleW(None) }.map_err(failed)?;
        let class = w!("ThalviOverlay");
        let description = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // SAFETY: `description` outlives the call and names a static class name. A second
        // registration fails harmlessly and the first stays.
        unsafe { RegisterClassW(&raw const description) };
        let mut style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if !takes_clicks {
            style |= WS_EX_TRANSPARENT;
        }
        // SAFETY: a popup of the class registered above, owned by this thread.
        let window = unsafe {
            CreateWindowExW(
                style,
                class,
                &HSTRING::from(OVERLAY_TITLE),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(failed)?;
        // SAFETY: a window this thread just made.
        let dpi = unsafe { GetDpiForWindow(window) }.max(96);
        #[expect(clippy::cast_precision_loss, reason = "a DPI")]
        let scale = dpi as f32 / 96.0;
        SCALE.with(|cell| cell.set(scale));
        let mut area = RECT::default();
        // SAFETY: SPI_GETWORKAREA fills the RECT that `area` points to, which outlives the call.
        unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&raw mut area).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .map_err(failed)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the overlay's size in pixels"
        )]
        let size = SIZE {
            cx: (width * scale).ceil() as i32,
            cy: (height * scale).ceil() as i32,
        };
        let at = POINT {
            x: area.left + (area.right - area.left - size.cx) / 2,
            y: area.top,
        };
        let (dc, bits) = pixels(size)?;
        Ok(OverlaySurface {
            window,
            scale,
            at,
            size,
            dc,
            bits,
        })
    }
}

/// A memory DC with a top-down 32-bit DIB of `size` selected, and the DIB's pixels.
fn pixels(size: SIZE) -> Result<(HDC, *mut u8), HostError> {
    let header = BITMAPINFOHEADER {
        biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).map_err(failed)?,
        biWidth: size.cx,
        biHeight: -size.cy,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    // SAFETY: a memory DC compatible with the screen, kept for the window's life.
    let dc = unsafe { CreateCompatibleDC(None) };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` describes the DIB and outlives the call; `bits` receives its pixels.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(dc),
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    }
    .map_err(failed)?;
    // SAFETY: both handles were just made; the DIB stays selected for the window's life.
    unsafe { SelectObject(dc, bitmap.into()) };
    if bits.is_null() {
        return Err(failed("no pixels for the window"));
    }
    Ok((dc, bits.cast()))
}

/// Wakes the overlay's thread from any thread.
#[derive(Clone, Copy, Debug)]
pub struct OverlayWaker(isize);

impl OverlayWaker {
    pub fn wake(&self) {
        // SAFETY: posting to a window by handle; a closed window just drops the message.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.0 as *mut c_void)),
                WM_WAKE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

/// The status overlay's window: hidden until shown, never activated.
pub struct OverlaySurface {
    window: HWND,
    scale: f32,
    at: POINT,
    size: SIZE,
    dc: HDC,
    bits: *mut u8,
}

impl OverlaySurface {
    #[must_use]
    pub fn waker(&self) -> OverlayWaker {
        OverlayWaker(self.window.0 as isize)
    }

    /// Physical pixels per point; frames are drawn at this scale.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Shows `frame` on a window that was hidden.
    ///
    /// # Errors
    /// Why Windows did not take the frame.
    pub fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        self.present(frame)?;
        // SAFETY: this thread's own window; SW_SHOWNOACTIVATE leaves the focus where it is.
        let _ = unsafe { ShowWindow(self.window, SW_SHOWNOACTIVATE) };
        Ok(())
    }

    /// Replaces what the window shows.
    ///
    /// # Errors
    /// Why Windows did not take the frame.
    pub fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        let fits = i32::try_from(frame.width) == Ok(self.size.cx)
            && i32::try_from(frame.height) == Ok(self.size.cy);
        if !fits {
            return Err(failed("a frame of the wrong size"));
        }
        // SAFETY: `bits` holds the DIB's width × height × 4 bytes for the window's life, and
        // the frame has exactly that many.
        unsafe { std::ptr::copy_nonoverlapping(frame.bgra.as_ptr(), self.bits, frame.bgra.len()) };
        let blend = BLENDFUNCTION {
            BlendOp: u8::try_from(AC_SRC_OVER).unwrap_or(0),
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: u8::try_from(AC_SRC_ALPHA).unwrap_or(1),
        };
        let origin = POINT::default();
        // SAFETY: this thread's window and memory DC; every pointer outlives the call.
        unsafe {
            UpdateLayeredWindow(
                self.window,
                None,
                Some(&raw const self.at),
                Some(&raw const self.size),
                Some(self.dc),
                Some(&raw const origin),
                COLORREF(0),
                Some(&raw const blend),
                ULW_ALPHA,
            )
        }
        .map_err(failed)
    }

    pub fn hide(&mut self) {
        // SAFETY: this thread's own window.
        let _ = unsafe { ShowWindow(self.window, SW_HIDE) };
    }

    /// Runs the window's messages until a press, a wake or `deadline`; with no deadline, until
    /// one of the first two.
    pub fn wait(&mut self, deadline: Option<Instant>) -> OverlayEvent {
        loop {
            let mut message = MSG::default();
            // SAFETY: drains this thread's queue into its own window procedure.
            while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: a message just taken from this thread's queue.
                unsafe {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
            if let Some(event) = EVENTS.with(|events| events.borrow_mut().pop_front()) {
                return event;
            }
            let timeout = match deadline {
                None => u32::MAX,
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return OverlayEvent::Due;
                    }
                    u32::try_from(left.as_millis())
                        .unwrap_or(u32::MAX - 1)
                        .max(1)
                }
            };
            // SAFETY: waits on this thread's queue only.
            if unsafe { MsgWaitForMultipleObjects(None, false, timeout, QS_ALLINPUT) }
                == WAIT_TIMEOUT
                && deadline.is_some_and(|at| Instant::now() >= at)
            {
                return OverlayEvent::Due;
            }
        }
    }
}

unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(isize::try_from(MA_NOACTIVATE).unwrap_or_default()),
        WM_LBUTTONDOWN => {
            #[expect(clippy::cast_possible_truncation, reason = "the low and high words")]
            let (x, y) = (lparam.0 as i16, (lparam.0 >> 16) as i16);
            let scale = SCALE.with(Cell::get);
            push(OverlayEvent::Press {
                x: f32::from(x) / scale,
                y: f32::from(y) / scale,
            });
            LRESULT(0)
        }
        WM_WAKE => {
            push(OverlayEvent::Woken);
            LRESULT(0)
        }
        // SAFETY: the default handling of every other message for this window.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn push(event: OverlayEvent) {
    EVENTS.with(|events| events.borrow_mut().push_back(event));
}
