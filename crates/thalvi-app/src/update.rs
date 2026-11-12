//! Keeps the app on the user's update channel: checks in the background, downloads and verifies,
//! and installs only when the engine says Thalvi is idle.

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thalvi_desktop::config::{DesktopConfig, UpdateChannel};
use thalvi_desktop::host::WebUrl;
use thalvi_desktop::metrics::{Event, Recorder};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until, timeout};

use crate::settings::write_atomically;

const STABLE_FEED: &str =
    "https://github.com/NotPellew/Thalvi/releases/latest/download/latest.json";
const DEVELOPMENT_FEED: &str =
    "https://github.com/NotPellew/Thalvi/releases/download/desktop-dev/latest.json";

/// Where to install Thalvi by hand when an update cannot be applied.
pub const RELEASES_PAGE: &str = "https://github.com/NotPellew/Thalvi/releases";

#[must_use]
pub const fn feed(channel: UpdateChannel) -> &'static str {
    match channel {
        UpdateChannel::Stable => STABLE_FEED,
        UpdateChannel::Development => DEVELOPMENT_FEED,
    }
}

/// After the app is usable, how long before the first automatic check.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(10);

/// The least time between automatic checks while the app runs.
const CHECK_EVERY: Duration = Duration::from_hours(24);

/// How long the update server may take to answer a check.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a whole download may take.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(20);

/// How often a downloaded update asks again whether Thalvi is idle, besides each time it becomes quiet.
const INSTALL_RETRY: Duration = Duration::from_secs(30);

/// Why an update step failed, which decides the outcome recorded in the metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Timeout,
    Network,
    /// The feed's answer was unusable or named a version the channel must not install.
    Feed,
    /// The download did not match its signature.
    Signature,
    Install,
}

impl Cause {
    const fn name(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::Feed => "feed",
            Self::Signature => "signature",
            Self::Install => "install",
        }
    }
}

/// An update step that failed, with the plain words the user is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateError {
    pub cause: Cause,
    pub message: String,
}

impl UpdateError {
    fn timeout(what: &str) -> Self {
        Self {
            cause: Cause::Timeout,
            message: format!(
                "{what} took too long. Check your internet connection, then try again."
            ),
        }
    }
}

/// An update the feed offers, with whatever the source needs to download and install it.
pub struct Found<P> {
    pub version: Version,
    /// The release page the feed names.
    pub notes: Option<String>,
    pub package: P,
}

/// How much of a download has arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
}

/// Where updates come from and how they are applied. The app's source is Tauri's updater; a fake
/// stands in for it in tests.
pub trait Source: Send + Sync + 'static {
    /// What a found update is carried as until it installs.
    type Package: Send + Sync + 'static;

    /// Why this install cannot update itself, in words that say what to do instead.
    fn unsupported(&self) -> Option<String>;

    /// The newest release `channel`'s feed offers for this computer, if the feed names one.
    fn check(
        &self,
        channel: UpdateChannel,
    ) -> impl Future<Output = Result<Option<Found<Self::Package>>, UpdateError>> + Send;

    /// Downloads the package and verifies its signature; nothing unverified is returned.
    fn download(
        &self,
        found: &Found<Self::Package>,
        progress: &(dyn Fn(Progress) + Send + Sync),
    ) -> impl Future<Output = Result<Vec<u8>, UpdateError>> + Send;

    /// Replaces the installed app with the verified `bytes`. On Windows the installer takes over
    /// and this does not return when it starts.
    ///
    /// # Errors
    /// Why the app could not be replaced; the installed version is then as it was.
    fn install(&self, found: &Found<Self::Package>, bytes: &[u8]) -> Result<(), UpdateError>;
}

/// What stopping the app for an update came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The engine, the microphone and the recognizer are closed and the marker is written.
    Stopped,
    /// Something is in progress; nothing was stopped.
    Busy,
    /// Thalvi could not get ready to restart; nothing was stopped.
    Refused(String),
}

/// The app's side of an install: stopping when idle, and starting again.
pub trait Restarter: Send + Sync + 'static {
    /// Stops the engine, the microphone and the recognizer if Thalvi is idle, after saving what the
    /// next start needs. The engine decides, so a wake cannot slip in between the check and the stop.
    fn stop_if_idle(&self, from: &Version, to: &Version) -> impl Future<Output = Stop> + Send;

    /// Starts Thalvi again, as the new version or, when `failure` says why, as the old one.
    fn relaunch(&self, failure: Option<&str>);
}

/// What the update is doing, as the UI and the tray show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Phase {
    /// No check has run yet.
    Idle,
    Checking,
    UpToDate,
    Downloading {
        version: String,
        percent: Option<u8>,
    },
    /// Downloaded and verified; installs once Thalvi is idle.
    Ready {
        version: String,
    },
    Installing {
        version: String,
    },
    /// The latest check, download or install failed; the installed version is as it was.
    Failed {
        message: String,
    },
    /// This install cannot update itself.
    Unsupported {
        message: String,
    },
}

/// Everything the UI and the tray need to know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: Phase,
    /// The release page of the version being fetched or waiting.
    pub notes: Option<String>,
    /// What happened at this start, such as "Updated to 0.2.0".
    pub notice: Option<String>,
}

impl Snapshot {
    #[must_use]
    pub fn start(arrival: Option<Arrival>) -> Self {
        let (phase, notice) = match arrival {
            None => (Phase::Idle, None),
            Some(Arrival::Updated { version }) => {
                (Phase::Idle, Some(format!("Updated to {version}.")))
            }
            Some(Arrival::NotApplied {
                wanted,
                still,
                error,
            }) => (
                Phase::Failed {
                    message: not_applied(&wanted, &still, error.as_deref()),
                },
                None,
            ),
        };
        Self {
            phase,
            notes: None,
            notice,
        }
    }

