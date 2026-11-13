//! Owns the list of speech servers in Settings → Speech: their kinds, validation, the one active live entry, the switched-on extra recognizers, and the migration from the single-recognizer settings.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::models::CatalogSpeech;
use crate::recognizer::SpeechSource;
use crate::secondary::{
    Language, LiveRequest, OpenAiLive, SecondaryApi, SecondaryStt, SecondarySttError,
};
use crate::transducer::SpeechChoice;

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
    #[error(
        "stt.entries replaces stt.engine, stt.model, stt.secondary and the top-level server; remove them."
    )]
    Mixed,
    #[error(
        "Extra recognizers need a writing model. Set one up under Settings → Speech → Writing first, or switch the extras off."
    )]
    ExtrasNeedWriting,
    #[error("stt.model only applies when stt.engine is local.")]
    ModelWithoutLocal,
    #[error("server must be a ws or wss URL")]
    LegacyServerUrl,
    #[error("server root")]
    LegacyServerRoot,
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

/// An entry whose transcription of the finished capture is merged with the live text; it runs when switched on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraEntry {
    pub id: String,
    pub name: String,
    pub settings: SecondaryStt,
}

impl ExtraEntry {
    /// Whether the entry runs after each capture.
    #[must_use]
    pub fn ticked(&self) -> bool {
        self.settings.enabled()
    }
}

/// The speech servers: any number of live and extra entries, exactly one live entry active.
#[derive(Clone, Debug)]
pub struct Entries {
    live: Vec<LiveEntry>,
    /// Index into `live`; always in range.
    active: usize,
    extras: Vec<ExtraEntry>,
    dropped_legacy_server: bool,
    dropped_active_legacy_server: bool,
}

impl PartialEq for Entries {
    fn eq(&self, other: &Self) -> bool {
        self.live == other.live && self.active == other.active && self.extras == other.extras
    }
}

impl Eq for Entries {}

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
            extras: Vec::new(),
            dropped_legacy_server: false,
            dropped_active_legacy_server: false,
        }
    }
}

