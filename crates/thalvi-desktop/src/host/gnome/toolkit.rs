//! GNOME's use of the app's own GTK (the X11 backend): the clipboard and the work area, all on the GTK main thread.

use std::sync::mpsc;
use std::time::Duration;

use gtk::glib::MainContext;
use gtk::{gdk, gdk::prelude::MonitorExt};

use crate::host::HostError;

/// How long a Host call waits for the GTK main thread before giving up.
const LIMIT: Duration = Duration::from_secs(2);

/// What GNOME needs from GTK, which only the main thread may use.
pub trait Toolkit: Send + Sync {
    /// Puts `text` on the CLIPBOARD selection.
    ///
    /// # Errors
    /// Why the clipboard could not be set.
    fn set_clipboard(&self, text: &str) -> Result<(), HostError>;

    /// The top of the primary monitor's work area, below the panel, in logical pixels.
    ///
    /// # Errors
    /// Why the work area could not be read.
    fn work_area_top(&self) -> Result<f64, HostError>;
}

/// The running app's GTK.
pub struct Gtk;

fn unanswered() -> HostError {
    HostError::NothingTyped(
        "The desktop window system did not answer in time. Nothing was copied.".to_owned(),
    )
}

fn not_ready() -> HostError {
    HostError::NothingTyped("The desktop window system is not ready yet.".to_owned())
}

impl Toolkit for Gtk {
    fn set_clipboard(&self, text: &str) -> Result<(), HostError> {
        let text = text.to_owned();
        run_on(&MainContext::default(), LIMIT, move || {
            if !gtk::is_initialized_main_thread() {
                return Err(not_ready());
            }
            gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).set_text(&text);
            Ok(())
        })
        .unwrap_or_else(|()| Err(unanswered()))
    }

    fn work_area_top(&self) -> Result<f64, HostError> {
        run_on(&MainContext::default(), LIMIT, || {
            if !gtk::is_initialized_main_thread() {
                return Err(not_ready());
            }
            let display = gdk::Display::default().ok_or_else(not_ready)?;
            let monitor = display
                .primary_monitor()
                .or_else(|| display.monitor(0))
                .ok_or_else(not_ready)?;
            Ok(f64::from(monitor.workarea().y()))
        })
        .unwrap_or_else(|()| Err(unanswered()))
    }
}

/// Runs `work` on `context`'s thread and waits up to `limit` for what it returns; `Err(())` when
/// that thread never ran it, as when its loop is not running.
fn run_on<T: Send + 'static>(
    context: &MainContext,
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ()> {
    let (sender, receiver) = mpsc::channel();
    context.invoke(move || {
        let _ = sender.send(work());
    });
    receiver.recv_timeout(limit).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn work_runs_on_the_thread_that_iterates_the_context_and_gives_up_when_none_does() {
        let idle = MainContext::new();
        assert_eq!(run_on(&idle, Duration::from_millis(50), || 1), Err(()));

        let context = MainContext::new();
        let ran = Arc::new(AtomicBool::new(false));
        let looping = context.clone();
        let seen = Arc::clone(&ran);
        let main = std::thread::spawn(move || {
            let _owner = looping.acquire().expect("a fresh context has no owner");
            while !seen.load(Ordering::SeqCst) {
                looping.iteration(true);
            }
            std::thread::current().id()
        });
        let flag = Arc::clone(&ran);
        let ran_on = run_on(&context, Duration::from_secs(30), move || {
            flag.store(true, Ordering::SeqCst);
            std::thread::current().id()
        });
        assert_eq!(ran_on, Ok(main.join().unwrap()));
    }
}