    /// The tray's update line, and what a click on it does.
    #[must_use]
    pub fn tray(&self) -> (String, TrayClick) {
        match &self.phase {
            Phase::Idle | Phase::UpToDate => (
                self.notice
                    .clone()
                    .unwrap_or_else(|| "Check for updates".to_owned()),
                TrayClick::Check,
            ),
            Phase::Failed { .. } => ("Update failed — check again".to_owned(), TrayClick::Check),
            Phase::Checking => ("Checking for updates…".to_owned(), TrayClick::Settings),
            Phase::Downloading { version, .. } => (
                format!("Downloading update {version}…"),
                TrayClick::Settings,
            ),
            Phase::Ready { version } => (
                format!("Update {version} ready — installs when Thalvi is idle"),
                TrayClick::Settings,
            ),
            Phase::Installing { version } => {
                (format!("Installing update {version}…"), TrayClick::Settings)
            }
            Phase::Unsupported { .. } => ("Updates: see Settings".to_owned(), TrayClick::Settings),
        }
    }

    /// A short remark for the tray header and tooltip, only when there is news.
    #[must_use]
    pub fn remark(&self) -> Option<String> {
        match &self.phase {
            Phase::Ready { version } => Some(format!("update {version} ready")),
            Phase::Idle | Phase::UpToDate => self
                .notice
                .as_deref()
                .map(|notice| notice.trim_end_matches('.').to_lowercase()),
            _ => None,
        }
    }
}

/// What clicking the tray's update line does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayClick {
    Check,
    Settings,
}

fn not_applied(wanted: &str, still: &str, error: Option<&str>) -> String {
    let why = error.map_or_else(String::new, |error| format!(" {error}"));
    format!(
        "The update to {wanted} did not finish, so Thalvi is still {still}.{why} Install {wanted} from {RELEASES_PAGE}."
    )
}

/// What a request from the UI, the tray or the engine's status asks of the update driver.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    CheckNow,
    ConfigChanged,
    Quiet,
}

/// The app's handle on the update driver.
#[derive(Clone)]
pub struct Updates {
    requests: mpsc::UnboundedSender<Request>,
    snapshot: watch::Receiver<Snapshot>,
}

impl Updates {
    /// Checks now, or joins the check that is running.
    pub fn check_now(&self) {
        let _ = self.requests.send(Request::CheckNow);
    }

    /// The saved update channel may have changed.
    pub fn config_changed(&self) {
        let _ = self.requests.send(Request::ConfigChanged);
    }

    /// Thalvi looks idle: a downloaded update may install.
    pub fn quiet(&self) {
        let _ = self.requests.send(Request::Quiet);
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    #[must_use]
    pub fn changes(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }
}

/// The driver's side of [`Updates`].
pub struct Inbox {
    requests: mpsc::UnboundedReceiver<Request>,
    out: watch::Sender<Snapshot>,
}

#[must_use]
pub fn channel(start: Snapshot) -> (Updates, Inbox) {
    let (requests, inbox) = mpsc::unbounded_channel();
    let (out, snapshot) = watch::channel(start);
    (
        Updates { requests, snapshot },
        Inbox {
            requests: inbox,
            out,
        },
    )
}

/// An update that is downloaded, verified and waiting for Thalvi to be idle.
struct Pending<P> {
    found: Found<P>,
    bytes: Vec<u8>,
    channel: UpdateChannel,
}

struct Driver<S: Source, H, C> {
    source: S,
    host: H,
    config: C,
    current: Version,
    recorder: Option<Recorder>,
    out: watch::Sender<Snapshot>,
    pending: Option<Pending<S::Package>>,
    notes: Option<String>,
}

/// Whether the loop goes on.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    /// The app is restarting.
    Done,
}

/// Whether `offered` is an update `channel` may install on `current`.
fn judge(
    current: &Version,
    channel: UpdateChannel,
    offered: &Version,
) -> Result<bool, UpdateError> {
    if channel == UpdateChannel::Stable && !offered.pre.is_empty() {
        return Err(UpdateError {
            cause: Cause::Feed,
            message: format!(
                "The Stable update feed offered {offered}, a development build, so Thalvi ignored it. Try again later."
            ),
        });
    }
    Ok(offered > current)
}

impl<S: Source, H: Restarter, C: Fn() -> DesktopConfig + Send + Sync> Driver<S, H, C> {
    fn show(&self, phase: Phase) {
        let notes = match phase {
            Phase::Downloading { .. } | Phase::Ready { .. } | Phase::Installing { .. } => {
                self.notes.clone()
            }
            _ => None,
        };
        self.out.send_modify(|snapshot| {
            snapshot.phase = phase;
            snapshot.notes = notes;
        });
    }

    fn fail(&self, error: UpdateError) {
        println!("{}", error.message);
        self.show(Phase::Failed {
            message: error.message,
        });
    }

    fn record(
        &self,
        step: &str,
        outcome: &str,
        took: Option<Duration>,
        channel: UpdateChannel,
        version: Option<&Version>,
    ) {
        if let Some(recorder) = &self.recorder {
            recorder.set_kinds((self.config)().recordings);
            recorder.metric(
                Event::Update,
                outcome,
                took,
                json!({
                    "step": step,
                    "channel": channel.name(),
                    "version": version.map(ToString::to_string),
                    "current": self.current.to_string(),
                }),
            );
        }
    }

