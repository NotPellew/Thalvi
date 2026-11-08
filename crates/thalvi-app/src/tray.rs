//! The tray icon: one drawn shape per state, a title without dictated text, and the menu texts.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Wry};
use thalvi_desktop::capture::CaptureRoute;

use crate::status::Status;

const SIZE: u8 = 32;

/// Samples per axis and pixel; more samples give smoother edges.
const SAMPLES: u8 = 4;

pub const TOGGLE: &str = "toggle";
pub const CANCEL: &str = "cancel";
pub const COPY_ERROR: &str = "copy-error";
pub const LAST_DICTATION: &str = "last-dictation";
pub const SETTINGS: &str = "settings";
pub const QUIT: &str = "quit";

/// The six looks of the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Paused,
    Listening,
    /// The wake word was heard or words are being taken.
    Capturing,
    Pending,
    Working,
    Error,
}

impl Icon {
    #[must_use]
    pub fn of(status: &Status) -> Self {
        match status {
            Status::Paused => Self::Paused,
            Status::Listening | Status::Done { .. } => Self::Listening,
            Status::Starting { .. } | Status::Capturing { .. } => Self::Capturing,
            Status::Pending { .. } => Self::Pending,
            Status::Working { .. } | Status::Loading { .. } => Self::Working,
            Status::Failed { .. } => Self::Error,
        }
    }

    fn image(self) -> Image<'static> {
        Image::new_owned(self.pixels(), u32::from(SIZE), u32::from(SIZE))
    }

    /// 32x32 RGBA pixels.
    fn pixels(self) -> Vec<u8> {
        let (shape, color): (fn(f32, f32) -> bool, [u8; 3]) = match self {
            Self::Paused => (paused, [160, 160, 172]),
            Self::Listening => (listening, [34, 197, 94]),
            Self::Capturing => (capturing, [239, 68, 68]),
            Self::Pending => (pending, [245, 158, 11]),
            Self::Working => (working, [59, 130, 246]),
            Self::Error => (error, [249, 115, 22]),
        };
        let mut rgba = Vec::with_capacity(usize::from(SIZE) * usize::from(SIZE) * 4);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let mut covered = 0_u32;
                for sy in 0..SAMPLES {
                    for sx in 0..SAMPLES {
                        let px = f32::from(x) + (f32::from(sx) + 0.5) / f32::from(SAMPLES);
                        let py = f32::from(y) + (f32::from(sy) + 0.5) / f32::from(SAMPLES);
                        covered += u32::from(shape(px, py));
                    }
                }
                let alpha = covered * 255 / u32::from(SAMPLES * SAMPLES);
                rgba.extend_from_slice(&[
                    color[0],
                    color[1],
                    color[2],
                    u8::try_from(alpha).unwrap_or(u8::MAX),
                ]);
            }
        }
        rgba
    }
}

fn distance(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    (x - cx).hypot(y - cy)
}

fn paused(x: f32, y: f32) -> bool {
    (7.0..=25.0).contains(&y) && ((9.0..=14.0).contains(&x) || (18.0..=23.0).contains(&x))
}

fn listening(x: f32, y: f32) -> bool {
    (8.0..=12.0).contains(&distance(x, y, 16.0, 16.0))
}

fn capturing(x: f32, y: f32) -> bool {
    distance(x, y, 16.0, 16.0) <= 11.0
}

fn pending(x: f32, y: f32) -> bool {
    (7.0..=25.0).contains(&x) && (7.0..=25.0).contains(&y)
}

fn working(x: f32, y: f32) -> bool {
    [6.5, 16.0, 25.5]
        .iter()
        .any(|cx| distance(x, y, *cx, 16.0) <= 3.4)
}

/// A triangle with an exclamation mark cut out of it.
fn error(x: f32, y: f32) -> bool {
    let inside = (4.0..=27.0).contains(&y) && (x - 16.0).abs() <= (y - 4.0) * 0.55;
    let mark = ((15.0..=17.0).contains(&x) && (12.0..=20.0).contains(&y))
        || distance(x, y, 16.0, 23.2) <= 1.5;
    inside && !mark
}

fn seconds(milliseconds: u64) -> String {
    if milliseconds.is_multiple_of(1000) {
        (milliseconds / 1000).to_string()
    } else {
        format!("{}.{}", milliseconds / 1000, milliseconds % 1000 / 100)
    }
}

/// The state in words, with the pending pause but never what was dictated or any notice.
#[must_use]
pub fn title(status: &Status) -> String {
    let state = match status {
        Status::Paused => "paused".to_owned(),
        Status::Listening | Status::Done { .. } => "listening".to_owned(),
        Status::Starting { .. } | Status::Capturing { .. } => "capturing".to_owned(),
        Status::Pending {
            route, pause_ms, ..
        } => format!(
            "{} pending, {} s",
            match route {
                CaptureRoute::Paste => "paste",
                CaptureRoute::Submit => "submit",
                CaptureRoute::Task => "task",
            },
            seconds(*pause_ms)
        ),
        Status::Working { .. } => "working".to_owned(),
        Status::Loading { .. } => "starting".to_owned(),
        Status::Failed { .. } => "something went wrong".to_owned(),
    };
    format!("Thalvi: {state}")
}

