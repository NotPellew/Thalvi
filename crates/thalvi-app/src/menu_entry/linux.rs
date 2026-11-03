//! The Linux app menu entry: a desktop file and an icon for an `AppImage` in the user's data folder.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::MenuEntry;
use crate::switch::Unavailable;

const ICON: &[u8] = include_bytes!("../../icons/icon.png");
/// The side of the bundled icon; a test checks it against the PNG header.
const ICON_SIZE: u32 = 128;
const NAME: &str = "thalvi";

/// The current user's menu entry, written when Thalvi runs from an `AppImage`.
pub struct System {
    appimage: Option<PathBuf>,
    data_home: Option<PathBuf>,
}

impl System {
    pub fn from_env() -> Self {
        Self {
            appimage: std::env::var_os("APPIMAGE")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            data_home: dirs::data_dir(),
        }
    }

    fn desktop_file(data_home: &Path) -> PathBuf {
        data_home
            .join("applications")
            .join(format!("{NAME}.desktop"))
    }

    fn icon_file(data_home: &Path) -> PathBuf {
        data_home
            .join("icons/hicolor")
            .join(format!("{ICON_SIZE}x{ICON_SIZE}"))
            .join("apps")
            .join(format!("{NAME}.png"))
    }

    fn data_home(&self) -> Result<&Path, String> {
        self.data_home
            .as_deref()
            .ok_or_else(|| "Thalvi could not find your data folder.".to_owned())
    }
}

impl MenuEntry for System {
    fn unavailable(&self) -> Option<Unavailable> {
        self.appimage.is_none().then(|| Unavailable {
            reason: "Your package manager adds Thalvi to the app menu.".to_owned(),
            line: None,
        })
    }

    fn is_shown(&self) -> Result<bool, String> {
        let file = Self::desktop_file(self.data_home()?);
        file.try_exists()
            .map_err(|error| format!("{}: {error}", file.display()))
    }

    fn show(&self) -> Result<(), String> {
        let (Some(appimage), data_home) = (self.appimage.as_deref(), self.data_home()?) else {
            return Err("Thalvi is not running from an AppImage.".to_owned());
        };
        let Some(appimage) = appimage.to_str() else {
            return Err("The AppImage's path is not valid text.".to_owned());
        };
        write(&Self::icon_file(data_home), ICON)?;
        write(
            &Self::desktop_file(data_home),
            desktop_entry(appimage).as_bytes(),
        )
    }

    fn hide(&self) -> Result<(), String> {
        let data_home = self.data_home()?;
        [Self::desktop_file(data_home), Self::icon_file(data_home)]
            .iter()
            .try_for_each(|file| match std::fs::remove_file(file) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    Err(format!("{}: {error}", file.display()))
                }
                _ => Ok(()),
            })
    }
}

fn write(file: &Path, contents: &[u8]) -> Result<(), String> {
    let fail = |error: std::io::Error| format!("{}: {error}", file.display());
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    std::fs::write(file, contents).map_err(fail)
}

fn desktop_entry(appimage: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Thalvi\nComment=Voice assistant for when hands and eyes are busy\nExec={}\nIcon={NAME}\nCategories=Utility;\nTerminal=false\n",
        exec_argument(appimage)
    )
}

/// A desktop entry `Exec` word: quoted, with the characters it reads specially escaped (a
/// backslash twice over, as the file format escapes it once more).
fn exec_argument(path: &str) -> String {
    let mut quoted = String::from('"');
    for character in path.chars() {
        match character {
            '\\' => quoted.push_str(r"\\\\"),
            '"' | '`' | '$' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thalvi-menu-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_entry_and_icon_are_written_for_the_appimage_and_removed_together() {
        let dir = temp("write");
        let system = System {
            appimage: Some("/home/me/My Apps/Thalvi.AppImage".into()),
            data_home: Some(dir.clone()),
        };
        let before = system.is_shown();
        system.show().unwrap();
        let after_show = system.is_shown();
        let desktop = std::fs::read_to_string(System::desktop_file(&dir)).unwrap();
        let icon = std::fs::read(System::icon_file(&dir)).unwrap();
        system.hide().unwrap();
        let after_hide = (
            system.is_shown(),
            System::icon_file(&dir).exists(),
            system.hide(),
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!((before, after_show), (Ok(false), Ok(true)));
        assert!(desktop.contains("Exec=\"/home/me/My Apps/Thalvi.AppImage\"\n"));
        assert!(desktop.contains("Icon=thalvi\n"));
        assert_eq!(icon, ICON);
        assert_eq!(after_hide, (Ok(false), false, Ok(())));
    }

    #[test]
    fn only_an_appimage_gets_an_entry_and_a_packaged_install_says_who_adds_it() {
        let packaged = System {
            appimage: None,
            data_home: Some(temp("packaged")),
        };
        assert_eq!(
            packaged.unavailable().map(|unavailable| unavailable.reason),
            Some("Your package manager adds Thalvi to the app menu.".to_owned())
        );
        let appimage = System {
            appimage: Some("/opt/Thalvi.AppImage".into()),
            data_home: None,
        };
        assert_eq!(appimage.unavailable(), None);
    }

    #[test]
    fn exec_words_are_quoted_and_their_special_characters_escaped() {
        for (path, word) in [
            ("/opt/Thalvi.AppImage", r#""/opt/Thalvi.AppImage""#),
            ("/a b/c", r#""/a b/c""#),
            (r#"/a"b`c$d"#, r#""/a\"b\`c\$d""#),
            (r"/a\b", r#""/a\\\\b""#),
            ("/100%/x", r#""/100%%/x""#),
        ] {
            assert_eq!(exec_argument(path), word, "{path}");
        }
    }

    #[test]
    fn the_bundled_icon_is_the_size_its_folder_names() {
        let mut size = Vec::new();
        size.extend(ICON_SIZE.to_be_bytes());
        size.extend(ICON_SIZE.to_be_bytes());
        assert_eq!(&ICON[16..24], size.as_slice());
    }
}