    /// Asks the feed for an update and downloads it; then offers it to the engine.
    async fn cycle(&mut self) -> Flow {
        let channel = (self.config)().update_channel;
        if let Some(message) = self.source.unsupported() {
            self.pending = None;
            self.show(Phase::Unsupported { message });
            return Flow::Continue;
        }
        if let Some(pending) = &self.pending {
            if pending.channel == channel {
                // What is downloaded and verified waits; asking the feed again would change nothing.
                self.show(Phase::Ready {
                    version: pending.found.version.to_string(),
                });
                return self.install_if_idle().await;
            }
            self.pending = None;
        }
        self.show(Phase::Checking);
        let started = Instant::now();
        let asked = timeout(CHECK_TIMEOUT, self.source.check(channel))
            .await
            .unwrap_or_else(|_| Err(UpdateError::timeout("Checking for updates")));
        let took = Some(started.elapsed());
        let found = match asked.and_then(|found| {
            found
                .map(|found| {
                    judge(&self.current, channel, &found.version).map(|newer| (found, newer))
                })
                .transpose()
        }) {
            Err(error) => {
                self.record("check", error.cause.name(), took, channel, None);
                self.fail(error);
                return Flow::Continue;
            }
            Ok(None | Some((_, false))) => {
                self.record("check", "none", took, channel, None);
                self.show(Phase::UpToDate);
                return Flow::Continue;
            }
            Ok(Some((found, true))) => found,
        };
        self.record("check", "ok", took, channel, Some(&found.version));
        self.notes = found
            .notes
            .as_deref()
            .and_then(|notes| WebUrl::parse(notes).ok())
            .map(|url| url.as_str().to_owned());
        let version = found.version.to_string();
        self.show(Phase::Downloading {
            version: version.clone(),
            percent: None,
        });
        let started = Instant::now();
        let progress = |progress: Progress| {
            let percent = progress
                .total
                .filter(|total| *total > 0)
                .map(|total| u8::try_from((progress.done.min(total) * 100) / total).unwrap_or(100));
            self.out.send_if_modified(|snapshot| {
                let next = Phase::Downloading {
                    version: version.clone(),
                    percent,
                };
                if snapshot.phase == next {
                    return false;
                }
                snapshot.phase = next;
                true
            });
        };
        let downloaded = timeout(DOWNLOAD_TIMEOUT, self.source.download(&found, &progress))
            .await
            .unwrap_or_else(|_| Err(UpdateError::timeout("Downloading the update")));
        let took = Some(started.elapsed());
        match downloaded {
            Err(error) => {
                self.record(
                    "download",
                    error.cause.name(),
                    took,
                    channel,
                    Some(&found.version),
                );
                self.fail(error);
                Flow::Continue
            }
            Ok(bytes) => {
                self.record("download", "ok", took, channel, Some(&found.version));
                self.show(Phase::Ready { version });
                self.pending = Some(Pending {
                    found,
                    bytes,
                    channel,
                });
                self.install_if_idle().await
            }
        }
    }

    /// Installs the waiting update if the engine finds Thalvi idle; otherwise it keeps waiting.
    async fn install_if_idle(&mut self) -> Flow {
        let Some(pending) = &self.pending else {
            return Flow::Continue;
        };
        if pending.channel != (self.config)().update_channel {
            self.pending = None;
            return Flow::Continue;
        }
        let version = pending.found.version.clone();
        match self.host.stop_if_idle(&self.current, &version).await {
            Stop::Busy => Flow::Continue,
            Stop::Refused(message) => {
                self.pending = None;
                self.fail(UpdateError {
                    cause: Cause::Install,
                    message: format!(
                        "Thalvi could not get ready to restart for the update: {message} Your current version is unchanged."
                    ),
                });
                Flow::Continue
            }
            Stop::Stopped => {
                let Some(pending) = self.pending.take() else {
                    return Flow::Continue;
                };
                self.show(Phase::Installing {
                    version: version.to_string(),
                });
                if let Some(recorder) = self.recorder.take() {
                    recorder.finish().await;
                }
                let installed = self.source.install(&pending.found, &pending.bytes);
                self.host.relaunch(
                    installed
                        .err()
                        .inspect(|error| println!("{}", error.message))
                        .map(|error| error.message)
                        .as_deref(),
                );
                Flow::Done
            }
        }
    }
}

/// Runs the update driver until the app restarts or the handle is dropped: the first check comes
/// shortly after `usable`, later ones at most once a day, and a manual check or a change of channel
/// checks at once. Requests that arrive during a check join it.
pub async fn drive<S, H, C>(
    mut inbox: Inbox,
    source: S,
    host: H,
    config: C,
    current: Version,
    recorder: Option<Recorder>,
    usable: impl Future<Output = ()> + Send,
) where
    S: Source,
    H: Restarter,
    C: Fn() -> DesktopConfig + Send + Sync,
{
    let mut channel = config().update_channel;
    let mut driver = Driver {
        source,
        host,
        config,
        current,
        recorder,
        out: inbox.out,
        pending: None,
        notes: None,
    };
    tokio::pin!(usable);
    let mut waiting = true;
    let mut next_check: Option<Instant> = None;
    let mut retry: Option<Instant> = None;
    let mut backlog: VecDeque<Request> = VecDeque::new();
    loop {
        let request = if let Some(request) = backlog.pop_front() {
            Some(request)
        } else {
            tokio::select! {
                request = inbox.requests.recv() => {
                    let Some(request) = request else { return };
                    Some(request)
                }
                () = &mut usable, if waiting => {
                    waiting = false;
                    next_check = Some(Instant::now() + FIRST_CHECK_AFTER);
                    None
                }
                () = wait_for(next_check), if next_check.is_some() => {
                    Some(Request::CheckNow)
                }
                () = wait_for(retry), if retry.is_some() => {
                    Some(Request::Quiet)
                }
            }
        };
        let flow = match request {
            None => Flow::Continue,
            Some(Request::CheckNow) => {
                let flow = driver.cycle().await;
                next_check = Some(Instant::now() + CHECK_EVERY);
                while let Ok(request) = inbox.requests.try_recv() {
                    if request != Request::CheckNow {
                        backlog.push_back(request);
                    }
                }
                flow
            }
            Some(Request::ConfigChanged) => {
                let now = (driver.config)().update_channel;
                if now == channel {
                    Flow::Continue
                } else {
                    channel = now;
                    backlog.push_back(Request::CheckNow);
                    Flow::Continue
                }
            }
            Some(Request::Quiet) => driver.install_if_idle().await,
        };
        if flow == Flow::Done {
            return;
        }
        retry = driver
            .pending
            .as_ref()
            .map(|_| Instant::now() + INSTALL_RETRY);
    }
}

