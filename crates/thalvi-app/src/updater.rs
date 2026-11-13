//! The app's update source: Tauri's updater plugin reading the channel's feed and replacing the
//! installed app with a package whose signature it has verified.

use std::path::{Path, PathBuf};

use semver::Version;
use tauri::AppHandle;
use tauri_plugin_updater::{Error, Update, UpdaterExt};
use thalvi_desktop::config::UpdateChannel;

use crate::update::{
    CHECK_TIMEOUT, Cause, DOWNLOAD_TIMEOUT, Found, Progress, RELEASES_PAGE, Source, UpdateError,
    feed,
};

/// How this copy of Thalvi was installed, which decides whether it can replace itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Install {
    /// Running from this `AppImage` file.
    AppImage(PathBuf),
    /// On Linux, but not from an `AppImage`: a build from source or an unpacked image.
    Loose,
    /// The per-user installer's folder, which its user can write.
    PerUser,
}

impl Install {
    /// Why this install cannot update itself, in words that say what to do instead.
    /// `can_write` tells whether the folder holding the `AppImage` can be written.
    #[must_use]
    pub fn unsupported(&self, can_write: impl FnOnce(&Path) -> bool) -> Option<String> {
        match self {
            Self::PerUser => None,
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

impl Install {
    /// How the running app was installed: the per-user installer's folder on Windows, and on Linux
    /// the `AppImage` it runs from, if any.
    fn detect(app: &AppHandle) -> Self {
        Self::of(appimage_of(app), cfg!(windows))
    }

    fn of(appimage: Option<PathBuf>, per_user: bool) -> Self {
        match (per_user, appimage) {
            (true, _) => Self::PerUser,
            (false, Some(path)) => Self::AppImage(path),
            (false, None) => Self::Loose,
        }
    }
}

#[cfg(target_os = "linux")]
fn appimage_of(app: &AppHandle) -> Option<PathBuf> {
    use tauri::Manager;

    app.env().appimage.map(PathBuf::from)
}

#[cfg(not(target_os = "linux"))]
fn appimage_of(_: &AppHandle) -> Option<PathBuf> {
    None
}

/// Tauri's updater, pointed at the channel's feed.
pub struct PluginSource {
    app: AppHandle,
}

impl PluginSource {
    #[must_use]
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[derive(Clone, Copy)]
enum Step {
    Check,
    Download,
    Install,
}

/// The plain words for an updater error, and what kind of failure it is.
fn explain(error: &Error, step: Step) -> UpdateError {
    let unchanged = "Your current version is unchanged.";
    let (cause, message) = match error {
        Error::Reqwest(error) if error.is_timeout() => (
            Cause::Timeout,
            "The update server took too long to answer. Check your internet connection, then try again.".to_owned(),
        ),
        Error::Reqwest(_) | Error::Network(_) | Error::Io(_) if !matches!(step, Step::Install) => (
            Cause::Network,
            match step {
                Step::Check => "Thalvi could not reach the update server. Check your internet connection, then try again.".to_owned(),
                _ => format!("The update could not be downloaded. Check your internet connection, then try again. {unchanged}"),
            },
        ),
        Error::TargetNotFound(_) | Error::TargetsNotFound(_) => (
            Cause::Feed,
            format!("The latest release has no package for this computer, so Thalvi did not update. Install it from {RELEASES_PAGE}."),
        ),
        Error::ReleaseNotFound
        | Error::Serialization(_)
        | Error::Semver(_)
        | Error::UrlParse(_)
        | Error::EmptyEndpoints
        | Error::UnsupportedOs
        | Error::UnsupportedArch => (
            Cause::Feed,
            "The update information was not valid, so Thalvi ignored it. Try again later.".to_owned(),
        ),
        Error::Minisign(_)
        | Error::Base64(_)
        | Error::SignatureUtf8(_)
        | Error::SignedVersionMismatch { .. }
        | Error::MissingSignedVersion => (
            Cause::Signature,
            format!("The downloaded update failed its signature check, so Thalvi did not install it. {unchanged}"),
        ),
        other => (
            Cause::Install,
            format!("The update could not be installed: {other}. {unchanged} Install the newest version from {RELEASES_PAGE}."),
        ),
    };
    UpdateError { cause, message }
}

impl Source for PluginSource {
    type Package = Update;

    fn unsupported(&self) -> Option<String> {
        Install::detect(&self.app).unsupported(can_write)
    }

    async fn check(&self, channel: UpdateChannel) -> Result<Option<Found<Update>>, UpdateError> {
        let feed = reqwest::Url::parse(feed(channel)).map_err(|error| UpdateError {
            cause: Cause::Feed,
            message: format!("The update address is not valid: {error}"),
        })?;
        let updater = self
            .app
            .updater_builder()
            .endpoints(vec![feed])
            .and_then(|builder| builder.timeout(CHECK_TIMEOUT).build())
            .map_err(|error| explain(&error, Step::Check))?;
        let found = updater
            .check()
            .await
            .map_err(|error| explain(&error, Step::Check))?;
        found
            .map(|mut update| {
                update.timeout = Some(DOWNLOAD_TIMEOUT);
                let version = Version::parse(&update.version).map_err(|error| UpdateError {
                    cause: Cause::Feed,
                    message: format!(
                        "The update information named the version {}, which is not valid: {error}",
                        update.version
                    ),
                })?;
                Ok(Found {
                    version,
                    notes: update.body.clone(),
                    package: update,
                })
            })
            .transpose()
    }

    async fn download(
        &self,
        found: &Found<Update>,
        progress: &(dyn Fn(Progress) + Send + Sync),
    ) -> Result<Vec<u8>, UpdateError> {
        let mut done = 0_u64;
        found
            .package
            .download(
                |chunk, total| {
                    done += u64::try_from(chunk).unwrap_or(u64::MAX);
                    progress(Progress { done, total });
                },
                || {},
            )
            .await
            .map_err(|error| explain(&error, Step::Download))
    }

    fn install(&self, found: &Found<Update>, bytes: &[u8]) -> Result<(), UpdateError> {
        found
            .package
            .install(bytes)
            .map_err(|error| explain(&error, Step::Install))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_appimage_in_a_writable_folder_and_the_per_user_install_update_themselves() {
        let image = || Install::AppImage(PathBuf::from("/opt/thalvi/Thalvi.AppImage"));
        assert_eq!(Install::PerUser.unsupported(|_| false), None);
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
    fn the_install_is_the_installers_folder_on_windows_and_the_appimage_on_linux() {
        let image = || Some(PathBuf::from("/home/me/Thalvi.AppImage"));
        assert_eq!(Install::of(None, true), Install::PerUser);
        assert_eq!(Install::of(image(), true), Install::PerUser);
        assert_eq!(
            Install::of(image(), false),
            Install::AppImage(PathBuf::from("/home/me/Thalvi.AppImage"))
        );
        assert_eq!(Install::of(None, false), Install::Loose);
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

    #[test]
    fn updater_errors_are_told_in_plain_words_with_the_right_cause() {
        let cases: Vec<(Error, Step, Cause, &str)> = vec![
            (
                Error::Network("Download request failed with status: 404".to_owned()),
                Step::Check,
                Cause::Network,
                "Thalvi could not reach the update server.",
            ),
            (
                Error::Network("reset".to_owned()),
                Step::Download,
                Cause::Network,
                "The update could not be downloaded.",
            ),
            (
                Error::TargetNotFound("windows-x86_64".to_owned()),
                Step::Check,
                Cause::Feed,
                "The latest release has no package for this computer",
            ),
            (
                Error::ReleaseNotFound,
                Step::Check,
                Cause::Feed,
                "The update information was not valid",
            ),
            (
                Error::MissingSignedVersion,
                Step::Download,
                Cause::Signature,
                "The downloaded update failed its signature check",
            ),
            (
                Error::SignatureUtf8("x".to_owned()),
                Step::Download,
                Cause::Signature,
                "The downloaded update failed its signature check",
            ),
            (
                Error::TempDirNotOnSameMountPoint,
                Step::Install,
                Cause::Install,
                "The update could not be installed:",
            ),
            (
                Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                Step::Install,
                Cause::Install,
                "The update could not be installed:",
            ),
        ];
        for (error, step, cause, start) in cases {
            let told = explain(&error, step);
            assert_eq!(told.cause, cause, "{error}");
            assert!(told.message.starts_with(start), "{error}: {}", told.message);
        }
    }
}