const ON_DEVICE_ID: &str = "on-device";
const ON_DEVICE_NAME: &str = "On this computer";
const EXTRA_ID: &str = "extra";
const EXTRA_NAME: &str = "Extra recognizer";

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

    /// Every extra entry, switched on or not; their order means nothing.
    #[must_use]
    pub fn extras(&self) -> &[ExtraEntry] {
        &self.extras
    }

    /// Whether loading this config dropped a saved Thalvi server entry.
    #[must_use]
    pub fn dropped_legacy_server(&self) -> bool {
        self.dropped_legacy_server
    }

    /// Whether the dropped Thalvi server entry was active when the config was loaded.
    #[must_use]
    pub fn dropped_active_legacy_server(&self) -> bool {
        self.dropped_active_legacy_server
    }

    /// The extra entries that are switched on.
    pub fn ticked(&self) -> impl Iterator<Item = &ExtraEntry> {
        self.extras.iter().filter(|entry| entry.ticked())
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
            .or_else(|| {
                self.extras
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| entry.settings.api_key())
            })
            .unwrap_or_default()
    }

    /// Builds the list from entries in file order. Names and ids are distinct, and exactly one
    /// live entry is active.
    fn new(raw: Vec<RawEntry>) -> Result<Self, EntryError> {
        let mut live = Vec::new();
        let mut active = Vec::new();
        let mut extras = Vec::new();
        let mut seen: Vec<(String, String)> = Vec::new();
        let mut dropped_legacy_server = false;
        let mut removed_active = false;
        for entry in raw {
            let built = entry.build(false)?;
            if let Built::RemovedLegacyServer { active } = &built {
                dropped_legacy_server = true;
                removed_active |= *active;
                continue;
            }
            let (id, name) = built.identity();
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
            match built {
                Built::Live { entry, on } => {
                    active.push(on);
                    live.push(entry);
                }
                Built::Extra(entry) => extras.push(entry),
                Built::RemovedLegacyServer { .. } => {}
            }
        }
        let active_indexes: Vec<_> = active
            .iter()
            .enumerate()
            .filter_map(|(index, on)| on.then_some(index))
            .collect();
        let active = match active_indexes.as_slice() {
            [active] => *active,
            [] if removed_active => {
                if let Some(index) = live
                    .iter()
                    .position(|entry| matches!(entry.kind, LiveKind::OnDevice(_)))
                {
                    index
                } else {
                    live.push(LiveEntry {
                        id: ON_DEVICE_ID.to_owned(),
                        name: ON_DEVICE_NAME.to_owned(),
                        kind: LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::ALL[0])),
                    });
                    live.len() - 1
                }
            }
            _ => return Err(EntryError::ActiveLive),
        };
        Ok(Self {
            live,
            active,
            extras,
            dropped_legacy_server,
            dropped_active_legacy_server: removed_active,
        })
    }

    /// The entries a config written before the list existed meant: its on-device model and
    /// extra recognizer, switched on or not, with the key kept.
    fn migrated(
        engine: Option<EngineName>,
        model: Option<String>,
        server: Option<String>,
        secondary: Option<SecondaryStt>,
    ) -> Result<Self, EntryError> {
        if let Some(server) = server {
            let parsed =
                reqwest::Url::parse(server.trim()).map_err(|_| EntryError::LegacyServerUrl)?;
            if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
                return Err(EntryError::LegacyServerUrl);
            }
            if parsed.path() != "/" {
                return Err(EntryError::LegacyServerRoot);
            }
        }
        let on_device = |choice: SpeechChoice| LiveEntry {
            id: ON_DEVICE_ID.to_owned(),
            name: ON_DEVICE_NAME.to_owned(),
            kind: LiveKind::OnDevice(choice),
        };
        let live = match (engine, model) {
            // Legacy server settings are discarded; without a usable local selection the default
            // on-device recognizer is active.
            (None | Some(EngineName::Server), None) => {
                vec![on_device(SpeechChoice::Catalog(CatalogSpeech::ALL[0]))]
            }
            (None | Some(EngineName::Server), Some(_)) => {
                return Err(EntryError::ModelWithoutLocal);
            }
            (Some(EngineName::Local), model) => {
                let choice =
                    speech_choice(model.as_deref()).map_err(|reason| EntryError::Fields {
                        name: ON_DEVICE_NAME.to_owned(),
                        reason,
                    })?;
                let live = vec![on_device(choice)];
                live
            }
        };
        Ok(Self {
            live,
            active: 0,
            extras: secondary
                .map(|settings| ExtraEntry {
                    id: EXTRA_ID.to_owned(),
                    name: EXTRA_NAME.to_owned(),
                    settings,
                })
                .into_iter()
                .collect(),
            dropped_legacy_server: false,
            dropped_active_legacy_server: false,
        })
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
    } else if model.contains(['/', '\\']) {
        Ok(SpeechChoice::Folder(PathBuf::from(model)))
    } else {
        Err(format!(
            "Unknown speech model \"{model}\". Use {} or the path of a folder holding encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx and tokens.txt.",
            CatalogSpeech::ALL.map(CatalogSpeech::name).join(", ")
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EngineName {
    Server,
    Local,
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
    #[serde(rename = "thalvi_server")]
    LegacyServer {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        active: bool,
        #[serde(default, rename = "address")]
        _address: String,
    },
    OnDevice {
        id: String,
        name: String,
        #[serde(default)]
        active: bool,
        #[serde(default)]
        model: Option<String>,
    },
    #[serde(alias = "second_pass")]
    Extra {
        id: String,
        name: String,
        #[serde(default)]
        active: bool,
        address: String,
        model: String,
        #[serde(default)]
        api_key: String,
        #[serde(default)]
        api: SecondaryApi,
    },
}

enum Built {
    Live { entry: LiveEntry, on: bool },
    Extra(ExtraEntry),
    RemovedLegacyServer { active: bool },
}

impl Built {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Live { entry, .. } => (&entry.id, &entry.name),
            Self::Extra(entry) => (&entry.id, &entry.name),
            Self::RemovedLegacyServer { .. } => ("", ""),
        }
    }
}

impl RawEntry {
    /// Checks the fields. An extra entry that is switched off is kept as written unless
    /// `strict`, which is how an entry is checked where it is added or edited.
    fn build(self, strict: bool) -> Result<Built, EntryError> {
        if let Self::LegacyServer { active, .. } = &self
            && !strict
        {
            return Ok(Built::RemovedLegacyServer { active: *active });
        }
        let (id, name) = match &self {
            Self::Realtime { id, name, .. }
            | Self::OpenAi { id, name, .. }
            | Self::LegacyServer { id, name, .. }
            | Self::OnDevice { id, name, .. }
            | Self::Extra { id, name, .. } => (id.trim().to_owned(), name.trim().to_owned()),
        };
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(EntryError::Name);
        }
        if id.is_empty() || id.chars().count() > MAX_ID_CHARS {
            return Err(EntryError::Id(name));
        }
        let live = |kind, on| Built::Live {
            entry: LiveEntry {
                id: id.clone(),
                name: name.clone(),
                kind,
            },
            on,
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
            ),
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
                    settings: SecondaryStt::new(
                        &address,
                        &model,
                        api_key,
                        SecondaryApi::OpenAi,
                        true,
                    )
                    .map_err(|e| fields(&name, extra_reason(&e)))?,
                    request,
                    language: Language::parse(&language).map_err(|e| fields(&name, e))?,
                }),
                active,
            ),
            Self::LegacyServer { .. } => {
                return Err(fields(&name, "this speech server kind was removed."));
            }
            Self::OnDevice { active, model, .. } => live(
                LiveKind::OnDevice(speech_choice(model.as_deref()).map_err(|e| fields(&name, e))?),
                active,
            ),
            Self::Extra {
                active,
                address,
                model,
                api_key,
                api,
                ..
            } => Built::Extra(ExtraEntry {
                settings: SecondaryStt::new(&address, &model, api_key, api, active || strict)
                    .map(|settings| settings.with_enabled(active))
                    .map_err(|e| fields(&name, extra_reason(&e)))?,
                id,
                name,
            }),
        })
    }
}

