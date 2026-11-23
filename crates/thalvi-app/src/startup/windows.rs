//! Whether Windows runs the login entry: its `Run` key, and the Task Manager toggle beside it.

use windows_registry::CURRENT_USER;

use super::APP_NAME;
use crate::switch::Unavailable;

/// The registry key Windows records a Task Manager or Settings toggle in, apart from the `Run` entry.
const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Whether the user turned Thalvi off in the system's own startup list.
pub fn turned_off_by_user() -> bool {
    CURRENT_USER
        .open(KEY)
        .and_then(|key| key.get_value(APP_NAME))
        .is_ok_and(|value| is_off(&value))
}

/// The `Run` key is the OS's own startup list on Windows, so every session runs an entry.
pub fn unavailable() -> Option<Unavailable> {
    None
}

/// The first byte has bit 0 set when the entry is disabled (`03`, `07`) and clear when it is
/// enabled (`02`, `06`); a disabled value then carries the time it was turned off.
fn is_off(value: &[u8]) -> bool {
    value.first().is_some_and(|first| first & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_marks_a_startup_entry_off_by_bit_zero_of_its_first_byte() {
        let with_time = |first: u8| {
            let mut value = vec![first, 0, 0, 0];
            value.extend([0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x01]);
            value
        };
        for (value, expected) in [
            (with_time(2), false),
            (with_time(3), true),
            (with_time(6), false),
            (with_time(7), true),
            (vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], false),
            (vec![], false),
        ] {
            assert_eq!(is_off(&value), expected, "{value:?}");
        }
    }
}
