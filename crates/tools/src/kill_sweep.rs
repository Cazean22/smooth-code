//! Process-group kill sweeps that must complete before process exit.
//!
//! `run_command` defers its SIGKILL sweep by a SIGTERM grace on a detached
//! tokio task so the kill survives the tool future being dropped or its turn
//! task being hard-aborted. A detached task cannot survive the *process*
//! exiting, though — and shutdown deliberately finishes faster than the
//! grace — so every scheduled sweep is also recorded here, and exit paths
//! call [`sweep_pending_process_kills`] to fire the outstanding SIGKILLs
//! immediately. The registry entry is removed by whichever side kills first,
//! so a group is never SIGKILLed twice (a pgid could in principle be reused
//! after the first kill reaps the group).

#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
static PENDING_KILL_PGIDS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

/// Record `pgid` as pending and SIGKILL it after `grace` unless something
/// (the exit sweep) already did.
#[cfg(unix)]
pub(crate) fn schedule_kill_sweep(pgid: libc::pid_t, grace: std::time::Duration) {
    if let Ok(mut pending) = PENDING_KILL_PGIDS.lock() {
        pending.push(pgid);
    }
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        if take_pending(pgid) {
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    });
}

/// Remove `pgid` from the registry; `true` means the caller owns the kill.
#[cfg(unix)]
fn take_pending(pgid: libc::pid_t) -> bool {
    PENDING_KILL_PGIDS
        .lock()
        .map(|mut pending| {
            let len_before = pending.len();
            pending.retain(|candidate| *candidate != pgid);
            pending.len() != len_before
        })
        .unwrap_or(false)
}

/// SIGKILL every process group whose graceful sweep has not fired yet.
///
/// Call on process-exit paths (core shutdown, the TUI exit epilogue): the
/// deferred grace is a courtesy to the subprocess that must not outlive this
/// process — exiting before the sweep fires would orphan any group member
/// that ignored the SIGTERM.
pub fn sweep_pending_process_kills() {
    #[cfg(unix)]
    {
        let drained = PENDING_KILL_PGIDS
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for pgid in drained {
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}
