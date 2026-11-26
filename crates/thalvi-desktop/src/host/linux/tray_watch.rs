//! Keeps the tray item listed when the tray host restarts or forgets its items.

use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::glib::ToVariant;

/// The name of the host that lists status notifier items; Waybar's tray takes it on Linux.
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// Where that host serves its interface.
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The standard interface for reading a D-Bus property.
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// The host's property holding the items it shows, each as `bus name/object path`.
const ITEMS: &str = "RegisteredStatusNotifierItems";
/// A reload must be noticed well inside the 5 s the user waits for the icon.
const INTERVAL: Duration = Duration::from_secs(1);
const CALL_TIMEOUT_MS: i32 = 5_000;

/// What one look at the tray host found.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The host lists this item already.
    Listed,
    /// The host had lost the item, and it was registered again.
    Registered,
    /// No host answered: no tray icon shows until one starts.
    NoHost,
    /// The host answered but would not take the item.
    Refused(String),
}

/// Keeps the tray item of the tray with id `id` registered, in a thread of its own.
pub fn keep_registered(id: &str) {
    let item = item_path(id);
    let _ = std::thread::Builder::new()
        .name("tray-watch".to_owned())
        .spawn(move || follow(&item));
}

/// The object path the tray library exports an item at, for the tray id `id`.
#[must_use]
pub fn item_path(id: &str) -> String {
    // The library's indicator id and the path it derives from it are its own convention.
    let id = format!("tray-icon tray app {id}");
    let clean: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("/org/ayatana/NotificationItem/{clean}")
}

