//! The commands an outside program sends the running app, and how the app answers each with one JSON reply.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thalvi_desktop::capture::CaptureRoute;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::hold::HoldKind;
use thalvi_desktop::metrics::{self, SUMMARY_WINDOW};
use tokio::sync::watch;

use crate::control;
use crate::runner::{AudioStatus, Control};
use crate::settings::{judge, merge_patch, settings_json};
use crate::status::{Failure, Status};

/// How long a control is given to show in the engine's state before it is said to have had no effect.
pub const EFFECT_WAIT: Duration = Duration::from_secs(3);

/// The commands a refusal lists, so that it need not repeat the one that was sent.
const COMMANDS: [&str; 16] = [
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
];

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
    /// Open the settings window.
    Show,
    /// Every setting the Settings window offers; keys only as set or not set.
    SettingsGet,
    /// Validate a merge patch over the settings without saving it.
    SettingsCheck { settings: Value },
    /// Validate, save and apply a merge patch over the settings, as a Settings save does.
    SettingsSet { settings: Value },
    /// Delays per stage and outcomes of the recent dictations.
    MetricsSummary,
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
            Self::SettingsCheck { .. } | Self::SettingsSet { .. }
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
    fn show_settings(&self);
    async fn copy_error(&self) -> Result<(), String>;
    fn config(&self) -> DesktopConfig;
    /// Saves and applies `settings` exactly as a Settings save does.
    fn save(&self, settings: Value) -> Result<DesktopConfig, String>;
    /// Gives the Thalvi server the stored word list, as Settings does after a save.
    async fn share_word_list(&self) -> Result<(), String>;
    fn set_switch(&self, switch: Switch, on: bool) -> Result<(), String>;
    /// The folder `metrics.jsonl` is written in.
    fn recordings(&self) -> Result<PathBuf, String>;
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
        Request::Show => {
            running.show_settings();
            done(json!({"took_effect": true}))
        }
        Request::SettingsGet => settings_get(running),
        Request::SettingsCheck { settings } => settings_check(running, &settings),
        Request::SettingsSet { settings } => settings_set(running, &settings).await,
        Request::MetricsSummary => metrics_summary(running).await,
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
        Status::Starting { .. } | Status::Capturing { .. } => "capturing",
        Status::Pending { .. } => "pending",
        Status::Working {
            cancellable: true, ..
        } => "transcribing",
        Status::Working { .. } => "delivering",
        Status::Writing { .. } => "writing",
        Status::Loading { .. } => "loading",
        Status::Failed { .. } => "failed",
    }
}

