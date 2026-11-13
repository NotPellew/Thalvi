//! The commands an outside program sends the running app, and how the app answers each with one JSON reply.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thalvi_desktop::capture::CaptureRoute;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::entries::{self, LiveKind};
use thalvi_desktop::feed::{Clip, FeedEnd};
use thalvi_desktop::hold::HoldKind;
use thalvi_desktop::metrics::{self, SUMMARY_WINDOW};
use thalvi_desktop::models::{SpeechModelInfo, SpeechProgress, WakeModelInfo};
use thalvi_desktop::transducer::SpeechChoice;
use tokio::sync::watch;

use crate::control;
use crate::runner::{AudioStatus, Control};
use crate::settings::{judge, merge_patch, settings_json};
use crate::status::{Failure, FailureKind, Status};

/// How long a control is given to show in the engine's state before it is said to have had no effect.
pub const EFFECT_WAIT: Duration = Duration::from_secs(3);

/// The commands a refusal lists, so that it need not repeat the one that was sent.
const COMMANDS: [&str; 21] = [
    "status",
    "state",
    "pause",
    "resume",
    "toggle",
    "cancel",
    "hold-start",
    "hold-end",
    "write-start",
    "write-end",
    "copy-error",
    "show",
    "settings-get",
    "settings-check",
    "settings-set",
    "metrics-summary",
    "feed",
    "models",
    "model-download",
    "speech-check",
    "quit",
];

/// The settings pages `show` opens: the ids the Settings window defines.
const PAGES: [&str; 7] = [
    "speech",
    "microphone",
    "capture",
    "feedback",
    "tasks",
    "history",
    "app",
];

/// Which catalog a model is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Catalog {
    Wake,
    Speech,
}

/// What an outside program may ask. A request is one JSON object naming its `command`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// The state word for a status bar.
    Status,
    /// What Thalvi is doing, without anything that was said.
    State,
    /// Close the microphone and drop any capture in progress.
    Pause,
    /// Open the microphone again; the wake word still starts a capture.
    Resume,
    /// Pause listening, or resume it.
    Toggle,
    /// Cancel the capture in progress.
    Cancel,
    /// The dictation key went down: start a capture that its release ends.
    HoldStart,
    /// The dictation key came up.
    HoldEnd,
    /// The writing key went down: release writes out and pastes the dictation.
    WriteStart,
    /// The writing key came up.
    WriteEnd,
    /// Copy the report of the last error shown.
    CopyError,
    /// Open the settings window, at a page when one is named.
    Show {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<String>,
    },
    /// Every setting the Settings window offers; keys only as set or not set.
    SettingsGet,
    /// Validate a merge patch over the settings without saving it.
    SettingsCheck { settings: Value },
    /// Validate, save and apply a merge patch over the settings, as a Settings save does.
    SettingsSet { settings: Value },
    /// Delays per stage and outcomes of the recent dictations.
    MetricsSummary,
    /// Play a WAV file in place of the microphone, as if it were spoken.
    Feed { path: String },
    /// The catalog models with whether each is downloaded.
    Models,
    /// Download a catalog model.
    ModelDownload { kind: Catalog, name: String },
    /// Check a speech server entry as Settings does before saving it.
    SpeechCheck { entry: Value },
    /// Quit as the tray's Quit does.
    Quit,
}

impl Request {
    /// # Errors
    /// Why `line` is no request Thalvi knows, in plain words and without repeating what it held.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(line).map_err(|_| "That request is not JSON.".to_owned())?;
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .ok_or("That request names no command.")?
            .to_owned();
        let details = value.as_object().map_or(0, serde_json::Map::len);
        let request: Self = serde_json::from_value(value).map_err(|_| {
            let known = serde_json::from_value::<Self>(json!({"command": command}));
            match known {
                Err(error) if error.to_string().starts_with("unknown variant") => {
                    format!(
                        "Thalvi does not know that command. The commands are {}.",
                        COMMANDS.join(", ")
                    )
                }
                _ => format!("The command {command} was sent without the details it needs."),
            }
        })?;
        // Serde ignores extra fields on a command that takes none.
        let takes_details = matches!(
            request,
            Self::SettingsCheck { .. }
                | Self::SettingsSet { .. }
                | Self::Feed { .. }
                | Self::ModelDownload { .. }
                | Self::SpeechCheck { .. }
                | Self::Show { .. }
        );
        if !takes_details && details > 1 {
            return Err(format!("The command {command} takes no other details."));
        }
        Ok(request)
    }

    /// The request as the one line that is sent.
    #[must_use]
    pub fn line(&self) -> String {
        // A request holds only JSON values and a name, which always serialize.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The switches that act on the operating system at once rather than with the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    StartAtLogin,
    ShowInMenu,
}

impl Switch {
    const ALL: [Self; 2] = [Self::StartAtLogin, Self::ShowInMenu];

    fn key(self) -> &'static str {
        match self {
            Self::StartAtLogin => "start_at_login",
            Self::ShowInMenu => "show_in_menu",
        }
    }

    fn of(self, config: &DesktopConfig) -> bool {
        match self {
            Self::StartAtLogin => config.start_at_login,
            Self::ShowInMenu => config.show_in_menu,
        }
    }
}

/// What the commands act on: the running app, or a stand-in for it in tests.
pub trait Running {
    fn status(&self) -> watch::Receiver<Status>;
    fn audio(&self) -> watch::Receiver<AudioStatus>;
    /// The latest failure shown, which `copy-error` can report.
    fn last_error(&self) -> Option<Failure>;
    fn control(&self, control: Control);
    /// Presses or releases the dictation key.
    fn hold(&self, kind: HoldKind, down: bool);
    /// Opens the Settings window, at `page` when one is given.
    fn show_settings(&self, page: Option<&str>);
    async fn copy_error(&self) -> Result<(), String>;
    fn config(&self) -> DesktopConfig;
    /// Saves and applies a settings form exactly as a Settings save does. `form` runs under the
    /// save lock on the config in use, so a change merged over it cannot undo a save that finished
    /// first; it returns a value for the caller and the form to save, or `None` to save nothing.
    fn save<T>(
        &self,
        form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
    ) -> Result<T, String>;
    fn set_switch(&self, switch: Switch, on: bool) -> Result<(), String>;
    /// The folder `metrics.jsonl` is written in.
    fn recordings(&self) -> Result<PathBuf, String>;
    /// Whether a file's audio is being fed in place of the microphone's.
    fn feeding(&self) -> watch::Receiver<bool>;
    /// Feeds `clip` in place of the microphone and says how it went, once it has.
    async fn feed(&self, clip: Clip) -> FeedEnd;
    /// The catalog models as Settings lists them.
    async fn models(&self) -> Result<Vec<ModelRow>, String>;
    /// Downloads a catalog model as Settings does, returning when it is ready or has failed.
    async fn download_model(&self, kind: Catalog, name: &str) -> Result<(), String>;
    /// Whether the overlay is turned on.
    fn overlay_on(&self) -> bool;
    /// Quits as the tray's Quit does.
    fn quit(&self);
}

/// A catalog model as `models` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelRow {
    kind: Catalog,
    name: &'static str,
    title: &'static str,
    /// Bytes to download in all.
    size: u64,
    downloaded: bool,
    /// How far a download has got, only while one runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    progress_percent: Option<u8>,
    /// What the Speech page says about the hardware the model runs on; speech models only.
    #[serde(skip_serializing_if = "Option::is_none")]
    runs_on: Option<String>,
}

impl ModelRow {
    #[must_use]
    pub fn wake(info: &WakeModelInfo) -> Self {
        Self {
            kind: Catalog::Wake,
            name: info.name,
            title: info.title,
            size: info.size,
            downloaded: info.cached,
            progress_percent: None,
            runs_on: None,
        }
    }

    /// `runs_on` is the Speech page's hardware line; `progress` the download running now, if any.
    #[must_use]
    pub fn speech(
        info: &SpeechModelInfo,
        runs_on: String,
        progress: Option<SpeechProgress>,
    ) -> Self {
        Self {
            kind: Catalog::Speech,
            name: info.name,
            title: info.title,
            size: info.size,
            downloaded: info.cached,
            progress_percent: progress.map(|progress| match progress {
                SpeechProgress::Verifying => 0,
                SpeechProgress::Downloading { percent } => percent,
                SpeechProgress::Done => 100,
            }),
            runs_on: Some(runs_on),
        }
    }
}

fn done(fields: Value) -> String {
    let mut reply = json!({"ok": true});
    if let (Some(reply), Value::Object(fields)) = (reply.as_object_mut(), fields) {
        reply.extend(fields);
    }
    reply.to_string()
}

fn refused(message: &str, fields: Value) -> String {
    let mut reply = json!({"ok": false, "error": message});
    if let (Some(reply), Value::Object(fields)) = (reply.as_object_mut(), fields) {
        reply.extend(fields);
    }
    reply.to_string()
}

/// Every API key in `config` and in `change`, so that none is repeated in a message.
fn secrets(config: &DesktopConfig, change: Option<&Value>) -> Vec<String> {
    fn collect(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                for (name, value) in fields {
                    match value {
                        Value::String(key) if name == "api_key" && !key.is_empty() => {
                            found.push(key.clone());
                        }
                        _ => collect(value, found),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for value in serde_json::to_value(config).iter().chain(change) {
        collect(value, &mut found);
    }
    found.sort_by_key(|key| std::cmp::Reverse(key.len()));
    found
}

/// `text` without any of `secrets`.
fn scrub(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret.as_str(), "(a key)")
    })
}

/// Answers the request on `line`.
pub async fn answer(running: &impl Running, wait: Duration, line: &str) -> String {
    match Request::parse(line) {
        Err(message) => refused(&message, Value::Null),
        Ok(request) => handle(running, wait, request).await,
    }
}

