//! Owns the writing model on this computer: its pinned files, which llama.cpp build runs it, and the `llama-server` child process.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::models::{Asset, ModelError, ModelKind, ModelStore, SpeechProgress};
use crate::writing::{Endpoint, WritingError};

#[cfg(test)]
pub mod fakes;

/// How often a starting server is asked whether it is ready.
const POLL: Duration = Duration::from_millis(100);

/// How long after a broken connection the server is given to show that it exited.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// The model that writes on this computer.
#[derive(Debug, Clone, Copy)]
pub struct Model {
    pub title: &'static str,
    pub license: &'static str,
    pub languages: &'static [&'static str],
    pub asset: Asset<'static>,
}

/// The one GGUF that "On this computer" downloads.
pub const MODEL: Model = Model {
    title: "Qwen3 4B · Q4_K_M",
    license: "Apache-2.0",
    languages: &["English", "German", "Spanish"],
    asset: Asset {
        kind: ModelKind::ChatModel,
        file_name: "Qwen3-4B-Q4_K_M.gguf",
        url: "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/bc640142c66e1fdd12af0bd68f40445458f3869b/Qwen3-4B-Q4_K_M.gguf",
        size: 2_497_280_256,
        sha256: "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5",
    },
};

/// Which prebuilt llama.cpp runs the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Build {
    Vulkan,
    Cpu,
}

impl Build {
    /// Uses the Vulkan runtime's own device check, falling back to CPU when it cannot use a GPU.
    #[must_use]
    pub fn installed(store: &ModelStore, requested: Option<Self>) -> Self {
        if requested == Some(Self::Cpu) {
            return Self::Cpu;
        }
        let Some(server) = Self::Vulkan.files().server_path(store) else {
            return Self::Cpu;
        };
        let mut command = Command::new(server);
        command
            .arg("--list-devices")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut command);
        if command.output().is_ok_and(|out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|line| line.trim_start().starts_with("Vulkan"))
        }) {
            Self::Vulkan
        } else {
            Self::Cpu
        }
    }

    /// The name used in metrics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }

    /// The name shown to the user.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::Cpu => "CPU",
        }
    }

    /// The files this build needs.
    #[must_use]
    pub fn files(self) -> Files<'static> {
        match self {
            Self::Vulkan => Files {
                runtime: VULKAN_ARCHIVE,
                folder: "runtimes/b12255-vulkan",
                model: MODEL.asset,
            },
            Self::Cpu => Files {
                runtime: CPU_ARCHIVE,
                folder: "runtimes/b12255-cpu",
                model: MODEL.asset,
            },
        }
    }
}

/// A llama.cpp release archive pinned by size and checksum, stored under `runtimes/`.
macro_rules! archive {
    ($file:literal, $size:expr, $sha256:literal) => {
        Asset {
            kind: ModelKind::ChatRuntime,
            file_name: concat!("runtimes/", $file),
            url: concat!(
                "https://github.com/ggml-org/llama.cpp/releases/download/b12255/",
                $file
            ),
            size: $size,
            sha256: $sha256,
        }
    };
}

/// The files of one build: the archive of the runtime, the folder under the store it is unpacked
/// into, and the model.
#[derive(Debug, Clone, Copy)]
pub struct Files<'a> {
    pub runtime: Asset<'a>,
    pub folder: &'a str,
    pub model: Asset<'a>,
}

impl Files<'_> {
    /// The bytes of both downloads together.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.runtime.size + self.model.size
    }

    /// Where the model is or would be stored.
    #[must_use]
    pub fn model_path(&self, store: &ModelStore) -> PathBuf {
        store.path(&self.model)
    }

    /// The unpacked `llama-server`, when the runtime is installed.
    #[must_use]
    pub fn server_path(&self, store: &ModelStore) -> Option<PathBuf> {
        find_server(&store.dir().join(self.folder), 3)
    }

    fn model_cached(&self, store: &ModelStore) -> bool {
        std::fs::metadata(self.model_path(store)).is_ok_and(|meta| meta.len() == self.model.size)
    }
}

/// What the settings window shows of the model on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Info {
    pub title: &'static str,
    /// Bytes to download in all: the runtime and the model.
    pub size: u64,
    pub license: &'static str,
    pub languages: &'static [&'static str],
    pub build: Build,
    /// The build as shown to the user.
    pub build_label: &'static str,
    /// The model is on disk at its pinned size.
    pub model_installed: bool,
    pub runtime_installed: bool,
}

