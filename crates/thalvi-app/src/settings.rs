//! Reads, validates and writes the desktop config file for the settings window.

use std::path::{Path, PathBuf};

use serde_json::Value;
use thalvi_desktop::config::{ConfigError, DesktopConfig};
use thalvi_desktop::entries::{self, LiveKind};
use thalvi_desktop::transducer::{SpeechChoice, check_folder};
use thalvi_desktop::wake::validate_wake_models;
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

    /// The config to start with, and whether this is the first start: no file at the per-user path.
    /// A first start writes the defaults as a save with nothing changed would. A path given on the
    /// command line must exist, so a missing one is an error before it can be a first start.
    ///
    /// # Errors
    /// Why the file cannot be read or is not valid, or why the defaults cannot be written.
    pub fn start(&self) -> Result<(DesktopConfig, bool), String> {
        let first_start = !self.path.exists();
        let config = self.load()?;
        if first_start {
            self.save_switches(&config)?;
        }
        Ok((config, first_start))
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
    pub fn save(
        &self,
        mut settings: Value,
        current: &DesktopConfig,
    ) -> Result<DesktopConfig, String> {
        prepare(&mut settings, current);
        let config = load(&settings)?;
        let json = serde_json::to_string_pretty(&config)
            .map_err(|error| format!("The settings could not be written: {error}"))?;
        write_atomically(&self.path, &json).map_err(|error| {
            format!(
                "The settings could not be saved to {}: {error}",
                self.path.display()
            )
        })?;
        Ok(config)
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

/// Refuses a microphone change while listening is not paused, before anything is written.
///
/// # Errors
/// The plain refusal when the saved device would change while the microphone may be open.
pub fn ensure_microphone_change_allowed(
    settings: &Value,
    current: Option<&str>,
    paused: bool,
) -> Result<(), String> {
    let incoming = match settings.get("microphone") {
        Some(Value::String(name)) => Some(name.trim().to_owned()),
        _ => None,
    };
    if incoming.as_deref() != current && !paused {
        return Err(
            "Pause listening before changing the microphone. Turn listening off, then save again."
                .to_owned(),
        );
    }
    Ok(())
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

/// Everything Settings checks before a save, without writing: the microphone rule, the loader's
/// validation, and for what differs from `current` the checks Settings runs as an entry, a model
/// folder or a wake model is chosen.
///
/// # Errors
/// The plain message Settings would show.
pub fn judge(
    mut settings: Value,
    current: &DesktopConfig,
    paused: bool,
) -> Result<DesktopConfig, String> {
    ensure_microphone_change_allowed(&settings, current.microphone.as_deref(), paused)?;
    prepare(&mut settings, current);
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
    if settings.get("wake") != before.get("wake") {
        let model = |head: &str| {
            settings
                .pointer(&format!("/wake/{head}/model"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        validate_wake_models(&model("capture"), &model("conversation"))
            .map_err(|error| error.to_string())?;
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
    }
    for entry in entries_of(&mut settings) {
        if let Some(key) = entry.remove("api_key") {
            entry.insert(
                "has_key".to_owned(),
                Value::Bool(key != Value::String(String::new())),
            );
        }
    }
    if let Some(writing) = settings.get_mut("writing").and_then(Value::as_object_mut)
        && let Some(key) = writing.remove("api_key")
    {
        writing.insert(
            "has_key".to_owned(),
            Value::Bool(key != Value::String(String::new())),
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
    }
    if let Some(settings) = settings.as_object_mut() {
        settings.insert(
            "writing_default_prompt".to_owned(),
            Value::String(DEFAULT_PROMPT.to_owned()),
        );
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
            Some("realtime" | "second_pass")
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
        if !writing.contains_key("api_key") {
            let saved = current.writing.as_ref().map_or("", Writing::key);
            writing.insert("api_key".to_owned(), Value::String(saved.to_owned()));
        }
    }
}

fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let written =
        write_private(&temporary, contents).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Writes a new file only its owner can read: the config can hold an API key.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // On Windows a file under the per-user %APPDATA% is readable by its owner only.
    options.open(path)?.write_all(contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use thalvi_desktop::config::Look;
    use thalvi_desktop::entries::LiveKind;
    use thalvi_desktop::transducer::SpeechChoice;

    use super::*;

    fn store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("thalvi-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("desktop.json");
        (Store::new(Some(path)).unwrap(), dir)
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
    fn only_a_start_without_a_file_writes_the_defaults_and_is_a_first_start() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("desktop.json");
        let store = Store {
            path: path.clone(),
            required: false,
        };

        let (config, first) = store.start().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert!(first, "no file yet");
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

        let (again, first) = store.start().unwrap();
        assert!(!first, "the written file ends the first start");
        assert_eq!(again, config);

        let server = r#"{"stt": {"entries": [{"kind": "thalvi_server", "id": "s", "name": "Thalvi server", "active": true, "address": "ws://127.0.0.1:8080"}]}}"#;
        std::fs::write(&path, server).unwrap();
        let (kept, first) = store.start().unwrap();
        assert!(!first);
        assert!(matches!(
            kept.stt.entries.active_live().kind,
            LiveKind::ThalviServer(_)
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            server,
            "an existing file is not rewritten"
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
        let mut stale = form.clone();
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
        let entry = |id: &str, kind: &str, key: Option<&str>| {
            let mut entry = json!({"kind": kind, "id": id, "name": id, "active": id == "live",
                "address": if kind == "realtime" { "ws://127.0.0.1:8012" } else { "http://127.0.0.1:9/v1" },
                "model": "m"});
            if let Some(key) = key {
                entry["api_key"] = json!(key);
            }
            entry
        };
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["stt"] = json!({"entries": [
            entry("live", "realtime", Some("live-secret")),
            entry("second", "second_pass", Some("second-secret")),
            entry("blank", "second_pass", None),
        ]});
        store.save(settings, &store_defaults()).unwrap();
        let saved = store.load().unwrap();

        let form = settings_json(&saved).unwrap();
        assert!(!form.to_string().contains("secret"), "{form}");
        let has: Vec<_> = form["stt"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["has_key"].clone())
            .collect();
        assert_eq!(has, [json!(true), json!(true), json!(false)]);

        // An untouched form keeps both keys; a replaced key changes and a cleared one empties.
        let keys = |config: &DesktopConfig| {
            ["live", "second", "blank"].map(|id| config.stt.entries.key_of(id).to_owned())
        };
        let after_untouched = store.save(form.clone(), &saved).unwrap();
        let mut edited = form;
        edited["stt"]["entries"][0]["api_key"] = json!("replaced");
        edited["stt"]["entries"][1]["api_key"] = json!("");
        let after_edit = store.save(edited, &after_untouched).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(keys(&after_untouched), ["live-secret", "second-secret", ""]);
        assert_eq!(keys(&after_edit), ["replaced", "", ""]);
    }

    #[test]
    fn the_writing_prompt_is_shown_as_in_effect_saved_when_changed_and_refused_when_blank() {
        let (store, dir) = store("prompt");
        let endpoint = json!({"url": "http://127.0.0.1:8000/v1", "model": "m", "api_key": "k"});
        let mut settings = settings_json(&store_defaults()).unwrap();
        assert_eq!(settings["writing_default_prompt"], DEFAULT_PROMPT);
        settings["writing"] = endpoint.clone();
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
        settings["stt"] = json!({"entries": [
            {"kind": "thalvi_server", "id": "s", "name": "Thalvi server", "active": true, "address": "ws://127.0.0.1:8080"},
            {"kind": "second_pass", "id": "p", "name": "P", "active": true, "address": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k"}
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
    fn a_microphone_change_needs_a_pause_but_other_saves_do_not() {
        let refusal =
            "Pause listening before changing the microphone. Turn listening off, then save again.";
        for (settings, current, paused, expected) in [
            (json!({"microphone": "USB Mic"}), None, false, Err(refusal)),
            (
                json!({"microphone": null}),
                Some("USB Mic"),
                false,
                Err(refusal),
            ),
            (json!({"microphone": "USB Mic"}), None, true, Ok(())),
            (json!({"microphone": null}), Some("USB Mic"), true, Ok(())),
            (
                json!({"microphone": "USB Mic"}),
                Some("USB Mic"),
                false,
                Ok(()),
            ),
            (json!({"microphone": null}), None, false, Ok(())),
            (
                json!({"microphone": " USB Mic "}),
                Some("USB Mic"),
                false,
                Ok(()),
            ),
        ] {
            assert_eq!(
                ensure_microphone_change_allowed(&settings, current, paused),
                expected.map_err(str::to_owned),
                "{settings} current={current:?} paused={paused}"
            );
        }
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
    fn judged(
        current: &DesktopConfig,
        change: &Value,
        paused: bool,
    ) -> Result<DesktopConfig, String> {
        let mut form = settings_json(current).unwrap();
        merge_patch(&mut form, change);
        judge(form, current, paused)
    }

    #[test]
    fn only_what_a_change_touches_gets_the_checks_settings_runs_on_it() {
        let missing = |folder: &str| json!({"kind": "on_device", "id": "d", "name": "Here", "active": true, "model": folder});
        let current = judged(
            &store_defaults(),
            &json!({"stt": {"entries": [missing("/models/gone")]}}),
            true,
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
        assert!(judged(&config, &json!({"look": "neon"}), true).is_ok());

        let mut equal = settings_json(&store_defaults()).unwrap();
        let head = equal["wake"]["capture"]["model"].clone();
        equal["wake"]["conversation"]["model"] = head;
        let wake = judge(equal, &store_defaults(), true).unwrap_err();
        assert_eq!(
            wake,
            "The capture and conversation wake models must be different."
        );

        let loose = json!({"kind": "second_pass", "id": "p", "name": "P", "active": false,
            "address": "not a url", "model": "m"});
        let strict = judged(
            &store_defaults(),
            &json!({"stt": {"entries": [
            store_entries_default(), loose]}}),
            true,
        );
        assert!(
            strict.is_err(),
            "a new second pass entry is checked even when not ticked"
        );
    }

    fn store_entries_default() -> Value {
        settings_json(&store_defaults()).unwrap()["stt"]["entries"][0].clone()
    }

    fn store_defaults() -> DesktopConfig {
        DesktopConfig::from_json_str("{}").unwrap()
    }
}