async fn handle(running: &impl Running, wait: Duration, request: Request) -> String {
    match request {
        Request::Status => done(json!({
            "state": control::word(&running.status()),
            "copy_error": running.last_error().is_some(),
        })),
        Request::State => state(running),
        Request::Show { page } => show(running, page.as_deref()),
        Request::SettingsGet => settings_get(running),
        Request::SettingsCheck { settings } => settings_check(running, &settings),
        Request::SettingsSet { settings } => settings_set(running, &settings),
        Request::MetricsSummary => metrics_summary(running).await,
        Request::Feed { path } => feed(running, &path).await,
        Request::Models => models(running).await,
        Request::ModelDownload { kind, name } => model_download(running, kind, &name).await,
        Request::SpeechCheck { entry } => speech_check(running, &entry),
        Request::Quit => {
            running.quit();
            done(json!({"took_effect": true}))
        }
        Request::Pause => pause(running, wait).await,
        Request::Resume => resume(running, wait).await,
        Request::Toggle => toggle(running, wait).await,
        Request::Cancel => cancel(running, wait).await,
        Request::HoldStart => hold_start(running, HoldKind::Dictate, wait).await,
        Request::HoldEnd => hold_end(running, HoldKind::Dictate, wait).await,
        Request::WriteStart => hold_start(running, HoldKind::Write, wait).await,
        Request::WriteEnd => hold_end(running, HoldKind::Write, wait).await,
        Request::CopyError => copy_error(running).await,
    }
}

/// What Thalvi is doing now, in the words of `state`.
fn phase(status: &watch::Receiver<Status>) -> &'static str {
    if status.has_changed().is_err() {
        return "failed";
    }
    match &*status.borrow() {
        Status::Paused => "paused",
        Status::Listening | Status::Done { .. } => "listening",
        Status::KeyboardOnly => "keyboard",
        Status::Starting { .. } | Status::Capturing { .. } => "capturing",
        Status::Pending { .. } => "pending",
        Status::Working {
            cancellable: true, ..
        }
        | Status::Transcribing { .. } => "transcribing",
        Status::Working { .. } => "delivering",
        Status::Writing { .. } | Status::Thinking { .. } => "writing",
        Status::Loading { .. } => "loading",
        Status::Failed { .. } => "failed",
    }
}

/// What the overlay shows for `status`.
fn overlay_word(status: &Status, on: bool) -> &'static str {
    if !on {
        return "hidden";
    }
    match status {
        Status::Paused
        | Status::Listening
        | Status::KeyboardOnly
        | Status::Loading {
            downloading: false, ..
        } => "hidden",
        Status::Starting { .. } | Status::Capturing { .. } => "listening",
        Status::Pending {
            waiting_for_quiet: true,
            ..
        } => "quiet",
        Status::Pending { .. } => "countdown",
        Status::Writing { .. } => "writing",
        Status::Thinking { .. } => "thinking",
        Status::Working { .. } => "working",
        Status::Transcribing { .. } => "transcribing",
        Status::Done { .. } => "delivered",
        Status::Failed { .. } => "error",
        Status::Loading { .. } => "loading",
    }
}

/// The recognizer that handles the next dictation, and whether it can, as the Speech page says.
fn recognizer(config: &DesktopConfig, phase: &str) -> Value {
    let entry = config.stt.entries.active_live();
    let (kind, model) = match &entry.kind {
        LiveKind::Realtime(_) => ("realtime", None),
        LiveKind::OpenAi(_) => ("open_ai", None),
        LiveKind::OnDevice(SpeechChoice::Catalog(model)) => ("on_device", Some(model.name())),
        LiveKind::OnDevice(SpeechChoice::Folder(_)) => ("on_device", None),
    };
    let ready = match phase {
        "paused" | "loading" | "failed" => phase,
        _ => "ready",
    };
    let mut recognizer = json!({"name": entry.name, "kind": kind, "status": ready});
    if let Some(model) = model {
        recognizer["model"] = json!(model);
    }
    recognizer
}

fn state(running: &impl Running) -> String {
    let status = running.status();
    let config = running.config();
    let secrets = secrets(&config, None);
    let phase = phase(&status);
    let mut reply = json!({
        "state": phase,
        "paused": running.audio().borrow().paused,
        "copy_error": running.last_error().is_some(),
        "feeding": *running.feeding().borrow(),
        "overlay": {"state": overlay_word(&status.borrow(), running.overlay_on())},
        "recognizer": recognizer(&config, phase),
    });
    match &*status.borrow() {
        Status::Failed {
            notice,
            headline,
            kind,
            ..
        } => {
            reply["failure"] = match kind {
                FailureKind::Delivery => json!({"headline": headline}),
                FailureKind::Other => {
                    json!({"headline": headline, "reason": scrub(notice, &secrets)})
                }
            };
            if running.overlay_on() {
                reply["overlay"]["headline"] = json!(headline);
            }
        }
        // A submit presses Enter in another program, so the user alone confirms it.
        Status::Pending {
            route: CaptureRoute::Submit,
            ..
        } => reply["awaiting_confirmation"] = json!(true),
        _ => {}
    }
    if status.has_changed().is_err() {
        reply["failure"] = json!({
            "headline": "Thalvi stopped — restart it",
            "reason": "Thalvi stopped unexpectedly. Restart it.",
        });
    }
    done(reply)
}

/// Whether a capture is under way, which a control may still cancel.
fn capturing(status: &Status) -> bool {
    matches!(
        status,
        Status::Starting { .. }
            | Status::Capturing { .. }
            | Status::Pending { .. }
            | Status::Writing { .. }
            | Status::Thinking { .. }
            | Status::Working {
                cancellable: true,
                ..
            }
            | Status::Transcribing {
                cancellable: true,
                ..
            }
    )
}

/// Whether the engine has not ended a capture's recording of the user's speech.
fn speaking(status: &Status) -> bool {
    matches!(status, Status::Starting { .. } | Status::Capturing { .. })
}

async fn reached<T>(
    receiver: &mut watch::Receiver<T>,
    wait: Duration,
    reached: impl FnMut(&T) -> bool,
) -> bool {
    tokio::time::timeout(wait, receiver.wait_for(reached))
        .await
        .is_ok_and(|seen| seen.is_ok())
}

fn took_effect(running: &impl Running) -> String {
    done(json!({"took_effect": true, "state": phase(&running.status())}))
}

fn no_effect(running: &impl Running, why: &str) -> String {
    refused(
        why,
        json!({"took_effect": false, "state": phase(&running.status())}),
    )
}

async fn pause(running: &impl Running, wait: Duration) -> String {
    let mut audio = running.audio();
    if audio.borrow().paused {
        return no_effect(running, "Thalvi is already paused.");
    }
    running.control(Control::SetListening(false));
    if reached(&mut audio, wait, |audio| audio.paused).await {
        took_effect(running)
    } else {
        no_effect(running, "Thalvi did not pause.")
    }
}

async fn resume(running: &impl Running, wait: Duration) -> String {
    let mut audio = running.audio();
    if !audio.borrow().paused {
        return no_effect(running, "Thalvi is already listening.");
    }
    running.control(Control::SetListening(true));
    if reached(&mut audio, wait, |audio| !audio.paused).await {
        took_effect(running)
    } else {
        no_effect(running, "Thalvi did not resume.")
    }
}

async fn toggle(running: &impl Running, wait: Duration) -> String {
    let mut audio = running.audio();
    let was = audio.borrow().paused;
    running.control(Control::ToggleListening);
    if reached(&mut audio, wait, |audio| audio.paused != was).await {
        took_effect(running)
    } else {
        no_effect(
            running,
            "Thalvi did not change between listening and paused.",
        )
    }
}

async fn cancel(running: &impl Running, wait: Duration) -> String {
    let mut status = running.status();
    let mut feeding = running.feeding();
    let (busy, delivering) = {
        let current = status.borrow();
        (
            capturing(&current),
            matches!(
                &*current,
                Status::Working {
                    cancellable: false,
                    ..
                }
            ),
        )
    };
    let feeding_now = *feeding.borrow();
    if delivering {
        return no_effect(
            running,
            "The text is being delivered, which cannot be cancelled.",
        );
    }
    if !busy && !feeding_now {
        return no_effect(running, "There is no capture to cancel.");
    }
    running.control(Control::Cancel);
    if reached(&mut status, wait, |status| !capturing(status)).await
        && reached(&mut feeding, wait, |feeding| !*feeding).await
    {
        took_effect(running)
    } else {
        no_effect(running, "The capture did not stop.")
    }
}

async fn hold_start(running: &impl Running, kind: HoldKind, wait: Duration) -> String {
    let mut status = running.status();
    let refusal = match &*status.borrow() {
        Status::Paused => Some("Thalvi is paused. Resume it first."),
        Status::Loading { .. } => Some("Thalvi is still starting. Try again in a moment."),
        current if capturing(current) => Some("A capture is already running."),
        Status::Working { .. } => Some("The last text is still being delivered."),
        _ => None,
    };
    // Every press reaches the key input, as on Windows, so its release finds a press to end;
    // the engine refuses the ones it cannot act on.
    running.hold(kind, true);
    if let Some(refusal) = refusal {
        return no_effect(running, refusal);
    }
    if reached(&mut status, wait, |status| {
        capturing(status) || matches!(status, Status::Working { .. })
    })
    .await
    {
        took_effect(running)
    } else {
        no_effect(running, "Thalvi did not start a capture.")
    }
}

async fn hold_end(running: &impl Running, kind: HoldKind, wait: Duration) -> String {
    let mut status = running.status();
    let holding = speaking(&status.borrow());
    running.hold(kind, false);
    if !holding {
        return no_effect(running, "No capture is being taken.");
    }
    if reached(&mut status, wait, |status| !speaking(status)).await {
        took_effect(running)
    } else {
        no_effect(running, "The capture did not end.")
    }
}

async fn copy_error(running: &impl Running) -> String {
    if running.last_error().is_none() {
        return no_effect(running, "Thalvi has shown no error to copy.");
    }
    match running.copy_error().await {
        Ok(()) => took_effect(running),
        Err(reason) => no_effect(running, &reason),
    }
}