impl Info {
    #[must_use]
    pub fn of(store: &ModelStore, build: Build) -> Self {
        let files = build.files();
        Self {
            title: MODEL.title,
            size: files.size(),
            license: MODEL.license,
            languages: MODEL.languages,
            build,
            build_label: build.label(),
            model_installed: files.model_cached(store),
            runtime_installed: files.server_path(store).is_some(),
        }
    }
}

const SERVER_FILES: [&str; 2] = ["llama-server", "llama-server.exe"];

fn find_server(dir: &Path, depth: u8) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_dir && SERVER_FILES.iter().any(|name| entry.file_name() == *name) {
            return Some(path);
        }
        if is_dir
            && depth > 0
            && let Some(found) = find_server(&path, depth - 1)
        {
            return Some(found);
        }
    }
    None
}

/// Downloads and unpacks the runtime and downloads the model, whichever of them is not already
/// installed. `progress` is told how far the bytes of both have come.
///
/// # Errors
/// The first failure; what was installed before it stays.
pub async fn install(
    store: &ModelStore,
    files: &Files<'_>,
    mut progress: impl FnMut(SpeechProgress),
) -> Result<(), ModelError> {
    progress(SpeechProgress::Verifying);
    let needs_runtime = files.server_path(store).is_none();
    let total = files.model.size + if needs_runtime { files.runtime.size } else { 0 };
    let mut shown = None;
    let mut report = |bytes: u64| {
        let percent =
            u8::try_from((bytes * 100).checked_div(total).unwrap_or(100).min(100)).unwrap_or(100);
        if shown != Some(percent) {
            shown = Some(percent);
            progress(SpeechProgress::Downloading { percent });
        }
    };
    let mut done = 0;
    if needs_runtime {
        let archive = store.fetch_reporting(&files.runtime, &mut report).await?;
        done = files.runtime.size;
        report(done);
        let file = files.runtime.file_name.to_owned();
        let folder = store.dir().join(files.folder);
        let unpacked = {
            let archive = archive.clone();
            tokio::task::spawn_blocking(move || unpack(&archive, &folder))
                .await
                .unwrap_or_else(|_| Err("the unpacking was interrupted".to_owned()))
        };
        let _ = tokio::fs::remove_file(&archive).await;
        unpacked.map_err(|reason| ModelError::Unpack { file, reason })?;
    }
    store
        .fetch_reporting(&files.model, |bytes| report(done + bytes))
        .await?;
    report(total);
    progress(SpeechProgress::Done);
    Ok(())
}

