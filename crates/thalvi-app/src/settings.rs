//! Reads, validates and writes the desktop config file for the settings window.

use std::path::{Path, PathBuf};

use serde_json::Value;
use thalvi_desktop::config::{ConfigError, DesktopConfig, Look};
use thalvi_desktop::entries::{self, LiveKind};
use thalvi_desktop::hold::{HoldKey, HoldKind};
use thalvi_desktop::host::{Desktop, HostError};
use thalvi_desktop::models::CatalogSpeech;
use thalvi_desktop::theme::{self, LookOverrides};
use thalvi_desktop::transducer::{SpeechChoice, check_folder};
use thalvi_desktop::wake::validate_wake_model;
use thalvi_desktop::writing::{DEFAULT_PROMPT, Writing};

/// Where the config lives.
#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
    /// Whether the file must exist: a path given on the command line does.
    required: bool,
}

impl Store {
    /// The config at `path`, or at the per-user default when `None`.
    ///
    /// # Errors
    /// Fails when the platform has no per-user config directory.
    pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
        let required = path.is_some();
        let path = match path {
            Some(path) => path,
            None => dirs::config_dir()
                .ok_or("This system has no per-user config directory.")?
                .join("thalvi")
                .join("desktop.json"),
        };
        Ok(Self { path, required })
    }

    /// A file named `name` beside the config.
    #[must_use]
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    /// The config to start with, whether this is the first start, and whether the active legacy
    /// Thalvi server entry was removed.
    /// A first start writes the defaults as a save with nothing changed would. A path given on the
    /// command line must exist, so a missing one is an error before it can be a first start.
    ///
    /// # Errors
    /// Why the file cannot be read or is not valid, or why the defaults cannot be written.
    pub fn start(&self) -> Result<(DesktopConfig, bool, bool), String> {
        let first_start = !self.path.exists();
        let config = self.load()?;
        let removed_entry = config.stt.entries.dropped_legacy_server();
        let removed_active_entry = config.stt.entries.dropped_active_legacy_server();
        if first_start || removed_entry {
            self.write(&config)?;
        }
        Ok((config, first_start, removed_active_entry))
    }

    /// The config in effect: the file if there is one, else the defaults.
    ///
    /// # Errors
    /// Fails when the file cannot be read or is not valid.
    pub fn load(&self) -> Result<DesktopConfig, String> {
        if self.required || self.path.exists() {
            DesktopConfig::load(&self.path)
        } else {
            DesktopConfig::from_json_str("{}")
        }
        .map_err(|error| error.to_string())
    }

    /// Validates `settings` as the loader does at startup, then replaces the file in one step.
    /// The OS switches' preferences are `current`'s: the form never carries them, because a switch
    /// acts at once and a form opened earlier would otherwise write a stale value back.
    ///
    /// # Errors
    /// The loader's message when the settings are invalid, in which case nothing is written; or
    /// why the file could not be written.
    pub fn save(&self, settings: Value, current: &DesktopConfig) -> Result<DesktopConfig, String> {
        let config = Self::check(settings, current)?;
        self.write(&config)?;
        Ok(config)
    }

    /// Validates `settings` as [`Store::save`] does, without writing anything.
    ///
    /// # Errors
    /// The loader's message when the settings are invalid.
    pub fn check(mut settings: Value, current: &DesktopConfig) -> Result<DesktopConfig, String> {
        prepare(&mut settings, current);
        check_keys(&settings, current, Desktop::check_hold_keys)?;
        // A theme that did not change, including one this build cannot read, is written back as stored.
        restore_themes(&mut settings, current)?;
        load(&settings)
    }

    /// Replaces the file with `config` in one step.
    ///
    /// # Errors
    /// Why the file could not be written.
    pub fn write(&self, config: &DesktopConfig) -> Result<(), String> {
        let json = serde_json::to_string_pretty(config)
            .map_err(|error| format!("The settings could not be written: {error}"))?;
        write_atomically(&self.path, &json).map_err(|error| {
            format!(
                "The settings could not be saved to {}: {error}",
                self.path.display()
            )
        })
    }

    /// Writes `config` again, including its OS switches' preferences.
    ///
    /// # Errors
    /// Why the file could not be written.
    pub fn save_switches(&self, config: &DesktopConfig) -> Result<(), String> {
        let settings = serde_json::to_value(config).map_err(|error| error.to_string())?;
        self.save(settings, config).map(drop)
    }
}

/// Fills in what the form never carries: the OS switches' preferences are `current`'s, and each
/// speech entry without an `api_key` keeps the one saved for its id. What the form only shows,
/// the default writing prompt, is dropped.
fn prepare(settings: &mut Value, current: &DesktopConfig) {
    if let Some(settings) = settings.as_object_mut() {
        settings.remove("writing_default_prompt");
        settings.insert(
            "start_at_login".to_owned(),
            Value::Bool(current.start_at_login),
        );
        settings.insert("show_in_menu".to_owned(), Value::Bool(current.show_in_menu));
    }
    unmask_keys(settings, current);
}

/// Refuses changed hold keys this desktop can never bind, before the loader reads them: it takes
/// the key every config once stored (Alt+KeyZ, Alt+KeyX) for the desktop's default, so a refusal
/// after it would never see that key. A key that is no key name is the loader's to refuse.
fn check_keys(
    settings: &Value,
    current: &DesktopConfig,
    check: impl FnOnce(&[(HoldKind, HoldKey)]) -> Result<(), HostError>,
) -> Result<(), String> {
    let asked = |name: &str, current: &HoldKey| {
        settings
            .get(name)
            .and_then(Value::as_str)
            .and_then(|key| HoldKey::parse(key).ok())
            .unwrap_or_else(|| current.clone())
    };
    let hold = asked("hold_key", &current.hold_key);
    let write = asked("write_key", &current.write_key);
    if (&hold, &write) == (&current.hold_key, &current.write_key) {
        return Ok(());
    }
    check(&[(HoldKind::Dictate, hold), (HoldKind::Write, write)])
        .map_err(|error| format!("{error} The keys that were set before still work."))
}

