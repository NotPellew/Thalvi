//! Owns the list of speech servers in Settings → Recognition: their kinds, validation, and the one active live entry.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::models::CatalogSpeech;
use crate::recognizer::SpeechSource;
use crate::secondary::{Language, LiveRequest, OpenAiLive, SecondaryStt, SecondarySttError};
use crate::transducer::SpeechChoice;
use crate::whisper::WhisperBackend;

/// The longest entry name, in characters.
pub const MAX_NAME_CHARS: usize = 60;
const MAX_ID_CHARS: usize = 64;

/// Why the list of speech servers was refused. Messages are shown to the user as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    #[error("Every speech server needs a name of 1 to {MAX_NAME_CHARS} characters.")]
    Name,
    #[error("Two speech servers are called \"{0}\". Give each its own name.")]
    DuplicateName(String),
    #[error("Speech server \"{0}\" has no id, or one that is too long or shared with another.")]
    Id(String),
    #[error(
        "Exactly one live speech server must be active: a realtime recognizer or this computer."
    )]
    ActiveLive,
    #[error("Speech server \"{name}\": {reason}")]
    Fields { name: String, reason: String },
}

fn fields(name: &str, reason: impl fmt::Display) -> EntryError {
    EntryError::Fields {
        name: name.to_owned(),
        reason: reason.to_string(),
    }
}

/// A recognizer that speaks Meta's realtime ASR protocol, reached directly.
#[derive(Clone, PartialEq, Eq)]
pub struct Realtime {
    address: String,
    model: String,
    api_key: String,
}

impl fmt::Debug for Realtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Realtime")
            .field("address", &self.address)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Realtime {
    /// # Errors
    /// Fails when `address` is not a `ws` or `wss` URL with a host and no credentials or
    /// fragment, or `model` is blank.
    pub fn new(address: &str, model: &str, api_key: String) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(address.trim()).map_err(|_| ADDRESS)?;
        if !matches!(parsed.scheme(), "ws" | "wss")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ADDRESS.to_owned());
        }
        let model = model.trim();
        if model.is_empty() {
            return Err("the model must be nonempty text.".to_owned());
        }
        let address = parsed.as_str().trim_end_matches('/');
        Ok(Self {
            address: address.to_owned(),
            model: model.to_owned(),
            api_key,
        })
    }

    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

const ADDRESS: &str =
    "the address must be a ws or wss URL with a host and no credentials, query or fragment.";

/// Where a live entry recognizes speech.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveKind {
    /// Meta's realtime ASR protocol, spoken by the desktop itself.
    Realtime(Realtime),
    /// OpenAI-compatible audio transcription, uploaded when capture ends.
    OpenAi(OpenAiLive),
    /// The model on this computer.
    OnDevice(SpeechChoice),
}

/// An entry that dictation can be recognized by, live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveEntry {
    pub id: String,
    pub name: String,
    pub kind: LiveKind,
}

impl LiveEntry {
    /// The language the entry was told the speaker uses, when it names one; the number rules
    /// read it until the recognizer reports the language it detected.
    #[must_use]
    pub fn language(&self) -> Option<Language> {
        match &self.kind {
            LiveKind::OpenAi(live) => live.language,
            LiveKind::Realtime(_) | LiveKind::OnDevice(_) => None,
        }
    }

    /// What errors and notices call this entry.
    #[must_use]
    pub fn source(&self) -> SpeechSource {
        match &self.kind {
            LiveKind::Realtime(realtime) => {
                SpeechSource::direct(&self.name, realtime.address.clone())
            }
            LiveKind::OpenAi(live) => {
                SpeechSource::direct(&self.name, live.settings.address().to_owned())
            }
            LiveKind::OnDevice(choice) => SpeechSource::on_device(&self.name, choice.label()),
        }
    }
}

/// The speech servers: any number of live entries, exactly one live entry active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entries {
    live: Vec<LiveEntry>,
    /// Index into `live`; always in range.
    active: usize,
}

impl Default for Entries {
    /// One On this computer entry with the catalog default model, active.
    fn default() -> Self {
        Self {
            live: vec![LiveEntry {
                id: ON_DEVICE_ID.to_owned(),
                name: ON_DEVICE_NAME.to_owned(),
                kind: LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::ALL[0])),
            }],
            active: 0,
        }
    }
}