fn show(running: &impl Running, page: Option<&str>) -> String {
    if page.is_some_and(|page| !PAGES.contains(&page)) {
        return refused(
            &format!(
                "That is not a settings page. The pages are {}.",
                PAGES.join(", ")
            ),
            json!({"took_effect": false}),
        );
    }
    running.show_settings(page);
    done(json!({"took_effect": true}))
}

async fn feed(running: &impl Running, path: &str) -> String {
    let refusal = {
        let status = running.status();
        let current = status.borrow();
        match &*current {
            Status::Paused => Some("Thalvi is paused."),
            Status::Loading { .. } => Some("Thalvi is still starting. Try again in a moment."),
            current if capturing(current) => Some("A capture is running."),
            Status::Working { .. } => Some("The last text is still being delivered."),
            _ if *running.feeding().borrow() => Some("A file is already being fed."),
            _ => None,
        }
    };
    if let Some(refusal) = refusal {
        return no_effect(running, refusal);
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return no_effect(running, "The path of the audio file must be absolute.");
    }
    let max = running.config().capture_limits.max();
    let clip = tokio::task::spawn_blocking(move || Clip::from_wav(&path, max)).await;
    let clip = match clip {
        Ok(Ok(clip)) => clip,
        Ok(Err(error)) => return no_effect(running, &error.to_string()),
        Err(_) => return no_effect(running, "The audio file could not be read."),
    };
    match running.feed(clip).await {
        FeedEnd::Played => took_effect(running),
        FeedEnd::Cancelled => no_effect(
            running,
            "The file was not fed to the end: it was cancelled.",
        ),
        FeedEnd::Refused(reason) => no_effect(running, reason),
    }
}

async fn models(running: &impl Running) -> String {
    match running.models().await {
        Ok(models) => done(json!({"models": models})),
        Err(error) => refused(&error, Value::Null),
    }
}

async fn model_download(running: &impl Running, kind: Catalog, name: &str) -> String {
    match running.download_model(kind, name).await {
        Ok(()) => done(json!({"downloaded": true})),
        Err(error) => refused(&error, json!({"downloaded": false})),
    }
}

fn speech_check(running: &impl Running, entry: &Value) -> String {
    match entries::check(entry) {
        Ok(()) => done(json!({"ready": true})),
        Err(error) => refused(
            &scrub(&error, &secrets(&running.config(), Some(entry))),
            json!({"ready": false}),
        ),
    }
}

fn settings_get(running: &impl Running) -> String {
    let config = running.config();
    match settings_json(&config) {
        Ok(mut settings) => {
            if let Some(settings) = settings.as_object_mut() {
                for switch in Switch::ALL {
                    settings.insert(switch.key().to_owned(), json!(switch.of(&config)));
                }
            }
            done(json!({"settings": settings}))
        }
        Err(error) => refused(&scrub(&error, &secrets(&config, None)), Value::Null),
    }
}

/// A change to the settings, understood and checked, but not yet saved.
struct Plan {
    /// The settings form with the change merged in.
    merged: Value,
    config: DesktopConfig,
    current: DesktopConfig,
    switches: Vec<(Switch, bool)>,
}

/// Merges `change` over the settings now in use and validates the result as Settings does.
fn plan(running: &impl Running, change: &Value) -> Result<Plan, String> {
    let paused = running.audio().borrow().paused;
    plan_over(running.config(), paused, change)
}

/// Merges `change` over `current` and validates the result as Settings does.
fn plan_over(current: DesktopConfig, paused: bool, change: &Value) -> Result<Plan, String> {
    let Value::Object(fields) = change else {
        return Err("The settings change must be a JSON object.".to_owned());
    };
    let mut fields = fields.clone();
    let mut switches = Vec::new();
    for switch in Switch::ALL {
        if let Some(on) = fields.remove(switch.key()) {
            let Value::Bool(on) = on else {
                return Err(format!("{} must be true or false.", switch.key()));
            };
            switches.push((switch, on));
        }
    }
    let mut merged = settings_json(&current)?;
    merge_patch(&mut merged, &Value::Object(fields));
    let config = judge(merged.clone(), &current, paused)?;
    Ok(Plan {
        merged,
        config,
        current,
        switches,
    })
}

fn settings_check(running: &impl Running, change: &Value) -> String {
    match plan(running, change) {
        Ok(_) => done(json!({"valid": true})),
        Err(error) => refused(
            &scrub(&error, &secrets(&running.config(), Some(change))),
            json!({"valid": false}),
        ),
    }
}

fn settings_set(running: &impl Running, change: &Value) -> String {
    let secrets = secrets(&running.config(), Some(change));
    let plan = running.save(|current| {
        let paused = running.audio().borrow().paused;
        let plan = plan_over(current.clone(), paused, change)?;
        let form = (plan.config != plan.current).then(|| plan.merged.clone());
        Ok((plan, form))
    });
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return refused(&scrub(&error, &secrets), json!({"changed": false})),
    };
    let settings_changed = plan.config != plan.current;
    // What is under way keeps the settings it started with.
    let applies = if capturing(&running.status().borrow())
        || matches!(&*running.status().borrow(), Status::Working { .. })
    {
        "next_capture"
    } else {
        "now"
    };
    let mut switched = false;
    for (switch, on) in plan.switches {
        if switch.of(&plan.current) == on {
            continue;
        }
        if let Err(error) = running.set_switch(switch, on) {
            return refused(
                &scrub(&error, &secrets),
                json!({"changed": settings_changed || switched, "saved": settings_changed}),
            );
        }
        switched = true;
    }
    let mut reply = json!({"changed": settings_changed || switched});
    if settings_changed {
        reply["applies"] = json!(applies);
    }
    done(reply)
}

