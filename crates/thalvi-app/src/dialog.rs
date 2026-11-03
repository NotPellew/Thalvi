//! A native message box for a failure that happens before any Thalvi window can show it.

use std::io::IsTerminal;

const TITLE: &str = "Thalvi couldn't start";

/// Shows `message` in a message box unless stderr is a terminal, where the user already sees it.
pub fn startup_failed(message: &str) {
    if !std::io::stderr().is_terminal() {
        show(TITLE, message);
    }
}

#[cfg(target_os = "linux")]
fn show(title: &str, message: &str) {
    // Without a display there is nowhere to show it; the caller has written it to stderr.
    if gtk::init().is_err() {
        return;
    }
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Error,
        gtk::ButtonsType::Ok,
        message,
    );
    gtk::prelude::GtkWindowExt::set_title(&dialog, title);
    gtk::prelude::DialogExt::run(&dialog);
}

#[cfg(target_os = "windows")]
fn show(title: &str, message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;

    // SAFETY: both strings are valid, null-terminated wide strings for the whole call, and no
    // owner window is passed.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from(title),
            MB_OK | MB_ICONERROR,
        );
    }
}
