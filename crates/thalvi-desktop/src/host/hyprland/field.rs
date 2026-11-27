//! Hyprland's field probe: reads the text object that received a paste over AT-SPI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
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

/// The bus seam a probe waits on. Arming a probe touches none of it.
trait Source: Send + Sync {
    /// Whether a listener is connected and can deliver events.
    fn connected(&self) -> bool;
    /// Whether the bus was tried and could not be reached.
    fn unavailable(&self) -> bool;
    /// The oldest untaken event from `window`'s process since `since`, or none at `until`.
    fn wait(&self, since: Instant, window: u32, until: Instant) -> Option<Seen>;
}

/// The registry call that tells apps an assistive client is listening.
trait Subscribe: Send + Sync {
    /// Register `object:text-changed` for the listener named `bus_name`.
    fn register(&self, bus_name: &str) -> zbus::Result<()>;
}

/// Announce the listener so apps start emitting text events to it.
fn announce(registrar: &dyn Subscribe) -> bool {
    registrar.register("").is_ok()
}

/// Re-announce when a new application connects to the accessibility bus.
fn reannounce(registrar: &dyn Subscribe) {
    let _ = announce(registrar);
}

const WAIT: Duration = Duration::from_secs(1);
const MAX_CHARS: usize = 20_000;
const PASSWORD_TEXT: u32 = 40;
const MAX_RECENT: usize = 64;

/// One event's source: the process and the object that changed.
type Key = (String, String);
type Recent = (Instant, u32, Key, Arc<dyn Object>);
type Seen = (Instant, u32, Arc<dyn Object>);

/// A connection's view of the registry, used to announce the listener.
struct Registry {
    connection: Connection,
}

impl Subscribe for Registry {
    fn register(&self, bus_name: &str) -> zbus::Result<()> {
        Proxy::new(
            &self.connection,
            "org.a11y.atspi.Registry",
            "/org/a11y/atspi/registry",
            "org.a11y.atspi.Registry",
        )?
        .call(
            "RegisterEvent",
            &(
                "object:text-changed".to_owned(),
                Vec::<String>::new(),
                bus_name.to_owned(),
            ),
        )
    }
}

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

/// Recent event sources, oldest first, and a wakeup for probes waiting on a fresh one.
struct Events {
    recent: Mutex<Vec<Recent>>,
    wake: Condvar,
}

impl Events {
    fn new() -> Self {
        Self {
            recent: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        }
    }

    /// Keeps one recent object per source; an event with no process is dropped.
    fn record(&self, at: Instant, pid: Option<u32>, key: Key, object: Arc<dyn Object>) {
        let Some(pid) = pid else { return };
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        recent.retain(|(_, _, kept, _)| *kept != key);
        recent.push((at, pid, key, object));
        if recent.len() > MAX_RECENT {
            recent.remove(0);
        }
        self.wake.notify_all();
    }

    /// Takes the oldest event from `window`'s process at or after `since`.
    fn wait(&self, since: Instant, window: u32, until: Instant) -> Option<Seen> {
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let selected = recent
                .iter()
                .position(|(at, pid, _, _)| A11y::belongs(*pid, window) && *at >= since);
            if let Some(index) = selected {
                let (at, pid, _, object) = recent.remove(index);
                return Some((at, pid, object));
            }
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self
                .wake
                .wait_timeout(recent, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            recent = guard;
        }
    }

    fn wake_all(&self) {
        self.wake.notify_all();
    }
}

/// Sender-to-process lookup, cached per unique bus name so a keystroke needs no round trip.
#[derive(Default)]
struct PidCache(Mutex<HashMap<String, u32>>);

impl PidCache {
    fn lookup(&self, connection: &Connection, sender: &str) -> Option<u32> {
        if let Some(pid) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(sender)
        {
            return Some(*pid);
        }
        let Some(pid) = connection_pid(connection, sender) else {
            self.forget(sender);
            return None;
        };
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(sender.to_owned(), pid);
        Some(pid)
    }

    fn forget(&self, name: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }
}

/// The live seam: one a11y connection for the app, held for as long as learning is on.
struct A11y {
    events: Events,
    pids: PidCache,
    connected: AtomicBool,
    unavailable: AtomicBool,
    wanted: AtomicBool,
    generation: AtomicU64,
    connection: Mutex<Option<Connection>>,
}