const ON_DEVICE_ID: &str = "on-device";
const ON_DEVICE_NAME: &str = "On this computer";

impl Entries {
    /// The live entry that dictation uses now.
    #[must_use]
    pub fn active_live(&self) -> &LiveEntry {
        &self.live[self.active]
    }

    #[must_use]
    pub fn live(&self) -> &[LiveEntry] {
        &self.live
    }

    /// The API key saved for the entry with `id`; empty when it has none or there is no such entry.
    #[must_use]
    pub fn key_of(&self, id: &str) -> &str {
        self.live
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| match &entry.kind {
                LiveKind::Realtime(realtime) => Some(realtime.api_key()),
                LiveKind::OpenAi(live) => Some(live.settings.api_key()),
                LiveKind::OnDevice(_) => None,
            })
            .unwrap_or_default()
    }

    /// Builds the list from entries in file order. Names and ids are distinct, and exactly one
    /// live entry is active.
    fn new(raw: Vec<RawEntry>) -> Result<Self, EntryError> {
        let mut live = Vec::new();
        let mut active = Vec::new();
        let mut seen: Vec<(String, String)> = Vec::new();
        for entry in raw {
            let (entry, on) = entry.build()?;
            let (id, name) = (&entry.id, &entry.name);
            if seen
                .iter()
                .any(|(_, other)| other.to_lowercase() == name.to_lowercase())
            {
                return Err(EntryError::DuplicateName(name.to_owned()));
            }
            if seen.iter().any(|(other, _)| other == id) {
                return Err(EntryError::Id(name.to_owned()));
            }
            seen.push((id.to_owned(), name.to_owned()));
            active.push(on);
            live.push(entry);
        }
        let active_indexes: Vec<_> = active
            .iter()
            .enumerate()
            .filter_map(|(index, on)| on.then_some(index))
            .collect();
        let active = match active_indexes.as_slice() {
            [active] => *active,
            _ => return Err(EntryError::ActiveLive),
        };
        Ok(Self { live, active })
    }
}

/// Reads a speech model name: a catalog name, a folder path, or nothing for the default.
fn speech_choice(model: Option<&str>) -> Result<SpeechChoice, String> {
    let Some(model) = model else {
        return Ok(SpeechChoice::Catalog(CatalogSpeech::ALL[0]));
    };
    if let Some(catalog) = CatalogSpeech::ALL
        .iter()
        .find(|catalog| catalog.name() == model)
    {
        Ok(SpeechChoice::Catalog(*catalog))
    } else if let Some(backend) = WhisperBackend::named(model) {
        Ok(SpeechChoice::Whisper(backend))
    } else if model.contains(['/', '\\']) {
        Ok(SpeechChoice::Folder(PathBuf::from(model)))
    } else {
        Err(format!(
            "Unknown speech model \"{model}\". Use {} or the path of a folder holding encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx and tokens.txt.",
            CatalogSpeech::ALL.map(CatalogSpeech::name).join(", ")
        ))
    }
}

/// An entry as written in the file, before its fields are checked.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawEntry {
    Realtime {
        id: String,
        name: String,
        #[serde(default)]
        active: bool,
        address: String,
        model: String,
        #[serde(default)]
        api_key: String,
    },
    OpenAi {
        id: String,
        name: String,
        #[serde(default)]
        active: bool,
        address: String,
        model: String,
        #[serde(default)]
        api_key: String,
        #[serde(default)]
        request: LiveRequest,
        #[serde(default)]
        language: String,
    },
    OnDevice {
        id: String,
        name: String,
        #[serde(default)]
        active: bool,
        #[serde(default)]
        model: Option<String>,
    },
}

