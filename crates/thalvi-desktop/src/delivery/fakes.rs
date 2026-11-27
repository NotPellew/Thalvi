//! A host double that records every call and fails or panics the methods a test names.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use std::collections::VecDeque;

use crate::host::{App, AppMatchLimit, Delivered, Host, HostError, PasteText, WebUrl, WindowApp};
use crate::learn::{Field, FieldEnd, FieldLimit, FieldProbe};

/// The window the fake host binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeWindow(pub &'static str);

impl fmt::Display for FakeWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

type FakeReads = VecDeque<Result<String, FieldEnd>>;

/// The reads a fake field will give, shared with the test that drives them.
#[derive(Clone, Default)]
pub struct FakeFieldSource(Arc<Mutex<(FakeReads, usize)>>);

impl FakeFieldSource {
    /// Queues one read of `text`.
    pub fn text(&self, text: impl Into<String>) {
        self.0
            .lock()
            .expect("the fake field's lock")
            .0
            .push_back(Ok(text.into()));
    }

    /// Queues one read that ends the field visit.
    pub fn end(&self, end: FieldEnd) {
        self.0
            .lock()
            .expect("the fake field's lock")
            .0
            .push_back(Err(end));
    }

    /// A field that reads this source.
    pub fn field(&self) -> Box<dyn Field> {
        Box::new(FakeField {
            source: self.clone(),
        })
    }
}

struct FakeField {
    source: FakeFieldSource,
}

impl Field for FakeField {
    fn text(&mut self) -> Result<String, FieldEnd> {
        let mut source = self.source.0.lock().expect("the fake field's lock");
        source.1 += 1;
        source.0.pop_front().unwrap_or_else(|| Ok(String::new()))
    }
}

/// Resolves into the field a test set up, or into a reason it cannot be watched.
#[derive(Clone)]
pub struct FakeProbe {
    source: FakeFieldSource,
    limit: Option<FieldLimit>,
}

impl FakeProbe {
    /// A probe whose field reads `source`.
    #[must_use]
    pub fn new(source: FakeFieldSource) -> Self {
        Self {
            source,
            limit: None,
        }
    }

    /// Why this probe cannot watch a field.
    #[must_use]
    pub fn limit(&self) -> Option<FieldLimit> {
        self.limit.clone()
    }

    /// A probe that cannot watch a field.
    #[must_use]
    pub fn unavailable(limit: FieldLimit) -> Self {
        Self {
            source: FakeFieldSource::default(),
            limit: Some(limit),
        }
    }
}

impl FieldProbe for FakeProbe {
    fn field(self: Box<Self>, _pasted: &str) -> Result<Box<dyn Field>, FieldLimit> {
        match self.limit {
            Some(limit) => Err(limit),
            None => Ok(Box::new(FakeField {
                source: self.source,
            })),
        }
    }
}

#[derive(Clone)]
enum Behavior {
    Fail(HostError),
    Panic,
}

/// Succeeds at everything unless told otherwise.
#[derive(Clone, Default)]
pub struct FakeHost {
    calls: Arc<Mutex<Vec<String>>>,
    behavior: BTreeMap<&'static str, Behavior>,
    clipboard_only: bool,
    terminal: bool,
    probe: Arc<Mutex<Option<FakeProbe>>>,
    boxed_probe: Arc<Mutex<Option<Box<dyn FieldProbe>>>>,
}

impl FakeHost {
    #[must_use]
    pub fn failing(mut self, method: &'static str, error: HostError) -> Self {
        self.behavior.insert(method, Behavior::Fail(error));
        self
    }

    /// Delivers to the clipboard only, as a desktop that cannot type does.
    #[must_use]
    pub fn clipboard_only(mut self) -> Self {
        self.clipboard_only = true;
        self
    }

    /// Binds a terminal.
    #[must_use]
    pub fn terminal(mut self) -> Self {
        self.terminal = true;
        self
    }