impl A11y {
    fn new() -> Self {
        Self {
            events: Events::new(),
            pids: PidCache::default(),
            connected: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            connection: Mutex::new(None),
        }
    }

    fn active(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation && self.wanted.load(Ordering::Acquire)
    }

    fn fail(&self, generation: u64) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.unavailable.store(true, Ordering::Release);
        self.connected.store(false, Ordering::Release);
        self.events.wake_all();
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

    fn start(self: &Arc<Self>) {
        if self.wanted.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.unavailable.store(false, Ordering::Release);
        let source = Arc::clone(self);
        std::thread::Builder::new()
            .name("thalvi-a11y".to_owned())
            .spawn(move || source.connect(generation))
            .ok();
    }

    fn stop(self: &Arc<Self>) {
        if !self.wanted.swap(false, Ordering::AcqRel) {
            return;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.connected.store(false, Ordering::Release);
        self.events.wake_all();
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(connection) = connection {
            std::thread::Builder::new()
                .name("thalvi-a11y-stop".to_owned())
                .spawn(move || {
                    let _ = connection.close();
                })
                .ok();
        }
    }

    /// Opens the a11y connection and its two subscriptions, or `None` when the bus refuses.
    fn open() -> Option<(Connection, MessageIterator, MessageIterator)> {
        let session = Connection::session().ok()?;
        let address: String = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .and_then(|bus| bus.call("GetAddress", &()))
            .ok()?;
        let a11y = ConnectionBuilder::address(address.as_str())
            .and_then(ConnectionBuilder::build)
            .ok()?;
        let registry = Registry {
            connection: a11y.clone(),
        };
        if !announce(&registry) {
            return None;
        }
        let messages = MessageIterator::for_match_rule(
            "type='signal',interface='org.a11y.atspi.Event.Object',member='TextChanged'",
            &a11y,
            None,
        )
        .ok()?;
        let owners = MessageIterator::for_match_rule(
            "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
            &a11y,
            Some(8),
        )
        .ok()?;
        Some((a11y, messages, owners))
    }

    fn connect(self: Arc<Self>, generation: u64) {
        let Some((a11y, messages, owners)) = Self::open() else {
            self.fail(generation);
            return;
        };
        if !self.active(generation) {
            return;
        }
        *self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(a11y.clone());
        if !self.active(generation) {
            return;
        }
        self.unavailable.store(false, Ordering::Release);
        self.connected.store(true, Ordering::Release);
        self.events.wake_all();

        let owner_watcher = Arc::clone(&self);
        let registrar: Arc<dyn Subscribe> = Arc::new(Registry {
            connection: a11y.clone(),
        });
        let own = a11y
            .unique_name()
            .map_or_else(String::new, ToString::to_string);
        std::thread::Builder::new()
            .name("thalvi-a11y-owners".to_owned())
            .spawn(move || {
                for message in owners {
                    if !owner_watcher.active(generation) {
                        break;
                    }
                    let Ok(message) = message else { continue };
                    let Ok((name, _, owner)) =
                        message.body().deserialize::<(String, String, String)>()
                    else {
                        continue;
                    };
                    owner_watcher.pids.forget(&name);
                    if !owner.is_empty() && name != own {
                        reannounce(registrar.as_ref());
                    }
                }
            })
            .ok();

        for message in messages {
            if !self.active(generation) {
                break;
            }
            let Ok(message) = message else { continue };
            let header = message.header();
            let (Some(who), Some(path)) = (header.sender(), header.path()) else {
                continue;
            };
            let Some(pid) = self.pids.lookup(&a11y, who.as_str()) else {
                continue;
            };
            let object = Arc::new(FieldObject {
                connection: a11y.clone(),
                sender: who.to_string(),
                path: path.to_owned().into(),
            });
            self.events.record(
                Instant::now(),
                Some(pid),
                (who.to_string(), path.to_string()),
                object,
            );
        }
        self.fail(generation);
    }
}

impl Source for A11y {
    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn unavailable(&self) -> bool {
        self.unavailable.load(Ordering::Acquire)
    }

