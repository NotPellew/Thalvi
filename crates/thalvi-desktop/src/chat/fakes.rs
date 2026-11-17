//! A test double for the server process: it records how it was started and does what a test says.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use super::{Launch, Process};

/// How a started process behaves until a test says otherwise.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Born {
    /// Healthy at once.
    Ready,
    /// Still loading its model.
    Loading,
    /// Ended before it ever answered.
    Dead,
}

#[derive(Default)]
struct Flags {
    exited: Cell<bool>,
    healthy: Cell<bool>,
    killed: Cell<bool>,
    /// How long `healthy` waits before answering, so a test can outlast the cleanup limit.
    delay: Cell<std::time::Duration>,
}

/// How one process was started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub port: u16,
}

impl Spawned {
    /// The value after `--api-key`.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == "--api-key")?;
        self.args.get(at + 1).map(String::as_str)
    }
}

struct Shared {
    /// The port every start is given, or a new one each time when `None`.
    port: Option<u16>,
    refuse: bool,
    born: Born,
    /// How long a process waits before answering its health request.
    delay: std::time::Duration,
    spawned: Vec<Spawned>,
    flags: Vec<Rc<Flags>>,
}

/// Starts fake processes for [`Chat`](super::Chat).
pub struct FakeLaunch(Rc<RefCell<Shared>>);

/// Test-side handle of [`FakeLaunch`]; processes are counted from 0 in the order they started.
#[derive(Clone)]
pub struct LaunchControl(Rc<RefCell<Shared>>);

impl FakeLaunch {
    /// Processes are healthy as soon as they start.
    #[must_use]
    pub fn new() -> (Self, LaunchControl) {
        let shared = Rc::new(RefCell::new(Shared {
            port: None,
            refuse: false,
            born: Born::Ready,
            delay: std::time::Duration::ZERO,
            spawned: Vec::new(),
            flags: Vec::new(),
        }));
        (Self(Rc::clone(&shared)), LaunchControl(shared))
    }
}

struct FakeProcess(Rc<Flags>);

impl Process for FakeProcess {
    fn exited(&mut self) -> bool {
        self.0.exited.get()
    }

    fn healthy(&self) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async {
            let delay = self.0.delay.get();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.0.healthy.get()
        })
    }
}

impl Drop for FakeProcess {
    fn drop(&mut self) {
        self.0.killed.set(true);
    }
}

impl Launch for FakeLaunch {
    fn port(&self) -> io::Result<u16> {
        let shared = self.0.borrow();
        Ok(shared
            .port
            .unwrap_or_else(|| 40_000 + u16::try_from(shared.spawned.len()).unwrap_or_default()))
    }

    fn launch(&self, program: &Path, args: &[OsString], port: u16) -> io::Result<Box<dyn Process>> {
        let mut shared = self.0.borrow_mut();
        if shared.refuse {
            return Err(io::Error::other("scripted failure"));
        }
        let flags = Rc::new(Flags::default());
        flags.healthy.set(shared.born == Born::Ready);
        flags.exited.set(shared.born == Born::Dead);
        flags.delay.set(shared.delay);
        shared.flags.push(Rc::clone(&flags));
        shared.spawned.push(Spawned {
            program: program.to_owned(),
            args: args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            port,
        });
        Ok(Box::new(FakeProcess(flags)))
    }
}

impl LaunchControl {
    /// How many processes were started.
    #[must_use]
    pub fn spawns(&self) -> usize {
        self.0.borrow().spawned.len()
    }

    /// How process `index` was started.
    ///
    /// # Panics
    /// When fewer processes were started.
    #[must_use]
    pub fn spawned(&self, index: usize) -> Spawned {
        self.0.borrow().spawned[index].clone()
    }

    /// Process `index` ends.
    pub fn exit(&self, index: usize) {
        let shared = self.0.borrow();
        shared.flags[index].exited.set(true);
        shared.flags[index].healthy.set(false);
    }

    /// Process `index` answers its health request.
    pub fn healthy(&self, index: usize) {
        self.0.borrow().flags[index].healthy.set(true);
    }

    /// Whether process `index` was killed, which is what dropping it does.
    #[must_use]
    pub fn killed(&self, index: usize) -> bool {
        self.0.borrow().flags[index].killed.get()
    }

    /// Processes started from now on are still loading their model.
    pub fn loading_from_now(&self) {
        self.0.borrow_mut().born = Born::Loading;
    }

    /// Processes started from now on take `delay` to answer their health request.
    pub fn delay_health(&self, delay: std::time::Duration) {
        self.0.borrow_mut().delay = delay;
    }

    /// Processes started from now on have ended before they answer.
    pub fn dead_from_now(&self) {
        self.0.borrow_mut().born = Born::Dead;
    }

    /// Starting a process fails, or no longer does.
    pub fn refuse_to_launch(&self, refuse: bool) {
        self.0.borrow_mut().refuse = refuse;
    }

    /// Every process is given `port`, such as that of a fake chat endpoint.
    pub fn listen_on(&self, port: u16) {
        self.0.borrow_mut().port = Some(port);
    }
}