/// Whether the form's writing settings name an endpoint to write with; numbers as digits can be
/// kept without one.
fn writing_has_endpoint(writing: &serde_json::Map<String, Value>) -> bool {
    if writing.get("local") == Some(&Value::Bool(true)) {
        return true;
    }
    let filled = |key: &str| {
        writing
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    filled("url") && filled("model")
}

/// Puts an unchanged theme back as stored, and refuses a changed one this build cannot use.
/// The form shows an unreadable look as `{ "unreadable": reason }` and withholds its JSON, so a
/// save that did not edit that look sends the placeholder back and the original JSON is restored.
/// A first `themes` object that gives no version is version 1. A version other than 1 is refused
/// when the value differs from what is stored.
fn restore_themes(settings: &mut Value, current: &DesktopConfig) -> Result<(), String> {
    let Some(presented) = settings.get("themes").cloned() else {
        return Ok(());
    };
    if presented.is_null() {
        if let Some(object) = settings.as_object_mut() {
            object.remove("themes");
        }
        return Ok(());
    }
    let mut candidate = presented;
    if current.themes.is_empty()
        && let Some(object) = candidate.as_object_mut()
        && !object.contains_key("version")
    {
        object.insert("version".to_owned(), serde_json::json!(theme::VERSION));
    }
    if let Some(object) = candidate.as_object_mut() {
        for look in [Look::Neon, Look::Holo, Look::Gunmetal] {
            let Some(LookOverrides::Unreadable { raw, reason }) = current.themes.of(look) else {
                continue;
            };
            let placeholder = serde_json::json!({ "unreadable": reason });
            if object.get(look.name()) == Some(&placeholder) {
                object.insert(look.name().to_owned(), raw.clone());
            }
        }
    }
    theme::refuse_changed(&current.themes, &candidate)?;
    if let Some(object) = settings.as_object_mut() {
        object.insert("themes".to_owned(), candidate);
    }
    Ok(())
}

/// Validates `settings` as the loader does at startup.
fn load(settings: &Value) -> Result<DesktopConfig, String> {
    DesktopConfig::from_json_str(&settings.to_string()).map_err(|error| match error {
        // The window writes this JSON itself, so a line and column would mean nothing.
        ConfigError::Invalid(error) => {
            let text = error.to_string();
            let position = format!(" at line {} column {}", error.line(), error.column());
            text.strip_suffix(&position).unwrap_or(&text).to_owned()
        }
        error @ (ConfigError::Read { .. } | ConfigError::InvalidFile { .. }) => error.to_string(),
    })
}

/// `patch` merged over the form of `current`: how every change to the settings is built, so that
/// a change sends only what it changes and cannot undo one saved since its sender last looked.
/// Called under the save lock, on the config in use.
///
/// # Errors
/// That `patch` is not a JSON object, or why `current` has no form.
pub fn merged(current: &DesktopConfig, patch: &Value) -> Result<Value, String> {
    if !patch.is_object() {
        return Err("The settings change must be a JSON object.".to_owned());
    }
    let mut form = settings_json(current)?;
    merge_patch(&mut form, patch);
    Ok(form)
}

/// Applies a JSON merge patch (RFC 7386) to `target`: objects merge, `null` removes a key, and
/// anything else, an array included, replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    if let Some(target) = target.as_object_mut() {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

/// Everything Settings checks before a save, without writing: the loader's
/// validation, and for what differs from `current` the checks Settings runs as an entry, a model
/// folder or a wake model is chosen.
///
/// # Errors
/// The plain message Settings would show.
pub fn judge(mut settings: Value, current: &DesktopConfig) -> Result<DesktopConfig, String> {
    prepare(&mut settings, current);
    check_keys(&settings, current, Desktop::check_hold_keys)?;
    // The same restore-and-refuse step a save runs, so a check matches what set would do.
    restore_themes(&mut settings, current)?;
    let config = load(&settings)?;
    let mut before = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let known: Vec<Value> = entries_of(&mut before)
        .map(|entry| Value::Object(entry.clone()))
        .collect();
    for entry in entries_of(&mut settings) {
        let entry = Value::Object(entry.clone());
        if !known.contains(&entry) {
            entries::check(&entry)?;
        }
    }
    for entry in config.stt.entries.live() {
        if let LiveKind::OnDevice(SpeechChoice::Folder(folder)) = &entry.kind
            && !current
                .stt
                .entries
                .live()
                .iter()
                .any(|known| known.kind == entry.kind)
        {
            check_folder(folder).map_err(|error| error.to_string())?;
        }
    }
    if settings.pointer("/wake/capture") != before.pointer("/wake/capture") {
        let model = settings
            .pointer("/wake/capture/model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        validate_wake_model(model).map_err(|error| error.to_string())?;
    }
    Ok(config)
}

/// The settings window's form of `config`: the file form, without the OS switches, with API keys
/// replaced by whether the entry has one, the writing prompt in effect, and the default prompt
/// the window offers to restore.
///
/// # Errors
/// Fails when the config cannot be written as JSON.
pub fn settings_json(config: &DesktopConfig) -> Result<Value, String> {
    let mut settings = serde_json::to_value(config).map_err(|error| error.to_string())?;
    if let Some(settings) = settings.as_object_mut() {
        settings.remove("start_at_login");
        settings.remove("show_in_menu");
        // The file keeps a key only when it is not the desktop's default; the form shows the key in effect.
        settings.insert("hold_key".to_owned(), config.hold_key.to_string().into());
        settings.insert("write_key".to_owned(), config.write_key.to_string().into());
    }
    // The file keeps the switch only when it is off; the form always shows it.
    if let Some(wake) = settings.get_mut("wake").and_then(Value::as_object_mut) {
        wake.insert("enabled".to_owned(), config.wake.enabled().into());
    }
    // The file keeps this switch only when it is on; the form always shows it.
    if let Some(stt) = settings.get_mut("stt").and_then(Value::as_object_mut) {
        stt.insert(
            "learn_from_corrections".to_owned(),
            config.stt.learn_from_corrections().into(),
        );
    }
    for entry in entries_of(&mut settings) {
        if let Some(key) = entry.remove("api_key") {
            entry.insert(
                "has_key".to_owned(),
                Value::Bool(key != Value::String(String::new())),
            );
        }
    }
    if let Some(writing) = settings.get_mut("writing").and_then(Value::as_object_mut) {
        let key = writing.remove("api_key").unwrap_or(Value::Null);
        writing.insert(
            "has_key".to_owned(),
            Value::Bool(key.as_str().is_some_and(|key| !key.is_empty())),
        );
        writing.insert(
            "prompt".to_owned(),
            Value::String(
                config
                    .writing
                    .as_ref()
                    .map_or(DEFAULT_PROMPT, Writing::prompt)
                    .to_owned(),
            ),
        );
        writing.insert(
            "numbers_as_digits".to_owned(),
            Value::Bool(
                config
                    .writing
                    .as_ref()
                    .is_none_or(Writing::numbers_as_digits),
            ),
        );
    }
    if let Some(settings) = settings.as_object_mut() {
        settings.insert(
            "writing_default_prompt".to_owned(),
            Value::String(DEFAULT_PROMPT.to_owned()),
        );
        if config.themes.is_empty() {
            settings.remove("themes");
        } else {
            settings.insert("themes".to_owned(), theme::form_of_themes(&config.themes));
        }
    }
    Ok(settings)
}

/// The settings form that makes `name` the model of the active on-device entry, as a pick of the
/// Model drop-down is saved.
///
/// # Errors
/// When the live recognizer is not on this computer, `name` is not a catalog model, or it is not
/// lighter than the model in use.
pub fn switched_to_lighter_model(current: &DesktopConfig, name: &str) -> Result<Value, String> {
    let LiveKind::OnDevice(choice) = &current.stt.entries.active_live().kind else {
        return Err(
            "The live recognizer is not on this computer, so there is no model to switch."
                .to_owned(),
        );
    };
    let Some(lighter) = CatalogSpeech::named(name) else {
        return Err(format!("{name} is not a speech model Thalvi can download."));
    };
    if choice.lighter() != Some(lighter) {
        return Err(format!(
            "{} is not a lighter model for {}.",
            lighter.title(),
            choice.title()
        ));
    }
    let id = current.stt.entries.active_live().id.clone();
    let mut settings = settings_json(current)?;
    for entry in entries_of(&mut settings) {
        if entry.get("id").and_then(Value::as_str) == Some(id.as_str()) {
            entry.insert("model".to_owned(), Value::String(lighter.name().to_owned()));
        }
    }
    Ok(settings)
}

/// The entry objects of the speech server list in `settings`.
fn entries_of(settings: &mut Value) -> impl Iterator<Item = &mut serde_json::Map<String, Value>> {
    settings
        .pointer_mut("/stt/entries")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

/// Gives each entry without an `api_key` the one saved for its id: the form only carries a key it
/// replaces or clears.
fn unmask_keys(settings: &mut Value, current: &DesktopConfig) {
    for entry in entries_of(settings) {
        entry.remove("has_key");
        let keyed = matches!(
            entry.get("kind").and_then(Value::as_str),
            Some("realtime" | "open_ai")
        );
        if keyed && !entry.contains_key("api_key") {
            let saved = entry
                .get("id")
                .and_then(Value::as_str)
                .map_or("", |id| current.stt.entries.key_of(id));
            entry.insert("api_key".to_owned(), Value::String(saved.to_owned()));
        }
    }
    if let Some(writing) = settings.get_mut("writing").and_then(Value::as_object_mut) {
        writing.remove("has_key");
        // A key belongs to an endpoint; numbers as digits alone keeps none.
        if writing_has_endpoint(writing) && !writing.contains_key("api_key") {
            let saved = current.writing.as_ref().map_or("", Writing::key);
            writing.insert("api_key".to_owned(), Value::String(saved.to_owned()));
        }
    }
}

pub(crate) fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    thalvi_desktop::private::write_atomically(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use thalvi_desktop::config::Look;
    use thalvi_desktop::entries::LiveKind;
    use thalvi_desktop::transducer::SpeechChoice;

    use super::*;

    #[test]
    fn switching_to_the_lighter_model_changes_only_the_active_on_device_entry() {
        let current = DesktopConfig::from_json_str(
            r#"{"stt":{"entries":[{"kind":"on_device","id":"dev","name":"On this computer","active":true,"model":"nemotron_160ms"},{"kind":"open_ai","id":"server","name":"Server","active":false,"address":"http://127.0.0.1:1/v1","model":"m","api_key":"saved"}]}}"#,
        )
        .unwrap();
        let form = switched_to_lighter_model(&current, "parakeet_tdt_v3").unwrap();
        let saved = Store::check(form, &current).unwrap();
        let LiveKind::OnDevice(choice) = &saved.stt.entries.active_live().kind else {
            panic!("the on-device entry is still live");
        };
        assert_eq!(choice, &SpeechChoice::Catalog(CatalogSpeech::ParakeetTdtV3));
        assert_eq!(
            saved.stt.entries.live().len(),
            current.stt.entries.live().len()
        );
        assert_eq!(
            saved.stt.entries.key_of("server"),
            "saved",
            "a server's key survives the switch"
        );
        assert!(
            switched_to_lighter_model(&saved, "nemotron_160ms").is_err(),
            "only a lighter model may be offered"
        );
    }

    fn store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("thalvi-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("desktop.json");
        (Store::new(Some(path)).unwrap(), dir)
    }

    #[test]
    fn local_chat_keeps_the_manual_setup_key_and_prompt_across_saved_source_switches() {
        let config = DesktopConfig::from_json_str(r#"{"writing":{"url":"http://localhost:8080/v1","model":"saved","api_key":"test-secret","prompt":"Keep my wording."}}"#).unwrap();
        let local = merged(
            &config,
            &json!({"writing":{"local":true,"chat_build":"cpu"}}),
        )
        .unwrap();
        let config = judge(local, &config).unwrap();
        let shown = settings_json(&config).unwrap();
        assert_eq!(shown["writing"]["local"], true);
        assert_eq!(shown["writing"]["prompt"], "Keep my wording.");
        assert_eq!(shown["writing"]["has_key"], true);
        assert!(shown["writing"].get("api_key").is_none());
        assert!(!format!("{:?}", config.writing).contains("test-secret"));
        let manual = merged(&config, &json!({"writing":{"local":false}})).unwrap();
        let config = judge(manual, &config).unwrap();
        assert_eq!(config.writing.as_ref().unwrap().key(), "test-secret");
        assert_eq!(
            config.writing.as_ref().unwrap().chat_build(),
            Some(thalvi_desktop::chat::Build::Cpu)
        );
        assert_eq!(settings_json(&config).unwrap()["writing"]["model"], "saved");
        let local = DesktopConfig::from_json_str(r#"{"writing":{"local":true}}"#).unwrap();
        assert_eq!(
            settings_json(&local).unwrap()["writing"]["prompt"],
            DEFAULT_PROMPT
        );
    }

    #[test]
    fn the_update_marker_is_kept_beside_the_config() {
        let (store, dir) = store("sibling");
        assert_eq!(
            store.sibling("update.json"),
            dir.join("nested").join("update.json")
        );
    }

    #[test]
    fn the_effective_config_has_every_field_and_says_what_is_recorded() {
        let config =
            DesktopConfig::from_json_str(r#"{"recordings": {"metrics": true, "audio": true}}"#)
                .unwrap();
        let settings = settings_json(&config).unwrap();

        let mut keys: Vec<&str> = settings
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "apps",
                "capture_limits",
                "cues",
                "ending_pause",
                "endings",
                "history",
                "hold_key",
                "look",
                "microphone",
                "overlay",
                "recordings",
                "scripts",
                "stt",
                "update_channel",
                "wake",
                "write_key",
                "writing",
                "writing_default_prompt"
            ]
        );
        assert_eq!(
            settings["recordings"],
            json!({"metrics": true, "text": false, "audio": true})
        );
        assert_eq!(settings["history"], json!({"enabled": true, "limit": 200}));
        assert_eq!(settings["microphone"], Value::Null);
        assert_eq!(settings["look"], json!("gunmetal"));
        assert_eq!(settings["overlay"], json!({"show": true, "draft": true}));
    }

    #[test]
    fn saved_settings_replace_the_file_and_are_what_the_next_start_loads() {
        let (store, dir) = store("save");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["overlay"]["draft"] = json!(false);
        settings["microphone"] = json!("USB Mic");
        settings["look"] = json!("holo");
        settings["recordings"] = json!({"metrics": true, "text": false, "audio": true});
        settings["wake"]["conversation"] = json!({"model": "hey_jarvis", "threshold": 0.65});

        let saved = store.save(settings, &store_defaults()).unwrap();
        let reloaded = store.load().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(store.path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!saved.overlay.draft);
        assert_eq!(saved.microphone.as_deref(), Some("USB Mic"));
        assert_eq!(saved.look, Look::Holo);
        assert_eq!(
            saved.wake.conversation().model,
            thalvi_desktop::wake::HeadModel::Catalog(
                thalvi_desktop::models::CatalogHead::HeyJarvis
            )
        );
        assert_eq!(saved.wake.conversation().threshold.get(), 0.65);
        assert_eq!(reloaded, saved);
        assert_eq!(
            (
                saved.recordings.metrics,
                saved.recordings.text,
                saved.recordings.audio
            ),
            (true, false, true)
        );
        assert_eq!(leftovers, ["desktop.json"]);
    }

    #[test]
    fn an_unrelated_save_keeps_a_newer_a_non_object_or_a_malformed_themes_value_verbatim() {
        type Check = fn(&DesktopConfig, &DesktopConfig);
        type Row<'a> = (&'a str, &'a str, &'a [(&'a str, Value)], Check);
        let rows: [Row; 3] = [
            (
                "newer",
                r##"{"look":"holo","themes":{"version":2,"holo":{"colors":{"dark":{"accent":"#ffffff"}}}}}"##,
                &[
                    ("/themes/version", json!(2)),
                    ("/themes/holo/colors/dark/accent", json!("#ffffff")),
                ],
                |current, again| {
                    assert_eq!(current.themes.notices().len(), 1);
                    assert!(current.themes.notices()[0].contains("newer"));
                    assert!(!current.themes.notices()[0].contains("Holo"));
                    assert_eq!(again.themes.to_value()["version"], 2);
                    assert_eq!(again.themes.notices(), current.themes.notices());
                },
            ),
            (
                "non-object",
                r#"{"look":"holo","themes":"bad"}"#,
                &[("/themes", json!("bad"))],
                |current, _| {
                    assert_eq!(
                        current.themes.notices(),
                        ["Your look customizations could not be read (themes must be an object)."]
                    );
                    assert!(current.themes.of(Look::Gunmetal).is_none());
                },
            ),
            (
                "malformed",
                r##"{"look":"holo","themes":{"version":1,"holo":{"colors":{"dark":{"accent":"red"}}},"gunmetal":{"colors":{"dark":{"accent":"#ffffff"}}}}}"##,
                &[
                    ("/themes/holo/colors/dark/accent", json!("red")),
                    ("/themes/gunmetal/colors/dark/accent", json!("#ffffff")),
                ],
                |_, _| {},
            ),
        ];
        for (name, raw, kept, check) in rows {
            let (store, dir) = store(name);
            std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
            std::fs::write(&store.path, raw).unwrap();
            let current = store.load().unwrap();
            let mut form = settings_json(&current).unwrap();
            form["overlay"]["draft"] = json!(false);
            form["microphone"] = json!("USB Mic");
            let saved = store.save(form, &current).unwrap();
            let file: Value =
                serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap();
            let again = store.load().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            assert!(!saved.overlay.draft, "{name}");
            assert_eq!(saved.microphone.as_deref(), Some("USB Mic"), "{name}");
            for (pointer, expected) in kept {
                assert_eq!(file.pointer(pointer), Some(expected), "{name} {pointer}");
            }
            check(&current, &again);
        }
    }

    #[test]
    fn an_empty_themes_object_is_omitted() {
        let (store, dir) = store("theme-shape");
        let empty = DesktopConfig::from_json_str(r#"{"look":"holo","themes":{}}"#).unwrap();
        assert!(empty.themes.is_empty());
        store.write(&empty).unwrap();
        let written = std::fs::read_to_string(&store.path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!written.contains("\"themes\""));
    }

    #[test]
    fn a_first_theme_patch_without_a_version_defaults_to_1_and_an_invalid_theme_or_any_other_version_is_refused()
     {
        let (store, dir) = store("theme-version");
        let start = store
            .save(settings_json(&store_defaults()).unwrap(), &store_defaults())
            .unwrap();
        let mut settings = settings_json(&start).unwrap();
        settings["themes"] = json!({"version": 1, "holo": {"colors": {"dark": {"accent": "red"}}}});
        let error = Store::check(settings, &start).unwrap_err();
        assert!(
            error.contains("themes.holo.colors.dark.accent: use a colour like #6bf8ff"),
            "{error}"
        );
        assert!(store.load().unwrap().themes.is_empty());
        let form = merged(
            &start,
            &json!({"themes": {"holo": {"font": {"scale": 0.85}}}}),
        )
        .unwrap();
        let saved = store.save(form, &start).unwrap();
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap();
        assert_eq!(file["themes"]["version"], 1);
        let scale = match saved.themes.of(Look::Holo) {
            Some(thalvi_desktop::theme::LookOverrides::Usable(overrides)) => {
                overrides.font.scale.unwrap().get()
            }
            other => panic!("{other:?}"),
        };
        assert!((scale - 0.85).abs() < 0.001, "{scale}");

        for version in [json!(0), json!(2)] {
            let form = merged(
                &saved,
                &json!({"themes": {"version": version, "holo": {"font": {"scale": 1.0}}}}),
            )
            .unwrap();
            let error = Store::check(form, &saved).unwrap_err();
            assert_eq!(error, "themes.version must be 1", "{version}");
        }
        let kept = store.load().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(kept.themes.to_value()["version"], 1);
        match kept.themes.of(Look::Holo) {
            Some(thalvi_desktop::theme::LookOverrides::Usable(overrides)) => {
                assert!((overrides.font.scale.unwrap().get() - 0.85).abs() < 0.001);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_property_and_a_look_reset_leave_the_rest() {
        let (store, dir) = store("theme-reset");
        let current = DesktopConfig::from_json_str(
            r##"{"look":"holo","microphone":"USB Mic","themes":{"version":1,"holo":{"colors":{"dark":{"accent":"#ffffff","text":"#eafff3"}},"font":{"scale":1.1}},"gunmetal":{"font":{"scale":1.2}}}}"##,
        )
        .unwrap();
        store.write(&current).unwrap();
        let form = merged(
            &current,
            &json!({"themes": {"holo": {"colors": {"dark": {"accent": null}}}}}),
        )
        .unwrap();
        let saved = store.save(form, &current).unwrap();
        let holo = match saved.themes.of(Look::Holo) {
            Some(thalvi_desktop::theme::LookOverrides::Usable(overrides)) => overrides.clone(),
            other => panic!("{other:?}"),
        };
        assert!(holo.colors.dark.accent.is_none());
        assert_eq!(holo.colors.dark.text.unwrap().to_hex(), "#eafff3");
        assert_eq!(holo.font.scale.unwrap().get(), 1.1);
        assert_eq!(saved.microphone.as_deref(), Some("USB Mic"));

        let form = merged(&saved, &json!({"themes": {"holo": null}})).unwrap();
        let saved = store.save(form, &saved).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(saved.themes.of(Look::Holo).is_none());
        match saved.themes.of(Look::Gunmetal) {
            Some(thalvi_desktop::theme::LookOverrides::Usable(overrides)) => {
                assert_eq!(overrides.font.scale.unwrap().get(), 1.2);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(saved.microphone.as_deref(), Some("USB Mic"));
    }

    #[test]
    fn the_form_shows_the_hold_keys_in_effect_and_the_file_keeps_only_a_changed_one() {
        let (store, dir) = store("hold-keys");
        let (hold, write) = Desktop::default_hold_keys();
        let mut settings = settings_json(&store_defaults()).unwrap();
        assert_eq!(settings["hold_key"], hold.to_string());
        assert_eq!(settings["write_key"], write.to_string());

        let file = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap()
        };
        store.save(settings.clone(), &store_defaults()).unwrap();
        assert_eq!(
            (file().get("hold_key"), file().get("write_key")),
            (None, None)
        );

        settings["hold_key"] = json!("F8");
        let saved = store.save(settings, &store_defaults()).unwrap();
        let (kept, absent) = (file()["hold_key"].clone(), file().get("write_key").cloned());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(saved.hold_key.to_string(), "F8");
        assert_eq!((kept, absent), (json!("F8"), None));
    }

    #[test]
    fn a_changed_hold_key_the_desktop_can_never_bind_is_refused_even_when_it_was_the_old_default() {
        let refuse_modifiers = |keys: &[(HoldKind, HoldKey)]| match keys
            .iter()
            .find(|(_, key)| !key.modifiers().is_empty())
        {
            Some((_, key)) => Err(HostError::Unsupported(format!("No {key} here."))),
            None => Ok(()),
        };
        let mut current = store_defaults();
        current.hold_key = HoldKey::f9();
        current.write_key = HoldKey::f10();
        let asked = |hold: &str, write: &str| {
            let mut settings = settings_json(&current).unwrap();
            settings["hold_key"] = json!(hold);
            settings["write_key"] = json!(write);
            check_keys(&settings, &current, refuse_modifiers)
        };

        for (hold, write, refused) in [
            ("F9", "F10", None),
            ("F8", "F10", None),
            ("Alt+KeyZ", "F10", Some("No Alt+KeyZ here.")),
            ("F9", "Alt+KeyX", Some("No Alt+KeyX here.")),
            ("Ctrl+F8", "F10", Some("No Ctrl+F8 here.")),
            ("Alt+Hyper", "F10", None),
        ] {
            assert_eq!(
                asked(hold, write),
                refused.map_or(Ok(()), |reason| Err(format!(
                    "{reason} The keys that were set before still work."
                ))),
                "{hold} {write}"
            );
        }
    }

    #[test]
    fn the_form_always_shows_the_wake_switch_and_the_file_keeps_it_only_when_off() {
        let (store, dir) = store("wake-words");
        let file = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap()
        };
        let mut settings = settings_json(&store_defaults()).unwrap();
        assert_eq!(settings["wake"]["enabled"], json!(true));

        settings["wake"]["enabled"] = json!(false);
        let off = store.save(settings.clone(), &store_defaults()).unwrap();
        let kept = file()["wake"]["enabled"].clone();
        let reloaded = store.load().unwrap();
        assert_eq!(
            settings_json(&off).unwrap()["wake"]["enabled"],
            json!(false)
        );

        settings["wake"]["enabled"] = json!(true);
        store.save(settings, &store_defaults()).unwrap();
        let absent = file()["wake"].get("enabled").cloned();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!off.wake.enabled());
        assert_eq!(kept, json!(false));
        assert!(!reloaded.wake.enabled(), "it survives a restart");
        assert_eq!(absent, None);
    }

    #[test]
    fn the_form_always_shows_the_learning_switch_and_the_file_keeps_it_only_when_on() {
        let (store, dir) = store("learning-switch");
        let file = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap()
        };
        let defaults = store_defaults();
        let mut settings = settings_json(&defaults).unwrap();
        assert_eq!(settings["stt"]["learn_from_corrections"], json!(false));

        settings["stt"]["learn_from_corrections"] = json!(true);
        let on = store.save(settings.clone(), &defaults).unwrap();
        assert_eq!(file()["stt"]["learn_from_corrections"], json!(true));
        assert!(on.stt.learn_from_corrections());
        assert!(
            store.load().unwrap().stt.learn_from_corrections(),
            "it survives a restart"
        );

        settings["stt"]["learn_from_corrections"] = json!(false);
        store.save(settings, &defaults).unwrap();
        let absent = file()["stt"].get("learn_from_corrections").cloned();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(absent, None);
    }

    #[test]
    fn only_a_start_without_a_file_writes_the_defaults_and_is_a_first_start() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("desktop.json");
        let store = Store {
            path: path.clone(),
            required: false,
        };

        let (config, first, removed) = store.start().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert!(first, "no file yet");
        assert!(!removed);
        assert_eq!(
            file["stt"]["entries"],
            json!([{"kind": "on_device", "id": "on-device", "name": "On this computer", "active": true, "model": "nemotron_160ms"}])
        );
        assert_eq!(
            file["writing"],
            Value::Null,
            "no stored-override field: {text}"
        );
        assert!(!text.contains("\"prompt\""), "{text}");

        let (again, first, removed) = store.start().unwrap();
        assert!(!first, "the written file ends the first start");
        assert!(!removed);
        assert_eq!(again, config);

        let server = r#"{"stt": {"entries": [{"kind": "thalvi_server", "id": "s", "name": "Thalvi server", "active": true, "address": "ws://127.0.0.1:8080"}]}}"#;
        std::fs::write(&path, server).unwrap();
        let (kept, first, removed) = store.start().unwrap();
        assert!(!first);
        assert!(removed, "the active legacy entry needs one notice");
        assert!(matches!(
            kept.stt.entries.active_live().kind,
            LiveKind::OnDevice(SpeechChoice::Catalog(_))
        ));
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["stt"]["entries"][0]["kind"], "on_device");
        assert_eq!(saved["stt"]["entries"][0]["active"].as_bool(), Some(true));
        assert!(saved.to_string().find("thalvi_server").is_none());
        let (_, _, removed_again) = store.start().unwrap();
        assert!(
            !removed_again,
            "the normalized config does not repeat the notice"
        );

        let required = Store {
            path: dir.join("missing.json"),
            required: true,
        };
        assert!(
            required.start().is_err(),
            "a path given on the command line must exist"
        );
        assert!(!dir.join("missing.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_switch_preferences_are_kept_by_their_switches_and_never_by_the_form() {
        let (store, dir) = store("switches");
        let off = store_defaults();
        let on = DesktopConfig {
            start_at_login: true,
            show_in_menu: true,
            ..off.clone()
        };
        let form = settings_json(&off).unwrap();
        assert!(form.get("start_at_login").is_none() && form.get("show_in_menu").is_none());
        let switches = |config: &DesktopConfig| (config.start_at_login, config.show_in_menu);

        store.save_switches(&on).unwrap();
        let after_switches = switches(&store.load().unwrap());
        // A form opened before the switches were flipped saves them as they were then.
        let mut stale = form;
        stale["start_at_login"] = json!(false);
        stale["show_in_menu"] = json!(false);
        let after_stale_save = switches(&store.save(stale, &on).unwrap());
        store.save_switches(&off).unwrap();
        let after_off = switches(&store.load().unwrap());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            (after_switches, after_stale_save, after_off),
            ((true, true), (true, true), (false, false))
        );
    }

    #[test]
    fn a_custom_speech_model_folder_survives_save_and_reload() {
        let (store, dir) = store("folder");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["stt"] = json!({"entries": [
            {"kind": "on_device", "id": "d", "name": "On this computer", "active": true, "model": "/models/mine"}
        ]});

        let saved = store.save(settings, &store_defaults()).unwrap();
        let reloaded = store.load().unwrap();
        let shown = settings_json(&reloaded).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            saved.stt.entries.active_live().kind,
            LiveKind::OnDevice(SpeechChoice::Folder("/models/mine".into()))
        );
        assert_eq!(reloaded, saved);
        assert_eq!(shown["stt"]["entries"][0]["model"], json!("/models/mine"));
    }

    #[test]
    fn keys_never_reach_the_form_and_stay_unless_replaced_or_cleared() {
        let (store, dir) = store("keys");
        let entry = |id: &str, kind: &str, address: &str, key: Option<&str>| {
            let mut entry = json!({"kind": kind, "id": id, "name": id, "active": id == "live",
                "address": address, "model": "m"});
            if let Some(key) = key {
                entry["api_key"] = json!(key);
            }
            entry
        };
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["stt"] = json!({"entries": [
            entry("live", "realtime", "ws://127.0.0.1:8012", Some("live-secret")),
            entry("open-ai", "open_ai", "http://127.0.0.1:9/v1", Some("open-ai-secret")),
            entry("second", "realtime", "ws://127.0.0.1:8013", Some("second-secret")),
            entry("blank", "open_ai", "http://127.0.0.1:9/v2", None),
        ]});
        store.save(settings, &store_defaults()).unwrap();
        let saved = store.load().unwrap();
        assert_eq!(saved.stt.entries.key_of("open-ai"), "open-ai-secret");

        let form = settings_json(&saved).unwrap();
        assert!(!form.to_string().contains("secret"), "{form}");
        let has: Vec<_> = form["stt"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["has_key"].clone())
            .collect();
        assert_eq!(has, [json!(true), json!(true), json!(true), json!(false)]);

        // An untouched form keeps both keys; a replaced key changes and a cleared one empties.
        let keys = |config: &DesktopConfig| {
            ["live", "open-ai", "second", "blank"]
                .map(|id| config.stt.entries.key_of(id).to_owned())
        };
        let after_untouched = store.save(form.clone(), &saved).unwrap();
        let mut edited = form;
        edited["stt"]["entries"][0]["api_key"] = json!("replaced");
        edited["stt"]["entries"][1]["api_key"] = json!("open-ai-replaced");
        edited["stt"]["entries"][2]["api_key"] = json!("");
        let after_edit = store.save(edited, &after_untouched).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            keys(&after_untouched),
            ["live-secret", "open-ai-secret", "second-secret", ""]
        );
        assert_eq!(keys(&after_edit), ["replaced", "open-ai-replaced", "", ""]);
    }

    #[test]
    fn the_writing_prompt_is_shown_as_in_effect_saved_when_changed_and_refused_when_blank() {
        let (store, dir) = store("prompt");
        let endpoint = json!({"url": "http://127.0.0.1:8000/v1", "model": "m", "api_key": "k"});
        let mut settings = settings_json(&store_defaults()).unwrap();
        assert_eq!(settings["writing_default_prompt"], DEFAULT_PROMPT);
        settings["writing"] = endpoint;
        let mut current = store.save(settings, &store_defaults()).unwrap();
        let file = || std::fs::read_to_string(&store.path).unwrap();
        assert!(!file().contains("prompt"), "the default is not stored");
        let mut shown = settings_json(&current).unwrap();
        assert_eq!(shown["writing"]["prompt"], DEFAULT_PROMPT);

        shown["writing"]["prompt"] = json!("  Be brief.\nNo greeting.  ");
        current = store.save(shown, &current).unwrap();
        assert_eq!(
            current.writing.as_ref().unwrap().prompt(),
            "Be brief.\nNo greeting."
        );
        assert_eq!(store.load().unwrap(), current, "kept after a restart");
        let mut shown = settings_json(&current).unwrap();
        assert_eq!(shown["writing"]["prompt"], "Be brief.\nNo greeting.");

        for blank in ["", " \n\t"] {
            shown["writing"]["prompt"] = json!(blank);
            let refused = store.save(shown.clone(), &current).unwrap_err();
            assert!(
                refused.starts_with("writing.prompt must not be blank"),
                "{refused}"
            );
            assert_eq!(store.load().unwrap(), current, "nothing was written");
        }

        shown["writing"]["prompt"] = json!(DEFAULT_PROMPT);
        store.save(shown, &current).unwrap();
        assert!(
            !file().contains("prompt"),
            "restoring the default removes it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_openings_are_shown_saved_across_a_restart_and_refused_when_unusable() {
        let (store, dir) = store("openings");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["writing"] =
            json!({"url": "http://127.0.0.1:8000/v1", "model": "m", "api_key": "k"});
        let mut current = store.save(settings, &store_defaults()).unwrap();
        let file = || std::fs::read_to_string(&store.path).unwrap();
        assert!(!file().contains("openings"));
        assert!(
            settings_json(&current).unwrap()["writing"]
                .get("openings")
                .is_none()
        );

        let mut shown = settings_json(&current).unwrap();
        shown["writing"]["openings"] = json!([
            {"phrase": "Write me an email", "prompt": " Greeting, body, sign-off. "},
            {"phrase": "write a message", "prompt": "Short and casual."},
        ]);
        current = store.save(shown, &current).unwrap();
        let expected = json!([
            {"phrase": "write me an email", "prompt": "Greeting, body, sign-off."},
            {"phrase": "write a message", "prompt": "Short and casual."},
        ]);
        assert_eq!(
            settings_json(&current).unwrap()["writing"]["openings"],
            expected
        );
        assert_eq!(store.load().unwrap(), current, "kept after a restart");

        for (openings, refusal) in [
            (
                json!([{"phrase": "a", "prompt": "x"}, {"phrase": "A!", "prompt": "y"}]),
                "writing.openings has “a” twice.",
            ),
            (
                json!([{"phrase": "a", "prompt": "  "}]),
                "writing.openings has a blank prompt for “a”.",
            ),
            (
                json!([{"phrase": " ", "prompt": "x"}]),
                "writing.openings needs a phrase",
            ),
        ] {
            let mut shown = settings_json(&current).unwrap();
            shown["writing"]["openings"] = openings;
            let refused = store.save(shown, &current).unwrap_err();
            assert!(refused.starts_with(refusal), "{refused}");
            assert_eq!(store.load().unwrap(), current, "nothing was written");
        }

        let mut shown = settings_json(&current).unwrap();
        shown["writing"]["openings"] = json!([]);
        store.save(shown, &current).unwrap();
        assert!(!file().contains("openings"), "an empty list is not written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writing_endpoint_is_checked_and_its_key_is_hidden_preserved_replaced_or_cleared() {
        let (store, dir) = store("writing");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["writing"] = json!({"url": "ftp://x", "model": "m", "api_key": "k"});
        let refused = store.save(settings.clone(), &store_defaults()).unwrap_err();
        assert!(refused.starts_with("writing.url must be"), "{refused}");
        assert!(!store.path.exists());
        settings["writing"] =
            json!({"url": "http://127.0.0.1:8000/v1/", "model": "m", "api_key": "k"});
        let mut current = store.save(settings, &store_defaults()).unwrap();
        for replacement in [None, Some("new-key"), Some("")] {
            let mut shown = settings_json(&current).unwrap();
            assert!(shown["writing"].get("api_key").is_none());
            assert_eq!(
                shown["writing"]["has_key"],
                !current.writing.as_ref().unwrap().key().is_empty()
            );
            shown["writing"]["model"] = json!("updated");
            let expected = replacement.unwrap_or("k");
            if let Some(key) = replacement {
                shown["writing"]["api_key"] = json!(key);
            }
            current = store.save(shown, &current).unwrap();
            assert_eq!(current.writing.as_ref().unwrap().key(), expected);
            assert_eq!(store.load().unwrap(), current);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_config_stays_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (store, dir) = store("private");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["writing"] = json!({"url": "http://127.0.0.1:8000/v1", "model": "m"});
        settings["stt"] = json!({"entries": [
            {"kind": "realtime", "id": "s", "name": "Realtime", "active": true, "address": "ws://127.0.0.1:8012", "model": "m"},
            {"kind": "open_ai", "id": "p", "name": "P", "active": false, "address": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k"}
        ]});
        // A leftover from an earlier crash must not lend its permissions to the new file.
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let leftover = store.path.with_extension("json.tmp");
        std::fs::write(&leftover, "old").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o644)).unwrap();

        store.save(settings.clone(), &store_defaults()).unwrap();
        let first = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        store.save(settings, &store_defaults()).unwrap();
        let again = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!((first, again), (0o600, 0o600));
    }

    #[test]
    fn invalid_settings_are_refused_with_the_loaders_message_and_nothing_is_written() {
        let (store, dir) = store("invalid");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["ending_pause"] = json!({"paste": 9});

        let error = store.save(settings, &store_defaults()).unwrap_err();
        let exists = store.path.exists() || store.path.parent().unwrap().exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            error,
            "The paste pause must be a number of seconds from 0 to 5."
        );
        assert!(!exists, "nothing is created for invalid settings");
    }

    #[test]
    fn a_merge_patch_merges_objects_removes_nulls_and_replaces_lists() {
        for (target, patch, expected) in [
            (
                json!({"a": {"b": 1, "c": 2}}),
                json!({"a": {"b": 3}}),
                json!({"a": {"b": 3, "c": 2}}),
            ),
            (json!({"a": 1, "b": 2}), json!({"a": null}), json!({"b": 2})),
            (
                json!({"a": [1, 2, 3]}),
                json!({"a": [4]}),
                json!({"a": [4]}),
            ),
            (
                json!({"a": 1}),
                json!({"a": {"b": 1}}),
                json!({"a": {"b": 1}}),
            ),
            (json!({"a": {"b": 1}}), json!({"a": 5}), json!({"a": 5})),
            (
                json!({"a": 1}),
                json!({"b": {"c": null}}),
                json!({"a": 1, "b": {}}),
            ),
        ] {
            let mut merged = target.clone();
            merge_patch(&mut merged, &patch);
            assert_eq!(merged, expected, "{target} + {patch}");
        }
    }

    /// The form of the default config with `change` merged in, as the commands build it.
    fn judged(current: &DesktopConfig, change: &Value) -> Result<DesktopConfig, String> {
        judge(merged(current, change).unwrap(), current)
    }

    #[test]
    fn only_what_a_change_touches_gets_the_checks_settings_runs_on_it() {
        let missing = |folder: &str| json!({"kind": "on_device", "id": "d", "name": "Here", "active": true, "model": folder});
        let current = judged(
            &store_defaults(),
            &json!({"stt": {"entries": [missing("/models/gone")]}}),
        )
        .unwrap_err();
        assert!(
            current.contains("/models/gone"),
            "a new folder is checked: {current}"
        );

        // Settings checks a folder when it is chosen, so one already saved stays through other changes.
        let mut config = store_defaults();
        let mut form = settings_json(&config).unwrap();
        form["stt"]["entries"] = json!([missing("/models/gone")]);
        form.as_object_mut()
            .unwrap()
            .remove("writing_default_prompt");
        config = DesktopConfig::from_json_str(&form.to_string()).unwrap();
        assert!(judged(&config, &json!({"look": "neon"})).is_ok());

        let (_, dir) = store("judge-wake");
        let missing = dir.join("missing.onnx");
        let defaults = store_defaults();
        let patch = json!({"wake": {"capture": {"model": missing}}});
        let refused = judged(&defaults, &patch).unwrap_err();
        assert!(
            refused.contains("could not be loaded") && refused.contains("missing.onnx"),
            "{refused}"
        );
        let saved = load(&patch).unwrap();
        assert!(judged(&saved, &json!({"look": "neon"})).is_ok());

        let mut saved_conversation = settings_json(&store_defaults()).unwrap();
        saved_conversation["wake"]["conversation"]["model"] = json!("hey_jarvis");
        saved_conversation["look"] = json!("neon");
        let config = judge(saved_conversation, &store_defaults()).unwrap();
        assert_eq!(
            config.wake.conversation().model,
            thalvi_desktop::wake::HeadModel::Catalog(
                thalvi_desktop::models::CatalogHead::HeyJarvis
            )
        );
    }

    fn store_defaults() -> DesktopConfig {
        DesktopConfig::from_json_str("{}").unwrap()
    }
}