impl RawEntry {
    fn validated_identity(&self) -> Result<(String, String), EntryError> {
        let (id, name) = match self {
            Self::Realtime { id, name, .. }
            | Self::OpenAi { id, name, .. }
            | Self::OnDevice { id, name, .. } => (id.trim().to_owned(), name.trim().to_owned()),
        };
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(EntryError::Name);
        }
        if id.is_empty() || id.chars().count() > MAX_ID_CHARS {
            return Err(EntryError::Id(name));
        }
        Ok((id, name))
    }

    /// Checks the fields of one entry.
    fn build(self) -> Result<(LiveEntry, bool), EntryError> {
        let (id, name) = self.validated_identity()?;
        let live = |kind, on| {
            Ok((
                LiveEntry {
                    id: id.clone(),
                    name: name.clone(),
                    kind,
                },
                on,
            ))
        };
        Ok(match self {
            Self::Realtime {
                active,
                address,
                model,
                api_key,
                ..
            } => live(
                LiveKind::Realtime(
                    Realtime::new(&address, &model, api_key).map_err(|e| fields(&name, e))?,
                ),
                active,
            )?,
            Self::OpenAi {
                active,
                address,
                model,
                api_key,
                request,
                language,
                ..
            } => live(
                LiveKind::OpenAi(OpenAiLive {
                    settings: SecondaryStt::new(&address, &model, api_key)
                        .map_err(|e| fields(&name, open_ai_reason(&e)))?,
                    request,
                    language: Language::parse(&language).map_err(|e| fields(&name, e))?,
                }),
                active,
            )?,
            Self::OnDevice { active, model, .. } => live(
                LiveKind::OnDevice(speech_choice(model.as_deref()).map_err(|e| fields(&name, e))?),
                active,
            )?,
        })
    }
}

fn open_ai_reason(error: &SecondarySttError) -> &'static str {
    match error {
        SecondarySttError::Url => {
            "the address must be an http or https URL with a host, no credentials, query or fragment."
        }
        SecondarySttError::Model => "the model must be nonempty text.",
    }
}

/// Checks one entry as it is added or edited: every field, whether or not it is in use.
///
/// # Errors
/// The plain reason the entry cannot be used.
pub fn check(entry: &Value) -> Result<(), String> {
    serde_json::from_value::<RawEntry>(entry.clone())
        .map_err(|error| error.to_string())?
        .build()
        .map(drop)
        .map_err(|error| error.to_string())
}

/// The speech servers and spellings a file holds under `stt`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawStt {
    #[serde(default)]
    entries: Option<Vec<RawEntry>>,
    #[serde(default)]
    pub(crate) phrases: crate::phrases::WordList,
}

impl RawStt {
    /// The speech servers and spellings this file means.
    pub(crate) fn entries(self) -> Result<(Entries, crate::phrases::WordList), EntryError> {
        let entries = match self.entries {
            None => Entries::default(),
            Some(entries) => Entries::new(entries)?,
        };
        Ok((entries, self.phrases))
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FileEntry<'a> {
    Realtime {
        id: &'a str,
        name: &'a str,
        active: bool,
        address: &'a str,
        model: &'a str,
        api_key: &'a str,
    },
    OpenAi {
        id: &'a str,
        name: &'a str,
        active: bool,
        address: &'a str,
        model: &'a str,
        api_key: &'a str,
        request: LiveRequest,
        language: String,
    },
    OnDevice {
        id: &'a str,
        name: &'a str,
        active: bool,
        model: String,
    },
}
impl Serialize for Entries {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let live = self.live.iter().enumerate().map(|(index, entry)| {
            let (id, name, active) = (entry.id.as_str(), entry.name.as_str(), index == self.active);
            match &entry.kind {
                LiveKind::Realtime(realtime) => FileEntry::Realtime {
                    id,
                    name,
                    active,
                    address: &realtime.address,
                    model: &realtime.model,
                    api_key: &realtime.api_key,
                },
                LiveKind::OpenAi(live) => FileEntry::OpenAi {
                    id,
                    name,
                    active,
                    address: live.settings.address(),
                    model: live.settings.model(),
                    api_key: live.settings.api_key(),
                    request: live.request,
                    language: live.language.map(|l| l.to_string()).unwrap_or_default(),
                },
                LiveKind::OnDevice(choice) => FileEntry::OnDevice {
                    id,
                    name,
                    active,
                    model: choice.label(),
                },
            }
        });
        serializer.collect_seq(live)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::DesktopConfig;
    use crate::whisper::WhisperBackend;

    fn entries(json: &str) -> Result<Entries, String> {
        DesktopConfig::from_json_str(json)
            .map(|config| config.stt.entries)
            .map_err(|error| error.to_string())
    }

    fn entries_of(json: &Value) -> Entries {
        entries(&json.to_string()).unwrap()
    }