fn extra_reason(error: &SecondarySttError) -> &'static str {
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
        .build(true)
        .map(drop)
        .map_err(|error| error.to_string())
}

/// What `stt` holds in a file: either the list, or the settings it replaced.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawStt {
    #[serde(default)]
    entries: Option<Vec<RawEntry>>,
    #[serde(default)]
    secondary: Option<SecondaryStt>,
    #[serde(default)]
    engine: Option<EngineName>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    pub(crate) phrases: crate::phrases::WordList,
}

impl RawStt {
    /// The speech servers this file means; `server` is the file's old top-level address.
    pub(crate) fn entries(
        self,
        server: Option<String>,
    ) -> Result<(Entries, crate::phrases::WordList), EntryError> {
        let phrases = self.phrases;
        let entries = match self.entries {
            Some(_)
                if self.secondary.is_some()
                    || self.engine.is_some()
                    || self.model.is_some()
                    || server.is_some() =>
            {
                return Err(EntryError::Mixed);
            }
            Some(entries) => Entries::new(entries)?,
            None => Entries::migrated(self.engine, self.model, server, self.secondary)?,
        };
        Ok((entries, phrases))
    }
}

impl Serialize for Entries {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum File<'a> {
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
            Extra {
                id: &'a str,
                name: &'a str,
                active: bool,
                address: &'a str,
                model: &'a str,
                api_key: &'a str,
                api: SecondaryApi,
            },
        }
        let live = self.live.iter().enumerate().map(|(index, entry)| {
            let (id, name, active) = (entry.id.as_str(), entry.name.as_str(), index == self.active);
            match &entry.kind {
                LiveKind::Realtime(realtime) => File::Realtime {
                    id,
                    name,
                    active,
                    address: &realtime.address,
                    model: &realtime.model,
                    api_key: &realtime.api_key,
                },
                LiveKind::OpenAi(live) => File::OpenAi {
                    id,
                    name,
                    active,
                    address: live.settings.address(),
                    model: live.settings.model(),
                    api_key: live.settings.api_key(),
                    request: live.request,
                    language: live.language.map(|l| l.to_string()).unwrap_or_default(),
                },
                LiveKind::OnDevice(choice) => File::OnDevice {
                    id,
                    name,
                    active,
                    model: choice.label(),
                },
            }
        });
        let second = self.extras.iter().map(|entry| File::Extra {
            id: &entry.id,
            name: &entry.name,
            active: entry.ticked(),
            address: entry.settings.address(),
            model: entry.settings.model(),
            api_key: entry.settings.api_key(),
            api: entry.settings.api(),
        });
        serializer.collect_seq(live.chain(second))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::DesktopConfig;

    fn entries(json: &str) -> Result<Entries, String> {
        DesktopConfig::from_json_str(json)
            .map(|config| config.stt.entries)
            .map_err(|error| error.to_string())
    }

    fn entries_of(json: &Value) -> Entries {
        entries(&json.to_string()).unwrap()
    }

    /// `(id, kind, address or model, active)` for every entry, live ones first.
    fn summary(entries: &Entries) -> Vec<(String, &'static str, String, bool)> {
        let live = entries.live().iter().map(|entry| {
            let (kind, place) = match &entry.kind {
                LiveKind::Realtime(realtime) => ("realtime", realtime.address().to_owned()),
                LiveKind::OpenAi(live) => ("open_ai", live.settings.address().to_owned()),
                LiveKind::OnDevice(choice) => ("on_device", choice.label()),
            };
            (
                entry.id.clone(),
                kind,
                place,
                entry.id == entries.active_live().id,
            )
        });
        let second = entries.extras().iter().map(|entry| {
            (
                entry.id.clone(),
                "extra",
                entry.settings.address().to_owned(),
                entry.ticked(),
            )
        });
        live.chain(second).collect()
    }

    fn row(
        id: &str,
        kind: &'static str,
        place: &str,
        active: bool,
    ) -> (String, &'static str, String, bool) {
        (id.to_owned(), kind, place.to_owned(), active)
    }

    const SECOND: &str = r#""url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k""#;

    #[test]
    fn every_old_config_shape_becomes_entries_with_nothing_re_entered() {
        let server = row("on-device", "on_device", "nemotron_160ms", true);
        let cases = [
            (
                "{}".to_owned(),
                vec![row("on-device", "on_device", "nemotron_160ms", true)],
            ),
            (r#"{"server": "ws://h:1"}"#.to_owned(), vec![server.clone()]),
            (
                r#"{"server": "ws://h:1", "stt": {"engine": "server"}}"#.to_owned(),
                vec![server],
            ),
            // A 0.1.x file always wrote its engine, so it keeps its recognizer.
            (
                r#"{"stt": {"engine": "server"}}"#.to_owned(),
                vec![row("on-device", "on_device", "nemotron_160ms", true)],
            ),
            (
                r#"{"stt": {"engine": "local"}}"#.to_owned(),
                vec![row("on-device", "on_device", "nemotron_160ms", true)],
            ),
            (
                r#"{"stt": {"engine": "local", "model": "unified_160ms"}}"#.to_owned(),
                vec![row("on-device", "on_device", "unified_160ms", true)],
            ),
            (
                r#"{"server": "ws://h:1", "stt": {"engine": "local", "model": "/m"}}"#.to_owned(),
                vec![row("on-device", "on_device", "/m", true)],
            ),
            (
                format!(
                    r#"{{"writing": {{"url": "http://127.0.0.1:9/v1", "model": "c"}}, "stt": {{"secondary": {{"enabled": true, {SECOND}}}}}}}"#
                ),
                vec![
                    row("on-device", "on_device", "nemotron_160ms", true),
                    row("extra", "extra", "http://127.0.0.1:9/v1", true),
                ],
            ),
            (
                format!(
                    r#"{{"stt": {{"engine": "local", "secondary": {{"enabled": false, {SECOND}}}}}}}"#
                ),
                vec![
                    row("on-device", "on_device", "nemotron_160ms", true),
                    row("extra", "extra", "http://127.0.0.1:9/v1", false),
                ],
            ),
        ];
        for (json, expected) in cases {
            let got = entries(&json).unwrap();
            assert_eq!(summary(&got), expected, "{json}");
        }
        let kept = entries(&format!(
            r#"{{"stt": {{"engine": "local", "secondary": {{"enabled": false, {SECOND}}}}}}}"#
        ))
        .unwrap();
        assert_eq!(kept.key_of("extra"), "k", "an unticked entry keeps its key");
        let blank = entries(
            r#"{"stt": {"secondary": {"enabled": false, "url": "", "model": "", "api_key": "k"}}}"#,
        )
        .unwrap();
        assert_eq!(blank.key_of("extra"), "k");
    }

    #[test]
    fn entries_round_trip_with_their_order_keys_and_inactive_setups() {
        let json = json!({"writing": {"url": "http://c/v1", "model": "c"}, "stt": {"entries": [
            {"kind": "realtime", "id": "a", "name": "Cybrex2", "active": false,
             "address": "ws://100.64.0.2:8012", "model": "nemotron", "api_key": "live-key"},
            {"kind": "open_ai", "id": "o", "name": "OpenAI local", "active": true,
             "address": "http://localhost:9000/v1", "model": "whisper", "api_key": "open-key"},
            {"kind": "thalvi_server", "id": "b", "name": "Office", "active": true,
             "address": "ws://127.0.0.1:8080"},
            {"kind": "on_device", "id": "c", "name": "Offline", "active": false, "model": "/m"},
            {"kind": "extra", "id": "z", "name": "Zed", "active": false,
             "address": "http://z/v1", "model": "m", "api_key": "zk", "api": "meta"},
            {"kind": "second_pass", "id": "y", "name": "Why", "active": true,
             "address": "http://y/v1", "model": "m"}
        ]}})
        .to_string();
        let config = DesktopConfig::from_json_str(&json).unwrap();
        let got = &config.stt.entries;
        assert_eq!(got.active_live().name, "OpenAI local");
        assert_eq!(
            got.extras()
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Zed", "Why"],
            "order is kept"
        );
        assert_eq!(
            got.ticked().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["Why"]
        );
        assert_eq!(
            (
                got.key_of("a"),
                got.key_of("o"),
                got.key_of("z"),
                got.key_of("y")
            ),
            ("live-key", "open-key", "zk", "")
        );
        let written = serde_json::to_string(&config).unwrap();
        assert_eq!(
            DesktopConfig::from_json_str(&written).unwrap(),
            config,
            "{written}"
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
        let second = |address: &str, model: &str| {
            json!({"kind": "extra", "id": "p", "name": "P", "active": true,
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
                with(vec![live("a", "A", true), second("ftp://h", "m")]),
                "Speech server \"P\": the address must be an http or https URL",
            ),
            (
                json!({"stt": {"entries": [live("a", "A", true)], "engine": "local"}}).to_string(),
                "stt.entries replaces stt.engine",
            ),
            (
                json!({"server": "ws://h:1", "stt": {"entries": [live("a", "A", true)]}})
                    .to_string(),
                "stt.entries replaces stt.engine",
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
    fn an_entry_is_checked_whole_where_it_is_added_but_kept_as_written_when_unticked() {
        let blank_second = json!({"kind": "extra", "id": "p", "name": "P", "active": false,
                                  "address": "", "model": "", "api_key": "k"});
        // Saved from an old config this way, so it still loads …
        let loaded = entries(
            &json!({"stt": {"entries": [
                {"kind": "on_device", "id": "d", "name": "Device", "active": true},
                blank_second
            ]}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(loaded.key_of("p"), "k");
        // … but where an entry is added or edited, every field is checked.
        assert!(
            check(&blank_second)
                .unwrap_err()
                .contains("the address must be an http or https URL")
        );
        let good = json!({"kind": "extra", "id": "p", "name": "P", "active": false,
                          "address": "http://h/v1", "model": "m"});
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
    fn a_saved_thalvi_server_entry_is_dropped_and_the_config_says_whether_it_was_active() {
        let device = |active: bool| json!({"kind": "on_device", "id": "d", "name": "Device", "active": active});
        let server = |active: bool| {
            json!({"kind": "thalvi_server", "id": "s", "name": "Office", "active": active,
                   "address": "ws://127.0.0.1:8080"})
        };
        let cases = [
            ("none", vec![device(true)], (false, false), "Device"),
            (
                "inactive",
                vec![server(false), device(true)],
                (true, false),
                "Device",
            ),
            (
                "active beside a device",
                vec![server(true), device(false)],
                (true, true),
                "Device",
            ),
            (
                "active alone",
                vec![server(true)],
                (true, true),
                ON_DEVICE_NAME,
            ),
        ];
        for (case, list, flags, active) in cases {
            let got = entries(&json!({"stt": {"entries": list}}).to_string()).unwrap();
            assert_eq!(
                (
                    got.dropped_legacy_server(),
                    got.dropped_active_legacy_server()
                ),
                flags,
                "{case}"
            );
            assert_eq!(got.active_live().name, active, "{case}");
            assert!(got.live().iter().all(|entry| entry.id != "s"), "{case}");
        }
        assert!(
            check(&server(false))
                .unwrap_err()
                .contains("this speech server kind was removed"),
            "where an entry is added, the removed kind is refused"
        );
    }

    #[test]
    fn lists_are_equal_only_when_entries_active_one_and_extras_all_match() {
        let live = |id: &str, name: &str, active: bool| json!({"kind": "on_device", "id": id, "name": name, "active": active});
        let second = json!({"kind": "extra", "id": "p", "name": "P", "active": true,
                            "address": "http://h/v1", "model": "m"});
        let list = |entries: Vec<Value>| {
            entries_of(
                &json!({"writing": {"url": "http://c/v1", "model": "c"}, "stt": {"entries": entries}}),
            )
        };
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
            (
                "an extra",
                list(vec![live("a", "A", true), live("b", "B", false), second]),
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