fn state(running: &impl Running) -> String {
    let status = running.status();
    let secrets = secrets(&running.config(), None);
    let mut reply = json!({
        "state": phase(&status),
        "paused": running.audio().borrow().paused,
        "copy_error": running.last_error().is_some(),
    });
    match &*status.borrow() {
        Status::Failed {
            notice, headline, ..
        } => {
            reply["failure"] = json!({"headline": headline, "reason": scrub(notice, &secrets)});
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
            | Status::Working {
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
    if delivering {
        return no_effect(
            running,
            "The text is being delivered, which cannot be cancelled.",
        );
    }
    if !busy {
        return no_effect(running, "There is no capture to cancel.");
    }
    running.control(Control::Cancel);
    if reached(&mut status, wait, |status| !capturing(status)).await {
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
    if let Some(refusal) = refusal {
        return no_effect(running, refusal);
    }
    running.hold(kind, true);
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
    let current = running.config();
    let mut merged = settings_json(&current)?;
    merge_patch(&mut merged, &Value::Object(fields));
    let paused = running.audio().borrow().paused;
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

async fn settings_set(running: &impl Running, change: &Value) -> String {
    let secrets = secrets(&running.config(), Some(change));
    let plan = match plan(running, change) {
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
    if settings_changed && let Err(error) = running.save(plan.merged) {
        return refused(&scrub(&error, &secrets), json!({"changed": false}));
    }
    let warning = if settings_changed && plan.config.stt.phrases != plan.current.stt.phrases {
        running
            .share_word_list()
            .await
            .err()
            .map(|error| scrub(&error, &secrets))
    } else {
        None
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
    if let Some(warning) = warning {
        reply["warning"] = json!(warning);
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

    #[cfg(target_os = "linux")]
    use thalvi_desktop::host::RequestServer;

    use super::*;
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
        store: Store,
        config: Mutex<DesktopConfig>,
        failure: Mutex<Option<Failure>>,
        sent: Mutex<Vec<String>>,
        switches: Mutex<Vec<(Switch, bool)>>,
        shared: AtomicUsize,
        shown: AtomicUsize,
        copy_refused: AtomicBool,
        ignoring: AtomicBool,
        folder: PathBuf,
    }

    impl Fake {
        fn new(name: &str, config: &str) -> Arc<Self> {
            let folder =
                std::env::temp_dir().join(format!("thalvi-agent-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&folder);
            std::fs::create_dir_all(&folder).unwrap();
            let config = DesktopConfig::from_json_str(config).unwrap();
            Arc::new(Self {
                status: watch::channel(Status::Listening).0,
                audio: watch::channel(AudioStatus {
                    selected: None,
                    active: None,
                    paused: false,
                })
                .0,
                store: Store::new(Some(folder.join("desktop.json"))).unwrap(),
                config: Mutex::new(config),
                failure: Mutex::new(None),
                sent: Mutex::new(Vec::new()),
                switches: Mutex::new(Vec::new()),
                shared: AtomicUsize::new(0),
                shown: AtomicUsize::new(0),
                copy_refused: AtomicBool::new(false),
                ignoring: AtomicBool::new(false),
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
                Control::Cancel => self.set_status(Status::Listening),
                _ => {}
            }
        }

        fn hold(&self, kind: HoldKind, down: bool) {
            let key = match kind {
                HoldKind::Dictate => "hold",
                HoldKind::Write => "write",
            };
            lock(&self.sent).push(format!("{key} {down}"));
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

        fn show_settings(&self) {
            self.shown.fetch_add(1, Ordering::SeqCst);
        }

        fn copy_error(&self) -> impl Future<Output = Result<(), String>> {
            std::future::ready(if self.copy_refused.load(Ordering::SeqCst) {
                Err("The clipboard is locked.".to_owned())
            } else {
                Ok(())
            })
        }

        fn config(&self) -> DesktopConfig {
            lock(&self.config).clone()
        }

        fn save(&self, settings: Value) -> Result<DesktopConfig, String> {
            let current = self.config();
            let saved = self.store.save(settings, &current)?;
            *lock(&self.config) = saved.clone();
            Ok(saved)
        }

        fn share_word_list(&self) -> impl Future<Output = Result<(), String>> {
            self.shared.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        }

        fn set_switch(&self, switch: Switch, on: bool) -> Result<(), String> {
            lock(&self.switches).push((switch, on));
            lock(&self.config).start_at_login = on;
            Ok(())
        }

        fn recordings(&self) -> Result<PathBuf, String> {
            Ok(self.folder.join("recordings"))
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
        assert_eq!(
            rig.fake.shared.load(Ordering::SeqCst),
            1,
            "the word list is shared"
        );

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
            rig.fake.store.load().unwrap(),
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
        assert_eq!(ask(Request::Show), json!({"ok": true, "took_effect": true}));
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
    fn state_names_what_thalvi_is_doing_and_never_what_was_said() {
        let rig = Rig::new(
            "state",
            &format!(
                r#"{{"stt": {{"entries": [{}]}}}}"#,
                entry("live", "realtime", true, Some(SECRET))
            ),
        );
        let words = [
            (
                Status::Writing {
                    target: Some("kitty".to_owned()),
                    text: "my password is hunter2".to_owned(),
                },
                "writing",
            ),
            (Status::Paused, "paused"),
            (Status::Listening, "listening"),
            (capturing_status(), "capturing"),
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
            ),
            (
                Status::Working {
                    target: None,
                    route: CaptureRoute::Paste,
                    cancellable: true,
                },
                "transcribing",
            ),
            (
                Status::Working {
                    target: None,
                    route: CaptureRoute::Paste,
                    cancellable: false,
                },
                "delivering",
            ),
            (
                Status::Loading {
                    notice: "/home/me/models".to_owned(),
                    downloading: false,
                },
                "loading",
            ),
        ];
        for (status, word) in words {
            rig.fake.set_status(status);
            let state = rig.ask(&Request::State);
            assert_eq!(state["state"], word);
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
            ["hold false"],
            "no control reached the engine except the key's release"
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