    #[test]
    fn entries_round_trip_with_their_order_keys_and_inactive_setups() {
        let json = json!({"writing": {"url": "http://c/v1", "model": "c"}, "stt": {"entries": [
            {"kind": "realtime", "id": "a", "name": "Cybrex2", "active": false,
             "address": "ws://100.64.0.2:8012", "model": "nemotron", "api_key": "live-key"},
            {"kind": "open_ai", "id": "o", "name": "OpenAI local", "active": true,
             "address": "http://localhost:9000/v1", "model": "whisper", "api_key": "open-key"},
            {"kind": "on_device", "id": "c", "name": "Offline", "active": false, "model": "/m"}
        ]}})
        .to_string();
        let config = DesktopConfig::from_json_str(&json).unwrap();
        let got = &config.stt.entries;
        assert_eq!(got.active_live().name, "OpenAI local");
        assert_eq!(
            got.live()
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Cybrex2", "OpenAI local", "Offline"]
        );
        assert_eq!((got.key_of("a"), got.key_of("o")), ("live-key", "open-key"));
        let written = serde_json::to_string(&config).unwrap();
        assert_eq!(
            DesktopConfig::from_json_str(&written).unwrap(),
            config,
            "{written}"
        );
    }

    #[test]
    fn an_on_device_entry_naming_a_whisper_backend_is_saved_under_that_name_and_an_unknown_name_is_still_refused()
     {
        for backend in WhisperBackend::ALL {
            let entry = json!({"kind": "on_device", "id": "w", "name": "Whisper", "active": true,
                               "model": backend.name()});
            let config = DesktopConfig::from_json_str(
                &json!({"stt": {"entries": [entry.clone()]}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                config.stt.entries.active_live().kind,
                LiveKind::OnDevice(SpeechChoice::Whisper(backend))
            );
            let written = serde_json::to_value(&config).unwrap();
            assert_eq!(written["stt"]["entries"][0], entry, "{}", backend.name());
        }
        let error = entries(
            &json!({"stt": {"entries": [{"kind": "on_device", "id": "w", "name": "Whisper",
                                          "active": true, "model": "whisper_turbo"}]}})
            .to_string(),
        )
        .unwrap_err();
        assert!(
            error.contains("Unknown speech model \"whisper_turbo\""),
            "{error}"
        );
    }

    #[test]
    fn an_open_ai_entry_keeps_its_request_kind_and_language() {
        let entry = |extra: Value| {
            let mut entry = json!({"kind": "open_ai", "id": "o", "name": "ASR", "active": true,
                                   "address": "http://h/v1", "model": "m"});
            entry
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            entry
        };
        let cases = [
            ("old file", json!({}), LiveRequest::Transcriptions, None),
            (
                "chat in German",
                json!({"request": "chat", "language": "de"}),
                LiveRequest::Chat,
                Language::parse("de").unwrap(),
            ),
            (
                "upper case and blanks",
                json!({"request": "transcriptions", "language": " DE "}),
                LiveRequest::Transcriptions,
                Language::parse("de").unwrap(),
            ),
            (
                "blank language",
                json!({"language": "  "}),
                LiveRequest::Transcriptions,
                None,
            ),
        ];
        for (what, extra, request, language) in cases {
            let config = entries_of(&json!({"stt": {"entries": [entry(extra)]}}));
            let LiveKind::OpenAi(live) = &config.active_live().kind else {
                panic!("{what}: not an open_ai entry");
            };
            assert_eq!((live.request, live.language), (request, language), "{what}");
            let written = serde_json::to_value(&config).unwrap();
            let saved = &written[0];
            assert_eq!(saved["request"], json!(request.name()), "{what}");
            assert_eq!(
                saved["language"],
                json!(language.map(|l| l.to_string()).unwrap_or_default()),
                "{what}"
            );
            assert_eq!(
                entries_of(&json!({"stt": {"entries": written}})),
                config,
                "{what}"
            );
        }
        for bad in ["deu", "d1", "e", "é", "d e", "zz"] {
            assert_eq!(
                check(&entry(json!({"language": bad}))),
                Err("Speech server \"ASR\": the language must be a two-letter ISO 639-1 code such as de, or empty.".to_owned()),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_list_that_cannot_work_is_refused_in_plain_words() {
        let live = |id: &str, name: &str, active: bool| {
            json!({"kind": "realtime", "id": id, "name": name, "active": active,
                   "address": "ws://127.0.0.1:8012", "model": "m"})
        };
        let with =
            |entries: Vec<serde_json::Value>| json!({"stt": {"entries": entries}}).to_string();
        let realtime = |address: &str, model: &str| {
            json!({"kind": "realtime", "id": "r", "name": "R", "active": true,
                   "address": address, "model": model})
        };
        let cases = [
            (
                with(vec![]),
                "Exactly one live speech server must be active",
            ),
            (
                with(vec![live("a", "A", false)]),
                "Exactly one live speech server must be active",
            ),
            (
                with(vec![live("a", "A", true), live("b", "B", true)]),
                "Exactly one live speech server must be active",
            ),
            (
                with(vec![live("a", "Same", true), live("b", "same", false)]),
                "Two speech servers are called \"same\"",
            ),
            (
                with(vec![live("a", "A", true), live("a", "B", false)]),
                "Speech server \"B\" has no id, or one that is too long or shared",
            ),
            (
                with(vec![live("a", "  ", true)]),
                "needs a name of 1 to 60 characters",
            ),
            (
                with(vec![live("a", "A", true), realtime("http://h", "m")]),
                "Speech server \"R\": the address must be a ws or wss URL",
            ),
            (
                with(vec![live("a", "A", true), realtime("ws://user:pw@h", "m")]),
                "no credentials, query or fragment",
            ),
            (
                with(vec![
                    live("a", "A", true),
                    realtime("ws://h/asr?token=x", "m"),
                ]),
                "no credentials, query or fragment",
            ),
            (
                with(vec![live("a", "A", true), realtime("ws://h", " ")]),
                "Speech server \"R\": the model must be nonempty text.",
            ),
            (
                with(vec![json!({"kind": "tape", "id": "t", "name": "T"})]),
                "unknown variant `tape`",
            ),
        ];
        for (json, expected) in cases {
            let error = entries(&json).unwrap_err();
            assert!(error.contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn an_entry_is_checked_whole_where_it_is_added() {
        let good = json!({"kind": "on_device", "id": "d", "name": "D", "active": false});
        assert_eq!(check(&good), Ok(()));
        let bad_live = json!({"kind": "realtime", "id": "r", "name": "R", "active": false,
                              "address": "http://h", "model": "m"});
        assert!(check(&bad_live).unwrap_err().contains("ws or wss"));
        let open_ai = json!({"kind": "open_ai", "id": "o", "name": "Local ASR", "active": false,
                             "address": "https://host.example/v1/", "model": "whisper", "api_key": "saved"});
        assert_eq!(check(&open_ai), Ok(()));
        for address in [
            "ftp://host/v1",
            "http://user:pass@host/v1",
            "http://host/v1?x=1",
            "http://host/v1#x",
        ] {
            let mut invalid = open_ai.clone();
            invalid["address"] = json!(address);
            assert!(
                check(&invalid).unwrap_err().contains("http or https URL"),
                "{address}"
            );
        }
    }

    #[test]
    fn lists_are_equal_only_when_entries_and_the_active_one_all_match() {
        let live = |id: &str, name: &str, active: bool| json!({"kind": "on_device", "id": id, "name": name, "active": active});
        let list = |entries: Vec<Value>| entries_of(&json!({"stt": {"entries": entries}}));
        let base = list(vec![live("a", "A", true), live("b", "B", false)]);
        assert_eq!(
            base,
            list(vec![live("a", "A", true), live("b", "B", false)])
        );
        let others = [
            (
                "another name",
                list(vec![live("a", "A", true), live("b", "C", false)]),
            ),
            (
                "another active entry",
                list(vec![live("a", "A", false), live("b", "B", true)]),
            ),
        ];
        for (case, other) in others {
            assert_ne!(base, other, "{case}");
        }
    }

    #[test]
    fn an_api_key_never_shows_in_debug_output() {
        let realtime = Realtime::new("ws://h:1", "m", "sk-secret".to_owned()).unwrap();
        assert!(!format!("{realtime:?}").contains("sk-secret"));
    }
}
