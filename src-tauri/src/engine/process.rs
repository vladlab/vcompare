//! The running ffmpeg children for the current analysis, and cancellation.
//!
//! The app owns one [`ProcessSlot`]. Each analysis gets a [`Job`] from
//! [`ProcessSlot::begin`], identified by a generation number. A job is
//! *live* while it is the newest job and hasn't been cancelled; starting a new
//! job supersedes the old one, which then counts as cancelled too. That way a
//! job that is still winding down after a cancel can't attach a new ffmpeg
//! next to the next job's, or save its results.
//!
//! A job may run several ffmpeg processes at once (chunked decoding). Each
//! attached child gets a [`ChildId`]; the runner that spawned it detaches and
//! kills it by that id.
//!
//! The children and the bookkeeping sit under one lock, so a cancel can't slip
//! between "is the job live?" and "attach the child": a child attached to a
//! dead job is killed on the spot.
//!
//! Convention the runners rely on: a cancel (or app quit) *removes* children
//! from the slot as it kills them. A runner that finds its child gone when it
//! comes to wait knows it was killed externally, and must neither retry nor
//! save.

use std::process::Child;
use std::sync::{Arc, Mutex};

/// Identifies one attached child within the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildId(u64);

#[derive(Default)]
pub struct ProcessSlot {
    inner: Mutex<SlotInner>,
}

struct Attached {
    gen: u64,
    id: u64,
    child: Child,
}

#[derive(Default)]
struct SlotInner {
    /// Generation of the newest job.
    generation: u64,
    /// Every job with generation <= this has been cancelled.
    cancelled_through: u64,
    next_id: u64,
    /// Running ffmpeg processes and the job that owns each.
    children: Vec<Attached>,
}

impl SlotInner {
    fn live(&self, gen: u64) -> bool {
        self.generation == gen && self.cancelled_through < gen
    }
}

fn kill_and_reap(mut children: Vec<Attached>) -> usize {
    let n = children.len();
    for a in children.iter_mut() {
        let _ = a.child.kill();
    }
    for a in children.iter_mut() {
        let _ = a.child.wait();
    }
    n
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
        let (gen, leftovers) = {
            let mut g = self.inner.lock().unwrap();
            g.generation += 1;
            (g.generation, std::mem::take(&mut g.children))
        };
        kill_and_reap(leftovers);
        Arc::new(Job { slot: self.clone(), gen })
    }

    /// User cancel or app quit: cancel the current job, then remove and kill
    /// all running children. Returns whether any process was running.
    pub fn cancel(&self) -> bool {
        let children = {
            let mut g = self.inner.lock().unwrap();
            g.cancelled_through = g.generation;
            std::mem::take(&mut g.children)
        };
        kill_and_reap(children) > 0
    }
}

impl Job {
    /// Cancelled, or superseded by a newer job.
    pub fn is_cancelled(&self) -> bool {
        !self.slot.inner.lock().unwrap().live(self.gen)
    }

    /// Hand a freshly spawned child to the slot. Returns `None` (after
    /// killing the child) if this job is no longer live.
    pub fn attach(&self, mut child: Child) -> Option<ChildId> {
        let mut g = self.slot.inner.lock().unwrap();
        if !g.live(self.gen) {
            drop(g);
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        g.next_id += 1;
        let id = g.next_id;
        g.children.push(Attached { gen: self.gen, id, child });
        Some(ChildId(id))
    }

    /// Take a child back to wait on it. `None` means it was killed from
    /// outside (cancel / quit / superseded).
    pub fn detach(&self, id: ChildId) -> Option<Child> {
        let mut g = self.slot.inner.lock().unwrap();
        let pos = g.children.iter().position(|a| a.id == id.0 && a.gen == self.gen)?;
        Some(g.children.swap_remove(pos).child)
    }

    /// Kill one of this job's children but leave it attached: for a runner's
    /// own error paths, which then detach and wait as usual.
    pub fn kill_attached(&self, id: ChildId) {
        let mut g = self.slot.inner.lock().unwrap();
        if let Some(a) = g.children.iter_mut().find(|a| a.id == id.0 && a.gen == self.gen) {
            let _ = a.child.kill();
        }
    }

    /// Kill all of this job's children, leaving them attached (so their
    /// runners report an error, not an external kill). Used when one chunk
    /// fails and the others are pointless.
    pub fn kill_all_attached(&self) {
        let mut g = self.slot.inner.lock().unwrap();
        for a in g.children.iter_mut().filter(|a| a.gen == self.gen) {
            let _ = a.child.kill();
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
        assert!(job.attach(child).is_none());
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn cancel_takes_and_kills_all_children() {
        let (Some(a), Some(b)) = (sleeper(), sleeper()) else { return };
        let slot = Arc::new(ProcessSlot::new());
        let job = slot.begin();
        let ia = job.attach(a).unwrap();
        let ib = job.attach(b).unwrap();
        assert_ne!(ia, ib);
        assert!(slot.cancel());
        assert!(job.detach(ia).is_none(), "cancel must remove every child");
        assert!(job.detach(ib).is_none());
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
        // The old job winding down can't attach…
        assert!(old.attach(a).is_none());
        // …nor touch the new job's child.
        let ib = new.attach(b).unwrap();
        old.kill_all_attached();
        assert!(old.detach(ib).is_none());
        let mut c = new.detach(ib).expect("new job's child untouched");
        assert!(c.try_wait().unwrap().is_none(), "still running");
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn kill_by_id_and_kill_all() {
        let (Some(a), Some(b), Some(c)) = (sleeper(), sleeper(), sleeper()) else { return };
        let job = standalone_job();
        let (ia, ib, ic) = (job.attach(a).unwrap(), job.attach(b).unwrap(), job.attach(c).unwrap());
        job.kill_attached(ia);
        let mut ca = job.detach(ia).expect("still attached after kill_attached");
        assert!(!ca.wait().unwrap().success());
        let mut cb = job.detach(ib).unwrap();
        assert!(cb.try_wait().unwrap().is_none(), "others untouched");
        job.attach(cb).map(|id| {
            job.kill_all_attached();
            let mut cb = job.detach(id).unwrap();
            assert!(!cb.wait().unwrap().success());
        });
        let mut cc = job.detach(ic).unwrap();
        assert!(!cc.wait().unwrap().success(), "kill_all_attached got it too");
        assert!(!job.is_cancelled());
    }
}
