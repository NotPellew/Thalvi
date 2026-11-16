//! Windows: hides the helper's console window and ties its life to the app's through a job object.

#![expect(
    unsafe_code,
    reason = "raw Win32 job object calls; each block says why it is sound"
)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::sync::OnceLock;

use ::windows::Win32::Foundation::HANDLE;
use ::windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(super) fn prepare(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// The job that every helper joins. It is never closed, so Windows closes it when the app ends
/// however it ends, and a job closed ends the processes in it.
fn job() -> io::Result<HANDLE> {
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    let raw = JOB.get_or_init(|| create().ok());
    raw.map(|raw| HANDLE(raw as *mut c_void)).ok_or_else(|| {
        io::Error::other("the job that ends the Whisper helper with Thalvi could not be made")
    })
}

fn create() -> ::windows::core::Result<usize> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: CreateJobObjectW takes no pointers here, and SetInformationJobObject reads exactly
    // the size of the `limits` struct that lives through the call.
    unsafe {
        let job = CreateJobObjectW(None, None)?;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast::<c_void>(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(u32::MAX),
        )?;
        Ok(job.0 as usize)
    }
}

pub(super) fn bind(child: &Child) -> io::Result<()> {
    let job = job()?;
    // SAFETY: both handles are open: the job lives for the whole process, and `child` owns its
    // process handle through the call.
    unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }.map_err(io::Error::from)
}
