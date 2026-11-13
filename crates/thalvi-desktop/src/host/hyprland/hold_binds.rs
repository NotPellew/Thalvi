//! Binds the hold and write keys in Hyprland at runtime, and puts them back after a config reload.

use std::io::{self, BufRead, BufReader};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;

use crate::hold::{HoldKey, HoldKind};
use crate::host::command::{CommandRunner, tool};
use crate::host::{HoldWatch, HostError};

/// What the user is told when Hyprland's reloads can no longer be followed.
const RELOADS_NOT_FOLLOWED: &str = "Thalvi cannot follow Hyprland's config reloads. The hold keys stop working after a reload until Thalvi is restarted.";

/// One bind Thalvi asks of Hyprland: a key without modifiers that runs `command`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bind {
    key: String,
    /// Fires when the key goes up instead of down.
    release: bool,
    command: String,
}

impl Bind {
    fn keyword(&self) -> String {
        let bind = if self.release { "bindrti" } else { "bind" };
        format!("keyword {bind} , {}, exec, {}", self.key, self.command)
    }

    /// Whether `row` is this very bind.
    fn is(&self, row: &Row) -> bool {
        row.on(&self.key)
            && row.release == self.release
            && row.dispatcher == "exec"
            && row.arg.trim() == self.command
    }
}

/// One bind as `hyprctl -j binds` prints it.
#[derive(Deserialize)]
struct Row {
    #[serde(default)]
    release: bool,
    #[serde(default)]
    modmask: u32,
    #[serde(default)]
    key: String,
    #[serde(default)]
    dispatcher: String,
    #[serde(default)]
    arg: String,
}

impl Row {
    /// Whether the bind fires on `key` pressed alone, which is when a bind of Thalvi's would.
    fn on(&self, key: &str) -> bool {
        self.modmask == 0 && self.key.eq_ignore_ascii_case(key)
    }

    /// Whether the bind starts or ends one of Thalvi's holds, whoever wrote it.
    fn is_thalvi_hold(&self) -> bool {
        self.dispatcher == "exec"
            && self.arg.to_ascii_lowercase().contains("thalvi")
            && self.arg.split_whitespace().next_back().is_some_and(|word| {
                ["hold-start", "hold-end", "write-start", "write-end"].contains(&word)
            })
    }
}

/// `exe` as one shell word for the `exec` of a bind.
fn quoted(exe: &Path) -> Result<String, HostError> {
    // A `;` would end the command in `hyprctl --batch`.
    exe.to_str()
        .filter(|path| !path.chars().any(|c| c == ';' || c.is_control()))
        .map(|path| format!("'{}'", path.replace('\'', r"'\''")))
        .ok_or_else(|| {
            HostError::Unsupported(format!(
                "Thalvi's path {} cannot be used in a Hyprland bind, so the hold keys cannot be bound.",
                exe.display()
            ))
        })
}

/// Refuses a key with a modifier, and `AltGr`, which is one: Hyprland types a held modifier into the paste.
pub(super) fn check(keys: &[(HoldKind, HoldKey)]) -> Result<(), HostError> {
    match keys
        .iter()
        .find(|(_, key)| !key.modifiers().is_empty() || key.is_altgr())
    {
        Some((_, key)) => Err(HostError::Unsupported(format!(
            "Hyprland can bind only F1 to F12 without a modifier for the hold keys, not {key}."
        ))),
        None => Ok(()),
    }
}

/// The binds that start and end each hold: Hyprland takes F1 to F12 alone and no other key.
fn wanted(exe: &Path, keys: &[(HoldKind, HoldKey)]) -> Result<Vec<Bind>, HostError> {
    check(keys)?;
    let exe = quoted(exe)?;
    let mut binds = Vec::new();
    for (kind, key) in keys {
        let (start, end) = match kind {
            HoldKind::Dictate => ("hold-start", "hold-end"),
            HoldKind::Write => ("write-start", "write-end"),
        };
        for (action, release) in [(start, false), (end, true)] {
            binds.push(Bind {
                key: key.to_string(),
                release,
                command: format!("{exe} {action}"),
            });
        }
    }
    Ok(binds)
}

