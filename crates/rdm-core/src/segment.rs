//! The segment table: which byte ranges exist, who is fetching them, and how
//! an idle connection takes work from a busy one (dynamic segmentation).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// `end` of a segment whose length the server did not announce.
pub const UNKNOWN_END: u64 = u64::MAX;

struct Range {
    /// Next byte to fetch from the network.
    pos: u64,
    /// Exclusive. Shrinks when another connection steals the tail.
    end: u64,
}

pub struct Segment {
    pub start: u64,
    range: Mutex<Range>,
    /// Absolute offset up to which data has been handed to the OS. Trails `pos`.
    written: AtomicU64,
    owned: AtomicBool,
    /// Smoothed bytes/s of the connection working on this segment.
    speed: AtomicU64,
    claimed_at: Mutex<Instant>,
    /// Seconds this segment has been far slower than its peers.
    pub(crate) slow_secs: AtomicU32,
    /// Wakes the owner so it drops a connection the supervisor judged too slow.
    pub(crate) kick: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegRecord {
    pub start: u64,
    pub written: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SegSnapshot {
    pub start: u64,
    pub pos: u64,
    pub written: u64,
    pub end: u64,
    pub owned: bool,
    pub speed: u64,
}

impl Segment {
    fn new(start: u64, pos: u64, end: u64, owned: bool) -> Arc<Self> {
        Arc::new(Self {
            start,
            range: Mutex::new(Range { pos, end }),
            written: AtomicU64::new(pos),
            owned: AtomicBool::new(owned),
            speed: AtomicU64::new(0),
            claimed_at: Mutex::new(Instant::now()),
            slow_secs: AtomicU32::new(0),
            kick: Notify::new(),
        })
    }

    pub fn pos(&self) -> u64 {
        self.range.lock().pos
    }

    pub fn end(&self) -> u64 {
        self.range.lock().end
    }

    pub fn pos_end(&self) -> (u64, u64) {
        let r = self.range.lock();
        (r.pos, r.end)
    }

    pub fn remaining(&self) -> u64 {
        let r = self.range.lock();
        r.end.saturating_sub(r.pos)
    }

    /// Reserve up to `len` bytes at the current position. Returns how many of
    /// them still belong to this segment (fewer if the tail was stolen).
    pub fn advance(&self, len: usize) -> usize {
        let mut r = self.range.lock();
        let allowed = (r.end - r.pos).min(len as u64);
        r.pos += allowed;
        allowed as usize
    }

    /// The stream ended and the size was never announced: what we have is all there is.
    pub fn finish_at_pos(&self) {
        let mut r = self.range.lock();
        r.end = r.pos;
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    pub(crate) fn set_written(&self, offset: u64) {
        self.written.store(offset, Ordering::Release);
    }

    pub fn speed(&self) -> u64 {
        self.speed.load(Ordering::Relaxed)
    }

    pub(crate) fn set_speed(&self, speed: u64) {
        self.speed.store(speed, Ordering::Relaxed);
    }

    pub fn is_owned(&self) -> bool {
        self.owned.load(Ordering::Acquire)
    }

    pub(crate) fn owned_for(&self) -> std::time::Duration {
        self.claimed_at.lock().elapsed()
    }

    fn claim(&self) {
        self.owned.store(true, Ordering::Release);
        self.speed.store(0, Ordering::Relaxed);
        self.slow_secs.store(0, Ordering::Relaxed);
        *self.claimed_at.lock() = Instant::now();
    }

    /// Give the segment back (finished, failed or cancelled). Progress is kept.
    pub fn release(&self) {
        self.speed.store(0, Ordering::Relaxed);
        self.owned.store(false, Ordering::Release);
    }

    fn snapshot(&self) -> SegSnapshot {
        let (pos, end) = self.pos_end();
        SegSnapshot { start: self.start, pos, written: self.written(), end, owned: self.is_owned(), speed: self.speed() }
    }
}

pub struct SegmentTable {
    /// Sorted by `start`, contiguous, covering the whole file.
    segs: Mutex<Vec<Arc<Segment>>>,
}

impl SegmentTable {
    /// `n` equal parts, none smaller than `min_segment` (so small files get fewer).
    pub fn even(size: u64, n: u32, min_segment: u64) -> Self {
        let max_parts = (size / min_segment.max(1)).max(1);
        let n = (n.max(1) as u64).min(max_parts);
        let base = size / n;
        let mut segs = Vec::with_capacity(n as usize);
        let mut start = 0;
        for i in 0..n {
            let end = if i == n - 1 { size } else { start + base };
            segs.push(Segment::new(start, start, end, false));
            start = end;
        }
        Self { segs: Mutex::new(segs) }
    }

    /// One segment for servers without range support or without a known size.
    pub fn single(end: u64) -> Self {
        Self { segs: Mutex::new(vec![Segment::new(0, 0, end, false)]) }
    }

    pub fn from_records(records: &[SegRecord]) -> Self {
        let segs = records.iter().map(|r| Segment::new(r.start, r.written.clamp(r.start, r.end), r.end, false)).collect();
        Self { segs: Mutex::new(segs) }
    }

    /// Consistent view for the resume file: one lock, so no split can slip in between.
    pub fn records(&self) -> Vec<SegRecord> {
        let segs = self.segs.lock();
        segs.iter().map(|s| SegRecord { start: s.start, written: s.written().min(s.end()), end: s.end() }).collect()
    }

    pub fn snapshot(&self) -> Vec<SegSnapshot> {
        self.segs.lock().iter().map(|s| s.snapshot()).collect()
    }

    /// Bytes fetched so far (including bytes still on their way to disk).
    pub fn fetched(&self) -> u64 {
        self.segs.lock().iter().map(|s| s.pos() - s.start).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.segs.lock().iter().all(|s| {
            let (pos, end) = s.pos_end();
            pos >= end
        })
    }

    pub fn is_fully_written(&self) -> bool {
        self.segs.lock().iter().all(|s| s.written() >= s.end())
    }

    pub fn owned_count(&self) -> usize {
        self.segs.lock().iter().filter(|s| s.is_owned()).count()
    }

    /// The segment starting at byte 0, if nobody has it and nothing was fetched
    /// yet — the probe response can be streamed straight into it.
    pub fn claim_head(&self) -> Option<Arc<Segment>> {
        let segs = self.segs.lock();
        let head = segs.first()?;
        if head.start == 0 && !head.is_owned() && head.pos() == 0 && head.remaining() > 0 {
            head.claim();
            Some(head.clone())
        } else {
            None
        }
    }

    /// Work for an idle connection: an abandoned segment if there is one,
    /// otherwise the back half of whichever busy segment will take longest.
    pub fn acquire(&self, min_segment: u64, allow_split: bool) -> Option<Arc<Segment>> {
        let mut segs = self.segs.lock();

        if let Some(seg) = segs.iter().find(|s| !s.is_owned() && s.remaining() > 0) {
            seg.claim();
            return Some(seg.clone());
        }
        if !allow_split {
            return None;
        }

        // Connections that have not reported a speed yet are assumed average,
        // otherwise a freshly stolen segment would look infinitely slow and be
        // split again and again by every thief that arrives in the same instant.
        let known: Vec<u64> = segs.iter().filter(|s| s.is_owned()).map(|s| s.speed()).filter(|&v| v > 0).collect();
        let avg = if known.is_empty() { 1 } else { (known.iter().sum::<u64>() / known.len() as u64).max(1) };

        let mut best: Option<(usize, f64)> = None;
        for (i, seg) in segs.iter().enumerate() {
            if !seg.is_owned() {
                continue;
            }
            let (pos, end) = seg.pos_end();
            if end == UNKNOWN_END {
                continue;
            }
            let remaining = end.saturating_sub(pos);
            let speed = seg.speed();
            // Not worth a new connection if the owner is about to finish anyway.
            let floor = (2 * min_segment).max(speed + speed / 2);
            if remaining < floor {
                continue;
            }
            let eta = remaining as f64 / (if speed > 0 { speed } else { avg }) as f64;
            if best.is_none_or(|(_, b)| eta > b) {
                best = Some((i, eta));
            }
        }
        let (idx, _) = best?;

        let victim = segs[idx].clone();
        let stolen = {
            let mut r = victim.range.lock();
            // Re-check under the victim's lock: it kept downloading while we chose.
            let remaining = r.end.saturating_sub(r.pos);
            if remaining < 2 * min_segment {
                return None;
            }
            let mid = r.pos + remaining / 2;
            let old_end = r.end;
            r.end = mid;
            Segment::new(mid, mid, old_end, true)
        };
        segs.insert(idx + 1, stolen.clone());
        Some(stolen)
    }

    /// How far a single request starting in `seg` could usefully read: the end
    /// of the run of untouched, unclaimed segments directly behind it.
    pub fn chain_end(&self, seg: &Arc<Segment>) -> u64 {
        let segs = self.segs.lock();
        let mut end = seg.end();
        let Some(idx) = segs.iter().position(|s| Arc::ptr_eq(s, seg)) else { return end };
        for next in &segs[idx + 1..] {
            let (pos, next_end) = next.pos_end();
            if next.is_owned() || pos != next.start || next.start != end || next_end == UNKNOWN_END {
                break;
            }
            end = next_end;
        }
        end
    }

    /// `seg` is finished and its connection has more data coming (the response
    /// runs to `limit`). If the following segment is untouched and unclaimed,
    /// merge it into `seg` so the same connection keeps going — no new request,
    /// which matters when the server is refusing or rationing them.
    pub fn absorb_next(&self, seg: &Arc<Segment>, limit: u64) -> bool {
        let mut segs = self.segs.lock();
        let Some(idx) = segs.iter().position(|s| Arc::ptr_eq(s, seg)) else { return false };
        let Some(next) = segs.get(idx + 1).cloned() else { return false };
        let (next_pos, next_end) = next.pos_end();
        {
            let mut r = seg.range.lock();
            if r.pos != r.end || next.start != r.end || next.is_owned() || next_pos != next.start || next_end > limit {
                return false;
            }
            r.end = next_end;
        }
        segs.remove(idx + 1);
        true
    }

    /// Whether `acquire` would currently hand out anything.
    pub fn has_work(&self, min_segment: u64, allow_split: bool) -> bool {
        let segs = self.segs.lock();
        segs.iter().any(|s| {
            let (pos, end) = s.pos_end();
            let remaining = end.saturating_sub(pos);
            if !s.is_owned() {
                remaining > 0
            } else {
                let speed = s.speed();
                allow_split && end != UNKNOWN_END && remaining >= (2 * min_segment).max(speed + speed / 2)
            }
        })
    }

    pub(crate) fn owned_segments(&self) -> Vec<Arc<Segment>> {
        self.segs.lock().iter().filter(|s| s.is_owned()).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.segs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_contiguous(t: &SegmentTable, size: u64) {
        let snap = t.snapshot();
        let mut at = 0;
        for s in &snap {
            assert_eq!(s.start, at, "gap or overlap at {at}");
            assert!(s.pos >= s.start && s.pos <= s.end);
            at = s.end;
        }
        assert_eq!(at, size);
    }

    #[test]
    fn even_split_covers_file() {
        let t = SegmentTable::even(1_000_003, 8, 1000);
        assert_eq!(t.len(), 8);
        assert_contiguous(&t, 1_000_003);
        // Small file: fewer segments than requested.
        let t = SegmentTable::even(2500, 8, 1000);
        assert_eq!(t.len(), 2);
        assert_contiguous(&t, 2500);
        let t = SegmentTable::even(10, 8, 1000);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn steals_half_of_largest_remaining() {
        let t = SegmentTable::even(1000, 2, 10);
        let a = t.acquire(10, true).unwrap();
        let b = t.acquire(10, true).unwrap();
        assert_eq!((a.start, a.end()), (0, 500));
        assert_eq!((b.start, b.end()), (500, 1000));
        // `a` is nearly done, `b` untouched: the thief must take from `b`.
        assert_eq!(a.advance(450), 450);
        let c = t.acquire(10, true).unwrap();
        assert_eq!((c.start, c.end()), (750, 1000));
        assert_eq!(b.end(), 750);
        assert_contiguous(&t, 1000);
    }

    #[test]
    fn victim_cannot_cross_stolen_boundary() {
        let t = SegmentTable::even(1000, 1, 10);
        let a = t.acquire(10, true).unwrap();
        a.advance(100);
        let b = t.acquire(10, true).unwrap();
        assert_eq!(b.start, 550);
        // The victim receives a 600-byte chunk but only 450 bytes are still its own.
        assert_eq!(a.advance(600), 450);
        assert_eq!(a.advance(10), 0);
        assert_contiguous(&t, 1000);
    }

    #[test]
    fn refuses_tiny_splits_and_prefers_abandoned() {
        let t = SegmentTable::even(100, 1, 40);
        let a = t.acquire(40, true).unwrap();
        a.advance(30); // 70 left < 2 * 40
        assert!(t.acquire(40, true).is_none());
        assert!(!t.has_work(40, true));
        a.release();
        assert!(t.has_work(40, true));
        let again = t.acquire(40, true).unwrap();
        assert_eq!(again.pos(), 30);
    }

    #[test]
    fn slow_connection_is_the_victim() {
        let t = SegmentTable::even(2000, 2, 10);
        let fast = t.acquire(10, true).unwrap();
        let slow = t.acquire(10, true).unwrap();
        fast.set_speed(1000);
        slow.set_speed(10);
        // The slow one has less left in bytes but far more in time.
        slow.advance(400);
        let c = t.acquire(10, true).unwrap();
        assert_eq!(c.end(), 2000);
        assert_eq!(slow.end(), c.start);
        assert_eq!(fast.end(), 1000);
    }

    #[test]
    fn absorbs_untouched_neighbours_only() {
        let t = SegmentTable::even(400, 4, 10);
        let a = t.acquire(10, false).unwrap(); // [0,100)
        assert_eq!(t.chain_end(&a), 400);

        // Not finished yet: nothing to absorb.
        assert!(!t.absorb_next(&a, 400));
        a.advance(100);
        // The response only covers up to 150: the neighbour [100,200) does not fit.
        assert!(!t.absorb_next(&a, 150));
        assert!(t.absorb_next(&a, 400));
        assert_eq!((a.pos(), a.end()), (100, 200));
        assert_eq!(t.len(), 3);
        assert_contiguous(&t, 400);

        // A claimed neighbour is off limits, and so is one that already has data.
        a.advance(100);
        let c = t.acquire(10, false).unwrap(); // [200,300)
        assert_eq!(c.start, 200);
        assert!(!t.absorb_next(&a, 400));
        assert_eq!(t.chain_end(&a), 200);
        c.advance(100);
        c.release();
        let d = t.acquire(10, false).unwrap(); // [300,400)
        d.advance(5);
        d.release();
        assert_eq!(t.chain_end(&c), 300);
        assert!(!t.absorb_next(&c, 400));
        assert_contiguous(&t, 400);
    }

    #[test]
    fn records_roundtrip() {
        let t = SegmentTable::even(1000, 4, 10);
        let a = t.acquire(10, true).unwrap();
        a.advance(100);
        a.set_written(60);
        let rec = t.records();
        assert_eq!(rec[0], SegRecord { start: 0, written: 60, end: 250 });
        let t2 = SegmentTable::from_records(&rec);
        assert_eq!(t2.fetched(), 60);
        assert_contiguous(&t2, 1000);
    }
}
