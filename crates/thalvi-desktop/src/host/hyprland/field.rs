//! Hyprland's field probe: reads the text object that received a paste over AT-SPI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use zbus::blocking::connection::Builder as ConnectionBuilder;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::OwnedObjectPath;

use super::Window;
use crate::learn::{Field, FieldEnd, FieldLimit, FieldProbe};

/// The source of an `object:text-changed` event, read only when a watch needs it.
trait Object: Send + Sync {
    /// The object's whole text, or why it cannot be read.
    fn text(&self) -> Result<String, FieldEnd>;
    /// Whether this is a password field.
    fn protected(&self) -> bool;
    /// Whether the object still exists.
    fn alive(&self) -> bool;
}

/// The seam over the accessibility bus: connect once, then wait for events.
trait Source: Send + Sync {
    /// Starts the connection and subscription; false when the bus cannot be reached.
    fn connect(&self) -> bool;
    /// Whether events can now be delivered.
    fn connected(&self) -> bool;
    /// Whether the connection attempt failed.
    fn unavailable(&self) -> bool;
    /// The next event at or after `since`, or none at `until`.
    fn wait(&self, since: Instant, until: Instant) -> Option<(Instant, u32, Box<dyn Object>)>;
}

const WAIT: Duration = Duration::from_secs(1);
const MAX_CHARS: usize = 20_000;
const PASSWORD_TEXT: u32 = 40;

/// One AT-SPI object that changed. Holds no text until a watch asks for it.
struct FieldObject {
    connection: Connection,
    sender: String,
    path: OwnedObjectPath,
}

impl FieldObject {
    fn accessible(&self) -> Result<Proxy<'_>, ()> {
        Proxy::new(
            &self.connection,
            self.sender.clone(),
            self.path.clone(),
            "org.a11y.atspi.Accessible",
        )
        .map_err(|_| ())
    }

    fn role(&self) -> Option<u32> {
        self.accessible().ok()?.call("GetRole", &()).ok()
    }
}

impl Object for FieldObject {
    fn text(&self) -> Result<String, FieldEnd> {
        let text = Proxy::new(
            &self.connection,
            self.sender.clone(),
            self.path.clone(),
            "org.a11y.atspi.Text",
        )
        .map_err(|_| FieldEnd::Unreadable)?
        .call("GetText", &(0i32, -1i32))
        .map_err(|_| FieldEnd::Unreadable)?;
        Ok(text)
    }

    fn protected(&self) -> bool {
        self.role() == Some(PASSWORD_TEXT)
    }

    fn alive(&self) -> bool {
        self.role().is_some()
    }
}

fn connection_pid(connection: &Connection, sender: &str) -> Option<u32> {
    Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .ok()?
    .call("GetConnectionUnixProcessID", &(sender.to_owned(),))
    .ok()
}

/// The live seam: one a11y connection for the app, a bounded queue of event sources.
struct A11y {
    connected: AtomicBool,
    unavailable: AtomicBool,
    events: Mutex<Option<Receiver<(Instant, u32, FieldObject)>>>,
}

impl A11y {
    fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
            events: Mutex::new(None),
        }
    }

    /// Whether `pid` is the window's process or one of its children: Electron's a11y connection
    /// can come from a renderer rather than the process that owns the window.
    fn belongs(pid: u32, window: u32) -> bool {
        let mut current = pid;
        for _ in 0..16 {
            if current == window {
                return true;
            }
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{current}/stat")) else {
                return false;
            };
            let Some(fields) = stat.rsplit_once(')') else {
                return false;
            };
            let Some(parent) = fields.1.split_whitespace().nth(1) else {
                return false;
            };
            let Ok(parent) = parent.parse() else {
                return false;
            };
            current = parent;
        }
        false
    }
}