/// The binds of `wanted` that Hyprland lacks, or the notice why they cannot be added: another bind
/// holds a key, or a hold bind that Thalvi did not add does. A bind identical to a wanted one is
/// adopted, so it is neither added again nor in the way.
fn missing<'a>(wanted: &'a [Bind], rows: &[Row]) -> Result<Vec<&'a Bind>, String> {
    let mut keys: Vec<&str> = wanted.iter().map(|bind| bind.key.as_str()).collect();
    keys.dedup();
    let (mut doubled, mut taken) = (Vec::new(), Vec::new());
    for key in keys {
        let strangers: Vec<&Row> = rows
            .iter()
            .filter(|row| row.on(key) && !wanted.iter().any(|bind| bind.is(row)))
            .collect();
        if strangers.iter().any(|row| row.is_thalvi_hold()) {
            doubled.push(key);
        } else if !strangers.is_empty() {
            taken.push(key);
        }
    }
    let mut notices = Vec::new();
    if !doubled.is_empty() {
        notices.push(format!(
            "Hyprland already binds {} to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start and write-end lines from hyprland.conf. If it has none, a Thalvi that did not quit left them, and hyprctl reload clears them.",
            doubled.join(" and ")
        ));
    }
    match taken[..] {
        [] => {}
        [key] => notices.push(format!(
            "{key} is already bound in Hyprland. Pick another key or remove that bind."
        )),
        _ => notices.push(format!(
            "{} are already bound in Hyprland. Pick other keys or remove those binds.",
            taken.join(" and ")
        )),
    }
    if !notices.is_empty() {
        return Err(notices.join(" "));
    }
    Ok(wanted
        .iter()
        .filter(|bind| !rows.iter().any(|row| bind.is(row)))
        .collect())
}

/// The binds this process has put in Hyprland, each with the watches that rely on it: a key change
/// binds the new keys before the old go, so a key kept by both must stay bound.
#[derive(Clone, Default)]
pub(super) struct Installed(Arc<Mutex<Vec<(Bind, usize)>>>);

impl Installed {
    fn entries(&self) -> MutexGuard<'_, Vec<(Bind, usize)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rely_on(&self, binds: &[Bind]) {
        let mut entries = self.entries();
        for bind in binds {
            match entries.iter_mut().find(|(held, _)| held == bind) {
                Some((_, watches)) => *watches += 1,
                None => entries.push((bind.clone(), 1)),
            }
        }
    }

    /// Stops relying on `binds`; returns the keys no watch relies on any longer.
    fn give_up(&self, binds: &[Bind]) -> Vec<String> {
        let mut entries = self.entries();
        for bind in binds {
            if let Some(at) = entries.iter().position(|(held, _)| held == bind) {
                entries[at].1 -= 1;
                if entries[at].1 == 0 {
                    entries.remove(at);
                }
            }
        }
        let mut free: Vec<String> = Vec::new();
        for bind in binds {
            if !free.contains(&bind.key) && !entries.iter().any(|(held, _)| held.key == bind.key) {
                free.push(bind.key.clone());
            }
        }
        free
    }

    fn relies_on_key(&self, key: &str) -> bool {
        self.entries().iter().any(|(held, _)| held.key == key)
    }
}

struct Shared<R> {
    run: Arc<R>,
    installed: Installed,
    binds: Vec<Bind>,
    /// False once the watch is dropped, after which the binds are no longer to be kept.
    live: Mutex<bool>,
    tell: Box<dyn Fn(String) + Send + Sync>,
}

impl<R: CommandRunner> Shared<R> {
    /// Adds the binds Hyprland lacks, after checking that nothing else holds their keys.
    fn ensure(&self) -> Result<(), HostError> {
        let list = self
            .run
            .run("hyprctl", &["-j", "binds"], None)
            .map_err(|error| tool(&error))?;
        let rows: Vec<Row> = serde_json::from_str(&list).map_err(|_| {
            HostError::NothingTyped("The desktop returned an invalid bind list.".to_owned())
        })?;
        let missing = missing(&self.binds, &rows).map_err(HostError::Unsupported)?;
        if missing.is_empty() {
            return Ok(());
        }
        let batch = missing
            .iter()
            .map(|bind| bind.keyword())
            .collect::<Vec<_>>()
            .join("; ");
        let refusal = match self.run.run("hyprctl", &["--batch", &batch], None) {
            Ok(reply) => {
                let replies: Vec<&str> = reply
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                if replies.len() == missing.len() && replies.iter().all(|reply| *reply == "ok") {
                    return Ok(());
                }
                replies
                    .iter()
                    .find(|reply| **reply != "ok")
                    .map_or_else(|| "no answer".to_owned(), |reply| (*reply).to_owned())
            }
            Err(error) => error.to_string(),
        };
        // Some of the batch may have gone in.
        let mut added: Vec<String> = Vec::new();
        for bind in &missing {
            if !added.contains(&bind.key) && !self.installed.relies_on_key(&bind.key) {
                added.push(bind.key.clone());
            }
        }
        self.unbind(&added);
        Err(HostError::NothingTyped(format!(
            "Hyprland would not bind the hold keys: {}.",
            refusal.trim_end_matches('.')
        )))
    }

