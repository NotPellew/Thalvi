//! Binds the hold and write keys in Hyprland at runtime, and puts them back after a config reload.

use std::io::{self, BufRead, BufReader};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;

use crate::hold::{KeyBind, KeyMode};
use crate::host::command::{CommandRunner, tool};
use crate::host::{HoldReport, HoldSource, HoldWatch, HostError};

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

    /// Whether the bind reports one of Thalvi's key rows, whoever wrote it.
    fn is_thalvi_hold(&self) -> bool {
        let words: Vec<&str> = self.arg.split_whitespace().collect();
        let legacy = words.last().is_some_and(|word| {
            ["hold-start", "hold-end", "write-start", "write-end"].contains(word)
        });
        let row = words.len() >= 3
            && words[words.len() - 3] == "key"
            && matches!(words[words.len() - 1], "down" | "up");
        self.dispatcher == "exec"
            && self.arg.to_ascii_lowercase().contains("thalvi")
            && (legacy || row)
    }

    /// Whether this is a Thalvi hold bind whose program, the first shell word, is an absolute path that no longer exists.
    fn is_leftover(&self) -> bool {
        if !self.is_thalvi_hold() {
            return false;
        }
        let mut word = String::new();
        let mut quoted = false;
        let mut chars = self.arg.trim_start().chars();
        while let Some(c) = chars.next() {
            match c {
                '\'' => quoted = !quoted,
                '\\' if !quoted => match chars.next() {
                    Some(c) => word.push(c),
                    None => return false,
                },
                c if c.is_whitespace() && !quoted => break,
                c => word.push(c),
            }
        }
        !quoted && Path::new(&word).is_absolute() && !Path::new(&word).exists()
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
pub(super) fn check(keys: &[KeyBind]) -> Result<(), HostError> {
    match keys
        .iter()
        .find(|bind| !bind.key.modifiers().is_empty() || bind.key.is_altgr())
    {
        Some(bind) => Err(HostError::Unsupported(format!(
            "Hyprland can bind only F1 to F12 without a modifier for the keys, not {}.",
            bind.key
        ))),
        None => Ok(()),
    }
}

/// The binds each row needs: a Hold key fires on its press and its release, and Toggle, Start and
/// Stop rows only on the press, since Hyprland reports no release for them.
fn wanted(exe: &Path, keys: &[KeyBind]) -> Result<Vec<Bind>, HostError> {
    check(keys)?;
    let exe = quoted(exe)?;
    let mut binds = Vec::new();
    for (row, bind) in keys.iter().enumerate() {
        let mut adds = vec![(format!("key {row} down"), false)];
        if bind.mode == KeyMode::Hold {
            adds.push((format!("key {row} up"), true));
        }
        for (command, release) in adds {
            binds.push(Bind {
                key: bind.key.to_string(),
                release,
                command: format!("{exe} {command}"),
            });
        }
    }
    Ok(binds)
}

/// What Hyprland needs to hold every wanted bind.
struct Changes<'a> {
    /// Keys whose previous binds are obsolete or left over from a missing executable.
    replace: Vec<&'a str>,
    add: Vec<&'a Bind>,
}