/// The next action follows listening intent, independently of a failure notice.
#[must_use]
pub fn toggle_text(paused: bool) -> &'static str {
    if paused {
        "Resume listening"
    } else {
        "Pause listening"
    }
}

/// Whether there is a capture to cancel: until the delivery starts.
#[must_use]
pub fn can_cancel(status: &Status) -> bool {
    matches!(
        status,
        Status::Starting { .. }
            | Status::Capturing { .. }
            | Status::Pending { .. }
            | Status::Working {
                cancellable: true,
                ..
            }
    )
}

/// What the "copy last error" menu item offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyItem {
    /// Nothing has failed yet.
    NoError,
    Ready,
    /// The last copy failed, for this reason; clicking tries again.
    Failed(String),
}

impl CopyItem {
    fn text(&self) -> String {
        match self {
            Self::NoError => "No error to copy".to_owned(),
            Self::Ready => "Copy last error".to_owned(),
            Self::Failed(reason) => format!("Could not copy: {reason}"),
        }
    }

    fn enabled(&self) -> bool {
        !matches!(self, Self::NoError)
    }
}

/// The tray icon and the menu items that follow the status.
pub struct Tray {
    icon: tauri::tray::TrayIcon,
    /// The state in words; Linux trays show no tooltip, but they show menus.
    header: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    cancel: MenuItem<Wry>,
    copy_error: MenuItem<Wry>,
}

impl Tray {
    /// # Errors
    /// Fails when the tray icon or its menu cannot be created.
    pub fn new(app: &AppHandle, status: &Status, paused: bool) -> tauri::Result<Self> {
        let header = MenuItem::with_id(app, "state", title(status), false, None::<&str>)?;
        let toggle = MenuItem::with_id(app, TOGGLE, toggle_text(paused), true, None::<&str>)?;
        let cancel = MenuItem::with_id(
            app,
            CANCEL,
            "Cancel capture",
            can_cancel(status),
            None::<&str>,
        )?;
        let copy_error = MenuItem::with_id(
            app,
            COPY_ERROR,
            CopyItem::NoError.text(),
            CopyItem::NoError.enabled(),
            None::<&str>,
        )?;
        let last = MenuItem::with_id(app, LAST_DICTATION, "Last dictation…", true, None::<&str>)?;
        let settings = MenuItem::with_id(app, SETTINGS, "Settings…", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, QUIT, "Quit", true, None::<&str>)?;
        let menu = Menu::with_items(
            app,
            &[
                &header,
                &toggle,
                &cancel,
                &copy_error,
                &last,
                &settings,
                &quit,
            ],
        )?;
        let icon = TrayIconBuilder::with_id("thalvi")
            .icon(Image::new_owned(
                Icon::of(status).pixels(),
                u32::from(SIZE),
                u32::from(SIZE),
            ))
            .tooltip(title(status))
            .menu(&menu)
            .on_menu_event(|app, event| crate::app::on_menu(app, event.id().as_ref()))
            .build(app)?;
        Ok(Self {
            icon,
            header,
            toggle,
            cancel,
            copy_error,
        })
    }

    pub fn listening(&self, paused: bool) {
        let _ = self.toggle.set_text(toggle_text(paused));
    }

    pub fn show(&self, status: &Status) {
        let _ = self.icon.set_icon(Some(Icon::of(status).image()));
        let _ = self.icon.set_tooltip(Some(title(status)));
        let _ = self.header.set_text(title(status));
        let _ = self.cancel.set_enabled(can_cancel(status));
        if matches!(status, Status::Failed { .. }) {
            self.copy_item(&CopyItem::Ready);
        }
    }

