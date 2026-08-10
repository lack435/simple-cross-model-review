//! Windows job objects, for cleaning up a reviewer's entire process tree.
//!
//! `Child::kill` is a single-process TerminateProcess. On Windows that orphans
//! descendants rather than reaping them, and an orphan that inherited one of our pipe
//! handles keeps that pipe open. So a reviewer CLI that spawns helpers can outlive both
//! its own exit and our kill, leaking processes and blocking our reader threads.
//!
//! A job object fixes it at the right level: the child is assigned to the job before it
//! does any work, and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means every process still in
//! the job dies when the handle closes -- including on the path where the direct child
//! exits normally but leaves something behind.
//!
//! The alternative, shelling out to `taskkill /T`, was rejected on two counts. It is
//! post-hoc, so it cannot help once the direct child has exited and the parent/child
//! links are gone; and invoking it by bare name is an execution hazard, because Windows
//! resolves an unqualified executable through the current directory before System32 --
//! and our current directory is the repository under review. A repo containing
//! `taskkill.exe` would have been enough.

#![cfg(windows)]

use std::os::raw::c_void;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

type Handle = *mut c_void;

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;

// LARGE_INTEGER -> i64, DWORD -> u32, SIZE_T / ULONG_PTR -> usize.
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(
        job: Handle,
        info_class: u32,
        info: *mut c_void,
        info_len: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
}

/// Whether this process belongs to a Windows job object.
///
/// `job = NULL` asks about any job, which is exactly the compatibility question for the
/// Codex evidence-service probe: a server spawned as the reviewer CLI's grandchild should
/// inherit cross-review's kill-on-close job unless Codex explicitly breaks away. `None`
/// means Windows refused the query, so callers report "unknown" rather than asserting no.
pub fn current_process_in_job() -> Option<bool> {
    let mut in_job = 0i32;
    // SAFETY: GetCurrentProcess returns the documented pseudo-handle, NULL selects any job,
    // and `in_job` is a valid writable BOOL for the duration of the call.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    (ok != 0).then_some(in_job != 0)
}

/// A job object that kills everything still inside it when dropped.
pub struct JobObject {
    handle: Handle,
}

impl JobObject {
    /// Create a job configured to terminate its members on close. Returns `None` if the
    /// OS refuses, in which case the caller simply proceeds without tree cleanup: losing
    /// it degrades hygiene, and is not worth failing a review over.
    pub fn new() -> Option<Self> {
        // SAFETY: a null attributes pointer and null name request the documented
        // defaults (unnamed job, default security descriptor).
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let job = Self { handle };

        let mut info = ExtendedLimitInformation::default();
        info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: `info` is a correctly laid out JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        // living on our stack for the duration of the call, and the length we pass is
        // its real size.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            // Without the kill-on-close limit the job would not clean anything up, so
            // report failure rather than hand back a job that does nothing.
            return None;
        }
        Some(job)
    }

    /// Put a freshly spawned child, and therefore everything it goes on to spawn, into
    /// the job.
    ///
    /// There is a small unavoidable window: `std::process::Command` cannot spawn
    /// suspended, so a child that spawns a grandchild before this call lands would leave
    /// that grandchild outside the job. Reviewer CLIs do meaningful startup work first,
    /// so in practice the assignment wins.
    pub fn assign(&self, child: &Child) -> bool {
        // SAFETY: the handle is valid while `child` is alive, which it is here.
        unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as Handle) != 0 }
    }

    /// Kill every process in the job now. Used on timeout and cancel so the pipes close
    /// promptly instead of leaving output collection to wait out its grace period.
    pub fn terminate(&self) {
        // SAFETY: the handle is owned and valid until Drop.
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // Closing the last handle triggers KILL_ON_JOB_CLOSE, so any straggler the
        // reviewer left behind dies here even after a clean exit.
        // SAFETY: the handle is owned, valid, and closed exactly once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

// The handle is owned exclusively by this value and every use goes through &self into a
// thread-safe Win32 call, so it is sound to move a job between threads.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}
