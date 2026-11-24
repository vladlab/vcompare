//! A small counting semaphore (std only): at most N holders at once.

use std::sync::{Condvar, Mutex};

pub struct Slots {
    free: Mutex<usize>,
    cv: Condvar,
}

pub struct SlotGuard<'a>(&'a Slots);

impl Slots {
    pub fn new(n: usize) -> Self {
        Self { free: Mutex::new(n), cv: Condvar::new() }
    }

    /// Wait for a free slot; it is released when the guard drops.
    pub fn acquire(&self) -> SlotGuard<'_> {
        let mut free = self.free.lock().unwrap();
        while *free == 0 {
            free = self.cv.wait(free).unwrap();
        }
        *free -= 1;
        SlotGuard(self)
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn never_more_than_n_at_once() {
        let slots = Arc::new(Slots::new(3));
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..12)
            .map(|_| {
                let (slots, inside, peak) = (slots.clone(), inside.clone(), peak.clone());
                std::thread::spawn(move || {
                    let _g = slots.acquire();
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }
}