impl Source for A11y {
    fn connect(&self) -> bool {
        let Ok(session) = Connection::session() else {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        };
        let Ok(address): zbus::Result<String> =
            Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
                .and_then(|bus| bus.call("GetAddress", &()))
        else {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        };
        let Ok(a11y) =
            ConnectionBuilder::address(address.as_str()).and_then(ConnectionBuilder::build)
        else {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        };
        let Ok(registry) = Proxy::new(
            &a11y,
            "org.a11y.atspi.Registry",
            "/org/a11y/atspi/registry",
            "org.a11y.atspi.Registry",
        ) else {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        };
        let registered: zbus::Result<()> = registry.call(
            "RegisterEvent",
            &(
                "object:text-changed".to_owned(),
                Vec::<String>::new(),
                a11y.unique_name()
                    .map_or_else(String::new, ToString::to_string),
            ),
        );
        if registered.is_err() {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        }
        let Ok(messages) = MessageIterator::for_match_rule(
            "type='signal',interface='org.a11y.atspi.Event.Object',member='TextChanged'",
            &a11y,
            Some(16),
        ) else {
            self.unavailable.store(true, Ordering::Relaxed);
            return false;
        };
        let (sender, receiver) = sync_channel(16);
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = Some(receiver);
        self.unavailable.store(false, Ordering::Relaxed);
        self.connected.store(true, Ordering::Relaxed);
        let reader = a11y.clone();
        std::thread::Builder::new()
            .name("thalvi-a11y".to_owned())
            .spawn(move || {
                for message in messages {
                    let Ok(message) = message else { continue };
                    let header = message.header();
                    let (Some(who), Some(path)) = (header.sender(), header.path()) else {
                        continue;
                    };
                    let pid = connection_pid(&reader, who.as_str()).unwrap_or(0);
                    let object = FieldObject {
                        connection: reader.clone(),
                        sender: who.to_string(),
                        path: path.to_owned().into(),
                    };
                    if sender.try_send((Instant::now(), pid, object)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        true
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn unavailable(&self) -> bool {
        self.unavailable.load(Ordering::Relaxed)
    }

    fn wait(&self, _since: Instant, until: Instant) -> Option<(Instant, u32, Box<dyn Object>)> {
        let guard = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        let receiver = guard.as_ref()?;
        let (at, pid, object) = receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .ok()?;
        Some((at, pid, Box::new(object)))
    }
}

static SOURCE: LazyLock<Arc<dyn Source>> = LazyLock::new(|| Arc::new(A11y::new()));
static LISTENER: LazyLock<()> = LazyLock::new(|| {
    let source = Arc::clone(&*SOURCE);
    std::thread::Builder::new()
        .name("thalvi-a11y-start".to_owned())
        .spawn(move || {
            source.connect();
        })
        .ok();
});

fn arm_with(
    source: &Arc<dyn Source>,
    window: &Window,
    active: fn(&Window) -> bool,
) -> Box<dyn FieldProbe> {
    Box::new(Armed {
        source: Arc::clone(source),
        window: window.clone(),
        since: Instant::now(),
        active,
    })
}

/// A probe armed just before a paste. It stores the window and the time; no bus work happens here.
struct Armed {
    source: Arc<dyn Source>,
    window: Window,
    since: Instant,
    active: fn(&Window) -> bool,
}

impl FieldProbe for Armed {
    fn field(self: Box<Self>, pasted: &str) -> Result<Box<dyn Field>, FieldLimit> {
        let until = self.since + WAIT;
        loop {
            if self.source.unavailable() {
                return Err(FieldLimit::NoAccessibility);
            }
            if Instant::now() >= until {
                return Err(FieldLimit::NotShared {
                    app: self.window.class.clone(),
                });
            }
            if !self.source.connected() {
                std::thread::park_timeout(Duration::from_millis(5));
                continue;
            }
            let Some((at, pid, object)) = self.source.wait(self.since, until) else {
                return Err(FieldLimit::NotShared {
                    app: self.window.class.clone(),
                });
            };
            if at < self.since
                || !A11y::belongs(pid, self.window.pid)
                || !object.text().is_ok_and(|text| text.contains(pasted))
            {
                continue;
            }
            return Ok(Box::new(FieldAt {
                object,
                window: self.window.clone(),
                active: self.active,
            }));
        }
    }
}

/// The field that received the paste, read on demand off the paste path.
struct FieldAt {
    object: Box<dyn Object>,
    window: Window,
    active: fn(&Window) -> bool,
}

impl Field for FieldAt {
    fn text(&mut self) -> Result<String, FieldEnd> {
        if !(self.active)(&self.window) {
            return Err(FieldEnd::FocusLeft);
        }
        if !self.object.alive() {
            return Err(FieldEnd::Closed);
        }
        if self.object.protected() {
            return Err(FieldEnd::Protected);
        }
        let text = self.object.text()?;
        if text.chars().count() > MAX_CHARS {
            return Err(FieldEnd::Unreadable);
        }
        Ok(text)
    }
}

/// Arms a watch of the focused field in `window`.
pub fn probe(window: &Window, active: fn(&Window) -> bool) -> Box<dyn FieldProbe> {
    LazyLock::force(&LISTENER);
    arm_with(&SOURCE, window, active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    type Case = (
        &'static str,
        Vec<(Instant, u32, FakeObject)>,
        Option<String>,
    );

    #[derive(Clone)]
    struct FakeObject {
        text: String,
        role: u32,
        alive: bool,
        fails: bool,
        reads: Arc<AtomicUsize>,
    }

    impl FakeObject {
        fn editable(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                role: 61,
                alive: true,
                fails: false,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Object for FakeObject {
        fn text(&self) -> Result<String, FieldEnd> {
            let read = self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fails && read > 0 {
                Err(FieldEnd::Unreadable)
            } else {
                Ok(self.text.clone())
            }
        }
        fn protected(&self) -> bool {
            self.role == PASSWORD_TEXT
        }
        fn alive(&self) -> bool {
            self.alive
        }
    }

    struct FakeSource {
        touched: Arc<AtomicBool>,
        events: Vec<(Instant, u32, FakeObject)>,
        next: AtomicUsize,
        unavailable: bool,
    }

    impl FakeSource {
        fn new(events: Vec<(Instant, u32, FakeObject)>, touched: Arc<AtomicBool>) -> Self {
            Self {
                touched,
                events,
                next: AtomicUsize::new(0),
                unavailable: false,
            }
        }
    }

    impl Source for FakeSource {
        fn connect(&self) -> bool {
            self.touched.store(true, Ordering::Relaxed);
            !self.unavailable
        }
        fn connected(&self) -> bool {
            !self.unavailable
        }
        fn unavailable(&self) -> bool {
            self.unavailable
        }
        fn wait(&self, since: Instant, until: Instant) -> Option<(Instant, u32, Box<dyn Object>)> {
            assert_eq!(until - since, WAIT, "the probe waits at most one second");
            let at = self.next.fetch_add(1, Ordering::Relaxed);
            let event = self.events.get(at)?;
            Some((
                event.0,
                event.1,
                Box::new(event.2.clone()) as Box<dyn Object>,
            ))
        }
    }

    static ACTIVE: AtomicBool = AtomicBool::new(true);
    fn active(_: &Window) -> bool {
        ACTIVE.load(Ordering::Relaxed)
    }

    fn window() -> Window {
        Window {
            address: "0x1".to_owned(),
            pid: 42,
            class: "t3code".to_owned(),
            title: String::new(),
        }
    }

    fn arm(source: &Arc<dyn Source>) -> Box<dyn FieldProbe> {
        arm_with(source, &window(), active)
    }

    #[test]
    fn the_probe_takes_the_first_matching_event_from_the_bound_window_after_arming() {
        let now = Instant::now() + Duration::from_secs(10);
        let pasted = "send the cyber x2";
        let matching = FakeObject::editable(&format!("before {pasted} after"));
        let other_text = FakeObject::editable("nothing pasted here");
        let cases: Vec<Case> = vec![
            (
                "another pid is ignored",
                vec![(now, 43, matching.clone())],
                None,
            ),
            (
                "an event before arming is ignored",
                vec![(
                    now.checked_sub(Duration::from_secs(11)).unwrap_or(now),
                    42,
                    matching.clone(),
                )],
                None,
            ),
            (
                "the first object containing the paste wins",
                vec![
                    (now, 42, other_text.clone()),
                    (now, 42, matching),
                    (now, 42, FakeObject::editable(&format!("later {pasted}"))),
                ],
                Some(format!("before {pasted} after")),
            ),
            ("no event gives NotShared", vec![], None),
            (
                "an event without the paste gives NotShared",
                vec![(now, 42, other_text)],
                None,
            ),
        ];
        for (name, events, expected) in cases {
            let touched = Arc::new(AtomicBool::new(false));
            let source: Arc<dyn Source> = Arc::new(FakeSource::new(events, Arc::clone(&touched)));
            let probe = arm(&source);
            assert!(
                !touched.load(Ordering::Relaxed),
                "{name}: arming must not touch the bus"
            );
            match (probe.field(pasted), expected) {
                (Ok(mut field), Some(want)) => {
                    assert_eq!(field.text().unwrap(), want, "{name}");
                }
                (Err(FieldLimit::NotShared { app }), None) => assert_eq!(app, "t3code", "{name}"),
                (Err(FieldLimit::Desktop(_) | FieldLimit::NoAccessibility), None) => {
                    panic!("{name}: the stub answered instead of selecting")
                }
                (Ok(_), None) => panic!("{name}: a field was found but none should be"),
                (Err(FieldLimit::NoAccessibility), Some(_)) => {
                    panic!("{name}: the bus was reported unreachable")
                }
                (Err(FieldLimit::Desktop(_)), Some(_)) => {
                    panic!("{name}: the desktop was reported unsupported")
                }
                (Err(FieldLimit::NotShared { .. }), Some(_)) => {
                    panic!("{name}: no field was shared")
                }
            }
        }
    }

    #[test]
    fn the_probe_reports_no_accessibility_when_the_bus_is_unreachable() {
        let touched = Arc::new(AtomicBool::new(false));
        let mut fake = FakeSource::new(vec![], touched);
        fake.unavailable = true;
        let source: Arc<dyn Source> = Arc::new(fake);
        let probe = arm(&source);
        assert!(matches!(
            probe.field("text"),
            Err(FieldLimit::NoAccessibility)
        ));
    }

    #[test]
    fn the_field_reports_why_its_text_cannot_be_read() {
        let now = Instant::now() + Duration::from_secs(10);
        let cases: Vec<(&str, FakeObject, bool, FieldEnd)> = vec![
            (
                "a password field is protected",
                FakeObject {
                    role: PASSWORD_TEXT,
                    ..FakeObject::editable("text")
                },
                true,
                FieldEnd::Protected,
            ),
            (
                "a vanished object is closed",
                FakeObject {
                    alive: false,
                    ..FakeObject::editable("text")
                },
                true,
                FieldEnd::Closed,
            ),
            (
                "a field left behind is FocusLeft",
                FakeObject::editable("text"),
                false,
                FieldEnd::FocusLeft,
            ),
            (
                "a failed read is unreadable",
                FakeObject {
                    fails: true,
                    ..FakeObject::editable("text")
                },
                true,
                FieldEnd::Unreadable,
            ),
            (
                "an overlong field is unreadable",
                FakeObject::editable(&format!("{}text", "x".repeat(MAX_CHARS + 1))),
                true,
                FieldEnd::Unreadable,
            ),
        ];
        for (name, object, is_active, expected) in cases {
            ACTIVE.store(is_active, Ordering::Relaxed);
            let touched = Arc::new(AtomicBool::new(false));
            let source: Arc<dyn Source> =
                Arc::new(FakeSource::new(vec![(now, 42, object)], touched));
            let probe = arm(&source);
            let mut field = probe
                .field("text")
                .unwrap_or_else(|limit| panic!("{name}: {limit:?}"));
            assert_eq!(field.text(), Err(expected), "{name}");
        }
        ACTIVE.store(true, Ordering::Relaxed);
    }
}