async fn metrics_summary(running: &impl Running) -> String {
    if !running.config().recordings.metrics {
        return done(json!({
            "enabled": false,
            "message": "Metrics are off. Turn them on in Settings → App, or send settings set with {\"recordings\": {\"metrics\": true}}.",
        }));
    }
    let dir = match running.recordings() {
        Ok(dir) => dir,
        Err(error) => return refused(&error, Value::Null),
    };
    let summary = tokio::task::spawn_blocking(move || metrics::summarize(&dir, SUMMARY_WINDOW))
        .await
        .unwrap_or_default();
    done(json!({
        "enabled": true,
        "window_hours": SUMMARY_WINDOW.as_secs() / 3600,
        "stages": summary.stages,
        "captures": summary.captures,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use thalvi_desktop::config::Look;
    use thalvi_desktop::feed::{Clip, FeedEnd};
    use thalvi_desktop::hold::{HoldInput, Presses};
    #[cfg(target_os = "linux")]
    use thalvi_desktop::host::RequestServer;
    use thalvi_desktop::writing::DEFAULT_PROMPT;

    use super::*;
    use crate::app::SyncedConfig;
    use crate::settings::Store;
    use crate::status::FailureKind;

    const SECRET: &str = "SECRET-KEY-1";

    /// Long enough for the fake engine to answer, short enough for a control it ignores.
    const WAIT: Duration = Duration::from_millis(150);

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The engine and the app around it, as far as the commands see them. The engine answers a
    /// control at once, unless told to ignore them.
    struct Fake {
        status: watch::Sender<Status>,
        audio: watch::Sender<AudioStatus>,
        saves: SyncedConfig,
        /// A Settings save that lands just before the agent's next save.
        settings_save_first: Mutex<Option<Value>>,
        failure: Mutex<Option<Failure>>,
        sent: Mutex<Vec<String>>,
        switches: Mutex<Vec<(Switch, bool)>>,
        shown: AtomicUsize,
        pages: Mutex<Vec<String>>,
        feeding: watch::Sender<bool>,
        /// Clips fed, as "Clip(n frames)".
        fed: Mutex<Vec<String>>,
        /// Ends the feed in flight when sent to; without it a feed is over at once.
        feed_release: Mutex<Option<tokio::sync::oneshot::Sender<FeedEnd>>>,
        /// The next feed stays in flight until cancelled or released.
        holding: AtomicBool,
        feed_ending: Mutex<FeedEnd>,
        downloaded: Mutex<Vec<(Catalog, String)>>,
        quit: AtomicUsize,
        overlay: AtomicBool,
        copy_refused: AtomicBool,
        ignoring: AtomicBool,
        keys: HoldInput,
        presses: Mutex<Presses>,
        folder: PathBuf,
    }

    impl Fake {
        fn new(name: &str, config: &str) -> Arc<Self> {
            let folder =
                std::env::temp_dir().join(format!("thalvi-agent-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&folder);
            std::fs::create_dir_all(&folder).unwrap();
            let config = DesktopConfig::from_json_str(config).unwrap();
            let (keys, state) = HoldInput::new();
            Arc::new(Self {
                status: watch::channel(Status::Listening).0,
                audio: watch::channel(AudioStatus {
                    selected: None,
                    active: None,
                    paused: false,
                })
                .0,
                saves: SyncedConfig::new(
                    Store::new(Some(folder.join("desktop.json"))).unwrap(),
                    config,
                ),
                settings_save_first: Mutex::new(None),
                failure: Mutex::new(None),
                sent: Mutex::new(Vec::new()),
                switches: Mutex::new(Vec::new()),
                shown: AtomicUsize::new(0),
                pages: Mutex::new(Vec::new()),
                feeding: watch::channel(false).0,
                fed: Mutex::new(Vec::new()),
                feed_release: Mutex::new(None),
                holding: AtomicBool::new(false),
                feed_ending: Mutex::new(FeedEnd::Played),
                downloaded: Mutex::new(Vec::new()),
                quit: AtomicUsize::new(0),
                overlay: AtomicBool::new(true),
                copy_refused: AtomicBool::new(false),
                ignoring: AtomicBool::new(false),
                keys,
                presses: Mutex::new(Presses::new(state)),
                folder,
            })
        }

        fn set_status(&self, status: Status) {
            self.status.send_replace(status);
        }

        fn current(&self) -> Status {
            self.status.borrow().clone()
        }

        fn sent(&self) -> Vec<String> {
            lock(&self.sent).clone()
        }

        fn pause_audio(&self, paused: bool) {
            self.audio.send_modify(|audio| audio.paused = paused);
            self.set_status(if paused {
                Status::Paused
            } else {
                Status::Listening
            });
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }

    impl Running for Arc<Fake> {
        fn status(&self) -> watch::Receiver<Status> {
            self.status.subscribe()
        }

        fn audio(&self) -> watch::Receiver<AudioStatus> {
            self.audio.subscribe()
        }

        fn last_error(&self) -> Option<Failure> {
            lock(&self.failure).clone()
        }

        fn control(&self, control: Control) {
            lock(&self.sent).push(format!("{control:?}"));
            if self.ignoring.load(Ordering::SeqCst) {
                return;
            }
            let paused = self.audio.borrow().paused;
            match control {
                Control::SetListening(on) => self.pause_audio(!on),
                Control::ToggleListening => self.pause_audio(!paused),
                Control::Cancel => {
                    self.set_status(Status::Listening);
                    self.feeding.send_replace(false);
                    if let Some(release) = lock(&self.feed_release).take() {
                        let _ = release.send(FeedEnd::Cancelled);
                    }
                }
                _ => {}
            }
        }

        fn hold(&self, kind: HoldKind, down: bool) {
            let key = match kind {
                HoldKind::Dictate => "hold",
                HoldKind::Write => "write",
            };
            lock(&self.sent).push(format!("{key} {down}"));
            match kind {
                HoldKind::Dictate => self.keys.set(down),
                HoldKind::Write => self.keys.set_write(down),
            }
            if self.ignoring.load(Ordering::SeqCst) {
                return;
            }
            match (down, self.current()) {
                (true, Status::Listening) => self.set_status(Status::Capturing {
                    target: Some("kitty".to_owned()),
                    committed: String::new(),
                    partial: None,
                    notice: None,
                }),
                (false, Status::Capturing { .. }) => self.set_status(Status::Working {
                    target: None,
                    route: CaptureRoute::Paste,
                    cancellable: false,
                }),
                _ => {}
            }
        }

        fn show_settings(&self, page: Option<&str>) {
            self.shown.fetch_add(1, Ordering::SeqCst);
            lock(&self.pages).extend(page.map(str::to_owned));
        }

        fn copy_error(&self) -> impl Future<Output = Result<(), String>> {
            std::future::ready(if self.copy_refused.load(Ordering::SeqCst) {
                Err("The clipboard is locked.".to_owned())
            } else {
                Ok(())
            })
        }

        fn config(&self) -> DesktopConfig {
            self.saves.config()
        }

        fn save<T>(
            &self,
            form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
        ) -> Result<T, String> {
            let paused = self.audio.borrow().paused;
            let settings_save = lock(&self.settings_save_first).take();
            if let Some(settings) = settings_save {
                self.saves.save_form(
                    |_| Ok(((), Some(settings))),
                    paused,
                    |_, config, write| {
                        write()?;
                        self.saves.set(config);
                        Ok(())
                    },
                )?;
            }
            self.saves
                .save_form(form, paused, |_, config, write| {
                    write()?;
                    self.saves.set(config);
                    Ok(())
                })
                .map(|(value, _)| value)
        }

        fn set_switch(&self, switch: Switch, on: bool) -> Result<(), String> {
            lock(&self.switches).push((switch, on));
            let mut config = self.saves.config();
            config.start_at_login = on;
            self.saves.set(&config);
            Ok(())
        }

        fn recordings(&self) -> Result<PathBuf, String> {
            Ok(self.folder.join("recordings"))
        }

        fn feeding(&self) -> watch::Receiver<bool> {
            self.feeding.subscribe()
        }

        async fn feed(&self, clip: Clip) -> FeedEnd {
            lock(&self.fed).push(format!("{clip:?}"));
            if !self.holding.swap(false, Ordering::SeqCst) {
                return *lock(&self.feed_ending);
            }
            let (release, ended) = tokio::sync::oneshot::channel();
            *lock(&self.feed_release) = Some(release);
            self.feeding.send_replace(true);
            let end = ended.await.unwrap_or(FeedEnd::Cancelled);
            self.feeding.send_replace(false);
            end
        }

        fn models(&self) -> impl Future<Output = Result<Vec<ModelRow>, String>> {
            let wake = WakeModelInfo {
                name: "hey_thalvi",
                title: "Hey Thalvi",
                size: 1_000,
                cached: true,
                threshold: 0.5,
            };
            let speech = SpeechModelInfo {
                name: "nemotron_160ms",
                title: "Nemotron",
                description: "d",
                size: 650,
                source: "s",
                license: "l",
                languages: &["en"],
                cached: false,
            };
            std::future::ready(Ok(vec![
                ModelRow::wake(&wake),
                ModelRow::speech(
                    &speech,
                    "Will run on the CPU".to_owned(),
                    Some(SpeechProgress::Downloading { percent: 42 }),
                ),
            ]))
        }

        fn download_model(
            &self,
            kind: Catalog,
            name: &str,
        ) -> impl Future<Output = Result<(), String>> {
            if name == "nope" {
                return std::future::ready(Err(
                    "nope is not a speech model Thalvi can download.".to_owned()
                ));
            }
            lock(&self.downloaded).push((kind, name.to_owned()));
            std::future::ready(Ok(()))
        }

        fn overlay_on(&self) -> bool {
            self.overlay.load(Ordering::SeqCst)
        }

        fn quit(&self) {
            self.quit.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A running app to ask: over the socket where there is one, else straight to the answer.
    struct Rig {
        fake: Arc<Fake>,
        _runtime: tokio::runtime::Runtime,
        #[cfg(not(target_os = "linux"))]
        handle: tokio::runtime::Handle,
        #[cfg(target_os = "linux")]
        socket: (PathBuf, RequestServer),
    }

    impl Rig {
        fn new(name: &str, config: &str) -> Self {
            let fake = Fake::new(name, config);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            #[cfg(target_os = "linux")]
            let socket = {
                use std::os::unix::fs::PermissionsExt;

                let dir = fake.folder.join("run");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                let engine = Arc::clone(&fake);
                let handle = runtime.handle().clone();
                let server = thalvi_desktop::host::Desktop::serve_requests_in(&dir, move |line| {
                    handle.block_on(answer(&engine, WAIT, line))
                })
                .unwrap();
                (dir, server)
            };
            Self {
                fake,
                #[cfg(not(target_os = "linux"))]
                handle: runtime.handle().clone(),
                _runtime: runtime,
                #[cfg(target_os = "linux")]
                socket,
            }
        }

        /// The reply to `line`, as JSON.
        fn ask_line(&self, line: &str) -> Value {
            #[cfg(target_os = "linux")]
            let reply = thalvi_desktop::host::Desktop::send_request_in(
                &self.socket.0,
                line,
                Duration::from_secs(10),
            )
            .unwrap()
            .unwrap();
            #[cfg(not(target_os = "linux"))]
            let reply = self.handle.block_on(answer(&self.fake, WAIT, line));
            serde_json::from_str(&reply).unwrap()
        }

        fn ask(&self, request: &Request) -> Value {
            self.ask_line(&request.line())
        }

        fn set(&self, change: Value) -> Value {
            self.ask(&Request::SettingsSet { settings: change })
        }
    }

    fn entry(id: &str, kind: &str, active: bool, key: Option<&str>) -> Value {
        let mut entry = json!({"kind": kind, "id": id, "name": id, "active": active,
            "address": if kind == "realtime" { "ws://127.0.0.1:8012" } else { "http://127.0.0.1:9/v1" },
            "model": "m"});
        if let Some(key) = key {
            entry["api_key"] = json!(key);
        }
        entry
    }

    fn capturing_status() -> Status {
        Status::Capturing {
            target: Some("kitty".to_owned()),
            committed: "my password is".to_owned(),
            partial: Some("hunter2".to_owned()),
            notice: None,
        }
    }

    #[test]
    fn settings_are_read_changed_and_read_back_with_a_speech_entry_and_its_key_never_returned() {
        let rig = Rig::new("round-trip", "{}");
        let before = rig.ask(&Request::SettingsGet);
        assert_eq!(before["ok"], true);
        let mut entries = before["settings"]["stt"]["entries"].clone();
        entries[0]["active"] = json!(false);
        entries
            .as_array_mut()
            .unwrap()
            .push(entry("cybrex2", "realtime", true, Some(SECRET)));

        let set = rig.set(json!({"stt": {"entries": entries, "phrases": ["Cybrex2"]}}));
        assert_eq!(set, json!({"ok": true, "changed": true, "applies": "now"}));

        let after = rig.ask(&Request::SettingsGet);
        assert!(!after.to_string().contains(SECRET), "{after}");
        let entries = &after["settings"]["stt"]["entries"];
        assert_eq!(entries[1]["id"], "cybrex2");
        assert_eq!(
            (entries[1]["active"].clone(), entries[1]["has_key"].clone()),
            (json!(true), json!(true))
        );
        assert!(entries[1].get("api_key").is_none());
        assert_eq!(after["settings"]["stt"]["phrases"], json!(["Cybrex2"]));
        let config = rig.fake.config();
        assert_eq!(config.stt.entries.active_live().id, "cybrex2");
        assert_eq!(
            config.stt.entries.key_of("cybrex2"),
            SECRET,
            "the key is kept"
        );
        assert_eq!(
            rig.fake.saves.store.load().unwrap(),
            config,
            "and written for the next start"
        );

        // A later change that leaves the entries alone keeps the key.
        let again = rig.set(json!({"look": "holo"}));
        assert_eq!(again["ok"], true);
        assert_eq!(rig.fake.config().stt.entries.key_of("cybrex2"), SECRET);
        let unchanged = rig.set(json!({"look": "holo"}));
        assert_eq!(unchanged, json!({"ok": true, "changed": false}));
    }

    #[test]
    fn the_writing_prompt_is_read_checked_and_set_like_settings_with_the_key_masked() {
        let rig = Rig::new(
            "prompt",
            &format!(
                r#"{{"look": "neon", "writing": {{"url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "{SECRET}"}}}}"#
            ),
        );
        let get = || rig.ask(&Request::SettingsGet);
        assert_eq!(get()["settings"]["writing"]["prompt"], DEFAULT_PROMPT);
        assert_eq!(get()["settings"]["writing_default_prompt"], DEFAULT_PROMPT);

        let blank = json!({"writing": {"prompt": "  "}});
        let message = "writing.prompt must not be blank. Restore the default to start over.";
        let check = rig.ask(&Request::SettingsCheck {
            settings: blank.clone(),
        });
        assert_eq!(
            check,
            json!({"ok": false, "error": message, "valid": false})
        );
        let set = rig.set(blank);
        assert_eq!(set["error"], message);
        assert_eq!(rig.fake.config().writing.unwrap().prompt(), DEFAULT_PROMPT);

        let set = rig.set(json!({"writing": {"prompt": "Be brief."}}));
        assert_eq!(set, json!({"ok": true, "changed": true, "applies": "now"}));
        let config = rig.fake.config();
        let writing = config.writing.as_ref().unwrap();
        assert_eq!((writing.prompt(), writing.key()), ("Be brief.", SECRET));
        assert_eq!(config.look, Look::Neon, "unrelated settings are kept");
        let shown = get();
        assert_eq!(shown["settings"]["writing"]["prompt"], "Be brief.");
        assert!(!shown.to_string().contains(SECRET), "{shown}");
        assert_eq!(rig.fake.saves.store.load().unwrap(), config);
    }

    #[test]
    fn writing_openings_are_checked_and_set_like_settings() {
        let rig = Rig::new(
            "openings",
            r#"{"writing": {"url": "http://127.0.0.1:9/v1", "model": "m"}}"#,
        );
        let rule = |phrase: &str, prompt: &str| json!({"phrase": phrase, "prompt": prompt});
        let refusals = [
            (
                vec![rule("Write me", "a"), rule("write, me", "b")],
                "writing.openings has “write me” twice.",
            ),
            (
                vec![rule("write me", " ")],
                "writing.openings has a blank prompt for “write me”.",
            ),
            (
                vec![rule("", "a")],
                "writing.openings needs a phrase with at least one word in every rule.",
            ),
        ];
        for (openings, message) in refusals {
            let change = json!({"writing": {"openings": openings}});
            let check = rig.ask(&Request::SettingsCheck {
                settings: change.clone(),
            });
            assert_eq!(
                check,
                json!({"ok": false, "error": message, "valid": false})
            );
            assert_eq!(rig.set(change)["error"], message);
        }

        let set = rig.set(json!({"writing": {"openings": [rule("Write me an email", "Email.")]}}));
        assert_eq!(set, json!({"ok": true, "changed": true, "applies": "now"}));
        let shown = rig.ask(&Request::SettingsGet);
        assert_eq!(
            shown["settings"]["writing"]["openings"],
            json!([rule("write me an email", "Email.")])
        );
        assert_eq!(rig.fake.saves.store.load().unwrap(), rig.fake.config());

        rig.set(json!({"writing": {"openings": []}}));
        let shown = rig.ask(&Request::SettingsGet);
        assert!(shown["settings"]["writing"].get("openings").is_none());
    }

    #[test]
    fn a_refused_change_says_what_settings_says_and_changes_nothing() {
        let rig = Rig::new("refused", "{}");
        let before = rig.fake.config();
        let message = "The paste pause must be a number of seconds from 0 to 5.";
        let change = json!({"ending_pause": {"paste": 9}});

        let set = rig.set(change.clone());
        assert_eq!(
            set,
            json!({"ok": false, "error": message, "changed": false})
        );
        let check = rig.ask(&Request::SettingsCheck { settings: change });
        assert_eq!(
            check,
            json!({"ok": false, "error": message, "valid": false})
        );
        assert_eq!(rig.fake.config(), before);
        assert!(
            !rig.fake.folder.join("desktop.json").exists(),
            "nothing was written"
        );

        let fine = rig.ask(&Request::SettingsCheck {
            settings: json!({"look": "neon"}),
        });
        assert_eq!(fine, json!({"ok": true, "valid": true}));
        assert_eq!(rig.fake.config(), before, "a check saves nothing");

        for (change, error) in [
            (json!([1]), "The settings change must be a JSON object."),
            (
                json!({"start_at_login": "yes"}),
                "start_at_login must be true or false.",
            ),
            (
                json!({"stt": {"entries": [entry("x", "realtime", true, None), entry("x", "realtime", false, None)]}}),
                "x",
            ),
        ] {
            let reply = rig.set(change.clone());
            assert_eq!(reply["ok"], false, "{change}");
            assert!(reply["error"].as_str().unwrap().contains(error), "{reply}");
        }
        assert_eq!(rig.fake.config(), before);
    }

    #[test]
    fn the_microphone_changes_only_while_paused_and_a_change_waits_for_a_capture_to_end() {
        let rig = Rig::new("microphone", "{}");
        let refusal =
            "Pause listening before changing the microphone. Turn listening off, then save again.";
        let mic = json!({"microphone": "USB Mic"});
        let refused = rig.set(mic.clone());
        assert_eq!(refused["error"], refusal);
        assert_eq!(rig.ask(&Request::Pause)["ok"], true);
        assert_eq!(rig.set(mic)["ok"], true);
        assert_eq!(rig.fake.config().microphone.as_deref(), Some("USB Mic"));

        rig.fake.pause_audio(false);
        rig.fake.set_status(capturing_status());
        let during = rig.set(json!({"look": "neon"}));
        assert_eq!(
            during,
            json!({"ok": true, "changed": true, "applies": "next_capture"})
        );
    }

    #[test]
    fn an_agent_change_keeps_a_settings_save_that_finished_while_it_was_planned() {
        let rig = Rig::new("lost-update", "{}");
        assert_eq!(rig.ask(&Request::Pause)["ok"], true);
        let mut form = settings_json(&rig.fake.config()).unwrap();
        form["look"] = json!("holo");
        *lock(&rig.fake.settings_save_first) = Some(form);

        let set = rig.set(json!({"microphone": "USB Mic"}));

        assert_eq!(set["ok"], true, "{set}");
        let config = rig.fake.config();
        assert_eq!(config.look, Look::Holo, "the Settings save is kept");
        assert_eq!(config.microphone.as_deref(), Some("USB Mic"));
        assert_eq!(rig.fake.saves.store.load().unwrap(), config);
    }

    #[test]
    fn a_control_answers_whether_it_took_effect_and_what_state_it_left() {
        let rig = Rig::new("control", "{}");
        let ask = |request: Request| rig.ask(&request);
        let took = |state: &str| json!({"ok": true, "took_effect": true, "state": state});
        let missed = |error: &str, state: &str| json!({"ok": false, "took_effect": false, "error": error, "state": state});

        assert_eq!(ask(Request::Pause), took("paused"));
        assert_eq!(
            ask(Request::Pause),
            missed("Thalvi is already paused.", "paused")
        );
        assert_eq!(
            ask(Request::HoldStart),
            missed("Thalvi is paused. Resume it first.", "paused")
        );
        assert_eq!(ask(Request::Resume), took("listening"));
        assert_eq!(
            ask(Request::Resume),
            missed("Thalvi is already listening.", "listening")
        );
        assert_eq!(ask(Request::Toggle), took("paused"));
        assert_eq!(ask(Request::Toggle), took("listening"));
        assert_eq!(
            ask(Request::Cancel),
            missed("There is no capture to cancel.", "listening")
        );
        assert_eq!(
            ask(Request::HoldEnd),
            missed("No capture is being taken.", "listening")
        );

        assert_eq!(ask(Request::HoldStart), took("capturing"));
        assert_eq!(
            ask(Request::HoldStart),
            missed("A capture is already running.", "capturing")
        );
        assert_eq!(ask(Request::Cancel), took("listening"));
        assert_eq!(ask(Request::HoldStart), took("capturing"));
        assert_eq!(ask(Request::HoldEnd), took("delivering"));
        assert_eq!(
            ask(Request::Cancel),
            missed(
                "The text is being delivered, which cannot be cancelled.",
                "delivering"
            )
        );

        // An engine that does nothing with a control is told so, rather than believed.
        rig.fake.set_status(Status::Listening);
        rig.fake.ignoring.store(true, Ordering::SeqCst);
        assert_eq!(
            ask(Request::Pause),
            missed("Thalvi did not pause.", "listening")
        );
        assert_eq!(
            ask(Request::HoldStart),
            missed("Thalvi did not start a capture.", "listening")
        );

        assert_eq!(
            ask(Request::CopyError),
            missed("Thalvi has shown no error to copy.", "listening")
        );
        *lock(&rig.fake.failure) = Some(Failure {
            notice: "No.".to_owned(),
            headline: "No",
            causes: Vec::new(),
            kind: FailureKind::Other,
        });
        assert_eq!(ask(Request::CopyError), took("listening"));
        rig.fake.copy_refused.store(true, Ordering::SeqCst);
        assert_eq!(
            ask(Request::CopyError),
            missed("The clipboard is locked.", "listening")
        );
        assert_eq!(
            ask(Request::Show { page: None }),
            json!({"ok": true, "took_effect": true})
        );
        assert_eq!(rig.fake.shown.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn writing_controls_use_the_write_key_and_writing_can_be_cancelled() {
        let rig = Rig::new("write-control", "{}");
        assert_eq!(rig.ask(&Request::WriteStart)["took_effect"], true);
        assert_eq!(rig.ask(&Request::WriteEnd)["took_effect"], true);
        assert_eq!(*lock(&rig.fake.sent), ["write true", "write false"]);
        rig.fake.set_status(Status::Writing {
            target: None,
            text: "private words".to_owned(),
        });
        let refused = rig.ask(&Request::HoldStart);
        assert_eq!(refused["took_effect"], false);
        assert_eq!(rig.ask(&Request::Cancel)["took_effect"], true);
        assert_eq!(rig.ask(&Request::Status)["state"], "listening");
    }

    #[test]
    fn a_key_pressed_during_the_other_keys_hold_starts_a_hold_when_pressed_again() {
        // Each key's press and release reach the key input, even while the other key's hold
        // refuses the press, so the release is never mistaken for one that overtook a press.
        for (first, second, name) in [
            (Request::WriteStart, Request::HoldStart, "write first"),
            (Request::HoldStart, Request::WriteStart, "hold first"),
        ] {
            let (first_end, second_end, expected) = match first {
                Request::WriteStart => (Request::WriteEnd, Request::HoldEnd, HoldKind::Dictate),
                _ => (Request::HoldEnd, Request::WriteEnd, HoldKind::Write),
            };
            let rig = Rig::new("cross-hold", "{}");
            assert_eq!(rig.ask(&first)["took_effect"], true, "{name}");
            assert!(lock(&rig.fake.presses).take().is_some(), "{name}");
            let refused = rig.ask(&second);
            assert_eq!(
                refused["error"], "A capture is already running.",
                "{name}: the other key's press is still refused in words"
            );
            assert_eq!(rig.ask(&first_end)["took_effect"], true, "{name}");
            assert_eq!(rig.ask(&second_end)["took_effect"], false, "{name}");
            rig.fake.set_status(Status::Listening);
            let again = rig.ask(&second);
            assert_eq!(again["took_effect"], true, "{name}: {again}");
            let press = lock(&rig.fake.presses).take();
            assert_eq!(
                press.map(|press| press.kind),
                Some(expected),
                "{name}: the pressed key holds"
            );
            assert_eq!(lock(&rig.fake.presses).take_lost(), 0, "{name}");
        }
    }

    /// Each status with what `state` says of it: its word, the overlay's and the recognizer's.
    fn state_words() -> Vec<(Status, &'static str, &'static str, &'static str)> {
        vec![
            (
                Status::Writing {
                    target: Some("kitty".to_owned()),
                    text: "my password is hunter2".to_owned(),
                },
                "writing",
                "writing",
                "ready",
            ),
            (
                Status::Thinking {
                    target: Some("kitty".to_owned()),
                },
                "writing",
                "thinking",
                "ready",
            ),
            (Status::Paused, "paused", "hidden", "paused"),
            (Status::Listening, "listening", "hidden", "ready"),
            (Status::KeyboardOnly, "keyboard", "hidden", "ready"),
            (capturing_status(), "capturing", "listening", "ready"),
            (
                Status::Pending {
                    target: Some("kitty".to_owned()),
                    route: CaptureRoute::Paste,
                    text: "my password is hunter2".to_owned(),
                    waiting_for_quiet: false,
                    pause_ms: 2500,
                    copies: false,
                },
                "pending",
                "countdown",
                "ready",
            ),
            (
                Status::Pending {
                    target: Some("kitty".to_owned()),
                    route: CaptureRoute::Paste,
                    text: "my password is hunter2".to_owned(),
                    waiting_for_quiet: true,
                    pause_ms: 2500,
                    copies: false,
                },
                "pending",
                "quiet",
                "ready",
            ),
            (
                Status::Working {
                    target: None,
                    route: CaptureRoute::Paste,
                    cancellable: true,
                },
                "transcribing",
                "working",
                "ready",
            ),
            (
                Status::Working {
                    target: None,
                    route: CaptureRoute::Paste,
                    cancellable: false,
                },
                "delivering",
                "working",
                "ready",
            ),
            (
                Status::Done {
                    target: Some("kitty".to_owned()),
                    route: CaptureRoute::Paste,
                    notice: "Pasted into kitty.".to_owned(),
                },
                "listening",
                "delivered",
                "ready",
            ),
            (
                Status::Loading {
                    notice: "/home/me/models".to_owned(),
                    downloading: false,
                },
                "loading",
                "hidden",
                "loading",
            ),
            (
                Status::Loading {
                    notice: "/home/me/models".to_owned(),
                    downloading: true,
                },
                "loading",
                "loading",
                "loading",
            ),
        ]
    }

    #[test]
    fn state_names_what_thalvi_is_doing_and_never_what_was_said() {
        let rig = Rig::new(
            "state",
            &format!(
                r#"{{"stt": {{"entries": [{}]}}}}"#,
                entry("live", "realtime", true, Some(SECRET))
            ),
        );
        let words = state_words();
        for (status, word, overlay, recognizer) in words {
            rig.fake.set_status(status);
            let state = rig.ask(&Request::State);
            assert_eq!(state["state"], word);
            assert_eq!(state["overlay"], json!({"state": overlay}), "{word}");
            assert_eq!(
                state["recognizer"],
                json!({"name": "live", "kind": "realtime", "status": recognizer}),
                "{word}"
            );
            let text = state.to_string();
            for said in ["hunter2", "password", "kitty", "/home/me"] {
                assert!(!text.contains(said), "{word}: {text}");
            }
            assert_eq!(rig.ask(&Request::Status)["ok"], true);
        }

        rig.fake.set_status(Status::Failed {
            notice: format!("The recognizer refused the key {SECRET}."),
            headline: "Recognition failed",
            causes: Vec::new(),
            kind: FailureKind::Other,
        });
        let failed = rig.ask(&Request::State);
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["failure"]["headline"], "Recognition failed");
        assert_eq!(
            failed["overlay"],
            json!({"state": "error", "headline": "Recognition failed"})
        );
        assert_eq!(failed["recognizer"]["status"], "failed");
        rig.fake.overlay.store(false, Ordering::SeqCst);
        assert_eq!(
            rig.ask(&Request::State)["overlay"],
            json!({"state": "hidden"}),
            "an overlay that is off shows nothing, whatever the status"
        );
        assert_eq!(
            failed["failure"]["reason"],
            "The recognizer refused the key (a key)."
        );
        assert!(!failed.to_string().contains(SECRET));
    }

    #[test]
    fn a_confirmation_stays_with_the_user() {
        let rig = Rig::new("confirmation", "{}");
        let submit = Status::Pending {
            target: Some("kitty".to_owned()),
            route: CaptureRoute::Submit,
            text: "run it".to_owned(),
            waiting_for_quiet: true,
            pause_ms: 2500,
            copies: false,
        };
        rig.fake.set_status(submit.clone());
        let state = rig.ask(&Request::State);
        assert_eq!(
            (
                state["state"].clone(),
                state["awaiting_confirmation"].clone()
            ),
            (json!("pending"), json!(true)),
            "an agent can see that the user is asked"
        );
        // No command confirms, submits or accepts; those are refused as unknown.
        for command in ["confirm", "submit", "accept", "yes", "enter"] {
            let reply = rig.ask_line(&json!({"command": command}).to_string());
            assert_eq!(
                reply,
                json!({"ok": false, "error": format!("Thalvi does not know that command. The commands are {}.", COMMANDS.join(", "))}),
                "{command}"
            );
        }
        // A control that starts or ends a capture does not answer it, and a change to the endings
        // waits for the next capture.
        assert_eq!(rig.ask(&Request::HoldStart)["took_effect"], false);
        assert_eq!(rig.ask(&Request::HoldEnd)["took_effect"], false);
        let endings = rig.set(json!({"ending_pause": {"submit": 0.5}}));
        assert_eq!(endings["applies"], "next_capture");
        assert_eq!(
            rig.fake.current(),
            submit,
            "the confirmation is still pending"
        );
        assert_eq!(
            rig.fake.sent(),
            ["hold true", "hold false"],
            "no control reached the engine except the key's press and release"
        );
    }

    #[test]
    fn a_request_is_checked_where_it_enters_and_never_repeats_a_key() {
        let rig = Rig::new("entry", "{}");
        let set = rig
            .set(json!({"stt": {"entries": [entry("cybrex2", "realtime", true, Some(SECRET))]}}));
        assert_eq!(set["ok"], true, "{set}");
        let unknown = || {
            format!(
                "Thalvi does not know that command. The commands are {}.",
                COMMANDS.join(", ")
            )
        };
        for command in COMMANDS {
            let parsed = Request::parse(&json!({"command": command}).to_string());
            assert!(
                parsed.as_ref().is_ok()
                    || parsed.as_ref().is_err_and(|e| !e.contains("does not know")),
                "{command} is listed but not a command: {parsed:?}"
            );
        }
        for (line, error) in [
            ("{", "That request is not JSON.".to_owned()),
            ("[]", "That request names no command.".to_owned()),
            (
                r#"{"command": "settings-set"}"#,
                "The command settings-set was sent without the details it needs.".to_owned(),
            ),
            (
                r#"{"command": "pause", "force": true}"#,
                "The command pause takes no other details.".to_owned(),
            ),
            (r#"{"command": "rm"}"#, unknown()),
            (&format!(r#"{{"command": "{SECRET}"}}"#), unknown()),
        ] {
            assert_eq!(
                rig.ask_line(line),
                json!({"ok": false, "error": error}),
                "{line}"
            );
        }
        // A key put where it does not belong is named in serde's message; it is scrubbed.
        let misplaced =
            rig.set(json!({"stt": {"entries": [{"kind": SECRET, "id": "x", "name": "x"}]}}));
        assert_eq!(misplaced["ok"], false);
        assert!(!misplaced.to_string().contains(SECRET), "{misplaced}");
        let checked = rig.ask(&Request::SettingsCheck {
            settings: json!({"stt": {"entries": [{"kind": SECRET, "id": "x", "name": "x"}]}}),
        });
        assert!(!checked.to_string().contains(SECRET), "{checked}");
        assert_eq!(rig.fake.config().stt.entries.key_of("cybrex2"), SECRET);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_request_past_the_size_bound_is_refused_before_any_command_reads_it() {
        let rig = Rig::new("size", "{}");
        let huge: Vec<String> = (0..60_000).map(|n| format!("word{n}")).collect();
        let line = Request::SettingsSet {
            settings: json!({"stt": {"phrases": huge}}),
        }
        .line();
        assert!(line.len() > thalvi_desktop::host::MAX_REQUEST);
        let reply = rig.ask_line(&line);
        assert_eq!(
            reply,
            json!({"ok": false, "error": "That request is too long."})
        );
        assert!(rig.fake.config().stt.phrases.is_empty());
    }

    #[test]
    fn the_switches_that_act_on_the_system_are_set_apart_from_the_settings() {
        let rig = Rig::new("switch", "{}");
        let on = rig.set(json!({"start_at_login": true}));
        assert_eq!(on, json!({"ok": true, "changed": true}));
        assert_eq!(*lock(&rig.fake.switches), [(Switch::StartAtLogin, true)]);
        let again = rig.set(json!({"start_at_login": true}));
        assert_eq!(again, json!({"ok": true, "changed": false}));
        assert_eq!(
            lock(&rig.fake.switches).len(),
            1,
            "the system is not asked twice"
        );
        assert_eq!(
            rig.ask(&Request::SettingsGet)["settings"]["start_at_login"],
            true
        );
    }

    #[test]
    fn metrics_are_summarized_from_the_log_or_said_to_be_off() {
        let off = Rig::new("metrics-off", "{}");
        let reply = off.ask(&Request::MetricsSummary);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["enabled"], false);
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .starts_with("Metrics are off.")
        );

        let rig = Rig::new("metrics-on", r#"{"recordings": {"metrics": true}}"#);
        let recordings = rig.fake.recordings().unwrap();
        std::fs::create_dir_all(&recordings).unwrap();
        let now = thalvi_desktop::metrics::now_ms();
        let line = |event: &str, outcome: &str, took: f64| {
            json!({"ts_ms": now, "event": event, "outcome": outcome, "duration_ms": took})
                .to_string()
        };
        let log = [
            line("desktop_transcription_ready", "ok", 100.0),
            line("desktop_transcription_ready", "ok", 300.0),
            line("desktop_partial_lag", "ok", 250.0),
            line("desktop_final_lag", "ok", 400.0),
            line("desktop_capture_flow", "ok", 3000.0),
            line("desktop_capture_flow", "cancelled", 900.0),
        ];
        std::fs::write(recordings.join("metrics.jsonl"), log.join("\n")).unwrap();
        let reply = rig.ask(&Request::MetricsSummary);
        assert_eq!(reply["enabled"], true);
        assert_eq!(reply["window_hours"], 24);
        assert_eq!(reply["stages"]["connect"]["count"], 2);
        assert_eq!(reply["stages"]["connect"]["p50_ms"], 100.0);
        assert_eq!(reply["stages"]["connect"]["p95_ms"], 300.0);
        assert_eq!(reply["stages"]["partial_lag"]["p50_ms"], 250.0);
        assert_eq!(reply["stages"]["final"]["p95_ms"], 400.0);
        assert_eq!(reply["stages"]["delivery"]["count"], 0);
        assert_eq!(reply["stages"]["delivery"]["p50_ms"], Value::Null);
        assert_eq!(reply["captures"], json!({"cancelled": 1, "ok": 1}));
    }

    /// A mono 16-bit PCM WAV of `samples` silent samples at `rate`.
    fn wav(rate: u32, samples: u32) -> Vec<u8> {
        let mut file = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        file.extend(16_u32.to_le_bytes());
        file.extend([1, 0, 1, 0]);
        file.extend(rate.to_le_bytes());
        file.extend((rate * 2).to_le_bytes());
        file.extend([2, 0, 16, 0]);
        file.extend(b"data");
        file.extend((samples * 2).to_le_bytes());
        file.resize(file.len() + 2 * samples as usize, 0);
        file
    }

    impl Fake {
        /// Writes `contents` as a file in the test folder and gives its path.
        fn file(&self, name: &str, contents: &[u8]) -> String {
            let path = self.folder.join(name);
            std::fs::write(&path, contents).unwrap();
            path.to_string_lossy().into_owned()
        }
    }

    #[test]
    fn a_file_is_fed_when_listening_with_no_capture_and_otherwise_refused_in_plain_words() {
        let rig = Rig::new("feed", "{}");
        let two_frames = rig.fake.file("two.wav", &wav(16_000, 2 * 1_280));
        let feed = |path: &str| Request::Feed {
            path: path.to_owned(),
        };
        let missed = |error: &str, state: &str| json!({"ok": false, "took_effect": false, "error": error, "state": state});
        let waiting = Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: String::new(),
            waiting_for_quiet: false,
            pause_ms: 700,
            copies: false,
        };
        let loading = Status::Loading {
            notice: String::new(),
            downloading: false,
        };
        let working = Status::Working {
            target: None,
            route: CaptureRoute::Paste,
            cancellable: false,
        };
        for (case, status, request, expected) in [
            (
                "listening",
                Status::Listening,
                feed(&two_frames),
                json!({"ok": true, "took_effect": true, "state": "listening"}),
            ),
            (
                "paused",
                Status::Paused,
                feed(&two_frames),
                missed("Thalvi is paused.", "paused"),
            ),
            (
                "a capture is running",
                capturing_status(),
                feed(&two_frames),
                missed("A capture is running.", "capturing"),
            ),
            (
                "waiting out an ending",
                waiting,
                feed(&two_frames),
                missed("A capture is running.", "pending"),
            ),
            (
                "delivering",
                working,
                feed(&two_frames),
                missed("The last text is still being delivered.", "delivering"),
            ),
            (
                "still loading",
                loading,
                feed(&two_frames),
                missed(
                    "Thalvi is still starting. Try again in a moment.",
                    "loading",
                ),
            ),
        ] {
            rig.fake.set_status(status);
            assert_eq!(rig.ask(&request), expected, "{case}");
        }
        assert_eq!(
            *lock(&rig.fake.fed),
            ["Clip(2 frames)"],
            "only the good file"
        );
    }

    #[test]
    fn a_file_that_cannot_be_fed_is_refused_with_the_reason() {
        let rig = Rig::new("feed-files", r#"{"capture_limits": {"max": 60}}"#);
        let text = rig.fake.file("text.wav", b"not audio at all");
        let long = rig.fake.file("long.wav", &wav(8_000, 61 * 8_000));
        let just_fits = rig.fake.file("fits.wav", &wav(8_000, 60 * 8_000));
        let missing = rig.fake.folder.join("missing.wav");
        let feed = |path: &str| Request::Feed {
            path: path.to_owned(),
        };
        let missed = |error: &str| json!({"ok": false, "took_effect": false, "error": error, "state": "listening"});
        for (case, request, expected) in [
            (
                "no such file",
                feed(&missing.to_string_lossy()),
                missed(&format!("There is no audio file at {}.", missing.display())),
            ),
            (
                "not a WAV file",
                feed(&text),
                missed("The file is not a WAV file."),
            ),
            (
                "past the capture limit",
                feed(&long),
                missed("The audio is longer than the 60 seconds a capture may take."),
            ),
            (
                "a path relative to nothing",
                feed("two.wav"),
                missed("The path of the audio file must be absolute."),
            ),
            (
                "exactly the capture limit",
                feed(&just_fits),
                json!({"ok": true, "took_effect": true, "state": "listening"}),
            ),
        ] {
            assert_eq!(rig.ask(&request), expected, "{case}");
        }
        assert_eq!(
            *lock(&rig.fake.fed),
            ["Clip(750 frames)"],
            "only the good file"
        );
    }

    #[test]
    fn a_feed_that_does_not_finish_says_how_it_ended_and_one_playing_is_told_apart() {
        let rig = Rig::new("feed-ends", "{}");
        let two_frames = rig.fake.file("two.wav", &wav(16_000, 2 * 1_280));
        let feed = |path: &str| Request::Feed {
            path: path.to_owned(),
        };
        let missed = |error: &str, state: &str| json!({"ok": false, "took_effect": false, "error": error, "state": state});
        rig.fake.set_status(Status::Listening);
        for (end, error) in [
            (
                FeedEnd::Cancelled,
                "The file was not fed to the end: it was cancelled.",
            ),
            (
                FeedEnd::Refused("Thalvi is not listening."),
                "Thalvi is not listening.",
            ),
        ] {
            *lock(&rig.fake.feed_ending) = end;
            assert_eq!(
                rig.ask(&feed(&two_frames)),
                missed(error, "listening"),
                "{end:?}"
            );
        }

        // One already playing is told apart from a capture.
        rig.fake.feeding.send_replace(true);
        assert_eq!(
            rig.ask(&feed(&two_frames)),
            missed("A file is already being fed.", "listening")
        );
    }

    #[test]
    fn a_cancel_stops_a_feed_that_has_started_no_capture() {
        let rig = Rig::new("cancel-feed", "{}");
        assert_eq!(
            rig.ask(&Request::Cancel)["error"],
            "There is no capture to cancel."
        );
        rig.fake.feeding.send_replace(true);
        assert_eq!(rig.ask(&Request::State)["feeding"], true);
        rig.fake.ignoring.store(true, Ordering::SeqCst);
        assert_eq!(
            rig.ask(&Request::Cancel),
            json!({"ok": false, "took_effect": false, "error": "The capture did not stop.", "state": "listening"}),
            "a feed the engine keeps playing is not called stopped"
        );
        rig.fake.ignoring.store(false, Ordering::SeqCst);
        assert_eq!(
            rig.ask(&Request::Cancel),
            json!({"ok": true, "took_effect": true, "state": "listening"})
        );
        assert!(!*rig.fake.feeding.borrow());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn state_and_cancel_are_answered_while_a_file_is_being_fed() {
        let rig = Rig::new("feed-in-flight", "{}");
        let path = rig.fake.file("in-flight.wav", &wav(16_000, 1_280));
        rig.fake.holding.store(true, Ordering::SeqCst);
        let socket = rig.socket.0.clone();
        let feed = std::thread::spawn(move || {
            thalvi_desktop::host::Desktop::send_request_in(
                &socket,
                &Request::Feed { path }.line(),
                Duration::from_secs(10),
            )
        });
        let give_up = std::time::Instant::now() + Duration::from_secs(10);
        let state = loop {
            let state = rig.ask(&Request::State);
            if state["feeding"] == true || std::time::Instant::now() > give_up {
                break state;
            }
            std::thread::yield_now();
        };
        assert_eq!(state["feeding"], true);
        assert_eq!(state["state"], "listening");
        assert_eq!(rig.ask(&Request::Cancel)["took_effect"], true);
        let reply: Value = serde_json::from_str(&feed.join().unwrap().unwrap().unwrap()).unwrap();
        assert_eq!(
            reply["error"],
            "The file was not fed to the end: it was cancelled."
        );
        assert_eq!(rig.ask(&Request::State)["feeding"], false);
    }

    #[test]
    fn the_models_are_listed_and_downloaded_by_kind_and_name() {
        let rig = Rig::new("models", "{}");
        assert_eq!(
            rig.ask(&Request::Models),
            json!({"ok": true, "models": [
                {"kind": "wake", "name": "hey_thalvi", "title": "Hey Thalvi", "size": 1000, "downloaded": true},
                {"kind": "speech", "name": "nemotron_160ms", "title": "Nemotron", "size": 650,
                 "downloaded": false, "progress_percent": 42, "runs_on": "Will run on the CPU"},
            ]})
        );
        for (kind, name) in [
            (Catalog::Speech, "nemotron_160ms"),
            (Catalog::Wake, "hey_thalvi"),
        ] {
            let reply = rig.ask(&Request::ModelDownload {
                kind,
                name: name.to_owned(),
            });
            assert_eq!(reply, json!({"ok": true, "downloaded": true}));
        }
        assert_eq!(
            *lock(&rig.fake.downloaded),
            [
                (Catalog::Speech, "nemotron_160ms".to_owned()),
                (Catalog::Wake, "hey_thalvi".to_owned())
            ]
        );
        let unknown = rig.ask(&Request::ModelDownload {
            kind: Catalog::Speech,
            name: "nope".to_owned(),
        });
        assert_eq!(
            unknown,
            json!({"ok": false, "downloaded": false, "error": "nope is not a speech model Thalvi can download."})
        );
        assert_eq!(lock(&rig.fake.downloaded).len(), 2);
    }

    #[test]
    fn a_speech_entry_is_checked_as_settings_does_and_its_key_is_never_repeated() {
        let rig = Rig::new(
            "speech-check",
            &format!(
                r#"{{"stt": {{"entries": [{}]}}}}"#,
                entry("cybrex2", "realtime", true, Some(SECRET))
            ),
        );
        let check = |entry| rig.ask(&Request::SpeechCheck { entry });
        assert_eq!(
            check(entry("new", "realtime", false, Some("ANOTHER-KEY"))),
            json!({"ok": true, "ready": true})
        );
        for entry in [
            json!({"kind": SECRET, "id": "x", "name": "x"}),
            json!({"kind": "realtime", "id": "x", "name": "x", "address": "http://nowhere", "model": "m", "api_key": "ANOTHER-KEY"}),
        ] {
            let reply = check(entry);
            assert_eq!(reply["ok"], false, "{reply}");
            assert_eq!(reply["ready"], false);
            let text = reply.to_string();
            assert!(
                !text.contains(SECRET) && !text.contains("ANOTHER-KEY"),
                "{text}"
            );
        }
    }

    #[test]
    fn settings_open_at_a_known_page_and_quit_quits() {
        let rig = Rig::new("show-quit", "{}");
        let show = |page: Option<&str>| {
            rig.ask(&Request::Show {
                page: page.map(str::to_owned),
            })
        };
        assert_eq!(
            show(Some("speech")),
            json!({"ok": true, "took_effect": true})
        );
        assert_eq!(show(None), json!({"ok": true, "took_effect": true}));
        assert_eq!(*lock(&rig.fake.pages), ["speech"]);
        let unknown = show(Some("wake"));
        assert_eq!(
            unknown,
            json!({"ok": false, "took_effect": false,
                "error": "That is not a settings page. The pages are speech, microphone, capture, feedback, tasks, history, app."})
        );
        assert_eq!(
            rig.fake.shown.load(Ordering::SeqCst),
            2,
            "the unknown page opened nothing"
        );

        assert_eq!(rig.fake.quit.load(Ordering::SeqCst), 0);
        assert_eq!(
            rig.ask(&Request::Quit),
            json!({"ok": true, "took_effect": true})
        );
        assert_eq!(rig.fake.quit.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_pages_show_opens_are_the_ones_the_settings_window_has() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/src/settings-pages.ts"),
        )
        .unwrap();
        let table = source
            .split("const PAGES: ")
            .nth(1)
            .and_then(|rest| rest.split("];").next())
            .unwrap();
        let ids: Vec<&str> = table
            .lines()
            .filter_map(|line| line.trim().strip_prefix("[\""))
            .filter_map(|line| line.split('"').next())
            .collect();
        assert_eq!(ids, PAGES);
    }

    #[test]
    fn the_new_commands_are_checked_where_they_enter() {
        let rig = Rig::new("new-entry", "{}");
        let needs =
            |command: &str| format!("The command {command} was sent without the details it needs.");
        for (line, error) in [
            (r#"{"command": "feed"}"#, needs("feed")),
            (
                r#"{"command": "feed", "path": "/a.wav", "loop": true}"#,
                needs("feed"),
            ),
            (
                r#"{"command": "model-download", "name": "m"}"#,
                needs("model-download"),
            ),
            (
                r#"{"command": "model-download", "kind": "voice", "name": "m"}"#,
                needs("model-download"),
            ),
            (
                r#"{"command": "model-download", "kind": "wake"}"#,
                needs("model-download"),
            ),
            (r#"{"command": "speech-check"}"#, needs("speech-check")),
            (r#"{"command": "show", "page": 3}"#, needs("show")),
            (
                r#"{"command": "models", "all": true}"#,
                "The command models takes no other details.".to_owned(),
            ),
            (
                r#"{"command": "quit", "force": true}"#,
                "The command quit takes no other details.".to_owned(),
            ),
        ] {
            assert_eq!(
                rig.ask_line(line),
                json!({"ok": false, "error": error}),
                "{line}"
            );
        }
        assert_eq!(rig.fake.quit.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_command_ever_answers_with_what_was_dictated() {
        let rig = Rig::new(
            "never-said",
            &format!(
                r#"{{"stt": {{"entries": [{}]}}}}"#,
                entry("live", "realtime", true, Some(SECRET))
            ),
        );
        let path = rig.fake.file("said.wav", &wav(16_000, 1_280));
        let requests = [
            Request::Status,
            Request::State,
            Request::Pause,
            Request::Resume,
            Request::Toggle,
            Request::Cancel,
            Request::HoldStart,
            Request::HoldEnd,
            Request::WriteStart,
            Request::WriteEnd,
            Request::CopyError,
            Request::Show {
                page: Some("history".to_owned()),
            },
            Request::SettingsGet,
            Request::SettingsCheck {
                settings: json!({"look": "neon"}),
            },
            Request::SettingsSet {
                settings: json!({"look": "neon"}),
            },
            Request::MetricsSummary,
            Request::Feed { path },
            Request::Models,
            Request::ModelDownload {
                kind: Catalog::Speech,
                name: "nope".to_owned(),
            },
            Request::SpeechCheck {
                entry: json!({"kind": "realtime"}),
            },
            Request::Quit,
        ];
        let named: Vec<String> = requests
            .iter()
            .map(|request| {
                serde_json::from_str::<Value>(&request.line()).unwrap()["command"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(named, COMMANDS, "every command is asked");
        *lock(&rig.fake.failure) = Some(Failure {
            notice: "Could not paste my password is hunter2.".to_owned(),
            headline: "Paste failed",
            causes: Vec::new(),
            kind: FailureKind::Delivery,
        });
        let said = |text: &str| Status::Writing {
            target: Some("kitty".to_owned()),
            text: text.to_owned(),
        };
        let statuses = [
            capturing_status(),
            Status::Pending {
                target: Some("kitty".to_owned()),
                route: CaptureRoute::Submit,
                text: "my password is hunter2".to_owned(),
                waiting_for_quiet: false,
                pause_ms: 700,
                copies: false,
            },
            said("my password is hunter2"),
            Status::Done {
                target: Some("kitty".to_owned()),
                route: CaptureRoute::Paste,
                notice: "Pasted into kitty.".to_owned(),
            },
            Status::Failed {
                notice: "Nothing was delivered. Clipboard recovery failed: No clipboard. Your text follows:\nmy password is hunter2".to_owned(),
                headline: "Clipboard failed — text in Settings",
                causes: Vec::new(),
                kind: FailureKind::Delivery,
            },
        ];
        for status in statuses {
            for request in &requests {
                rig.fake.set_status(status.clone());
                let reply = rig.ask(request).to_string();
                for said in ["hunter2", "my password"] {
                    assert!(!reply.contains(said), "{request:?}: {reply}");
                }
            }
        }
    }

    #[test]
    fn a_runtime_that_stops_serving_leaves_nothing_to_ask() {
        let rig = Rig::new("stop", "{}");
        assert_eq!(rig.ask(&Request::State)["ok"], true);
        #[cfg(target_os = "linux")]
        {
            let dir = rig.socket.0.clone();
            drop(rig);
            let gone = thalvi_desktop::host::Desktop::send_request_in(&dir, "{}", WAIT);
            assert_eq!(gone, Ok(None));
        }
    }
}