    fn wait(&self, since: Instant, window: u32, until: Instant) -> Option<Seen> {
        self.events.wait(since, window, until)
    }
}

static SOURCE: LazyLock<Arc<A11y>> = LazyLock::new(|| Arc::new(A11y::new()));

/// Starts or stops the a11y listener when learning switches; no bus work happens on the caller.
pub fn listen(on: bool) {
    if on {
        SOURCE.start();
    } else {
        SOURCE.stop();
    }
}

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
            if self.source.unavailable() || !self.source.connected() {
                return Err(FieldLimit::NoAccessibility);
            }
            if Instant::now() >= until {
                return Err(FieldLimit::NotShared {
                    app: self.window.class.clone(),
                });
            }
            let Some((at, pid, object)) = self.source.wait(self.since, self.window.pid, until)
            else {
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
                left: false,
            }));
        }
    }
}

/// The field that received the paste, read on demand off the paste path.
/// Once focus leaves the window, one more read returns the field as the user left it, then
/// [`FieldEnd::FocusLeft`].
struct FieldAt {
    object: Arc<dyn Object>,
    window: Window,
    active: fn(&Window) -> bool,
    left: bool,
}

impl Field for FieldAt {
    fn text(&mut self) -> Result<String, FieldEnd> {
        if !(self.active)(&self.window) {
            if self.left {
                return Err(FieldEnd::FocusLeft);
            }
            self.left = true;
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

/// Arms a watch of the focused field in `window`, without touching the bus.
pub fn probe(window: &Window, active: fn(&Window) -> bool) -> Box<dyn FieldProbe> {
    let live: Arc<A11y> = Arc::clone(&*SOURCE);
    let source: Arc<dyn Source> = live;
    arm_with(&source, window, active)
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
        waits: Arc<AtomicUsize>,
        events: Vec<(Instant, u32, FakeObject)>,
        unavailable: bool,
        listener: bool,
    }

    impl FakeSource {
        fn new(events: Vec<(Instant, u32, FakeObject)>, waits: Arc<AtomicUsize>) -> Self {
            Self {
                waits,
                events,
                unavailable: false,
                listener: true,
            }
        }
    }

    impl Source for FakeSource {
        fn connected(&self) -> bool {
            self.listener && !self.unavailable
        }
        fn unavailable(&self) -> bool {
            self.unavailable
        }
        fn wait(&self, since: Instant, window: u32, until: Instant) -> Option<Seen> {
            assert_eq!(until - since, WAIT, "the probe waits at most one second");
            let at = self.waits.fetch_add(1, Ordering::Relaxed);
            let event = self.events.get(at)?;
            let _ = window;
            Some((event.0, event.1, Arc::new(event.2.clone())))
        }
    }

    #[derive(Default)]
    struct FakeRegistry {
        names: Mutex<Vec<String>>,
    }

    impl Subscribe for FakeRegistry {
        fn register(&self, bus_name: &str) -> zbus::Result<()> {
            self.names
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bus_name.to_owned());
            Ok(())
        }
    }

    #[test]
    fn the_listener_announces_itself_to_apps_that_connect_later() {
        let cases: Vec<(&str, bool)> = vec![
            ("the initial registration names no listener", true),
            ("a new app connection re-announces the listener", false),
        ];
        for (name, initial) in cases {
            let registry = Arc::new(FakeRegistry::default());
            if initial {
                assert!(announce(registry.as_ref()));
            } else {
                reannounce(registry.as_ref());
            }
            assert_eq!(
                *registry
                    .names
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
                vec![String::new()],
                "{name}"
            );
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
            let waits = Arc::new(AtomicUsize::new(0));
            let source: Arc<dyn Source> = Arc::new(FakeSource::new(events, Arc::clone(&waits)));
            let probe = arm(&source);
            assert_eq!(
                waits.load(Ordering::Relaxed),
                0,
                "{name}: arming touched the bus"
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
        let waits = Arc::new(AtomicUsize::new(0));
        let mut fake = FakeSource::new(vec![], waits);
        fake.unavailable = true;
        let source: Arc<dyn Source> = Arc::new(fake);
        let probe = arm(&source);
        assert!(matches!(
            probe.field("text"),
            Err(FieldLimit::NoAccessibility)
        ));
    }

    #[test]
    fn a_probe_without_a_listener_reports_no_accessibility_without_waiting() {
        let waits = Arc::new(AtomicUsize::new(0));
        let mut fake = FakeSource::new(
            vec![(Instant::now(), 42, FakeObject::editable("some text"))],
            Arc::clone(&waits),
        );
        fake.listener = false;
        let source: Arc<dyn Source> = Arc::new(fake);
        let probe = arm(&source);
        assert!(
            matches!(probe.field("text"), Err(FieldLimit::NoAccessibility)),
            "no listener means the bus was never reached"
        );
        assert_eq!(
            waits.load(Ordering::Relaxed),
            0,
            "the probe never waited on the bus"
        );
    }

    #[test]
    fn a_burst_of_foreign_events_keeps_the_paste_and_the_listener_alive() {
        let pasted = "the pasted words";
        let cases: Vec<(&str, Vec<FakeObject>, &str)> = vec![
            (
                "the paste survives a burst of other apps' events",
                vec![FakeObject::editable(&format!("before {pasted} after"))],
                pasted,
            ),
            (
                "a later object from the same process does not hide the paste",
                vec![
                    FakeObject::editable(&format!("before {pasted} after")),
                    FakeObject::editable("an unrelated object in the same process"),
                ],
                pasted,
            ),
            (
                "the listener still takes events after a burst",
                vec![FakeObject::editable("a second paste")],
                "a second paste",
            ),
        ];
        for (name, objects, want) in cases {
            let events = Events::new();
            let at = Instant::now();
            for pid in 0..100 {
                events.record(
                    at,
                    Some(900_000 + pid),
                    ("other".to_owned(), pid.to_string()),
                    Arc::new(FakeObject::editable("other app's text")),
                );
            }
            for (i, object) in objects.into_iter().enumerate() {
                events.record(
                    at + Duration::from_millis(i as u64 + 1),
                    Some(42),
                    ("test".to_owned(), i.to_string()),
                    Arc::new(object),
                );
            }
            let (_, _, found) = events
                .wait(at, 42, at + Duration::from_secs(1))
                .unwrap_or_else(|| panic!("{name}: no event was kept"));
            assert!(found.text().is_ok_and(|text| text.contains(want)), "{name}");
        }
    }

    #[test]
    fn an_event_without_a_process_id_is_not_recorded() {
        let events = Events::new();
        let at = Instant::now();
        events.record(
            at,
            None,
            ("test".to_owned(), "0".to_owned()),
            Arc::new(FakeObject::editable("Cyber X2")),
        );
        assert!(
            events.wait(at, 0, at + Duration::from_millis(20)).is_none(),
            "an unknown pid is not invented as zero"
        );
    }

    /// Consecutive reads of one field.
    type Reads = Vec<Result<String, FieldEnd>>;

    #[test]
    fn the_field_reports_why_its_text_cannot_be_read() {
        let now = Instant::now() + Duration::from_secs(10);
        let cases: Vec<(&str, FakeObject, bool, Reads)> = vec![
            (
                "a password field is protected",
                FakeObject {
                    role: PASSWORD_TEXT,
                    ..FakeObject::editable("text")
                },
                true,
                vec![Err(FieldEnd::Protected)],
            ),
            (
                "a vanished object is closed",
                FakeObject {
                    alive: false,
                    ..FakeObject::editable("text")
                },
                true,
                vec![Err(FieldEnd::Closed)],
            ),
            (
                "a field left behind is read once more, then FocusLeft",
                FakeObject::editable("text"),
                false,
                vec![Ok("text".to_owned()), Err(FieldEnd::FocusLeft)],
            ),
            (
                "a failed read is unreadable",
                FakeObject {
                    fails: true,
                    ..FakeObject::editable("text")
                },
                true,
                vec![Err(FieldEnd::Unreadable)],
            ),
            (
                "an overlong field is unreadable",
                FakeObject::editable(&format!("{}text", "x".repeat(MAX_CHARS + 1))),
                true,
                vec![Err(FieldEnd::Unreadable)],
            ),
        ];
        for (name, object, is_active, expected) in cases {
            ACTIVE.store(is_active, Ordering::Relaxed);
            let waits = Arc::new(AtomicUsize::new(0));
            let source: Arc<dyn Source> = Arc::new(FakeSource::new(vec![(now, 42, object)], waits));
            let probe = arm(&source);
            let mut field = probe
                .field("text")
                .unwrap_or_else(|limit| panic!("{name}: {limit:?}"));
            let reads: Vec<_> = expected.iter().map(|_| field.text()).collect();
            assert_eq!(reads, expected, "{name}");
        }
        ACTIVE.store(true, Ordering::Relaxed);
    }
}
