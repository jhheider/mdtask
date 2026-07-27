//! Stopping a run that is already going.
//!
//! This exists for the agent surface. A person at a terminal has Ctrl-C; an MCP
//! client has only `notifications/cancelled`, and something has to be listening
//! for it and able to act. Without this, a task that waits on a network call, or
//! serves, or simply sleeps, runs to completion no matter what the client says.
//!
//! # Killing the group, not the child
//!
//! A shell task is `sh -c <script>`, so the thing we spawn is a shell, and the
//! work is its children. Killing the shell alone leaves `sleep 1000` (or a
//! `docker compose up`) orphaned and running, while we report the task stopped.
//! That is worse than not cancelling at all, because it is a lie.
//!
//! So each step is spawned into its **own process group** and the signal goes to
//! the group. `CommandExt::process_group` sets it with no dependency;
//! `killpg` needs `libc`, which is why this crate has one on unix.
//!
//! # Terminate, then kill
//!
//! `cancel` sends `SIGTERM` so a script's `trap` can clean up, then escalates to
//! `SIGKILL` after a grace period. The escalation runs on its own thread: the
//! caller is a request loop that has to stay responsive, and blocking it for the
//! grace period would recreate the exact problem this is here to solve.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// How long a cancelled step gets to handle `SIGTERM` before `SIGKILL`.
///
/// Long enough for a `trap` to remove a temporary file, short enough that a
/// client asking to cancel does not conclude nothing happened.
#[cfg(unix)]
const GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A handle for stopping a run from another thread.
///
/// Cheap to clone (one `Arc`), which is the point: the thread running the task
/// holds one, and so does whatever is listening for the request to stop.
#[derive(Clone, Default)]
pub struct Cancel {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    /// The process group of the step running right now, if one is.
    group: Mutex<Option<u32>>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop the run: signal whatever is running now, and refuse to start any
    /// further step.
    ///
    /// Idempotent, and safe to call when nothing is running (a run cancelled
    /// between steps simply never starts the next one).
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let group = *self.inner.group.lock().expect("cancel mutex");
        if let Some(pgid) = group {
            self.signal_group(pgid);
        }
    }

    /// Whether [`cancel`](Self::cancel) has been called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Record the group of a step that has just started, and signal it at once
    /// if cancellation arrived while it was being spawned.
    ///
    /// The check happens under the lock, which is what closes that race: a
    /// `cancel` landing between `spawn` and here would otherwise find no group
    /// recorded and signal nothing, leaving the step running after a successful
    /// cancellation.
    pub(crate) fn entered(&self, pgid: u32) {
        let mut group = self.inner.group.lock().expect("cancel mutex");
        *group = Some(pgid);
        drop(group);
        if self.is_cancelled() {
            self.signal_group(pgid);
        }
    }

    /// Forget the step's group, because it has exited. Also what stops the
    /// escalation thread from signalling a group id the system has since reused.
    pub(crate) fn left(&self) {
        *self.inner.group.lock().expect("cancel mutex") = None;
    }

    #[cfg(unix)]
    fn signal_group(&self, pgid: u32) {
        // Negative pid means "the group". ESRCH (already gone) is the expected
        // outcome of a race and is not worth reporting.
        unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGTERM) };

        // Escalate on a thread so the caller's loop keeps serving. The clone is
        // an Arc bump, not a copy of anything.
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || {
            std::thread::sleep(GRACE);
            // Only if this exact step is still running. Without the check, a
            // step that exited during the grace period could have had its group
            // id reused, and we would SIGKILL an unrelated process.
            let still = *inner.group.lock().expect("cancel mutex");
            if still == Some(pgid) {
                unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
            }
        });
    }

    /// Without process groups there is nothing to signal, so cancellation takes
    /// effect at the next step boundary. mdtask's tasks are shell scripts, so
    /// this platform is already a long way off the beaten path.
    #[cfg(not(unix))]
    fn signal_group(&self, _pgid: u32) {}
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_is_not_cancelled() {
        assert!(!Cancel::new().is_cancelled());
    }

    #[test]
    fn cancelling_is_visible_through_a_clone() {
        let a = Cancel::new();
        let b = a.clone();
        a.cancel();
        assert!(b.is_cancelled(), "the clone shares the state");
    }

    #[test]
    fn cancelling_twice_is_harmless() {
        let c = Cancel::new();
        c.cancel();
        c.cancel();
        assert!(c.is_cancelled());
    }

    /// Cancelling with nothing running must not panic or block: a run cancelled
    /// between two steps simply never starts the next one.
    #[test]
    fn cancelling_with_nothing_running_is_fine() {
        let c = Cancel::new();
        c.cancel();
        c.left();
        assert!(c.is_cancelled());
    }
}