    pub fn copy_item(&self, item: &CopyItem) {
        let _ = self.copy_error.set_text(item.text());
        let _ = self.copy_error.set_enabled(item.enabled());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FailureKind;

    fn kitty() -> String {
        "kitty".to_owned()
    }

    fn pending_status(route: CaptureRoute, pause_ms: u64) -> Status {
        Status::Pending {
            target: Some(kitty()),
            route,
            text: "secret words".to_owned(),
            waiting_for_quiet: false,
            pause_ms,
            copies: false,
        }
    }

    fn all() -> Vec<(Status, Icon, &'static str)> {
        vec![
            (Status::Paused, Icon::Paused, "Thalvi: paused"),
            (Status::Listening, Icon::Listening, "Thalvi: listening"),
            (
                Status::Starting {
                    target: Some(kitty()),
                },
                Icon::Capturing,
                "Thalvi: capturing",
            ),
            (
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "secret words".to_owned(),
                    partial: Some("more".to_owned()),
                    notice: None,
                },
                Icon::Capturing,
                "Thalvi: capturing",
            ),
            (
                pending_status(CaptureRoute::Submit, 2500),
                Icon::Pending,
                "Thalvi: submit pending, 2.5 s",
            ),
            (
                pending_status(CaptureRoute::Paste, 700),
                Icon::Pending,
                "Thalvi: paste pending, 0.7 s",
            ),
            (
                pending_status(CaptureRoute::Task, 3000),
                Icon::Pending,
                "Thalvi: task pending, 3 s",
            ),
            (
                Status::Working {
                    target: Some(kitty()),
                    route: CaptureRoute::Task,
                    cancellable: false,
                },
                Icon::Working,
                "Thalvi: working",
            ),
            (
                Status::Working {
                    target: Some(kitty()),
                    route: CaptureRoute::Task,
                    cancellable: true,
                },
                Icon::Working,
                "Thalvi: working",
            ),
            (
                Status::Done {
                    target: Some(kitty()),
                    route: CaptureRoute::Paste,
                    notice: "Delivered secret words.".to_owned(),
                },
                Icon::Listening,
                "Thalvi: listening",
            ),
            (
                Status::Failed {
                    kind: FailureKind::Other,
                    notice: "Your text follows: secret words".to_owned(),
                    headline: "Failed",
                    causes: Vec::new(),
                },
                Icon::Error,
                "Thalvi: something went wrong",
            ),
            (
                Status::Loading {
                    notice: "Loading the speech model.".to_owned(),
                    downloading: false,
                },
                Icon::Working,
                "Thalvi: starting",
            ),
        ]
    }

    #[test]
    fn each_state_has_an_icon_and_a_title_without_dictated_text() {
        for (status, icon, title_text) in all() {
            assert_eq!(Icon::of(&status), icon, "{status:?}");
            assert_eq!(title(&status), title_text, "{status:?}");
            assert!(!title(&status).contains("secret"), "{status:?}");
        }
    }

    #[test]
    fn the_six_icons_are_distinct_shapes_of_the_right_size() {
        let icons = [
            Icon::Paused,
            Icon::Listening,
            Icon::Capturing,
            Icon::Pending,
            Icon::Working,
            Icon::Error,
        ];
        let shapes: Vec<Vec<bool>> = icons
            .iter()
            .map(|icon| {
                let pixels = icon.pixels();
                assert_eq!(
                    pixels.len(),
                    usize::from(SIZE) * usize::from(SIZE) * 4,
                    "{icon:?}"
                );
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|pixel| pixel[3] > 0)
                    .collect()
            })
            .collect();
        for (at, shape) in shapes.iter().enumerate() {
            assert!(shape.iter().any(|&on| on), "{:?} is blank", icons[at]);
            for (other_at, other) in shapes.iter().enumerate().skip(at + 1) {
                assert_ne!(shape, other, "{:?} and {:?}", icons[at], icons[other_at]);
            }
        }
    }

    #[test]
    fn the_menu_offers_the_opposite_of_the_current_listening_intent() {
        for (paused, expected) in [(true, "Resume listening"), (false, "Pause listening")] {
            assert_eq!(toggle_text(paused), expected);
        }
    }

    #[test]
    fn a_capture_can_be_cancelled_until_the_delivery_starts() {
        let working = |cancellable| Status::Working {
            target: Some(kitty()),
            route: CaptureRoute::Task,
            cancellable,
        };
        for (status, cancellable) in [
            (Status::Paused, false),
            (Status::Listening, false),
            (
                Status::Starting {
                    target: Some(kitty()),
                },
                true,
            ),
            (pending_status(CaptureRoute::Paste, 700), true),
            (working(true), true),
            (working(false), false),
            (
                Status::Failed {
                    kind: FailureKind::Other,
                    notice: String::new(),
                    headline: "Failed",
                    causes: Vec::new(),
                },
                false,
            ),
        ] {
            assert_eq!(can_cancel(&status), cancellable, "{status:?}");
        }
    }

    #[test]
    fn the_copy_item_offers_the_last_error_when_there_is_one_and_says_why_a_copy_failed() {
        for (item, text, enabled) in [
            (CopyItem::NoError, "No error to copy", false),
            (CopyItem::Ready, "Copy last error", true),
            (
                CopyItem::Failed("wl-copy is not installed.".to_owned()),
                "Could not copy: wl-copy is not installed.",
                true,
            ),
        ] {
            assert_eq!(
                (item.text().as_str(), item.enabled()),
                (text, enabled),
                "{item:?}"
            );
        }
    }
}