async fn wait_for(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// What a start learns from the marker the last version left before it restarted for an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrival {
    Updated {
        version: String,
    },
    /// The restart came back as the same version: the installer did not do its work.
    NotApplied {
        wanted: String,
        still: String,
        error: Option<String>,
    },
}

impl Arrival {
    /// The outcome and the fields of the metrics line for the install step, which only the next
    /// start can see through: the versions and the channel, nothing else.
    #[must_use]
    pub fn metric(&self, channel: UpdateChannel) -> (&'static str, serde_json::Value) {
        let (outcome, version, current) = match self {
            Self::Updated { version } => ("ok", version, version),
            Self::NotApplied { wanted, still, .. } => ("not_applied", wanted, still),
        };
        (
            outcome,
            json!({"step": "install", "channel": channel.name(), "version": version, "current": current}),
        )
    }
}

/// Written just before an update is installed and read once by the next start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub from: String,
    pub to: String,
    /// Listening was paused, and stays so.
    pub paused: bool,
    /// When the restart began, in milliseconds since the epoch.
    pub at_ms: u64,
    /// Why the installer failed, when it did and the app came back as it was.
    #[serde(default)]
    pub error: Option<String>,
}

impl Marker {
    /// The marker's file name, beside the config.
    pub const FILE: &str = "update.json";

    /// A marker for an update from `from` to `to`, starting now.
    #[must_use]
    pub fn new(from: &Version, to: &Version, paused: bool) -> Self {
        Self {
            from: from.to_string(),
            to: to.to_string(),
            paused,
            at_ms: now_ms(),
            error: None,
        }
    }