/// The binds of `wanted` that Hyprland lacks, or the notice why they cannot be added: another bind
/// holds a key, or a hold bind that Thalvi did not add does. A bind identical to a wanted one is
/// adopted, so it is neither added again nor in the way, and a leftover is replaced.
fn missing<'a>(wanted: &[&'a Bind], rows: &[Row], owned: &[&Bind]) -> Result<Changes<'a>, String> {
    let mut keys: Vec<&str> = wanted.iter().map(|bind| bind.key.as_str()).collect();
    keys.dedup();
    let (mut doubled, mut taken, mut replace) = (Vec::new(), Vec::new(), Vec::new());
    for key in keys {
        let (leftovers, others): (Vec<_>, Vec<_>) =
            rows.iter().filter(|row| row.on(key)).partition(|row| {
                row.is_leftover()
                    || (owned.iter().any(|bind| bind.is(row))
                        && !wanted.iter().any(|bind| bind.is(row)))
            });
        let conflicts: Vec<_> = others
            .into_iter()
            .filter(|row| !wanted.iter().any(|bind| bind.is(row)))
            .collect();
        if !leftovers.is_empty() {
            replace.push(key);
        }
        if conflicts.iter().any(|row| row.is_thalvi_hold()) {
            doubled.push(key);
        } else if !conflicts.is_empty() {
            taken.push(key);
        }
    }
    let mut notices = Vec::new();
    if !doubled.is_empty() {
        notices.push(format!(
            "Hyprland already binds {} to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start, write-end and key lines from hyprland.conf, or quit the other Thalvi that holds them.",
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
    Ok(Changes {
        add: wanted
            .iter()
            .copied()
            .filter(|bind| {
                replace.contains(&bind.key.as_str()) || !rows.iter().any(|row| bind.is(row))
            })
            .collect(),
        replace,
    })
}

/// Successful watches, in installation order. The newest watch for a key owns that key while
/// the settings save is in flight. If the save fails its drop restores the older watch.
#[derive(Clone, Default)]
pub(super) struct Installed(Arc<Mutex<Vec<Arc<[Bind]>>>>);

impl Installed {
    fn watches(&self) -> MutexGuard<'_, Vec<Arc<[Bind]>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn latest<'a>(watches: &'a [Arc<[Bind]>], key: &str) -> impl Iterator<Item = &'a Bind> {
    watches
        .iter()
        .rev()
        .find(|binds| binds.iter().any(|bind| bind.key == key))
        .into_iter()
        .flat_map(move |binds| binds.iter().filter(move |bind| bind.key == key))
}

fn owned(watches: &[Arc<[Bind]>]) -> Vec<&Bind> {
    watches.iter().flat_map(|binds| binds.iter()).collect()
}

/// Whether a watch relies on its binds in [`Installed`]: only a bound watch gives them up when dropped.
enum State {
    /// The first bind was refused; a reload may still bind them.
    Waiting,
    Bound,
    Gone,
}

struct Shared<R> {
    run: Arc<R>,
    installed: Installed,
    binds: Arc<[Bind]>,
    state: Mutex<State>,
    tell: Box<dyn Fn(HoldReport) + Send + Sync>,
}

impl<R: CommandRunner> Shared<R> {
    /// Adds the binds Hyprland lacks, after checking that nothing else holds their keys.
    fn ensure(&self, wanted: &[&Bind], owned: &[&Bind]) -> Result<(), HostError> {
        let list = self
            .run
            .run("hyprctl", &["-j", "binds"], None)
            .map_err(|error| tool(&error))?;
        let rows: Vec<Row> = serde_json::from_str(&list).map_err(|_| {
            HostError::NothingTyped("The desktop returned an invalid bind list.".to_owned())
        })?;
        let Changes {
            replace,
            add: missing,
        } = missing(wanted, &rows, owned).map_err(HostError::Unsupported)?;
        if missing.is_empty() {
            return Ok(());
        }
        let mut commands = Vec::new();
        for bind in &missing {
            if replace.contains(&bind.key.as_str())
                && !commands.contains(&format!("keyword unbind , {}", bind.key))
            {
                commands.push(format!("keyword unbind , {}", bind.key));
            }
            commands.push(bind.keyword());
        }
        let batch = commands.join("; ");
        let refusal = match self.run.run("hyprctl", &["--batch", &batch], None) {
            Ok(reply) => {
                let replies: Vec<&str> = reply
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                if replies.len() == commands.len() && replies.iter().all(|reply| *reply == "ok") {
                    return Ok(());
                }
                replies
                    .iter()
                    .find(|reply| **reply != "ok")
                    .map_or_else(|| "no answer".to_owned(), |reply| (*reply).to_owned())
            }
            Err(error) => error.to_string(),
        };
        // Some commands may have succeeded. Put back the owned bindings the batch replaced.
        let mut changed: Vec<String> = replace.iter().map(ToString::to_string).collect();
        for bind in &missing {
            if !changed.contains(&bind.key) && !owned.iter().any(|old| old.key == bind.key) {
                changed.push(bind.key.clone());
            }
        }
        self.unbind(&changed);
        let mut restore = Vec::new();
        for bind in owned {
            if replace.contains(&bind.key.as_str()) && rows.iter().any(|row| bind.is(row)) {
                let command = bind.keyword();
                if !restore.contains(&command) {
                    restore.push(command);
                }
            }
        }
        if !restore.is_empty()
            && let Err(error) = self
                .run
                .run("hyprctl", &["--batch", &restore.join("; ")], None)
        {
            (self.tell)(HoldReport::Problem(format!(
                "Thalvi could not restore its previous keys: {error}."
            )));
        }
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
            (self.tell)(HoldReport::Problem(format!(
                "Thalvi could not remove its hold key binds from Hyprland: {error} They stay until Hyprland reloads its config."
            )));
        }
    }

    fn install(&self) -> Result<(), HostError> {
        let mut watches = self.installed.watches();
        self.ensure(&self.binds.iter().collect::<Vec<_>>(), &owned(&watches))?;
        watches.push(Arc::clone(&self.binds));
        Ok(())
    }

    fn uninstall(&self) {
        let mut watches = self.installed.watches();
        watches.retain(|binds| !Arc::ptr_eq(binds, &self.binds));
        let mut keys: Vec<_> = self.binds.iter().map(|bind| bind.key.as_str()).collect();
        keys.dedup();
        let free: Vec<_> = keys
            .iter()
            .filter(|key| latest(&watches, key).next().is_none())
            .map(ToString::to_string)
            .collect();
        let restore: Vec<_> = keys.iter().flat_map(|key| latest(&watches, key)).collect();
        let mut owned = owned(&watches);
        owned.extend(self.binds.iter());
        if !restore.is_empty()
            && let Err(error) = self.ensure(&restore, &owned)
        {
            (self.tell)(HoldReport::Problem(format!(
                "Thalvi could not restore its previous keys: {error}."
            )));
        }
        self.unbind(&free);
    }

    /// Puts the binds back after Hyprland dropped them, unless something else took a key meanwhile.
    fn rebind(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, State::Gone) {
            return;
        }
        let result = if matches!(*state, State::Waiting) {
            self.install()
        } else {
            let watches = self.installed.watches();
            let mut keys: Vec<_> = self.binds.iter().map(|bind| bind.key.as_str()).collect();
            keys.dedup();
            let wanted: Vec<_> = keys.iter().flat_map(|key| latest(&watches, key)).collect();
            self.ensure(&wanted, &owned(&watches))
        };
        match result {
            Ok(()) => {
                *state = State::Bound;
                (self.tell)(HoldReport::Working);
            }
            Err(error) => (self.tell)(HoldReport::Problem(format!(
                "{error} The hold keys do not work until that is fixed and Hyprland reloads its config again."
            ))),
        }
    }

    fn is_live(&self) -> bool {
        !matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Gone
        )
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
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let bound = matches!(*state, State::Bound);
            *state = State::Gone;
            if bound {
                self.shared.uninstall();
            }
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
/// A key Hyprland cannot take, or an unusable program path. A bind in the way returns a waiting watch.
pub(super) fn bind<R: CommandRunner + 'static>(
    run: Arc<R>,
    installed: Installed,
    exe: &Path,
    keys: &[KeyBind],
    events: io::Result<Events>,
    tell: impl Fn(HoldReport) + Send + Sync + 'static,
) -> Result<HoldSource, HostError> {
    let shared = Arc::new(Shared {
        run,
        installed,
        binds: wanted(exe, keys)?.into(),
        state: Mutex::new(State::Waiting),
        tell: Box::new(tell),
    });
    let refusal = match shared.install() {
        Ok(()) => {
            *shared.state.lock().unwrap_or_else(PoisonError::into_inner) = State::Bound;
            None
        }
        Err(error) => Some(error),
    };
    let close_events = if let Ok(Events { reader, close }) = events {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            follow(reader, || shared.rebind());
            if shared.is_live() {
                (shared.tell)(HoldReport::Problem(RELOADS_NOT_FOLLOWED.to_owned()));
            }
        });
        Some(close)
    } else {
        (shared.tell)(HoldReport::Problem(RELOADS_NOT_FOLLOWED.to_owned()));
        None
    };
    let watch = HoldWatch::new(HoldBinds {
        shared,
        close_events,
    });
    Ok(match refusal {
        Some(reason) => HoldSource::Refused { reason, watch },
        None => HoldSource::Watching(watch),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::hold::{Action, KeyMode, KeyName};
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

    const EXE: &str = "/proc/self/exe";

    fn hold(name: &str) -> KeyBind {
        KeyBind::new(
            Action::Dictate,
            KeyMode::Hold,
            KeyName::parse(name).unwrap(),
        )
    }

    fn write(name: &str) -> KeyBind {
        KeyBind::new(Action::Write, KeyMode::Hold, KeyName::parse(name).unwrap())
    }

    /// Binds `keys` in `hypr` with no event stream, collecting what the user is told.
    fn bind_keys(
        hypr: &Compositor,
        installed: &Installed,
        exe: &str,
        keys: &[KeyBind],
    ) -> (Result<HoldWatch, HostError>, mpsc::Receiver<String>) {
        let (told, notices) = mpsc::channel();
        let watch = bind(
            Arc::clone(&hypr.runner),
            installed.clone(),
            Path::new(exe),
            keys,
            Err(io::ErrorKind::NotFound.into()),
            move |notice| {
                if let HoldReport::Problem(notice) = notice {
                    let _ = told.send(notice);
                }
            },
        )
        .and_then(watching);
        (watch, notices)
    }

    fn watching(source: HoldSource) -> Result<HoldWatch, HostError> {
        match source {
            HoldSource::Watching(watch) => Ok(watch),
            HoldSource::Refused { reason, .. } => Err(reason),
            HoldSource::Unavailable(reason) => panic!("Hyprland has hold keys: {reason}"),
        }
    }

    fn defaults() -> [KeyBind; 2] {
        [hold("F9"), write("F10")]
    }

    fn bound_by_thalvi(exe: &str, keys: &[KeyBind]) -> Vec<(String, bool, String)> {
        keys.iter()
            .enumerate()
            .flat_map(|(row, bind)| {
                let key = bind.key.to_string();
                let mut binds = vec![(key.clone(), false, format!("'{exe}' key {row} down"))];
                if bind.mode == KeyMode::Hold {
                    binds.push((key, true, format!("'{exe}' key {row} up")));
                }
                binds
            })
            .collect()
    }

    #[test]
    fn the_keys_are_bound_in_one_batch_that_quotes_the_path_for_the_shell() {
        for (exe, quoted) in [
            ("/opt/Thalvi/thalvi-desktop", "'/opt/Thalvi/thalvi-desktop'"),
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
                         keyword bind , F9, exec, {quoted} key 0 down; \
                         keyword bindrti , F9, exec, {quoted} key 0 up; \
                         keyword bind , F10, exec, {quoted} key 1 down; \
                         keyword bindrti , F10, exec, {quoted} key 1 up"
                    )
                ],
                "{exe}"
            );
            assert_eq!(
                hypr.bound(),
                bound_by_thalvi(exe, &defaults())
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
    fn a_hold_row_binds_its_release_and_every_other_mode_only_its_press() {
        for (mode, release) in [
            (KeyMode::Hold, true),
            (KeyMode::Toggle, false),
            (KeyMode::Start, false),
            (KeyMode::Stop, false),
        ] {
            let row = KeyBind::new(Action::Dictate, mode, KeyName::f9());
            let hypr = Compositor::new(Vec::new());
            let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &[row]);
            assert!(watch.is_ok(), "{mode:?}");
            assert_eq!(
                hypr.commands(),
                [
                    "hyprctl -j binds".to_owned(),
                    if release {
                        format!(
                            "hyprctl --batch keyword bind , F9, exec, '{EXE}' key 0 down; keyword bindrti , F9, exec, '{EXE}' key 0 up"
                        )
                    } else {
                        format!("hyprctl --batch keyword bind , F9, exec, '{EXE}' key 0 down")
                    },
                ],
                "{mode:?}"
            );
            assert_eq!(
                hypr.bound(),
                if release {
                    vec![
                        ("F9".to_owned(), false, format!("'{EXE}' key 0 down")),
                        ("F9".to_owned(), true, format!("'{EXE}' key 0 up")),
                    ]
                } else {
                    vec![("F9".to_owned(), false, format!("'{EXE}' key 0 down"))]
                },
                "{mode:?}"
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
                            "Hyprland can bind only F1 to F12 without a modifier for the keys, not {key}."
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
    fn missing_programs_are_replaced_in_one_batch_including_adopted_pairs() {
        let gone = "/tmp/.mount_ThalviGone/usr/bin/thalvi-desktop";
        assert!(!Path::new(gone).exists());
        for (name, program, adopted) in [
            ("quoted", format!("'{gone}'"), false),
            ("bare", gone.to_owned(), false),
            (
                "escaped quote",
                "'/tmp/Thalvi gone'\\''s/thalvi-desktop'".to_owned(),
                false,
            ),
            ("adopted pair", format!("'{gone}'"), true),
        ] {
            let mut existing = vec![
                row("F9", false, "exec", &format!("{program} key 0 down")),
                row("F9", true, "exec", &format!("{program} key 0 up")),
                row("F10", false, "exec", &format!("{program} key 1 down")),
                row("F10", true, "exec", &format!("{program} key 1 up")),
            ];
            if adopted {
                existing.push(row("F9", false, "exec", &format!("'{EXE}' key 0 down")));
            }
            let hypr = Compositor::new(existing);
            let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
            assert!(watch.is_ok(), "{name}: dead binds must be replaced");
            assert_eq!(
                hypr.commands(),
                [
                    "hyprctl -j binds".to_owned(),
                    format!(
                        "hyprctl --batch keyword unbind , F9; keyword bind , F9, exec, '{EXE}' key 0 down; keyword bindrti , F9, exec, '{EXE}' key 0 up; keyword unbind , F10; keyword bind , F10, exec, '{EXE}' key 1 down; keyword bindrti , F10, exec, '{EXE}' key 1 up"
                    ),
                ],
                "{name}"
            );
            assert_eq!(hypr.bound(), bound_by_thalvi(EXE, &defaults()), "{name}");
            drop(watch);
            assert_eq!(hypr.bound(), [], "{name}: the watch owns the replacements");
        }
    }

    #[test]
    fn an_existing_program_is_refused_without_promising_reload_removes_it() {
        // Quoted, a space and a backslash are part of the program's path.
        let path =
            std::env::temp_dir().join(format!("thalvi live\\program-{}", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let command = format!("{} key 0 down", quoted(&path).unwrap());
        let hypr = Compositor::new(vec![row("F9", false, "exec", &command)]);
        let (watch, _) = bind_keys(&hypr, &Installed::default(), EXE, &defaults());
        std::fs::remove_file(path).unwrap();
        assert_eq!(watch.err(), Some(HostError::Unsupported(
            "Hyprland already binds F9 to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start, write-end and key lines from hyprland.conf, or quit the other Thalvi that holds them.".to_owned()
        )));
        assert_eq!(hypr.commands(), ["hyprctl -j binds"]);
        assert_eq!(hypr.bound().len(), 1);
    }

    #[test]
    fn a_key_another_bind_holds_is_refused_and_nothing_is_bound() {
        let taken = |key: &str| {
            format!("{key} is already bound in Hyprland. Pick another key or remove that bind.")
        };
        let thalvi = |keys: &str| {
            format!(
                "Hyprland already binds {keys} to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start, write-end and key lines from hyprland.conf, or quit the other Thalvi that holds them."
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
                    row("F9", false, "exec", "thalvi-desktop key 0 down"),
                    row("F10", false, "exec", "thalvi-desktop key 1 down"),
                ],
                thalvi("F9 and F10"),
            ),
            (
                "hand-written Thalvi binds",
                vec![
                    row("F9", false, "exec", "thalvi-desktop key 0 down"),
                    row("F9", true, "exec", "thalvi-desktop key 0 up"),
                ],
                thalvi("F9"),
            ),
            (
                "another program's bind that ends in a word like the hold commands",
                vec![row("F9", false, "exec", "obs key 0 down")],
                taken("F9"),
            ),
            (
                "a hand-written Thalvi bind next to another program's on another key",
                vec![
                    row("F9", false, "exec", "thalvi-desktop key 0 down"),
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
                    present("F9", false, "key 0 down"),
                    present("F9", true, "key 0 up"),
                    present("F10", false, "key 1 down"),
                    present("F10", true, "key 1 up"),
                ],
                vec![],
            ),
            (
                "the dictate key's pair",
                vec![
                    present("F9", false, "key 0 down"),
                    present("F9", true, "key 0 up"),
                ],
                vec![
                    add("F10", false, "key 1 down"),
                    add("F10", true, "key 1 up"),
                ],
            ),
            (
                "one bind of a pair",
                vec![present("F9", false, "key 0 down")],
                vec![
                    add("F9", true, "key 0 up"),
                    add("F10", false, "key 1 down"),
                    add("F10", true, "key 1 up"),
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
            [
                ("F10".to_owned(), false, format!("'{EXE}' key 1 down")),
                ("F10".to_owned(), true, format!("'{EXE}' key 1 up")),
                ("F8".to_owned(), false, format!("'{EXE}' key 0 down")),
                ("F8".to_owned(), true, format!("'{EXE}' key 0 up")),
            ],
            "each key reports the row it is in the new list"
        );

        let toggle = KeyBind::new(
            Action::Dictate,
            KeyMode::Toggle,
            KeyName::parse("F8").unwrap(),
        );
        let moved = [write("F10"), toggle];
        let (changed, _) = bind_keys(&hypr, &installed, EXE, &moved);
        let changed = changed.unwrap();
        assert_eq!(hypr.bound(), bound_by_thalvi(EXE, &moved));
        // A failed save drops the proposed watcher and restores the previous mode and row order.
        drop(changed);
        let mut restored = hypr.bound();
        restored.sort();
        let mut expected = bound_by_thalvi(EXE, &[hold("F8"), write("F10")]);
        expected.sort();
        assert_eq!(restored, expected);

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
        assert_eq!(hypr.bound(), bound_by_thalvi(EXE, &[hold("F9")]));
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
            let left = if second == 0 { hold("F9") } else { write("F8") };
            assert_eq!(
                hypr.bound(),
                bound_by_thalvi(EXE, &[left]),
                "{first} dropped first"
            );
            drop(watches[second].take());
            assert_eq!(hypr.bound(), [], "{second} dropped last");
        }
    }

    fn follow_start(
        hypr: &Compositor,
        installed: &Installed,
        keys: &[KeyBind],
    ) -> (
        Result<HoldSource, HostError>,
        UnixStream,
        mpsc::Receiver<HoldReport>,
    ) {
        let (hyprland, ours) = UnixStream::pair().unwrap();
        let (told, reports) = mpsc::channel();
        let source = bind(
            Arc::clone(&hypr.runner),
            installed.clone(),
            Path::new(EXE),
            keys,
            Events::on(ours),
            move |report| {
                let _ = told.send(report);
            },
        );
        (source, hyprland, reports)
    }

    #[test]
    fn a_refused_start_recovers_on_reload_and_reports_later_conflicts() {
        let conflict = row("F9", false, "exec", "thalvi-desktop key 0 down");
        let hypr = Compositor::new(vec![conflict.clone()]);
        let (source, mut events, reports) = follow_start(&hypr, &Installed::default(), &defaults());
        let Ok(HoldSource::Refused { reason, watch }) = source else {
            panic!("a refused start must keep following reloads");
        };
        assert!(
            reason
                .to_string()
                .contains("one press would start two holds")
        );
        assert_eq!(hypr.commands(), ["hyprctl -j binds"]);
        hypr.binds.lock().unwrap().clear();
        events.write_all(b"configreloaded>>\n").unwrap();
        assert_eq!(reports.recv_timeout(WAIT).unwrap(), HoldReport::Working);
        assert_eq!(hypr.bound(), bound_by_thalvi(EXE, &defaults()));
        // An already-bound watch reports success even if the compositor kept its binds.
        events.write_all(b"configreloaded>>\n").unwrap();
        assert_eq!(reports.recv_timeout(WAIT).unwrap(), HoldReport::Working);
        let before = hypr.commands().len();
        *hypr.binds.lock().unwrap() = vec![conflict];
        events.write_all(b"configreloaded>>\n").unwrap();
        assert_eq!(reports.recv_timeout(WAIT).unwrap(), HoldReport::Problem(
            "Hyprland already binds F9 to Thalvi with binds this Thalvi did not add, so one press would start two holds. Remove the hold-start, hold-end, write-start, write-end and key lines from hyprland.conf, or quit the other Thalvi that holds them. The hold keys do not work until that is fixed and Hyprland reloads its config again.".to_owned()
        ));
        assert_eq!(&hypr.commands()[before..], ["hyprctl -j binds"]);
        assert_eq!(hypr.bound().len(), 1);
        drop(watch);
        events.set_read_timeout(Some(WAIT)).unwrap();
        assert_eq!(events.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn dropping_a_never_bound_watch_leaves_the_earlier_watch_owning_its_keys() {
        let installed = Installed::default();
        let hypr = Compositor::new(vec![row("F7", false, "exec", "obs")]);
        let (earlier, _) = bind_keys(&hypr, &installed, EXE, &[hold("F9")]);
        let earlier = earlier.unwrap();
        let (source, mut events, reports) =
            follow_start(&hypr, &installed, &[hold("F9"), write("F7")]);
        let Ok(HoldSource::Refused { watch, .. }) = source else {
            panic!("a refused start must return its waiting watch");
        };
        let commands = hypr.commands();
        drop(watch);
        events.set_read_timeout(Some(WAIT)).unwrap();
        assert_eq!(events.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(
            reports.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(
            hypr.commands(),
            commands,
            "a never-bound watch unbinds nothing"
        );
        assert_eq!(hypr.bound().len(), 3, "the earlier watch still owns F9");
        drop(earlier);
        assert_eq!(hypr.bound(), [("F7".to_owned(), false, "obs".to_owned())]);
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
                if let HoldReport::Problem(notice) = notice {
                    let _ = told.send(notice);
                }
            },
        )
        .and_then(watching)
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
                if let HoldReport::Problem(notice) = notice {
                    let _ = told.send(notice);
                }
            },
        )
        .and_then(watching)
        .unwrap();
        assert_eq!(notices.try_recv().unwrap(), RELOADS_NOT_FOLLOWED);
        assert_eq!(hypr.bound().len(), 4);
        drop(watch);
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