    fn unbind(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let batch = keys
            .iter()
            .map(|key| format!("keyword unbind , {key}"))
            .collect::<Vec<_>>()
            .join("; ");
        if let Err(error) = self.run.run("hyprctl", &["--batch", &batch], None) {
            (self.tell)(format!(
                "Thalvi could not remove its hold key binds from Hyprland: {error} They stay until Hyprland reloads its config."
            ));
        }
    }

    /// Puts the binds back after Hyprland dropped them, unless something else took a key meanwhile.
    fn rebind(&self) {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if !*live {
            return;
        }
        if let Err(error) = self.ensure() {
            (self.tell)(format!(
                "{error} The hold keys do not work until that is fixed and Hyprland reloads its config again."
            ));
        }
    }

    fn is_live(&self) -> bool {
        *self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Removes the binds when dropped.
struct HoldBinds<R: CommandRunner> {
    shared: Arc<Shared<R>>,
    close_events: Option<Box<dyn FnOnce() + Send>>,
}

impl<R: CommandRunner> Drop for HoldBinds<R> {
    fn drop(&mut self) {
        {
            let mut live = self
                .shared
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *live = false;
            let free = self.shared.installed.give_up(&self.shared.binds);
            self.shared.unbind(&free);
        }
        if let Some(close) = self.close_events.take() {
            close();
        }
    }
}

/// Hyprland's event stream, one line per event.
pub(super) struct Events {
    reader: Box<dyn BufRead + Send>,
    /// Ends a read that is waiting for the next event.
    close: Box<dyn FnOnce() + Send>,
}

impl Events {
    pub(super) fn connect(socket: &Path) -> io::Result<Self> {
        Self::on(UnixStream::connect(socket)?)
    }

    fn on(stream: UnixStream) -> io::Result<Self> {
        let closer = stream.try_clone()?;
        Ok(Self {
            reader: Box::new(BufReader::new(stream)),
            close: Box::new(move || {
                let _ = closer.shutdown(Shutdown::Both);
            }),
        })
    }

    /// The event socket of the running Hyprland.
    pub(super) fn system() -> io::Result<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("{name} is not set"))
                })
        };
        Self::connect(
            &PathBuf::from(var("XDG_RUNTIME_DIR")?)
                .join("hypr")
                .join(var("HYPRLAND_INSTANCE_SIGNATURE")?)
                .join(".socket2.sock"),
        )
    }
}

/// Calls `on_reload` for each `configreloaded` event until the stream ends or fails.
fn follow(mut events: impl BufRead, mut on_reload: impl FnMut()) {
    // Event lines carry window titles, which need not be UTF-8.
    let mut line = Vec::new();
    loop {
        line.clear();
        match events.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if line.starts_with(b"configreloaded>>") {
                    on_reload();
                }
            }
        }
    }
}

