//! Linux: ties the helper's life to the app's, so it dies with the app however the app ends.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// The helper receives SIGKILL when the thread that starts it ends. That is the engine thread,
/// which lives as long as the app, so the helper outlives neither the app's exit nor its crash.
pub(super) fn prepare(command: &mut Command) {
    #[expect(
        unsafe_code,
        reason = "Linux installs the parent-death signal via CommandExt::pre_exec"
    )]
    // SAFETY: the closure makes one prctl system call, which allocates nothing and is
    // async-signal-safe, as pre_exec requires between fork and exec.
    unsafe {
        command.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(io::Error::from)
        });
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Windows can fail to put the helper in its job, and this is the same call"
)]
pub(super) fn bind(_child: &Child) -> io::Result<()> {
    Ok(())
}
