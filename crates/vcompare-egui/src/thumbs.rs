//! Thumbnail strips for event rows, loaded lazily (port of
//! src/lib/thumbLoader.svelte.ts).
//!
//! - Settle: after a detector parameter changes nothing loads until
//!   `SETTLE` of quiet. Opening an analysis or switching tabs uses `QUICK`.
//! - Visible first: the queue is what the list says is on screen, replaced
//!   (not appended to) whenever that changes.
//! - Bounded: at most `MAX_IN_FLIGHT` extractions; the service caps it too.
//! - Cached by span (first frame + count), so events that survive a
//!   parameter change keep their tiles. LRU.
//!
//! JPEG decode and texture upload happen on the worker thread; the UI
//! thread only swaps finished strips in.

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vcompare_service::Service;

pub const SETTLE: Duration = Duration::from_millis(2500);
pub const QUICK: Duration = Duration::from_millis(300);
const SCROLL: Duration = Duration::from_millis(150);
const MAX_IN_FLIGHT: usize = 3;
const CACHE_LIMIT: usize = 800;
/// Requested at twice the 54 pt display height (HiDPI).
const REQUEST_HEIGHT: u32 = 108;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: i64,
    pub count: u32,
}

/// One tile; `None` before the start of the file.
pub type Tile = Option<TextureHandle>;

pub struct Strip {
    /// One row per file (label, tiles).
    pub rows: Vec<(String, Vec<Tile>)>,
}

pub enum StripState {
    Ready(Arc<Strip>),
    Error,
}

pub struct ThumbLoader {
    svc: Arc<Service>,
    ctx: egui::Context,
    strips: HashMap<Span, (StripState, u64)>,
    tick: u64,
    queue: Vec<Span>,
    in_flight: HashSet<Span>,
    held_until: Instant,
    start_at: Option<Instant>,
    tx: Sender<(Span, Result<Strip, String>)>,
    rx: Receiver<(Span, Result<Strip, String>)>,
    /// First failure since the last hold, for the status line.
    pub error: Option<String>,
    reported: bool,
}

impl ThumbLoader {
    pub fn new(svc: Arc<Service>, ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        Self {
            svc,
            ctx,
            strips: HashMap::new(),
            tick: 0,
            queue: Vec::new(),
            in_flight: HashSet::new(),
            held_until: Instant::now(),
            start_at: None,
            tx,
            rx,
            error: None,
            reported: false,
        }
    }

    pub fn get(&mut self, s: Span) -> Option<&StripState> {
        self.tick += 1;
        let t = self.tick;
        self.strips.get_mut(&s).map(|e| {
            e.1 = t;
            &e.0
        })
    }

    /// The list's wishes, in priority order. Replaces the previous queue.
    pub fn want(&mut self, spans: &[Span]) {
        let mut seen = HashSet::new();
        let q: Vec<Span> = spans
            .iter()
            .copied()
            .filter(|s| !self.strips.contains_key(s) && !self.in_flight.contains(s) && seen.insert(*s))
            .collect();
        if q != self.queue {
            self.queue = q;
            if !self.queue.is_empty() {
                self.kick(SCROLL);
            }
        }
    }

    /// Parameters changed: load nothing until `d` of quiet.
    pub fn hold(&mut self, d: Duration) {
        self.held_until = Instant::now() + d;
        self.reported = false;
        self.kick(d);
    }

    /// Forget everything (another analysis was opened).
    pub fn clear(&mut self) {
        self.queue.clear();
        self.strips.clear();
        self.reported = false;
        self.error = None;
    }

    /// A failed strip is retried when its row comes back into view.
    pub fn forget_errors(&mut self, spans: &[Span]) {
        for s in spans {
            if matches!(self.strips.get(s), Some((StripState::Error, _))) {
                self.strips.remove(s);
            }
        }
    }

    fn kick(&mut self, d: Duration) {
        let at = (Instant::now() + d).max(self.held_until);
        self.start_at = Some(at);
    }

    /// Call once per frame: takes finished strips, starts queued ones.
    pub fn poll(&mut self) {
        while let Ok((span, r)) = self.rx.try_recv() {
            self.in_flight.remove(&span);
            let state = match r {
                Ok(strip) => StripState::Ready(Arc::new(strip)),
                Err(e) => {
                    if !self.reported {
                        self.reported = true;
                        self.error = Some(e);
                    }
                    StripState::Error
                }
            };
            self.tick += 1;
            self.strips.insert(span, (state, self.tick));
            self.start_at.get_or_insert_with(Instant::now);
        }
        self.evict();

        let Some(at) = self.start_at else { return };
        let now = Instant::now();
        if now < at.max(self.held_until) {
            self.ctx.request_repaint_after(at.max(self.held_until) - now);
            return;
        }
        self.start_at = None;
        while self.in_flight.len() < MAX_IN_FLIGHT && !self.queue.is_empty() {
            let s = self.queue.remove(0);
            if self.strips.contains_key(&s) || !self.in_flight.insert(s) {
                continue;
            }
            let (svc, ctx, tx) = (self.svc.clone(), self.ctx.clone(), self.tx.clone());
            std::thread::spawn(move || {
                let r = load_strip(&svc, &ctx, s);
                let _ = tx.send((s, r));
                ctx.request_repaint();
            });
        }
    }

    fn evict(&mut self) {
        if self.strips.len() <= CACHE_LIMIT {
            return;
        }
        let mut ages: Vec<u64> = self.strips.values().map(|v| v.1).collect();
        ages.sort_unstable();
        let cutoff = ages[self.strips.len() - CACHE_LIMIT * 3 / 4];
        self.strips.retain(|_, v| v.1 >= cutoff);
    }
}

fn load_strip(svc: &Service, ctx: &egui::Context, s: Span) -> Result<Strip, String> {
    let rows = svc.preview_frames(s.start, s.count, REQUEST_HEIGHT)?;
    let mut out = Vec::new();
    for row in rows {
        let mut tiles = Vec::new();
        for f in row.frames {
            let tile = match f.jpeg {
                None => None,
                Some(jpeg) => {
                    let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                        .map_err(|e| format!("thumbnail decode: {e}"))?
                        .to_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    Some(ctx.load_texture(format!("thumb-{}-{}", row.label, f.frame), ci, TextureOptions::LINEAR))
                }
            };
            tiles.push(tile);
        }
        out.push((row.label, tiles));
    }
    Ok(Strip { rows: out })
}
