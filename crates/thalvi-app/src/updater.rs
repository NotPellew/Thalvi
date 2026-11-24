//! The app's update source: Tauri's updater plugin reading the channel's feed and replacing the
//! installed app with a package whose signature it has verified.

use semver::Version;
use tauri::AppHandle;
use tauri_plugin_updater::{Error, Update, UpdaterExt};
use thalvi_desktop::config::UpdateChannel;

use crate::update::{
    CHECK_TIMEOUT, Cause, DOWNLOAD_TIMEOUT, Found, Progress, RELEASES_PAGE, Source, UpdateError,
    feed,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "windows")]
use windows as os;

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
        os::unsupported(&self.app)
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