/// Binds the hold and write keys in Hyprland to `exe`'s `hold-*` and `write-*` commands, and keeps
/// them bound through config reloads announced on `events`. Dropping the watch removes them.
///
/// Problems that come later, with no call to return them from, go to `tell`.
///
/// # Errors
/// Why the keys cannot be bound: a key Hyprland cannot take, a bind in the way, or a refusal.
pub(super) fn bind<R: CommandRunner + 'static>(
    run: Arc<R>,
    installed: Installed,
    exe: &Path,
    keys: &[(HoldKind, HoldKey)],
    events: io::Result<Events>,
    tell: impl Fn(String) + Send + Sync + 'static,
) -> Result<HoldWatch, HostError> {
    let shared = Arc::new(Shared {
        run,
        installed,
        binds: wanted(exe, keys)?,
        live: Mutex::new(true),
        tell: Box::new(tell),
    });
    shared.ensure()?;
    shared.installed.rely_on(&shared.binds);
    let close_events = if let Ok(Events { reader, close }) = events {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            follow(reader, || shared.rebind());
            if shared.is_live() {
                (shared.tell)(RELOADS_NOT_FOLLOWED.to_owned());
            }
        });
        Some(close)
    } else {
        (shared.tell)(RELOADS_NOT_FOLLOWED.to_owned());
        None
    };
    Ok(HoldWatch::new(HoldBinds {
        shared,
        close_events,
    }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::host::command::fakes::FakeRunner;

    /// How long a test waits for something that must happen.
    const WAIT: Duration = Duration::from_secs(30);

    /// The binds of a Hyprland that obeys `keyword bind`, `bindrti` and `unbind` through
    /// `hyprctl --batch` and lists them as `hyprctl -j binds` does.
    struct Compositor {
        binds: Mutex<Vec<serde_json::Value>>,
        /// The key whose binds Hyprland refuses.
        refuses: Mutex<Option<&'static str>>,
        runner: Arc<FakeRunner>,
    }

    impl Compositor {
        fn new(existing: Vec<serde_json::Value>) -> Arc<Self> {
            Arc::new_cyclic(|this: &std::sync::Weak<Self>| {
                let this = this.clone();
                Self {
                    binds: Mutex::new(existing),
                    refuses: Mutex::new(None),
                    runner: Arc::new(FakeRunner::new(move |argv, _| {
                        this.upgrade()
                            .map_or_else(|| Ok(String::new()), |hypr| Ok(hypr.answer(argv)))
                    })),
                }
            })
        }

        fn answer(&self, argv: &[String]) -> String {
            let mut binds = self.binds.lock().unwrap();
            match argv {
                [_, json, query] if json == "-j" && query == "binds" => {
                    serde_json::Value::Array(binds.clone()).to_string()
                }
                [_, batch, commands] if batch == "--batch" => {
                    let mut replies = String::new();
                    for command in commands.split("; ") {
                        let command = command.strip_prefix("keyword ").unwrap();
                        let (name, arguments) = command.split_once(' ').unwrap();
                        let mut fields = arguments.splitn(4, ',').map(str::trim);
                        let (_mods, key) = (fields.next().unwrap(), fields.next().unwrap());
                        let refused = *self.refuses.lock().unwrap() == Some(key);
                        match name {
                            "unbind" => binds.retain(|bind| bind["key"] != key),
                            _ if refused => {
                                replies.push_str("invalid dispatcher\n");
                                continue;
                            }
                            _ => {
                                let (dispatcher, arg) =
                                    (fields.next().unwrap(), fields.next().unwrap());
                                binds.push(row(key, name == "bindrti", dispatcher, arg));
                            }
                        }
                        replies.push_str("ok\n");
                    }
                    replies
                }
                _ => String::new(),
            }
        }

        /// What Hyprland binds, as (key, release, command).
        fn bound(&self) -> Vec<(String, bool, String)> {
            self.binds
                .lock()
                .unwrap()
                .iter()
                .map(|bind| {
                    (
                        bind["key"].as_str().unwrap().to_owned(),
                        bind["release"].as_bool().unwrap(),
                        bind["arg"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        }

        fn commands(&self) -> Vec<String> {
            self.runner.commands()
        }
    }

    fn row(key: &str, release: bool, dispatcher: &str, arg: &str) -> serde_json::Value {
        json!({"key": key, "release": release, "modmask": 0, "dispatcher": dispatcher, "arg": arg})
    }

    fn stranger(key: &str, modmask: u32, arg: &str) -> serde_json::Value {
        let mut bind = row(key, false, "exec", arg);
        bind["modmask"] = json!(modmask);
        bind
    }

    const EXE: &str = "/opt/Thalvi/thalvi-desktop";

    fn hold(name: &str) -> (HoldKind, HoldKey) {
        (HoldKind::Dictate, HoldKey::parse(name).unwrap())
    }

    fn write(name: &str) -> (HoldKind, HoldKey) {
        (HoldKind::Write, HoldKey::parse(name).unwrap())
    }

    /// Binds `keys` in `hypr` with no event stream, collecting what the user is told.
    fn bind_keys(
        hypr: &Compositor,
        installed: &Installed,
        exe: &str,
        keys: &[(HoldKind, HoldKey)],
    ) -> (Result<HoldWatch, HostError>, mpsc::Receiver<String>) {
        let (told, notices) = mpsc::channel();
        let watch = bind(
            Arc::clone(&hypr.runner),
            installed.clone(),
            Path::new(exe),
            keys,
            Err(io::ErrorKind::NotFound.into()),
            move |notice| {
                let _ = told.send(notice);
            },
        );
        (watch, notices)
    }

    fn defaults() -> [(HoldKind, HoldKey); 2] {
        [hold("F9"), write("F10")]
    }

    fn bound_by_thalvi(exe: &str, keys: &[(&str, &str)]) -> Vec<(String, bool, String)> {
        keys.iter()
            .flat_map(|(key, action)| {
                [
                    ((*key).to_owned(), false, format!("'{exe}' {action}-start")),
                    ((*key).to_owned(), true, format!("'{exe}' {action}-end")),
                ]
            })
            .collect()
    }

    #[test]
    fn the_keys_are_bound_in_one_batch_that_quotes_the_path_for_the_shell() {
        for (exe, quoted) in [
            (EXE, "'/opt/Thalvi/thalvi-desktop'"),
            (
                "/home/me/Apps/Thalvi 1.0.AppImage",
                "'/home/me/Apps/Thalvi 1.0.AppImage'",
            ),
            (
                "/home/o'brien/$HOME/thalvi",
                r"'/home/o'\''brien/$HOME/thalvi'",
            ),
        ] {
            let hypr = Compositor::new(Vec::new());
            let (watch, _) = bind_keys(&hypr, &Installed::default(), exe, &defaults());
            assert!(watch.is_ok(), "{exe}");
            assert_eq!(
                hypr.commands(),
                [
                    "hyprctl -j binds".to_owned(),
                    format!(
                        "hyprctl --batch \
                         keyword bind , F9, exec, {quoted} hold-start; \
                         keyword bindrti , F9, exec, {quoted} hold-end; \
                         keyword bind , F10, exec, {quoted} write-start; \
                         keyword bindrti , F10, exec, {quoted} write-end"
                    )
                ],
                "{exe}"
            );
            assert_eq!(
                hypr.bound(),
                bound_by_thalvi(exe, &[("F9", "hold"), ("F10", "write")])
                    .into_iter()
                    .map(|(key, release, arg)| (
                        key,
                        release,
                        arg.replace(&format!("'{exe}'"), quoted)
                    ))
                    .collect::<Vec<_>>(),
                "{exe}"
            );
        }
    }

    #[test]
    fn a_path_that_cannot_be_carried_in_a_bind_binds_nothing() {
        for exe in ["/opt/a;b/thalvi", "/opt/a\nb/thalvi"] {
            let hypr = Compositor::new(Vec::new());
            let (watch, _) = bind_keys(&hypr, &Installed::default(), exe, &defaults());
            let Err(HostError::Unsupported(reason)) = watch else {
                panic!("{exe:?} was bound");
            };
            assert!(
                reason.contains("cannot be used in a Hyprland bind"),
                "{reason}"
            );
            assert_eq!(hypr.commands(), Vec::<String>::new(), "{exe:?}");
        }
    }

    #[test]
    fn only_f1_to_f12_without_a_modifier_can_be_bound() {
        for (key, bindable) in [
            ("F1", true),
            ("F12", true),
            ("Alt+KeyZ", false),
            ("Ctrl+F9", false),
            ("Shift+F9", false),
            ("Win+F9", false),
            ("AltRight", false),
        ] {
            assert_eq!(check(&[hold(key)]).is_ok(), bindable, "{key}");
            let hypr = Compositor::new(Vec::new());
            let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &[hold(key)]);
            match watch {
                Ok(_) => assert!(bindable, "{key}"),
                Err(error) => {
                    assert!(!bindable, "{key}: {error}");
                    assert_eq!(
                        error,
                        HostError::Unsupported(format!(
                            "Hyprland can bind only F1 to F12 without a modifier for the hold keys, not {key}."
                        ))
                    );
                    assert_eq!(hypr.commands(), Vec::<String>::new(), "{key}");
                }
            }
        }
    }

    #[test]
    fn dropping_the_watch_removes_the_binds_it_added_and_no_others() {
        let hypr = Compositor::new(vec![stranger("Q", 64, "kitty")]);
        let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
        assert_eq!(hypr.bound().len(), 5);
        drop(watch);
        assert_eq!(hypr.bound(), [("Q".to_owned(), false, "kitty".to_owned())]);
        assert_eq!(
            hypr.commands().last().map(String::as_str),
            Some("hyprctl --batch keyword unbind , F9; keyword unbind , F10")
        );
    }

    #[test]
    fn a_key_another_bind_holds_is_refused_and_nothing_is_bound() {
        let taken = |key: &str| {
            format!("{key} is already bound in Hyprland. Pick another key or remove that bind.")
        };
        let thalvi = |keys: &str| {
            format!(
                "Hyprland already binds {keys} to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start and write-end lines from hyprland.conf. If it has none, a Thalvi that did not quit left them, and hyprctl reload clears them."
            )
        };
        let cases = [
            (
                "another program's bind on the dictate key",
                vec![row("F9", false, "exec", "obs --start")],
                taken("F9"),
            ),
            (
                "a bind of another dispatcher on the write key",
                vec![row("F10", false, "togglefloating", "")],
                taken("F10"),
            ),
            (
                "the key's name in lower case",
                vec![row("f9", false, "exec", "x")],
                taken("F9"),
            ),
            (
                "both keys",
                vec![
                    row("F9", false, "exec", "x"),
                    row("F10", false, "exec", "y"),
                ],
                "F9 and F10 are already bound in Hyprland. Pick other keys or remove those binds."
                    .to_owned(),
            ),
            (
                "hand-written Thalvi binds on both keys",
                vec![
                    row("F9", false, "exec", "thalvi-desktop hold-start"),
                    row("F10", false, "exec", "thalvi-desktop write-start"),
                ],
                thalvi("F9 and F10"),
            ),
            (
                "hand-written Thalvi binds",
                vec![
                    row("F9", false, "exec", "thalvi-desktop hold-start"),
                    row("F9", true, "exec", "thalvi-desktop hold-end"),
                ],
                thalvi("F9"),
            ),
            (
                "binds left by an instance that ran from another path",
                vec![row(
                    "F10",
                    false,
                    "exec",
                    "'/old/Thalvi.AppImage' write-start",
                )],
                thalvi("F10"),
            ),
            (
                "another program's bind that ends in a word like the hold commands",
                vec![row("F9", false, "exec", "obs hold-start")],
                taken("F9"),
            ),
            (
                "a hand-written Thalvi bind next to another program's on another key",
                vec![
                    row("F9", false, "exec", "thalvi-desktop hold-start"),
                    row("F10", false, "exec", "x"),
                ],
                format!("{} {}", thalvi("F9"), taken("F10")),
            ),
        ];
        for (name, existing, notice) in cases {
            let hypr = Compositor::new(existing.clone());
            let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
            assert_eq!(watch.err(), Some(HostError::Unsupported(notice)), "{name}");
            assert_eq!(hypr.commands(), ["hyprctl -j binds"], "{name}");
            assert_eq!(hypr.binds.lock().unwrap().len(), existing.len(), "{name}");
        }
    }

    #[test]
    fn binds_that_do_not_fire_on_the_bare_key_are_no_conflict() {
        let hypr = Compositor::new(vec![
            stranger("F9", 64, "kitty"),
            stranger("F5", 0, "kitty"),
            stranger("code:75", 0, "kitty"),
        ]);
        let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
        assert!(watch.is_ok());
        assert_eq!(hypr.bound().len(), 3 + 4);
    }

    #[test]
    fn an_identical_bind_is_adopted_not_added_again_and_goes_with_the_watch() {
        let present = |key: &str, release: bool, action: &str| {
            row(key, release, "exec", &format!("'{EXE}' {action}"))
        };
        let add = |key: &str, release: bool, action: &str| {
            let bind = if release { "bindrti" } else { "bind" };
            format!("keyword {bind} , {key}, exec, '{EXE}' {action}")
        };
        let cases = [
            (
                "every bind",
                vec![
                    present("F9", false, "hold-start"),
                    present("F9", true, "hold-end"),
                    present("F10", false, "write-start"),
                    present("F10", true, "write-end"),
                ],
                vec![],
            ),
            (
                "the dictate key's pair",
                vec![
                    present("F9", false, "hold-start"),
                    present("F9", true, "hold-end"),
                ],
                vec![
                    add("F10", false, "write-start"),
                    add("F10", true, "write-end"),
                ],
            ),
            (
                "one bind of a pair",
                vec![present("F9", false, "hold-start")],
                vec![
                    add("F9", true, "hold-end"),
                    add("F10", false, "write-start"),
                    add("F10", true, "write-end"),
                ],
            ),
        ];
        for (name, existing, added) in cases {
            let hypr = Compositor::new(existing);
            let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
            assert!(watch.is_ok(), "{name}");
            let mut expected = vec!["hyprctl -j binds".to_owned()];
            expected.extend(
                (!added.is_empty()).then(|| format!("hyprctl --batch {}", added.join("; "))),
            );
            assert_eq!(hypr.commands(), expected, "{name}");
            assert_eq!(hypr.bound().len(), 4, "{name}");
            drop(watch);
            assert_eq!(hypr.bound(), [], "{name}: adopted binds go with the watch");
        }
    }

    #[test]
    fn a_refused_bind_leaves_nothing_behind_and_says_what_hyprland_said() {
        let hypr = Compositor::new(Vec::new());
        *hypr.refuses.lock().unwrap() = Some("F10");
        let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
        assert_eq!(
            watch.err(),
            Some(HostError::NothingTyped(
                "Hyprland would not bind the hold keys: invalid dispatcher.".to_owned()
            ))
        );
        assert_eq!(hypr.bound(), []);
    }

    #[test]
    fn a_key_change_binds_the_new_keys_first_and_a_refused_change_keeps_the_old() {
        let installed = Installed::default();
        let hypr = Compositor::new(Vec::new());
        let (old, _) = bind_keys(&hypr, &installed, EXE, &defaults());
        let old = old.unwrap();

        // Dictation moves to F8; F10 stays the write key, so it stays bound when the old watch goes.
        let (new, _) = bind_keys(&hypr, &installed, EXE, &[hold("F8"), write("F10")]);
        let new = new.unwrap();
        assert_eq!(
            hypr.bound().len(),
            6,
            "the new keys are bound beside the old"
        );
        drop(old);
        assert_eq!(
            hypr.bound(),
            bound_by_thalvi(EXE, &[("F10", "write"), ("F8", "hold")])
        );

        // F7 is taken by another bind: the change is refused and the keys in use stay.
        hypr.binds
            .lock()
            .unwrap()
            .push(row("F7", false, "exec", "obs"));
        let (refused, _) = bind_keys(&hypr, &installed, EXE, &[hold("F7"), write("F10")]);
        assert!(refused.is_err());
        assert_eq!(
            hypr.bound().len(),
            5,
            "the keys in use and the other program's bind"
        );
        drop(new);
        assert_eq!(hypr.bound(), [("F7".to_owned(), false, "obs".to_owned())]);
    }

    #[test]
    fn a_refused_bind_keeps_the_keys_an_earlier_watch_relies_on() {
        let installed = Installed::default();
        let hypr = Compositor::new(Vec::new());
        let (_earlier, _) = bind_keys(&hypr, &installed, EXE, &[hold("F9")]);
        // A reload dropped its binds before the watch could put them back.
        hypr.binds.lock().unwrap().clear();
        *hypr.refuses.lock().unwrap() = Some("F10");

        let (later, _) = bind_keys(&hypr, &installed, EXE, &defaults());
        assert_eq!(
            later.err(),
            Some(HostError::NothingTyped(
                "Hyprland would not bind the hold keys: invalid dispatcher.".to_owned()
            ))
        );
        assert_eq!(hypr.bound(), bound_by_thalvi(EXE, &[("F9", "hold")]));
        assert_eq!(
            hypr.commands().last().map(String::as_str),
            Some("hyprctl --batch keyword unbind , F10")
        );
    }

    #[test]
    fn dropping_a_watch_unbinds_its_keys_whichever_watch_goes_first() {
        for (first, second) in [(0, 1), (1, 0)] {
            let installed = Installed::default();
            let hypr = Compositor::new(Vec::new());
            let (dictation, _) = bind_keys(&hypr, &installed, EXE, &[hold("F9")]);
            let (writing, _) = bind_keys(&hypr, &installed, EXE, &[write("F8")]);
            let mut watches = [dictation.ok(), writing.ok()];
            drop(watches[first].take());
            let left = [("F9", "hold"), ("F8", "write")][second];
            assert_eq!(
                hypr.bound(),
                bound_by_thalvi(EXE, &[left]),
                "{first} dropped first"
            );
            drop(watches[second].take());
            assert_eq!(hypr.bound(), [], "{second} dropped last");
        }
    }

    struct Following {
        watch: HoldWatch,
        /// Hyprland's end of the event socket.
        hyprland: UnixStream,
        notices: mpsc::Receiver<String>,
    }

    /// Binds `defaults()` with an event socket of its own, as Hyprland's is one.
    fn follow_events(hypr: &Compositor) -> Following {
        let (hyprland, ours) = UnixStream::pair().unwrap();
        let (told, notices) = mpsc::channel();
        let watch = bind(
            Arc::clone(&hypr.runner),
            Installed::default(),
            Path::new(EXE),
            &defaults(),
            Events::on(ours),
            move |notice| {
                let _ = told.send(notice);
            },
        )
        .unwrap();
        Following {
            watch,
            hyprland,
            notices,
        }
    }

    #[test]
    fn a_reload_puts_the_binds_back_and_other_events_change_nothing() {
        let hypr = Compositor::new(Vec::new());
        let mut following = follow_events(&hypr);
        let batches = || {
            hypr.commands()
                .iter()
                .filter(|command| command.starts_with("hyprctl --batch keyword bind "))
                .count()
        };
        assert_eq!(batches(), 1);

        // Window titles in events need not be UTF-8, and an event can arrive in pieces.
        following
            .hyprland
            .write_all(b"activewindow>>kitty,caf\xe9 \xff\nworkspace>>2\nconfigrel")
            .unwrap();
        // Hyprland drops the runtime binds when it reloads.
        hypr.binds.lock().unwrap().clear();
        following
            .hyprland
            .write_all(b"oaded>>\nfocusedmon>>DP-1,2\n")
            .unwrap();
        // The stream ends after the events: the notice that tells so comes once they were handled.
        drop(following.hyprland);
        assert_eq!(
            following.notices.recv_timeout(WAIT).unwrap(),
            RELOADS_NOT_FOLLOWED
        );
        assert_eq!(batches(), 2, "the reload added the binds again, once");
        assert_eq!(hypr.bound().len(), 4);
        drop(following.watch);
        assert_eq!(hypr.bound(), []);
    }

    #[test]
    fn a_reload_that_finds_a_key_taken_binds_nothing_and_says_so() {
        let hypr = Compositor::new(Vec::new());
        let mut following = follow_events(&hypr);
        *hypr.binds.lock().unwrap() = vec![row("F10", false, "exec", "obs")];
        following.hyprland.write_all(b"configreloaded>>\n").unwrap();
        drop(following.hyprland);
        assert_eq!(
            following.notices.recv_timeout(WAIT).unwrap(),
            "F10 is already bound in Hyprland. Pick another key or remove that bind. The hold keys do not work until that is fixed and Hyprland reloads its config again."
        );
        assert_eq!(hypr.bound().len(), 1, "only the other program's bind");
    }

    #[test]
    fn losing_the_event_socket_is_told_and_leaves_the_binds_working() {
        let hypr = Compositor::new(Vec::new());
        let (told, notices) = mpsc::channel();
        let watch = bind(
            Arc::clone(&hypr.runner),
            Installed::default(),
            Path::new(EXE),
            &defaults(),
            Err(io::ErrorKind::NotFound.into()),
            move |notice| {
                let _ = told.send(notice);
            },
        )
        .unwrap();
        assert_eq!(notices.try_recv().unwrap(), RELOADS_NOT_FOLLOWED);
        assert_eq!(hypr.bound().len(), 4);
        drop(watch);

        let following = follow_events(&hypr);
        drop(following.hyprland);
        assert_eq!(
            following.notices.recv_timeout(WAIT).unwrap(),
            RELOADS_NOT_FOLLOWED
        );
        assert_eq!(hypr.bound().len(), 4);
    }

    #[test]
    fn dropping_the_watch_ends_the_event_stream_without_telling_the_user() {
        let hypr = Compositor::new(Vec::new());
        let mut following = follow_events(&hypr);
        drop(following.watch);
        assert_eq!(hypr.bound(), []);
        // The socket was closed from our side, and the reading thread ended silently: every
        // sender of notices went with it.
        following.hyprland.set_read_timeout(Some(WAIT)).unwrap();
        assert_eq!(following.hyprland.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(
            following.notices.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn the_event_socket_is_read_in_whole_lines_and_closing_it_ends_the_wait() {
        let dir = std::env::temp_dir().join(format!("thalvi-hold-binds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(".socket2.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let events = Events::connect(&socket).unwrap();
        let (mut hyprland, _) = listener.accept().unwrap();
        hyprland.write_all(b"configrel").unwrap();
        hyprland
            .write_all(b"oaded>>\nworkspace>>1\nconfigreloaded>>\n")
            .unwrap();

        let (reloads, counted) = mpsc::channel();
        let Events { reader, close } = events;
        let (ended, ending) = mpsc::channel();
        std::thread::spawn(move || {
            follow(reader, || reloads.send(()).unwrap());
            ended.send(()).unwrap();
        });
        assert_eq!(counted.recv_timeout(WAIT), Ok(()));
        assert_eq!(counted.recv_timeout(WAIT), Ok(()));
        // Hyprland sends nothing more and keeps the socket open: only closing it ends the read.
        close();
        assert_eq!(ending.recv_timeout(WAIT), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
