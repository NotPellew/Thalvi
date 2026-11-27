//! Whether a Linux install can replace itself: only the `AppImage` it runs from, in a folder its user can write.

use std::path::{Path, PathBuf};

use crate::appimage;
use crate::update::RELEASES_PAGE;

/// Why the running app cannot update itself, in words that say what to do instead.
pub fn unsupported() -> Option<String> {
    Install::of(appimage::path()).unsupported(can_write)
}

/// How this copy of Thalvi was installed, which decides whether it can replace itself.
#[derive(Debug, PartialEq, Eq)]
enum Install {
    /// Running from this `AppImage` file.
    AppImage(PathBuf),
    /// Not from an `AppImage`: a build from source or an unpacked image.
    Loose,
}

impl Install {
    fn of(appimage: Option<PathBuf>) -> Self {
        match appimage {
            Some(path) => Self::AppImage(path),
            None => Self::Loose,
        }
    }

    /// `can_write` tells whether the folder holding the `AppImage` can be written.
    fn unsupported(&self, can_write: impl FnOnce(&Path) -> bool) -> Option<String> {
        match self {
            Self::Loose => Some(format!(
                "Thalvi can update itself only when it runs as the AppImage. Download the newest AppImage from {RELEASES_PAGE} and run that instead."
            )),
            Self::AppImage(path) => {
                let folder = path.parent().filter(|folder| can_write(folder));
                folder.is_none().then(|| {
                    format!(
                        "Thalvi cannot replace {} because you cannot write to its folder. Move the AppImage to a folder you own, or download the newest one from {RELEASES_PAGE}.",
                        path.display()
                    )
                })
            }
        }
    }
}

/// Whether a new file can be made in `folder`.
fn can_write(folder: &Path) -> bool {
    let probe = folder.join(format!(".thalvi-update-probe-{}", std::process::id()));
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    made
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_appimage_in_a_writable_folder_updates_itself() {
        let image = || Install::AppImage(PathBuf::from("/opt/thalvi/Thalvi.AppImage"));
        assert_eq!(
            image().unsupported(|folder| folder == Path::new("/opt/thalvi")),
            None
        );

        let loose = Install::Loose.unsupported(|_| true).unwrap();
        assert!(
            loose.contains("only when it runs as the AppImage"),
            "{loose}"
        );
        assert!(loose.contains(RELEASES_PAGE), "{loose}");

        let locked = image().unsupported(|_| false).unwrap();
        assert!(locked.contains("/opt/thalvi/Thalvi.AppImage"), "{locked}");
        assert!(locked.contains("cannot write to its folder"), "{locked}");
        assert!(locked.contains(RELEASES_PAGE), "{locked}");
    }

    #[test]
    fn the_install_is_the_appimage_it_runs_from_or_a_loose_build() {
        let image = "/home/me/Thalvi.AppImage";
        for (value, expected) in [
            (Some(image), Install::AppImage(PathBuf::from(image))),
            (None, Install::Loose),
            (Some(""), Install::Loose),
        ] {
            assert_eq!(
                Install::of(appimage::from_env_value(value.map(Into::into))),
                expected,
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_folder_that_cannot_be_written_is_found_without_leaving_anything_behind() {
        let dir = std::env::temp_dir().join(format!("thalvi-updater-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!can_write(&dir), "a folder that is not there");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(can_write(&dir));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
