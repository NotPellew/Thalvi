//! A host double that records every call and fails or panics the methods a test names.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use crate::host::{App, Host, HostError, TypedText, WebUrl};

/// The window the fake host binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeWindow(pub &'static str);

impl fmt::Display for FakeWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

enum Behavior {
    Fail(HostError),
    Panic,
}

/// Succeeds at everything unless told otherwise.
#[derive(Default)]
pub struct FakeHost {
    calls: Mutex<Vec<String>>,
    behavior: BTreeMap<&'static str, Behavior>,
}

impl FakeHost {
    #[must_use]
    pub fn failing(mut self, method: &'static str, error: HostError) -> Self {
        self.behavior.insert(method, Behavior::Fail(error));
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

    fn capture_target(&self) -> Result<FakeWindow, HostError> {
        self.record("capture_target", "capture_target".to_owned())?;
        Ok(FakeWindow("the editor"))
    }

    fn deliver(&self, target: &FakeWindow, text: &TypedText, enter: bool) -> Result<(), HostError> {
        self.record(
            "deliver",
            format!("deliver:{}:{:?}:enter={enter}", target.0, text.as_str()),
        )
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
}