    /// Saves the marker, replacing one that is there.
    ///
    /// # Errors
    /// Why the file could not be written.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|error| error.to_string())?;
        write_atomically(path, &json)
            .map_err(|error| format!("{} could not be written: {error}", path.display()))
    }

    /// Reads the marker and removes it, so only one start acts on it. A file that cannot be read
    /// is removed and counts as none.
    #[must_use]
    pub fn take(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let _ = std::fs::remove_file(path);
        serde_json::from_str(&text).ok()
    }

    /// Adds why the installer failed to the marker, for the start that follows.
    pub fn failed(path: &Path, error: &str) {
        let marker = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok());
        if let Some(marker) = marker {
            let _ = Self {
                error: Some(error.to_owned()),
                ..marker
            }
            .write(path);
        }
    }

    /// What this start, running `running`, makes of the marker, and how long the restart took.
    #[must_use]
    pub fn arrival(&self, running: &Version) -> (Arrival, Duration) {
        let took = Duration::from_millis(now_ms().saturating_sub(self.at_ms));
        let running = running.to_string();
        let arrival = if running == self.from {
            Arrival::NotApplied {
                wanted: self.to.clone(),
                still: running,
                error: self.error.clone(),
            }
        } else {
            Arrival::Updated { version: running }
        };
        (arrival, took)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// What a fake feed answers to one check.
    enum Answer {
        None,
        Offer(&'static str),
        Fail(Cause, &'static str),
        /// Never answers.
        Hang,
    }

    /// What a fake download does.
    enum Fetch {
        Bytes(&'static [u8]),
        Fail(Cause, &'static str),
        Hang,
    }

    #[derive(Default)]
    struct Script {
        answers: VecDeque<Answer>,
        fetches: VecDeque<Fetch>,
        installs: Vec<(String, Vec<u8>)>,
        install_error: Option<&'static str>,
        unsupported: Option<&'static str>,
        checks: usize,
        downloads: usize,
        asked: Vec<UpdateChannel>,
        running: usize,
        most_running: usize,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Script>>);

    impl Fake {
        fn script(&self) -> std::sync::MutexGuard<'_, Script> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    struct Overlap<'a>(&'a Fake);

    impl<'a> Overlap<'a> {
        fn new(fake: &'a Fake) -> Self {
            let mut script = fake.script();
            script.running += 1;
            script.most_running = script.most_running.max(script.running);
            drop(script);
            Self(fake)
        }
    }

    impl Drop for Overlap<'_> {
        fn drop(&mut self) {
            self.0.script().running -= 1;
        }
    }

    impl Source for Fake {
        type Package = ();

        fn unsupported(&self) -> Option<String> {
            self.script().unsupported.map(str::to_owned)
        }

        async fn check(&self, channel: UpdateChannel) -> Result<Option<Found<()>>, UpdateError> {
            let _overlap = Overlap::new(self);
            let answer = {
                let mut script = self.script();
                script.checks += 1;
                script.asked.push(channel);
                script.answers.pop_front().unwrap_or(Answer::None)
            };
            // A real request takes time, during which other requests arrive.
            tokio::time::sleep(Duration::from_secs(1)).await;
            match answer {
                Answer::None => Ok(None),
                Answer::Offer(version) => Ok(Some(Found {
                    version: v(version),
                    notes: Some(
                        "https://github.com/NotPellew/Thalvi/releases/tag/desktop-v9".to_owned(),
                    ),
                    package: (),
                })),
                Answer::Fail(cause, message) => Err(UpdateError {
                    cause,
                    message: message.to_owned(),
                }),
                Answer::Hang => std::future::pending().await,
            }
        }

        async fn download(
            &self,
            _: &Found<()>,
            progress: &(dyn Fn(Progress) + Send + Sync),
        ) -> Result<Vec<u8>, UpdateError> {
            let fetch = {
                let mut script = self.script();
                script.downloads += 1;
                script
                    .fetches
                    .pop_front()
                    .unwrap_or(Fetch::Bytes(b"package"))
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            match fetch {
                Fetch::Bytes(bytes) => {
                    // A server may announce a length of zero; that shows no percentage.
                    progress(Progress {
                        done: 0,
                        total: Some(0),
                    });
                    for done in [0, 50, 100] {
                        progress(Progress {
                            done,
                            total: Some(100),
                        });
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Ok(bytes.to_vec())
                }
                Fetch::Fail(cause, message) => Err(UpdateError {
                    cause,
                    message: message.to_owned(),
                }),
                Fetch::Hang => std::future::pending().await,
            }
        }

        fn install(&self, found: &Found<()>, bytes: &[u8]) -> Result<(), UpdateError> {
            let mut script = self.script();
            script
                .installs
                .push((found.version.to_string(), bytes.to_vec()));
            match script.install_error {
                Some(message) => Err(UpdateError {
                    cause: Cause::Install,
                    message: message.to_owned(),
                }),
                None => Ok(()),
            }
        }
    }

    #[derive(Default)]
    struct HostLog {
        stops: VecDeque<Stop>,
        asked: Vec<(String, String)>,
        relaunches: Vec<Option<String>>,
    }

    #[derive(Clone, Default)]
    struct FakeHost(Arc<Mutex<HostLog>>);

    impl FakeHost {
        fn log(&self) -> std::sync::MutexGuard<'_, HostLog> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl Restarter for FakeHost {
        fn stop_if_idle(&self, from: &Version, to: &Version) -> impl Future<Output = Stop> + Send {
            let stop = {
                let mut log = self.log();
                log.asked.push((from.to_string(), to.to_string()));
                log.stops.pop_front().unwrap_or(Stop::Busy)
            };
            std::future::ready(stop)
        }

        fn relaunch(&self, failure: Option<&str>) {
            self.log().relaunches.push(failure.map(str::to_owned));
        }
    }

    /// A driver running under the paused clock, with the handles a test reads and steers.
    struct Rig {
        updates: Updates,
        fake: Fake,
        host: FakeHost,
        channel: Arc<Mutex<UpdateChannel>>,
        usable: Arc<tokio::sync::Notify>,
        task: tokio::task::JoinHandle<()>,
        metrics: std::path::PathBuf,
    }

    impl Rig {
        fn start(running: &str) -> Self {
            Self::start_with(running, Snapshot::start(None), UpdateChannel::Stable)
        }

        fn start_with(running: &str, start: Snapshot, channel: UpdateChannel) -> Self {
            let (updates, inbox) = channel_pair(start);
            let fake = Fake::default();
            let host = FakeHost::default();
            let channel = Arc::new(Mutex::new(channel));
            let usable = Arc::new(tokio::sync::Notify::new());
            let metrics = std::env::temp_dir().join(format!(
                "thalvi-update-metrics-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&metrics);
            let recorder = Recorder::in_dir(
                thalvi_desktop::metrics::RecordKinds {
                    metrics: true,
                    ..Default::default()
                },
                &metrics,
            )
            .unwrap();
            let config = {
                let channel = Arc::clone(&channel);
                move || {
                    let name = channel
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .name();
                    DesktopConfig::from_json_str(&format!(
                        r#"{{"update_channel": "{name}", "recordings": {{"metrics": true}}}}"#
                    ))
                    .unwrap()
                }
            };
            let task = tokio::spawn(drive(
                inbox,
                fake.clone(),
                host.clone(),
                config,
                v(running),
                Some(recorder),
                {
                    let usable = Arc::clone(&usable);
                    async move { usable.notified().await }
                },
            ));
            Self {
                updates,
                fake,
                host,
                channel,
                usable,
                task,
                metrics,
            }
        }

        fn usable(&self) {
            self.usable.notify_one();
        }

        fn use_channel(&self, channel: UpdateChannel) {
            *self.channel.lock().unwrap_or_else(PoisonError::into_inner) = channel;
            self.updates.config_changed();
        }

        /// Lets the driver run for `seconds` of the paused clock.
        async fn pass(seconds: u64) {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
        }

        fn phase(&self) -> Phase {
            self.updates.snapshot().phase
        }

        /// Waits until the update is in a phase `wanted` accepts, or fails after a virtual week.
        async fn until(&self, wanted: impl Fn(&Phase) -> bool) {
            let mut changes = self.updates.changes();
            tokio::time::timeout(
                Duration::from_hours(168),
                changes.wait_for(|snapshot| wanted(&snapshot.phase)),
            )
            .await
            .expect("the update never reached that phase")
            .unwrap();
        }

        fn metric_lines(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(self.metrics.join("metrics.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.metrics);
        }
    }

    fn channel_pair(start: Snapshot) -> (Updates, Inbox) {
        channel(start)
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_check_follows_the_app_being_usable_and_later_ones_come_once_a_day() {
        let rig = Rig::start("0.1.0");
        Rig::pass(3600).await;
        assert_eq!(
            rig.fake.script().checks,
            0,
            "nothing runs before the app is usable"
        );
        assert_eq!(rig.phase(), Phase::Idle);

        rig.usable();
        Rig::pass(9).await;
        assert_eq!(
            rig.fake.script().checks,
            0,
            "the check waits a moment after that"
        );
        Rig::pass(3).await;
        assert_eq!(rig.fake.script().checks, 1);
        assert_eq!(rig.phase(), Phase::UpToDate);

        Rig::pass(CHECK_EVERY.as_secs() - 100).await;
        assert_eq!(rig.fake.script().checks, 1, "not before a day has passed");
        Rig::pass(200).await;
        assert_eq!(rig.fake.script().checks, 2);
        Rig::pass(CHECK_EVERY.as_secs()).await;
        assert_eq!(rig.fake.script().checks, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_check_is_not_retried_before_the_next_one_is_due_but_the_button_retries() {
        let rig = Rig::start("0.1.0");
        rig.fake
            .script()
            .answers
            .extend([Answer::Fail(Cause::Network, "No connection."), Answer::None]);
        rig.usable();
        Rig::pass(60).await;
        assert_eq!(
            rig.phase(),
            Phase::Failed {
                message: "No connection.".to_owned()
            },
            "a failed check does not look like being up to date"
        );
        Rig::pass(CHECK_EVERY.as_secs() / 2).await;
        assert_eq!(rig.fake.script().checks, 1, "no retry storm");

        rig.updates.check_now();
        Rig::pass(5).await;
        assert_eq!(rig.fake.script().checks, 2);
        assert_eq!(rig.phase(), Phase::UpToDate);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_during_a_check_join_it() {
        let rig = Rig::start("0.1.0");
        rig.usable();
        rig.until(|phase| *phase == Phase::Checking).await;
        for _ in 0..3 {
            rig.updates.check_now();
        }
        rig.until(|phase| *phase == Phase::UpToDate).await;
        Rig::pass(60).await;
        let script = rig.fake.script();
        assert_eq!((script.checks, script.most_running), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_or_download_that_never_answers_fails_in_plain_words_and_leaves_the_app_working()
     {
        let rig = Rig::start("0.1.0");
        let started = Instant::now();
        rig.fake.script().answers.push_back(Answer::Hang);
        rig.updates.check_now();
        rig.until(|phase| matches!(phase, Phase::Failed { .. }))
            .await;
        assert_eq!(
            started.elapsed(),
            CHECK_TIMEOUT,
            "the check is cut off at its limit"
        );
        let Phase::Failed { message } = rig.phase() else {
            panic!("{:?}", rig.phase());
        };
        assert!(
            message.starts_with("Checking for updates took too long."),
            "{message}"
        );

        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        rig.fake.script().fetches.push_back(Fetch::Hang);
        rig.updates.check_now();
        rig.until(|phase| matches!(phase, Phase::Downloading { .. }))
            .await;
        let started = Instant::now();
        rig.until(|phase| matches!(phase, Phase::Failed { .. }))
            .await;
        assert_eq!(
            started.elapsed(),
            DOWNLOAD_TIMEOUT,
            "the download is cut off at its limit"
        );
        let Phase::Failed { message } = rig.phase() else {
            panic!("{:?}", rig.phase());
        };
        assert!(
            message.starts_with("Downloading the update took too long."),
            "{message}"
        );
        assert_eq!(rig.fake.script().installs.len(), 0);
        assert!(
            rig.host.log().asked.is_empty(),
            "the engine was never touched"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_reports_its_progress_and_then_waits_for_an_idle_moment() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        let mut seen = rig.updates.changes();
        rig.updates.check_now();
        let mut phases = Vec::new();
        for _ in 0..40 {
            phases.push(seen.borrow_and_update().phase.clone());
            if tokio::time::timeout(Duration::from_secs(10), seen.changed())
                .await
                .is_err()
            {
                break;
            }
        }
        phases.dedup();
        let downloading = |percent| Phase::Downloading {
            version: "0.2.0".to_owned(),
            percent,
        };
        assert_eq!(
            phases,
            [
                Phase::Idle,
                Phase::Checking,
                downloading(None),
                downloading(Some(0)),
                downloading(Some(50)),
                downloading(Some(100)),
                Phase::Ready {
                    version: "0.2.0".to_owned()
                },
            ]
        );
        let snapshot = rig.updates.snapshot();
        assert_eq!(
            snapshot.notes.as_deref(),
            Some("https://github.com/NotPellew/Thalvi/releases/tag/desktop-v9")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_feed_is_checked_where_it_enters_and_only_a_newer_allowed_version_is_fetched() {
        // (channel, running, offered, phase after, fetched)
        let cases: [(UpdateChannel, &str, &str, &str, bool); 7] = [
            (UpdateChannel::Stable, "0.1.0", "0.2.0", "ready", true),
            (UpdateChannel::Stable, "0.1.0", "0.1.0", "up to date", false),
            (UpdateChannel::Stable, "0.2.0", "0.1.9", "up to date", false),
            (
                UpdateChannel::Stable,
                "0.1.0",
                "0.2.0-dev.3",
                "failed",
                false,
            ),
            (
                UpdateChannel::Stable,
                "0.2.0-dev.3",
                "0.1.9",
                "up to date",
                false,
            ),
            (
                UpdateChannel::Development,
                "0.1.0",
                "0.2.0-dev.3",
                "ready",
                true,
            ),
            (
                UpdateChannel::Development,
                "0.2.0-dev.3",
                "0.2.0-dev.2",
                "up to date",
                false,
            ),
        ];
        for (channel, running, offered, after, fetched) in cases {
            let rig = Rig::start_with(running, Snapshot::start(None), channel);
            rig.fake.script().answers.push_back(Answer::Offer(offered));
            rig.updates.check_now();
            Rig::pass(30).await;
            let label = match rig.phase() {
                Phase::Ready { .. } => "ready",
                Phase::UpToDate => "up to date",
                Phase::Failed { .. } => "failed",
                other => panic!("{other:?}"),
            };
            let case = format!("{channel:?} on {running} offered {offered}");
            assert_eq!(label, after, "{case}");
            assert_eq!(rig.fake.script().downloads == 1, fetched, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_feed_that_is_unusable_or_has_no_package_for_this_computer_is_a_failure() {
        let rig = Rig::start("0.1.0");
        for message in [
            "The update information was not valid.",
            "The latest release has no package for this computer.",
        ] {
            rig.fake
                .script()
                .answers
                .push_back(Answer::Fail(Cause::Feed, message));
            rig.updates.check_now();
            Rig::pass(5).await;
            assert_eq!(
                rig.phase(),
                Phase::Failed {
                    message: message.to_owned()
                }
            );
        }
        assert_eq!(rig.fake.script().downloads, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_signature_that_does_not_verify_installs_nothing_and_says_so() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        rig.fake.script().fetches.push_back(Fetch::Fail(
            Cause::Signature,
            "The update failed its signature check.",
        ));
        rig.host.log().stops.push_back(Stop::Stopped);
        rig.updates.check_now();
        Rig::pass(30).await;
        assert_eq!(
            rig.phase(),
            Phase::Failed {
                message: "The update failed its signature check.".to_owned()
            }
        );
        rig.updates.quiet();
        Rig::pass(120).await;
        assert_eq!(rig.fake.script().installs.len(), 0);
        assert!(
            rig.host.log().asked.is_empty(),
            "the engine was never asked to stop"
        );
        assert_eq!(rig.host.log().relaunches.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_update_waits_while_busy_and_installs_at_the_next_idle_moment() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        rig.fake
            .script()
            .fetches
            .push_back(Fetch::Bytes(b"signed package"));
        rig.host
            .log()
            .stops
            .extend([Stop::Busy, Stop::Busy, Stop::Stopped]);
        rig.updates.check_now();
        Rig::pass(10).await;
        assert_eq!(
            rig.phase(),
            Phase::Ready {
                version: "0.2.0".to_owned()
            }
        );
        assert_eq!(
            rig.host.log().asked.len(),
            1,
            "asked once when the download ended"
        );
        assert!(
            rig.fake.script().installs.is_empty(),
            "busy: nothing installs"
        );

        rig.updates.quiet();
        Rig::pass(1).await;
        assert_eq!(rig.host.log().asked.len(), 2);
        assert_eq!(rig.fake.script().installs.len(), 0, "still busy");

        Rig::pass(INSTALL_RETRY.as_secs()).await;
        assert_eq!(
            rig.fake.script().installs,
            [("0.2.0".to_owned(), b"signed package".to_vec())]
        );
        assert_eq!(rig.host.log().relaunches, [None]);
        assert_eq!(
            rig.host.log().asked.last(),
            Some(&("0.1.0".to_owned(), "0.2.0".to_owned()))
        );
        assert!(rig.task.is_finished(), "the app is restarting");
    }

    #[tokio::test(start_paused = true)]
    async fn an_installer_that_fails_restarts_the_old_version_with_the_reason() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        rig.fake.script().install_error = Some("Thalvi cannot write to the folder.");
        rig.host.log().stops.push_back(Stop::Stopped);
        rig.updates.check_now();
        Rig::pass(10).await;
        assert_eq!(
            rig.host.log().relaunches,
            [Some("Thalvi cannot write to the folder.".to_owned())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_that_cannot_prepare_a_restart_keeps_the_app_running_and_says_why() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.push_back(Answer::Offer("0.2.0"));
        rig.host.log().stops.push_back(Stop::Refused(
            "The restart note could not be saved.".to_owned(),
        ));
        rig.updates.check_now();
        Rig::pass(10).await;
        let Phase::Failed { message } = rig.phase() else {
            panic!("{:?}", rig.phase());
        };
        assert!(
            message.contains("The restart note could not be saved."),
            "{message}"
        );
        assert_eq!(rig.fake.script().installs.len(), 0);
        assert_eq!(rig.host.log().relaunches.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_install_that_cannot_update_itself_says_so_without_asking_the_network() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().unsupported = Some("Run Thalvi as the AppImage to update it.");
        rig.usable();
        Rig::pass(30).await;
        rig.updates.check_now();
        Rig::pass(5).await;
        assert_eq!(
            rig.phase(),
            Phase::Unsupported {
                message: "Run Thalvi as the AppImage to update it.".to_owned()
            }
        );
        assert_eq!(rig.fake.script().checks, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_channels_drops_a_waiting_update_of_the_other_one_and_checks_again() {
        let rig = Rig::start_with("0.1.0", Snapshot::start(None), UpdateChannel::Development);
        rig.fake
            .script()
            .answers
            .extend([Answer::Offer("0.2.0-dev.3"), Answer::None]);
        rig.updates.check_now();
        Rig::pass(10).await;
        assert!(matches!(rig.phase(), Phase::Ready { .. }));

        rig.use_channel(UpdateChannel::Stable);
        Rig::pass(10).await;
        assert_eq!(
            rig.fake.script().asked,
            [UpdateChannel::Development, UpdateChannel::Stable]
        );
        assert_eq!(rig.phase(), Phase::UpToDate);
        rig.host.log().stops.push_back(Stop::Stopped);
        rig.updates.quiet();
        Rig::pass(60).await;
        assert!(
            rig.fake.script().installs.is_empty(),
            "the development build is gone"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_step_records_its_latency_and_outcome_without_config_or_payloads() {
        let rig = Rig::start("0.1.0");
        rig.fake.script().answers.extend([
            Answer::Fail(Cause::Network, "Offline."),
            Answer::Offer("0.2.0"),
        ]);
        rig.updates.check_now();
        rig.until(|phase| matches!(phase, Phase::Failed { .. }))
            .await;
        rig.updates.check_now();
        rig.until(|phase| matches!(phase, Phase::Ready { .. }))
            .await;
        // The writer thread is real, so give it a moment of wall time.
        let lines = {
            let started = std::time::Instant::now();
            loop {
                let lines = rig.metric_lines();
                if lines.len() >= 3 || started.elapsed() > Duration::from_secs(5) {
                    break lines;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let summary: Vec<_> = lines
            .iter()
            .map(|line| {
                (
                    line["event"].as_str().unwrap().to_owned(),
                    line["step"].as_str().unwrap().to_owned(),
                    line["outcome"].as_str().unwrap().to_owned(),
                    line["channel"].as_str().unwrap().to_owned(),
                    line["version"].as_str().map(str::to_owned),
                    line["duration_ms"].as_f64().is_some(),
                )
            })
            .collect();
        let step = |step: &str, outcome: &str, version: Option<&str>| {
            (
                "desktop_update".to_owned(),
                step.to_owned(),
                outcome.to_owned(),
                "stable".to_owned(),
                version.map(str::to_owned),
                true,
            )
        };
        assert_eq!(
            summary,
            [
                step("check", "network", None),
                step("check", "ok", Some("0.2.0")),
                step("download", "ok", Some("0.2.0")),
            ],
        );
        for line in lines {
            let mut keys: Vec<_> = line.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "channel",
                    "current",
                    "duration_ms",
                    "event",
                    "outcome",
                    "step",
                    "ts_ms",
                    "version"
                ],
                "{line}"
            );
        }
    }

    #[test]
    fn the_marker_is_read_once_and_a_broken_one_counts_as_none() {
        let dir = std::env::temp_dir().join(format!("thalvi-update-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("update.json");
        for paused in [false, true] {
            let marker = Marker::new(&v("0.1.0"), &v("0.2.0"), paused);
            marker.write(&path).unwrap();
            assert_eq!(Marker::take(&path), Some(marker), "paused {paused}");
            assert_eq!(Marker::take(&path), None, "paused {paused}: read once");
        }
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Marker::take(&path), None);
        assert!(!path.exists(), "a broken marker does not come back");

        let marker = Marker::new(&v("0.1.0"), &v("0.2.0"), true);
        marker.write(&path).unwrap();
        Marker::failed(&path, "Access is denied.");
        assert_eq!(
            Marker::take(&path),
            Some(Marker {
                error: Some("Access is denied.".to_owned()),
                ..marker
            }),
            "the reason is added and the paused state stays"
        );
        Marker::failed(&path, "Nothing to add it to.");
        assert!(!path.exists(), "no marker is made from a reason alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_install_step_is_counted_at_the_next_start_with_versions_and_channel_only() {
        let updated = Arrival::Updated {
            version: "0.2.0".to_owned(),
        };
        let kept = Arrival::NotApplied {
            wanted: "0.2.0".to_owned(),
            still: "0.1.0".to_owned(),
            error: Some("Access is denied.".to_owned()),
        };
        assert_eq!(
            updated.metric(UpdateChannel::Stable),
            (
                "ok",
                json!({"step": "install", "channel": "stable", "version": "0.2.0", "current": "0.2.0"})
            )
        );
        assert_eq!(
            kept.metric(UpdateChannel::Development),
            (
                "not_applied",
                json!({"step": "install", "channel": "development", "version": "0.2.0", "current": "0.1.0"})
            ),
            "the reason the installer gave is not a field"
        );
    }

    #[test]
    fn a_start_after_an_update_says_what_became_of_it() {
        let marker = |error: Option<&str>| Marker {
            from: "0.1.0".to_owned(),
            to: "0.2.0".to_owned(),
            paused: true,
            at_ms: now_ms(),
            error: error.map(str::to_owned),
        };
        let (arrival, _) = marker(None).arrival(&v("0.2.0"));
        assert_eq!(
            arrival,
            Arrival::Updated {
                version: "0.2.0".to_owned()
            }
        );
        let snapshot = Snapshot::start(Some(arrival));
        assert_eq!(snapshot.notice.as_deref(), Some("Updated to 0.2.0."));
        assert_eq!(snapshot.phase, Phase::Idle);
        assert_eq!(snapshot.remark().as_deref(), Some("updated to 0.2.0"));

        let (arrival, _) = marker(Some("Access is denied.")).arrival(&v("0.1.0"));
        let Phase::Failed { message } = Snapshot::start(Some(arrival)).phase else {
            panic!("an update that did not happen is a failure");
        };
        assert_eq!(
            message,
            format!(
                "The update to 0.2.0 did not finish, so Thalvi is still 0.1.0. Access is denied. Install 0.2.0 from {RELEASES_PAGE}."
            )
        );
        assert_eq!(Snapshot::start(None).notice, None);
    }

    #[test]
    fn the_tray_line_says_what_the_update_is_doing_and_what_a_click_does() {
        let at = |phase| Snapshot {
            phase,
            notes: None,
            notice: None,
        };
        let version = || "0.2.0".to_owned();
        let cases = [
            (Phase::Idle, "Check for updates", TrayClick::Check),
            (Phase::UpToDate, "Check for updates", TrayClick::Check),
            (
                Phase::Checking,
                "Checking for updates…",
                TrayClick::Settings,
            ),
            (
                Phase::Downloading {
                    version: version(),
                    percent: Some(3),
                },
                "Downloading update 0.2.0…",
                TrayClick::Settings,
            ),
            (
                Phase::Ready { version: version() },
                "Update 0.2.0 ready — installs when Thalvi is idle",
                TrayClick::Settings,
            ),
            (
                Phase::Installing { version: version() },
                "Installing update 0.2.0…",
                TrayClick::Settings,
            ),
            (
                Phase::Failed {
                    message: "x".to_owned(),
                },
                "Update failed — check again",
                TrayClick::Check,
            ),
            (
                Phase::Unsupported {
                    message: "x".to_owned(),
                },
                "Updates: see Settings",
                TrayClick::Settings,
            ),
        ];
        for (phase, text, click) in cases {
            assert_eq!(
                at(phase.clone()).tray(),
                (text.to_owned(), click),
                "{phase:?}"
            );
        }
        let updated = Snapshot::start(Some(Arrival::Updated { version: version() }));
        assert_eq!(updated.tray().0, "Updated to 0.2.0.");
        assert_eq!(
            at(Phase::Ready { version: version() }).remark().as_deref(),
            Some("update 0.2.0 ready")
        );
    }

    #[test]
    fn each_channel_has_its_own_feed() {
        assert_eq!(
            feed(UpdateChannel::Stable),
            "https://github.com/NotPellew/Thalvi/releases/latest/download/latest.json"
        );
        assert_eq!(
            feed(UpdateChannel::Development),
            "https://github.com/NotPellew/Thalvi/releases/download/desktop-dev/latest.json"
        );
    }
}