/// Deletes the model's file; the runtime stays.
///
/// # Errors
/// Why the file could not be deleted. A file that is not there is deleted.
pub async fn remove_model(store: &ModelStore, files: &Files<'_>) -> io::Result<()> {
    match tokio::fs::remove_file(files.model_path(store)).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Unpacks `archive` into `folder` with the system `tar`, which also reads zip on Windows. The
/// folder appears whole or not at all.
fn unpack(archive: &Path, folder: &Path) -> Result<(), String> {
    let listing = tar(&[OsString::from("-tf"), archive.into()])?;
    let members: Vec<&str> = std::str::from_utf8(&listing)
        .map_err(|_| "the archive lists a name that is not text".to_owned())?
        .lines()
        .collect();
    check_members(&members)?;
    let name = folder
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let part = folder.with_file_name(format!("{name}.{}.part", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    let moved = std::fs::create_dir_all(&part)
        .map_err(|error| error.to_string())
        .and_then(|()| {
            tar(&[
                OsString::from("-xf"),
                archive.into(),
                "-C".into(),
                (&part).into(),
            ])
        })
        .and_then(|_| {
            let _ = std::fs::remove_dir_all(folder);
            std::fs::rename(&part, folder).map_err(|error| error.to_string())
        });
    if moved.is_err() {
        let _ = std::fs::remove_dir_all(&part);
    }
    moved
}

fn tar(args: &[OsString]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("tar");
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    no_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("tar could not run: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        Err(format!("tar failed: {}", said.trim()))
    }
}

/// Refuses an archive with a member outside its folder, or without exactly one `llama-server`.
fn check_members(members: &[&str]) -> Result<(), String> {
    let mut servers = 0;
    for member in members {
        let parts = || member.split(['/', '\\']);
        let drive = member.as_bytes().get(1) == Some(&b':');
        if member.starts_with(['/', '\\']) || drive || parts().any(|part| part == "..") {
            return Err(format!("the archive holds “{member}”, outside its folder"));
        }
        if !member.ends_with(['/', '\\'])
            && parts()
                .next_back()
                .is_some_and(|name| SERVER_FILES.contains(&name))
        {
            servers += 1;
        }
    }
    match servers {
        1 => Ok(()),
        0 => Err("the archive holds no llama-server".to_owned()),
        _ => Err("the archive holds more than one llama-server".to_owned()),
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
use linux::{CPU_ARCHIVE, VULKAN_ARCHIVE, no_window};
#[cfg(target_os = "windows")]
use windows::{CPU_ARCHIVE, VULKAN_ARCHIVE, no_window};

/// The arguments of `llama-server`: loopback only, behind `key`, with thinking off.
fn arguments(model: &Path, port: u16, key: &str, build: Build) -> Vec<OsString> {
    vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "--api-key".into(),
        key.into(),
        "--reasoning-budget".into(),
        "0".into(),
        "--temp".into(),
        "0".into(),
        "--jinja".into(),
        "--chat-template-kwargs".into(),
        r#"{"enable_thinking":false}"#.into(),
        "--ctx-size".into(),
        "2048".into(),
        "--parallel".into(),
        "1".into(),
        "--threads".into(),
        std::thread::available_parallelism()
            .map_or(4, |n| n.get().min(8))
            .to_string()
            .into(),
        "--gpu-layers".into(),
        match build {
            Build::Cpu => "0",
            Build::Vulkan => "99",
        }
        .into(),
        "-m".into(),
        model.into(),
    ]
}

/// A running server process. Dropping it kills the process and waits for it.
pub trait Process {
    /// Whether the process has ended.
    fn exited(&mut self) -> bool;

    /// Whether the server answers its health request with success.
    fn healthy(&self) -> Pin<Box<dyn Future<Output = bool> + '_>>;
}

/// How server processes are started.
pub trait Launch {
    /// A port nothing listens on.
    ///
    /// # Errors
    /// Why no port could be found.
    fn port(&self) -> io::Result<u16>;

    /// Starts `program` with `args`, a server that will listen on `port`.
    ///
    /// # Errors
    /// Why the process could not be started.
    fn launch(&self, program: &Path, args: &[OsString], port: u16) -> io::Result<Box<dyn Process>>;
}

/// Starts the real `llama-server`.
#[derive(Debug, Default)]
pub struct Spawn;

impl Launch for Spawn {
    fn port(&self) -> io::Result<u16> {
        Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port())
    }

    fn launch(&self, program: &Path, args: &[OsString], port: u16) -> io::Result<Box<dyn Process>> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut command);
        Ok(Box::new(Child {
            process: command.spawn()?,
            health: format!("http://127.0.0.1:{port}/health"),
            client: reqwest::Client::new(),
        }))
    }
}

struct Child {
    process: std::process::Child,
    health: String,
    client: reqwest::Client,
}

impl Process for Child {
    fn exited(&mut self) -> bool {
        !matches!(self.process.try_wait(), Ok(None))
    }

    fn healthy(&self) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async move {
            self.client
                .get(&self.health)
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

struct Server {
    process: Box<dyn Process>,
    port: u16,
    key: String,
    spawned: Instant,
    ready: bool,
    /// A server was started before this one.
    again: bool,
}

/// A server that answers, and what starting it took.
#[derive(Debug)]
pub struct Ready {
    pub endpoint: Endpoint,
    /// How long it took from spawn to its first healthy answer, when this call saw that.
    pub started: Option<Duration>,
}

/// Why no server answers.
#[derive(Debug, PartialEq, Eq)]
pub struct Failed {
    pub error: WritingError,
    /// How long the start took before it failed, when one was made.
    pub after: Option<Duration>,
}

/// Whether the build is fixed, or chosen from what is installed on the first start.
enum Selection {
    Fixed,
    Installed(Option<Build>),
}

/// The model on this computer: its files, and the one server process that runs it. The process
/// starts when writing first needs it and ends when this is dropped.
pub struct Chat {
    selection: Selection,
    store: ModelStore,
    build: Build,
    files: Files<'static>,
    launch: Box<dyn Launch>,
    server: Option<Server>,
    starts: u8,
}

impl Chat {
    #[must_use]
    pub fn new(
        store: ModelStore,
        build: Build,
        files: Files<'static>,
        launch: Box<dyn Launch>,
    ) -> Self {
        Self {
            selection: Selection::Fixed,
            store,
            build,
            files,
            launch,
            server: None,
            starts: 0,
        }
    }

    #[must_use]
    pub fn build(&self) -> Build {
        self.build
    }

    /// A lazy runtime whose backend is checked after installation, on its first use.
    #[must_use]
    pub fn on_computer(store: ModelStore, requested: Option<Build>) -> Self {
        let build = requested.unwrap_or(Build::Vulkan);
        Self {
            selection: Selection::Installed(requested),
            ..Self::new(store, build, build.files(), Box::new(Spawn))
        }
    }

    /// Stops the owned child before removing its mapped model file, including on Windows.
    ///
    /// # Errors
    /// Why the file could not be removed.
    pub async fn remove(&mut self) -> io::Result<()> {
        self.server = None;
        self.starts = 0;
        remove_model(&self.store, &self.files).await
    }

    /// How long the server has been starting, when it is not ready yet.
    #[must_use]
    pub fn starting_for(&self) -> Option<Duration> {
        self.server
            .as_ref()
            .filter(|server| !server.ready)
            .map(|server| server.spawned.elapsed())
    }

    /// A server that answers, starting one if there is none or it ended. At most one process is
    /// started per call. `starting` is told, with whether this is a start again, before waiting
    /// for a server that is not ready. Dropping the call leaves a starting server starting.
    ///
    /// # Errors
    /// [`WritingError::NotInstalled`] when a file is missing; otherwise the server failed to
    /// start: [`WritingError::Stopped`] when a server was started before, else
    /// [`WritingError::StartFailed`].
    pub async fn ready(&mut self, starting: impl Fn(bool)) -> Result<Ready, Failed> {
        if self.starts == 0
            && let Selection::Installed(requested) = self.selection
        {
            self.build = Build::installed(&self.store, requested);
            self.files = self.build.files();
        }
        let fail = |error, after| Err(Failed { error, after });
        let Some(program) = self.files.server_path(&self.store) else {
            return fail(WritingError::NotInstalled, None);
        };
        let model = self.files.model_path(&self.store);
        if !self.files.model_cached(&self.store) {
            return fail(WritingError::NotInstalled, None);
        }
        if self
            .server
            .as_mut()
            .is_some_and(|server| server.process.exited())
        {
            self.server = None;
        }
        if self.server.is_none() {
            if self.starts >= 2 {
                return fail(WritingError::Stopped, None);
            }
            let again = self.starts > 0;
            self.starts += 1;
            match self.spawn(&program, &model, again) {
                Ok(server) => self.server = Some(server),
                Err(_) => return fail(failure(again), Some(Duration::ZERO)),
            }
        }
        let Some(server) = self.server.as_mut() else {
            return fail(WritingError::StartFailed, None);
        };
        let endpoint = Endpoint {
            url: format!("http://127.0.0.1:{}/v1", server.port),
            model: self.files.model.file_name.to_owned(),
            key: server.key.clone(),
        };
        if server.ready {
            return Ok(Ready {
                endpoint,
                started: None,
            });
        }
        starting(server.again);
        loop {
            if server.process.healthy().await {
                server.ready = true;
                return Ok(Ready {
                    endpoint,
                    started: Some(server.spawned.elapsed()),
                });
            }
            if server.process.exited() || server.spawned.elapsed() >= Duration::from_secs(60) {
                let (after, again) = (server.spawned.elapsed(), server.again);
                self.server = None;
                return fail(failure(again), Some(after));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn spawn(&self, program: &Path, model: &Path, again: bool) -> io::Result<Server> {
        let port = self.launch.port()?;
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let key = crate::models::hex(&bytes);
        let process =
            self.launch
                .launch(program, &arguments(model, port, &key, self.build), port)?;
        Ok(Server {
            process,
            port,
            key,
            spawned: Instant::now(),
            ready: false,
            again,
        })
    }

    /// Whether the server has ended, given a moment for the end to show after a broken connection.
    pub async fn ended(&mut self) -> bool {
        let deadline = Instant::now() + EXIT_GRACE;
        loop {
            let Some(server) = self.server.as_mut() else {
                return false;
            };
            if server.process.exited() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// How a server that did not come up is reported.
fn failure(again: bool) -> WritingError {
    if again {
        WritingError::Stopped
    } else {
        WritingError::StartFailed
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::fakes::{FakeLaunch, LaunchControl};
    use super::*;

    const GGUF: Asset<'static> = Asset {
        kind: ModelKind::ChatModel,
        file_name: "test.gguf",
        url: "http://127.0.0.1:9/test.gguf",
        size: 4,
        sha256: "00",
    };
    const FILES: Files<'static> = Files {
        runtime: CPU_ARCHIVE,
        folder: "runtimes/test",
        model: GGUF,
    };

    fn installed(test: &str) -> (PathBuf, ModelStore) {
        let dir = std::env::temp_dir().join(format!("thalvi-chat-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("runtimes/test/llama")).expect("folder");
        std::fs::write(dir.join("runtimes/test/llama/llama-server"), "x").expect("server");
        std::fs::write(dir.join("test.gguf"), "gguf").expect("model");
        (dir.clone(), ModelStore::in_dir(dir))
    }

    fn chat(test: &str) -> (Chat, LaunchControl, PathBuf) {
        let (dir, store) = installed(test);
        let (launch, control) = FakeLaunch::new();
        (
            Chat::new(store, Build::Cpu, FILES, Box::new(launch)),
            control,
            dir,
        )
    }

    fn argv(control: &LaunchControl, index: usize) -> Vec<String> {
        control.spawned(index).args
    }

    /// Starts the server for one use and says how it ended: the endpoint's key, or the error.
    async fn use_it(chat: &mut Chat) -> (Result<String, WritingError>, Vec<bool>) {
        let told = RefCell::new(Vec::new());
        let result = chat.ready(|again| told.borrow_mut().push(again)).await;
        (
            result.map(|ready| ready.endpoint.key).map_err(|f| f.error),
            told.into_inner(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn the_server_listens_on_loopback_only_behind_a_fresh_key_with_thinking_off() {
        let (mut chat, control, dir) = chat("args");
        let first = use_it(&mut chat).await.0.expect("starts");
        let spawned = control.spawned(0);
        let model = dir.join("test.gguf").to_string_lossy().into_owned();
        assert_eq!(
            spawned.args,
            [
                "--host",
                "127.0.0.1",
                "--port",
                &spawned.port.to_string(),
                "--api-key",
                &first,
                "--reasoning-budget",
                "0",
                "--temp",
                "0",
                "--jinja",
                "--chat-template-kwargs",
                r#"{"enable_thinking":false}"#,
                "--ctx-size",
                "2048",
                "--parallel",
                "1",
                "--threads",
                &std::thread::available_parallelism()
                    .map_or(4, |n| n.get().min(8))
                    .to_string(),
                "--gpu-layers",
                "0",
                "-m",
                &model,
            ]
        );
        assert!(!argv(&control, 0).iter().any(|arg| arg.contains("0.0.0.0")));
        assert_eq!(first.len(), 64, "256 bits of key");

        control.exit(0);
        let second = use_it(&mut chat).await.0.expect("starts again");
        assert_ne!(first, second, "every start has its own key");
        assert_eq!(control.spawns(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_server_is_used_again_and_dropping_the_chat_kills_it() {
        let (mut chat, control, dir) = chat("kill");
        let (first, told) = use_it(&mut chat).await;
        let (second, told_again) = use_it(&mut chat).await;
        assert_eq!(first, second, "the same server answers both uses");
        assert_eq!(
            (told, told_again),
            (vec![false], vec![]),
            "only a start is told"
        );
        assert_eq!(control.spawns(), 1);
        assert!(!control.killed(0));
        drop(chat);
        assert!(control.killed(0), "the child is killed with the engine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_ended_is_started_once_more_on_the_next_use() {
        let (mut chat, control, dir) = chat("crash");
        use_it(&mut chat).await.0.expect("first start");
        control.exit(0);
        let (second, told) = use_it(&mut chat).await;
        assert!(second.is_ok(), "{second:?}");
        assert_eq!(told, [true], "told it is a start again");
        assert_eq!(control.spawns(), 2);
        assert!(control.killed(0), "the ended one is let go");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_that_fails_after_an_earlier_one_says_stopped_and_starts_nothing_more() {
        let (mut chat, control, dir) = chat("twice");
        use_it(&mut chat).await.0.expect("first start");
        control.exit(0);
        control.dead_from_now();
        let (result, told) = use_it(&mut chat).await;
        assert_eq!(result, Err(WritingError::Stopped));
        assert_eq!(told, [true]);
        assert_eq!(control.spawns(), 2, "no third start in that use");
        assert_eq!(use_it(&mut chat).await.0, Err(WritingError::Stopped));
        assert_eq!(
            control.spawns(),
            2,
            "later uses cannot start a third process"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_start_failing_is_a_start_failure_and_the_next_use_tries_again() {
        let (mut chat, control, dir) = chat("first");
        control.refuse_to_launch(true);
        let (first, _) = use_it(&mut chat).await;
        assert_eq!(first, Err(WritingError::StartFailed));
        assert_eq!(control.spawns(), 0);
        control.refuse_to_launch(false);
        let (second, told) = use_it(&mut chat).await;
        assert!(second.is_ok(), "{second:?}");
        assert_eq!(told, [true]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_use_before_the_files_are_there_starts_nothing() {
        let (mut chat, control, dir) = chat("missing");
        std::fs::remove_file(dir.join("test.gguf")).expect("remove");
        assert_eq!(use_it(&mut chat).await.0, Err(WritingError::NotInstalled));
        std::fs::write(dir.join("test.gguf"), "gguf").expect("model");
        std::fs::remove_dir_all(dir.join("runtimes")).expect("remove");
        assert_eq!(use_it(&mut chat).await.0, Err(WritingError::NotInstalled));
        assert_eq!(control.spawns(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_still_loading_is_waited_for_and_a_dropped_wait_leaves_it_loading() {
        let (mut chat, control, dir) = chat("loading");
        control.loading_from_now();
        let told = Cell::new(0);
        let waited = tokio::time::timeout(
            Duration::from_secs(5),
            chat.ready(|_| told.set(told.get() + 1)),
        )
        .await;
        assert!(waited.is_err(), "still loading after five seconds");
        assert_eq!(chat.starting_for(), Some(Duration::from_secs(5)));
        control.healthy(0);
        let (result, again) = use_it(&mut chat).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(again, [false], "the same start, not a start again");
        assert_eq!(control.spawns(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_archive_is_accepted_with_exactly_one_server_and_nothing_outside_its_folder() {
        let cases: [(&[&str], Result<(), &str>); 9] = [
            (
                &[
                    "llama-b12255/",
                    "llama-b12255/llama-server",
                    "llama-b12255/libllama.so",
                ],
                Ok(()),
            ),
            (&["llama-server.exe", "ggml.dll"], Ok(())),
            (&["a/llama-server/", "a/llama-server"], Ok(())),
            (&["../llama-server"], Err("outside its folder")),
            (&["a/../../llama-server"], Err("outside its folder")),
            (&["/usr/bin/llama-server"], Err("outside its folder")),
            (
                &["C:\\Windows\\llama-server.exe"],
                Err("outside its folder"),
            ),
            (
                &["a/llama-server", "b/llama-server.exe"],
                Err("more than one"),
            ),
            (&["a/libllama.so"], Err("no llama-server")),
        ];
        for (members, expected) in cases {
            let result = check_members(members);
            match expected {
                Ok(()) => assert_eq!(result, Ok(()), "{members:?}"),
                Err(why) => assert!(
                    result.as_ref().is_err_and(|reason| reason.contains(why)),
                    "{members:?}: {result:?}"
                ),
            }
        }
    }

    #[test]
    fn each_build_has_its_own_pinned_files() {
        let (cpu, vulkan) = (Build::Cpu.files(), Build::Vulkan.files());
        assert_ne!(cpu.folder, vulkan.folder);
        assert_ne!(cpu.runtime.sha256, vulkan.runtime.sha256);
        assert_eq!(cpu.model.file_name, vulkan.model.file_name);
        for files in [cpu, vulkan] {
            assert!(
                files
                    .runtime
                    .url
                    .starts_with("https://github.com/ggml-org/llama.cpp/releases/download/b12255/")
            );
            assert_eq!(files.runtime.sha256.len(), 64);
        }
    }
}

#[cfg(test)]
mod download_tests;