    /// Arms `probe` for the next paste.
    #[must_use]
    pub fn watching(self, probe: FakeProbe) -> Self {
        *self.probe.lock().expect("the fake host's lock") = Some(probe);
        self
    }

    /// Arms a custom probe for the next paste.
    #[must_use]
    pub fn watching_boxed(self, probe: Box<dyn FieldProbe>) -> Self {
        *self.boxed_probe.lock().expect("the fake host's lock") = Some(probe);
        self
    }

    #[must_use]
    pub fn panicking(mut self, method: &'static str) -> Self {
        self.behavior.insert(method, Behavior::Panic);
        self
    }

    /// Every call so far, as `method` or `method:argument`, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }

    fn record(&self, method: &'static str, call: String) -> Result<(), HostError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
        match self.behavior.get(method) {
            Some(Behavior::Fail(error)) => Err(error.clone()),
            Some(Behavior::Panic) => panic!("the fake host was told to panic in {method}"),
            None => Ok(()),
        }
    }
}

impl Host for FakeHost {
    type Target = FakeWindow;

    fn overlay_takes_clicks(&self) -> bool {
        true
    }

    fn copies_only(&self) -> bool {
        self.clipboard_only
    }

    fn is_terminal(&self, _target: &FakeWindow) -> bool {
        self.terminal
    }

    fn learn_fields(&self, on: bool) -> Result<(), FieldLimit> {
        self.record("learn_fields", format!("learn_fields:{on}"))
            .map_err(|_| FieldLimit::Desktop("The fake host refused the field listener."))
    }

    fn watch_field(&self, _target: &FakeWindow) -> Result<Box<dyn FieldProbe>, FieldLimit> {
        self.record("watch_field", "watch_field".to_owned())
            .map_err(|_| FieldLimit::Desktop("The fake host refused the field probe."))?;
        if let Some(probe) = self
            .boxed_probe
            .lock()
            .expect("the fake host's lock")
            .take()
        {
            return Ok(probe);
        }
        let probe = self
            .probe
            .lock()
            .expect("the fake host's lock")
            .clone()
            .unwrap_or_else(|| FakeProbe::new(FakeFieldSource::default()));
        match probe.limit() {
            Some(limit) => Err(limit),
            None => Ok(Box::new(probe)),
        }
    }

    fn writing_target(&self, target: &FakeWindow) -> Result<WindowApp, AppMatchLimit> {
        if self.clipboard_only {
            return Err(AppMatchLimit::Gnome);
        }
        Ok(WindowApp::new(target.0, ""))
    }

    fn recent_app(&self) -> Result<Option<WindowApp>, AppMatchLimit> {
        if self.clipboard_only {
            Err(AppMatchLimit::Gnome)
        } else {
            Ok(Some(WindowApp::new("the editor", "")))
        }
    }

    fn capture_target(&self) -> Result<FakeWindow, HostError> {
        self.record("capture_target", "capture_target".to_owned())?;
        Ok(FakeWindow("the editor"))
    }

    fn deliver(
        &self,
        target: &FakeWindow,
        text: &PasteText,
        enter: bool,
    ) -> Result<Delivered, HostError> {
        self.record(
            "deliver",
            format!("deliver:{}:{:?}:enter={enter}", target.0, text.as_str()),
        )
        .map(|()| {
            if self.clipboard_only {
                Delivered::OnClipboard
            } else {
                Delivered::InWindow
            }
        })
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        self.record("copy", format!("copy:{text:?}"))
    }

    fn focus_app(&self, app: &App) -> Result<FakeWindow, HostError> {
        self.record("focus_app", format!("focus_app:{}", app.classes.join(",")))?;
        Ok(FakeWindow("the app"))
    }

    fn open_url(&self, url: &WebUrl) -> Result<(), HostError> {
        self.record("open_url", format!("open_url:{}", url.as_str()))
    }

    fn open_folder(&self, path: &Path) -> Result<(), HostError> {
        self.record("open_folder", format!("open_folder:{}", path.display()))
    }
}
