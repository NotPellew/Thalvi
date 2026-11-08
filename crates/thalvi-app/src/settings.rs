//! Reads, validates and writes the desktop config file for the settings window.

use std::path::{Path, PathBuf};

use serde_json::Value;
use thalvi_desktop::config::{ConfigError, DesktopConfig};

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
        if let Some(settings) = settings.as_object_mut() {
            settings.insert(
                "start_at_login".to_owned(),
                Value::Bool(current.start_at_login),
            );
            settings.insert("show_in_menu".to_owned(), Value::Bool(current.show_in_menu));
        }
        let config =
            DesktopConfig::from_json_str(&settings.to_string()).map_err(|error| match error {
                // The window writes this JSON itself, so a line and column would mean nothing.
                ConfigError::Invalid(error) => {
                    let text = error.to_string();
                    let position = format!(" at line {} column {}", error.line(), error.column());
                    text.strip_suffix(&position).unwrap_or(&text).to_owned()
                }
                error @ (ConfigError::Read { .. } | ConfigError::InvalidFile { .. }) => {
                    error.to_string()
                }
            })?;
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

/// The settings window's form of `config`: the file form.
///
/// # Errors
/// Fails when the config cannot be written as JSON.
pub fn settings_json(config: &DesktopConfig) -> Result<Value, String> {
    let mut settings = serde_json::to_value(config).map_err(|error| error.to_string())?;
    if let Some(settings) = settings.as_object_mut() {
        settings.remove("start_at_login");
        settings.remove("show_in_menu");
    }
    Ok(settings)
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
    use thalvi_desktop::config::{Look, SttEngine};
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
                "server",
                "stt",
                "wake",
                "write_key",
                "writing"
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
        settings["stt"] = json!({"engine": "local", "model": "/models/mine"});

        let saved = store.save(settings, &store_defaults()).unwrap();
        let reloaded = store.load().unwrap();
        let shown = settings_json(&reloaded).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            saved.stt.engine,
            SttEngine::Local(SpeechChoice::Folder("/models/mine".into()))
        );
        assert_eq!(reloaded, saved);
        assert_eq!(shown["stt"]["engine"], json!("local"));
        assert_eq!(shown["stt"]["model"], json!("/models/mine"));
    }

    #[test]
    fn a_writing_endpoint_is_checked_when_saved_and_comes_back_with_its_key() {
        let (store, dir) = store("writing");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["writing"] = json!({"url": "ftp://x", "model": "m", "api_key": "k"});
        let refused = store.save(settings.clone(), &store_defaults()).unwrap_err();
        let written_when_refused = store.path.exists();
        settings["writing"] =
            json!({"url": "http://127.0.0.1:8000/v1/", "model": "m", "api_key": "k"});
        store.save(settings, &store_defaults()).unwrap();
        let shown = settings_json(&store.load().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(refused.starts_with("writing.url must be"), "{refused}");
        assert!(!written_when_refused);
        assert_eq!(
            shown["writing"],
            json!({"url": "http://127.0.0.1:8000/v1", "model": "m", "api_key": "k"})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_config_stays_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (store, dir) = store("private");
        let mut settings = settings_json(&store_defaults()).unwrap();
        settings["stt"] =
            json!({"secondary": {"url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k"}});
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

    fn store_defaults() -> DesktopConfig {
        DesktopConfig::from_json_str("{}").unwrap()
    }
}
