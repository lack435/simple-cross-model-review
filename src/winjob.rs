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

use std::io;
use std::os::raw::c_void;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

type Handle = *mut c_void;

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
// JOBOBJECTINFOCLASS::JobObjectBasicAccountingInformation
const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: u32 = 1;
// CreateProcess creation flag: start the primary thread suspended so the process can be assigned to a
// job before it runs a single instruction (creation-time association, impl-plan f10/f-r3.3).
const CREATE_SUSPENDED: u32 = 0x0000_0004;
// CreateToolhelp32Snapshot flag + thread-access right, for resuming the suspended primary thread.
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const INVALID_HANDLE_VALUE: isize = -1;

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

// JOBOBJECT_BASIC_ACCOUNTING_INFORMATION: only `active_processes` is read (the quiescence signal).
#[repr(C)]
#[derive(Default)]
struct BasicAccountingInformation {
    total_user_time: i64,
    total_kernel_time: i64,
    this_period_total_user_time: i64,
    this_period_total_kernel_time: i64,
    total_page_fault_count: u32,
    total_processes: u32,
    active_processes: u32,
    total_terminated_processes: u32,
}

// THREADENTRY32 for the Toolhelp thread walk (used to resume the suspended primary thread).
#[repr(C)]
#[derive(Default)]
struct ThreadEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_thread_id: u32,
    th32_owner_process_id: u32,
    tp_base_pri: i32,
    tp_delta_pri: i32,
    dw_flags: u32,
}

extern "system" {
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(
        job: Handle,
        info_class: u32,
        info: *mut c_void,
        info_len: u32,
    ) -> i32;
    fn QueryInformationJobObject(
        job: Handle,
        info_class: u32,
        info: *mut c_void,
        info_len: u32,
        return_len: *mut u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(desired_access: u32, inherit: i32, thread_id: u32) -> Handle;
    fn ResumeThread(thread: Handle) -> u32;
    fn GetProcessId(process: Handle) -> u32;
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

    /// Spawn `cmd` **already inside this job**, closing the assign-after-spawn window (impl-plan
    /// f10/f-r3.3): the process is created suspended, assigned to the job before it runs a single
    /// instruction — so every descendant it later spawns is a job member — and only then resumed.
    ///
    /// **Fail-closed containment:** if the process cannot be assigned to the job, the suspended child
    /// is killed and an error returned rather than resumed uncontained. Used only by the login runner,
    /// which must be able to reap the vendor login's whole tree on abort. Requires a job that keeps
    /// `KILL_ON_JOB_CLOSE` (as [`new`](Self::new) does), so a crash of *this* process also reaps the
    /// login child via the handle closing (f-b1).
    #[allow(dead_code)] // caller lands with the login runner (reviewer::run_login, #15 part 3b).
    pub fn spawn_in_job(&self, cmd: &mut Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        if !self.assign(&child) {
            let assign_err = io::Error::last_os_error();
            // The child is still suspended and uncontained: kill it directly rather than resume it.
            let _ = child.kill();
            return Err(io::Error::other(format!(
                "could not assign the login process to its containment job: {assign_err}"
            )));
        }
        // Resume the primary thread. The child is suspended, so exactly one thread exists; a
        // documented Toolhelp walk finds and resumes it (avoids any undocumented ntdll call).
        // SAFETY: the child handle is valid and owned by `child`.
        let pid = unsafe { GetProcessId(child.as_raw_handle() as Handle) };
        if pid == 0 {
            let _ = child.kill();
            return Err(io::Error::other(
                "could not read the login process id to resume it",
            ));
        }
        if let Err(e) = resume_process_threads(pid) {
            let _ = child.kill();
            return Err(e);
        }
        Ok(child)
    }

    /// How many processes are still live in the job. The login runner waits for this to reach zero
    /// (a bounded quiescence wait) before verifying credentials or cleaning up staging, so no lingering
    /// in-job helper is still writing while it does (impl-plan f14/f18). The browser is out-of-job and
    /// so excluded. Returns `Err` if the job cannot be queried (the caller treats that as uncontained).
    #[allow(dead_code)] // caller lands with the login runner (reviewer::run_login, #15 part 3b).
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info = BasicAccountingInformation::default();
        let mut ret_len: u32 = 0;
        // SAFETY: `info` is the correctly sized struct for the accounting info class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<BasicAccountingInformation>() as u32,
                &mut ret_len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.active_processes)
    }
}

/// Resume every thread owned by `pid`. With a `CREATE_SUSPENDED` child there is exactly one (the
/// primary) at this point, still suspended; resuming it starts the process. Documented Toolhelp APIs
/// only.
#[allow(dead_code)] // reached via spawn_in_job, whose caller lands with the login runner.
fn resume_process_threads(pid: u32) -> io::Result<()> {
    // SAFETY: a snapshot of all threads; `pid` filter is applied per-entry below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot as isize == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Err(io::Error::last_os_error());
    }
    // Ensure the snapshot handle is closed on every path.
    struct SnapshotGuard(Handle);
    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            // SAFETY: owned snapshot handle, closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let _guard = SnapshotGuard(snapshot);

    let mut entry = ThreadEntry32 {
        dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
        ..Default::default()
    };
    let mut resumed_any = false;
    // SAFETY: `entry` is a correctly sized THREADENTRY32.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32_owner_process_id == pid {
            // SAFETY: opening the thread by id for resume access; null on failure is handled.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32_thread_id) };
            if !thread.is_null() {
                // SAFETY: `thread` is a valid handle we own; ResumeThread decrements the suspend count.
                unsafe {
                    ResumeThread(thread);
                    CloseHandle(thread);
                }
                resumed_any = true;
            }
        }
        entry.dw_size = std::mem::size_of::<ThreadEntry32>() as u32;
        // SAFETY: same entry buffer.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    if !resumed_any {
        return Err(io::Error::other(
            "could not find the login process's primary thread to resume",
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    #[test]
    fn spawn_in_job_associates_and_resumes_the_child() {
        // A CREATE_SUSPENDED child that is never resumed would never exit; observing its real exit
        // code proves spawn_in_job assigned it to the job and then resumed it (f10/f-r3.3).
        let job = JobObject::new().expect("create job");
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = job.spawn_in_job(&mut cmd).expect("spawn in job");
        let status = child.wait().expect("wait");
        assert_eq!(
            status.code(),
            Some(7),
            "the child ran to completion (was resumed)"
        );

        // After the child exits the job drains to zero live processes (the quiescence signal).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let active = job.active_processes().expect("query active");
            if active == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "job never quiesced");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
