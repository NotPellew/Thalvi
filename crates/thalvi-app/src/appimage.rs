//! The path to the `AppImage` that launched Thalvi.

use std::ffi::OsString;
use std::path::PathBuf;

pub(super) fn path() -> Option<PathBuf> {
    from_env_value(std::env::var_os("APPIMAGE"))
}

pub(super) fn from_env_value(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|path| !path.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_nonempty_environment_value_names_an_appimage() {
        for (value, expected) in [
            (None, None),
            (Some(""), None),
            (Some("/opt/Thalvi.AppImage"), Some("/opt/Thalvi.AppImage")),
        ] {
            assert_eq!(
                from_env_value(value.map(OsString::from)),
                expected.map(PathBuf::from),
                "{value:?}"
            );
        }
    }
}
