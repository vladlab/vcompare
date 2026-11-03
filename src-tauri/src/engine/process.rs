//! The running ffmpeg child for the current analysis, and cancellation.
//!
//! The app owns one [`ProcessSlot`]. Each analysis gets a [`Job`] from
//! [`ProcessSlot::begin`], identified by a generation number. A job is
//! *live* while it is the newest job and hasn't been cancelled; starting a new
//! job supersedes the old one, which then counts as cancelled too. That way a
//! job that is still winding down after a cancel can't attach a new ffmpeg
//! over the next job's, or save its results.
//!
//! The child and the bookkeeping sit under one lock, so a cancel can't slip
//! between "is the job live?" and "attach the child": a child attached to a
//! dead job is killed on the spot.
//!
//! Convention the runners rely on: a cancel (or app quit) *removes* the child
//! from the slot as it kills it. A runner that finds no child when it comes
//! to wait knows it was killed externally, and must neither retry nor save.

use std::process::Child;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct ProcessSlot {
    inner: Mutex<SlotInner>,
}

#[derive(Default)]
struct SlotInner {
    /// Generation of the newest job.
    generation: u64,
    /// Every job with generation <= this has been cancelled.
    cancelled_through: u64,
    /// The running ffmpeg and the generation of the job that owns it.
    child: Option<(u64, Child)>,
}

impl SlotInner {
    fn live(&self, gen: u64) -> bool {
        self.generation == gen && self.cancelled_through < gen
    }
}

/// One analysis job's view of the slot.
pub struct Job {
    slot: Arc<ProcessSlot>,
    gen: u64,
}

impl ProcessSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new job, superseding (and killing the ffmpeg of) any previous one.
    pub fn begin(self: &Arc<Self>) -> Arc<Job> {
        let leftover = {
            let mut g = self.inner.lock().unwrap();
            g.generation += 1;
            g.child.take()
        };
        if let Some((_, mut c)) = leftover {
            let _ = c.kill();
            let _ = c.wait();
        }
        let gen = self.inner.lock().unwrap().generation;
        Arc::new(Job { slot: self.clone(), gen })
    }

    /// User cancel or app quit: cancel the current job, then remove and kill
    /// the running child, if any. Returns whether a process was running.
    pub fn cancel(&self) -> bool {
        let child = {
            let mut g = self.inner.lock().unwrap();
            g.cancelled_through = g.generation;
            g.child.take()
        };
        match child {
            Some((_, mut c)) => {
                let _ = c.kill();
                let _ = c.wait();
                true
            }
            None => false,
        }
    }
}

impl Job {
    /// Cancelled, or superseded by a newer job.
    pub fn is_cancelled(&self) -> bool {
        !self.slot.inner.lock().unwrap().live(self.gen)
    }

    /// Hand a freshly spawned child to the slot. Returns false (after killing
    /// the child) if this job is no longer live.
    pub fn attach(&self, mut child: Child) -> bool {
        let mut g = self.slot.inner.lock().unwrap();
        if !g.live(self.gen) {
            drop(g);
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        g.child = Some((self.gen, child));
        true
    }

    /// Take this job's child back to wait on it. `None` means it was killed
    /// from outside (cancel / quit / superseded).
    pub fn detach(&self) -> Option<Child> {
        let mut g = self.slot.inner.lock().unwrap();
        match g.child.take() {
            Some((gen, c)) if gen == self.gen => Some(c),
            other => {
                g.child = other; // someone else's: leave it alone
                None
            }
        }
    }

    /// Kill this job's child but leave it attached: for a runner's own error
    /// paths, which then detach and wait as usual.
    pub fn kill_attached(&self) {
        if let Some((gen, c)) = self.slot.inner.lock().unwrap().child.as_mut() {
            if *gen == self.gen {
                let _ = c.kill();
            }
        }
    }
}

/// A standalone job for tests and tools that don't need cancellation.
#[allow(dead_code)]
pub fn standalone_job() -> Arc<Job> {
    Arc::new(ProcessSlot::new()).begin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sleeper() -> Option<Child> {
        Command::new("sleep").arg("30").spawn().ok()
    }

    #[test]
    fn attach_after_cancel_kills_immediately() {
        let Some(child) = sleeper() else { return };
        let slot = Arc::new(ProcessSlot::new());
        let job = slot.begin();
        assert!(!slot.cancel(), "nothing running yet");
        assert!(job.is_cancelled());
        let t = std::time::Instant::now();
        assert!(!job.attach(child));
        assert!(t.elapsed().as_secs() < 5);
        assert!(job.detach().is_none());
    }

    #[test]
    fn cancel_takes_and_kills_running_child() {
        let Some(child) = sleeper() else { return };
        let slot = Arc::new(ProcessSlot::new());
        let job = slot.begin();
        assert!(job.attach(child));
        assert!(slot.cancel());
        assert!(job.detach().is_none(), "cancel must remove the child");
        let next = slot.begin();
        assert!(!next.is_cancelled(), "a new job starts live");
        assert!(job.is_cancelled(), "the old job stays cancelled");
    }

    #[test]
    fn new_job_supersedes_old_one() {
        let (Some(a), Some(b)) = (sleeper(), sleeper()) else { return };
        let slot = Arc::new(ProcessSlot::new());
        let old = slot.begin();
        let new = slot.begin();
        assert!(old.is_cancelled());
        // The old job winding down can't take over the slot…
        assert!(!old.attach(a));
        // …nor touch the new job's child.
        assert!(new.attach(b));
        old.kill_attached();
        assert!(old.detach().is_none());
        let mut c = new.detach().expect("new job's child untouched");
        assert!(c.try_wait().unwrap().is_none(), "still running");
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn kill_attached_leaves_child_for_wait() {
        let Some(child) = sleeper() else { return };
        let job = standalone_job();
        assert!(job.attach(child));
        job.kill_attached();
        let mut c = job.detach().expect("still attached");
        assert!(!c.wait().unwrap().success());
        assert!(!job.is_cancelled());
    }
}