/// Whether the tray host lists the item at `item`, registering it again when it does not.
#[must_use]
pub fn ensure_registered(bus: &gio::DBusConnection, item: &str) -> Outcome {
    let Some(unique) = bus.unique_name() else {
        return Outcome::Refused("Thalvi has no name on the session bus.".to_owned());
    };
    let listed = match listed_items(bus) {
        Ok(listed) => listed,
        Err(outcome) => return outcome,
    };
    let ours = format!("{unique}/");
    if listed.iter().any(|listed| listed.starts_with(&ours)) {
        return Outcome::Listed;
    }
    let call = bus.call_sync(
        Some(WATCHER),
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        Some(&(item,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
    );
    match call {
        Ok(_) => Outcome::Registered,
        Err(error) => Outcome::Refused(error.message().to_owned()),
    }
}

/// The items the tray host lists, or what to report instead when it cannot say.
fn listed_items(bus: &gio::DBusConnection) -> Result<Vec<String>, Outcome> {
    let reply = bus
        .call_sync(
            Some(WATCHER),
            WATCHER_PATH,
            PROPERTIES,
            "Get",
            Some(&(WATCHER, ITEMS).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .map_err(|_| Outcome::NoHost)?;
    let value = reply
        .get::<(glib::Variant,)>()
        .ok_or_else(|| Outcome::Refused("The tray host sent no item list.".to_owned()))?;
    value.0.get::<Vec<String>>().ok_or_else(|| {
        Outcome::Refused("The tray host sent an item list of another type.".to_owned())
    })
}

/// Watches the host, so a reload or a start after Thalvi registers the item again.
fn follow(item: &str) {
    let bus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(bus) => bus,
        Err(error) => {
            eprintln!(
                "The tray icon cannot follow the tray host: {}.",
                error.message()
            );
            return;
        }
    };
    // The tray was just built, so a host that is missing right away is what gets reported.
    let mut previous = Outcome::Listed;
    loop {
        std::thread::sleep(INTERVAL);
        let outcome = ensure_registered(&bus, item);
        announce(&previous, &outcome);
        previous = outcome;
    }
}

/// Says what changed in words, once per change, so a missing host is not reported every second.
fn announce(previous: &Outcome, outcome: &Outcome) {
    match outcome {
        Outcome::NoHost if !matches!(previous, Outcome::NoHost) => {
            eprintln!("No tray host is running. The tray icon appears when one starts.");
        }
        Outcome::Listed | Outcome::Registered
            if matches!(previous, Outcome::NoHost | Outcome::Refused(_)) =>
        {
            eprintln!("The tray host is back. The tray icon is registered again.");
        }
        Outcome::Refused(reason) if !matches!(previous, Outcome::Refused(_)) => {
            eprintln!("The tray icon could not be registered: {reason}");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use zbus::blocking::Connection;
    use zbus::blocking::connection::Builder;

    use super::*;

    const ID: &str = "thalvi";

    /// Runs the test again inside its own session bus, and says whether this run is that one.
    ///
    /// The tray host is a bus service, so a test needs a bus where it owns the watcher name and
    /// the developer's own tray cannot see anything.
    fn on_private_bus() -> bool {
        if std::env::var_os("THALVI_TRAY_WATCH_TEST_BUS").is_some() {
            return true;
        }
        let name = std::thread::current().name().expect("test name").to_owned();
        let exe = std::env::current_exe().expect("test binary");
        let status = Command::new("dbus-run-session")
            .arg("--")
            .arg(exe)
            .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env("THALVI_TRAY_WATCH_TEST_BUS", "1")
            .status()
            .expect("dbus-run-session");
        assert!(
            status.success(),
            "the test failed on its private session bus"
        );
        false
    }

    fn session_bus() -> gio::DBusConnection {
        gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).expect("session bus")
    }

    #[derive(Default)]
    struct Shown {
        /// The items the host lists, as `bus name/object path`.
        items: Vec<String>,
        /// What every `RegisterStatusNotifierItem` call was given.
        registered: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct FakeHost {
        shown: Arc<Mutex<Shown>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeHost {
        fn register_status_notifier_item(&self, service: &str) {
            self.shown
                .lock()
                .unwrap()
                .registered
                .push(service.to_owned());
        }

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> Vec<String> {
            self.shown.lock().unwrap().items.clone()
        }
    }

    impl FakeHost {
        fn start(items: Vec<String>) -> (Connection, Self) {
            let host = Self::default();
            host.shown.lock().unwrap().items = items;
            let connection = Builder::session()
                .expect("session bus")
                .name(WATCHER)
                .expect("watcher name")
                .serve_at(WATCHER_PATH, host.clone())
                .expect("watcher path")
                .build()
                .expect("host");
            (connection, host)
        }

        fn lists(&self, items: Vec<String>) {
            self.shown.lock().unwrap().items = items;
        }

        fn registered(&self) -> Vec<String> {
            self.shown.lock().unwrap().registered.clone()
        }
    }

    fn wait_for_no_watcher() {
        let bus = Connection::session().expect("session bus");
        for _ in 0..200 {
            let reply = bus
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(WATCHER,),
                )
                .expect("NameHasOwner");
            let (owned,): (bool,) = reply.body().deserialize().expect("NameHasOwner reply");
            if !owned {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the host kept {WATCHER}");
    }

    #[test]
    fn the_item_path_is_the_one_the_tray_library_exports() {
        assert_eq!(
            item_path(ID),
            "/org/ayatana/NotificationItem/tray_icon_tray_app_thalvi"
        );
    }

    #[test]
    fn a_host_that_resets_its_list_gets_the_item_again() {
        if !on_private_bus() {
            return;
        }
        let bus = session_bus();
        let unique = bus.unique_name().expect("bus name");
        let item = item_path(ID);
        let (_connection, host) = FakeHost::start(Vec::new());

        assert_eq!(ensure_registered(&bus, &item), Outcome::Registered);
        host.lists(vec![format!("{unique}{item}")]);
        assert_eq!(
            ensure_registered(&bus, &item),
            Outcome::Listed,
            "a listed item is not registered twice"
        );

        // Waybar's SIGUSR2 reload drops every item but keeps the watcher's name owner.
        host.lists(Vec::new());
        assert_eq!(ensure_registered(&bus, &item), Outcome::Registered);
        assert_eq!(host.registered(), vec![item.clone(), item]);
    }

    #[test]
    fn a_new_host_owner_gets_the_item_again() {
        if !on_private_bus() {
            return;
        }
        let bus = session_bus();
        let item = item_path(ID);
        let (first, _host) = FakeHost::start(Vec::new());
        assert_eq!(ensure_registered(&bus, &item), Outcome::Registered);
        first.release_name(WATCHER).expect("release watcher name");
        wait_for_no_watcher();
        drop(first);

        let (_second, host) = FakeHost::start(Vec::new());
        assert_eq!(ensure_registered(&bus, &item), Outcome::Registered);
        assert_eq!(host.registered(), vec![item]);
    }

    #[test]
    fn no_host_yet_registers_when_one_starts() {
        if !on_private_bus() {
            return;
        }
        let bus = session_bus();
        let item = item_path(ID);
        assert_eq!(ensure_registered(&bus, &item), Outcome::NoHost);

        let (_connection, host) = FakeHost::start(Vec::new());
        assert_eq!(ensure_registered(&bus, &item), Outcome::Registered);
        assert_eq!(host.registered(), vec![item]);
    }
}
