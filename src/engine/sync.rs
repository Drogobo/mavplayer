//! Queues and clocks shared between the engine threads.
//!
//! The design follows ffplay: every flush (seek) bumps a queue's *serial*.
//! Packets, frames and clocks carry the serial they were produced under, so
//! stale data from before a seek is recognised and discarded downstream.

use super::av::{Frame, Packet};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// Monotonic time in seconds.
pub fn now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub struct QueuedPacket {
    pub pkt: Packet,
    pub serial: i32,
}

struct PqInner {
    pkts: VecDeque<QueuedPacket>,
    bytes: usize,
    /// Sum of packet durations in stream time base units.
    duration: i64,
}

pub struct PacketQueue {
    inner: Mutex<PqInner>,
    cv: Condvar,
    serial: AtomicI32,
    abort: AtomicBool,
    /// Signalled when a consumer finds the queue empty (wakes the demuxer).
    pub empty_cv: Condvar,
}

impl PacketQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PqInner { pkts: VecDeque::new(), bytes: 0, duration: 0 }),
            cv: Condvar::new(),
            serial: AtomicI32::new(0),
            abort: AtomicBool::new(false),
            empty_cv: Condvar::new(),
        }
    }

    pub fn serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }

    pub fn put(&self, pkt: Packet) {
        if self.abort.load(Ordering::Acquire) {
            return;
        }
        let mut g = self.inner.lock();
        g.bytes += pkt.size() + std::mem::size_of::<QueuedPacket>();
        g.duration += pkt.duration().max(0);
        g.pkts.push_back(QueuedPacket { pkt, serial: self.serial() });
        drop(g);
        self.cv.notify_one();
    }

    /// Puts an empty packet, which makes the decoder drain.
    pub fn put_drain(&self) {
        self.put(Packet::new());
    }

    /// Discards everything and starts a new serial.
    pub fn flush(&self) {
        let mut g = self.inner.lock();
        g.pkts.clear();
        g.bytes = 0;
        g.duration = 0;
        self.serial.fetch_add(1, Ordering::AcqRel);
        drop(g);
        self.cv.notify_all();
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        let _g = self.inner.lock();
        self.cv.notify_all();
        self.empty_cv.notify_all();
    }

    pub fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    /// Blocking get. Returns `None` once the queue is aborted or `cancel` is set.
    pub fn get(&self, cancel: &AtomicBool) -> Option<QueuedPacket> {
        self.get_until(cancel, None)
    }

    /// Like `get`, but gives up (`None`) at `deadline`.
    pub fn get_until(&self, cancel: &AtomicBool, deadline: Option<std::time::Instant>) -> Option<QueuedPacket> {
        let mut g = self.inner.lock();
        loop {
            if self.is_aborted() || cancel.load(Ordering::Acquire) {
                return None;
            }
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return None;
            }
            if let Some(p) = g.pkts.pop_front() {
                g.bytes = g.bytes.saturating_sub(p.pkt.size() + std::mem::size_of::<QueuedPacket>());
                g.duration -= p.pkt.duration().max(0);
                return Some(p);
            }
            self.empty_cv.notify_all();
            self.cv.wait_for(&mut g, Duration::from_millis(100));
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().pkts.len()
    }

    pub fn bytes(&self) -> usize {
        self.inner.lock().bytes
    }

    pub fn duration_ts(&self) -> i64 {
        self.inner.lock().duration
    }
}

/// A decoded (and filtered) video frame waiting to be shown.
pub struct VideoFrame {
    pub frame: Frame,
    pub pts: f64,
    pub duration: f64,
    pub serial: i32,
    /// Sample aspect ratio (pixel width / pixel height).
    pub sar: f64,
    /// Set when the picture was resized on the GPU: the size and pixel
    /// aspect it was decoded at (what bitmap subtitles are positioned on,
    /// and what a picture for another window size is resized from).
    pub source: Option<(i32, i32, f64)>,
}

pub struct FrameQueue {
    inner: Mutex<VecDeque<VideoFrame>>,
    cv: Condvar,
    cap: usize,
    abort: AtomicBool,
}

impl FrameQueue {
    pub fn new(cap: usize) -> Self {
        Self { inner: Mutex::new(VecDeque::new()), cv: Condvar::new(), cap, abort: AtomicBool::new(false) }
    }

    /// Blocks while the queue is full. Returns false if aborted, or if
    /// `stale()` reports the frame is no longer wanted (seek happened).
    pub fn push(&self, f: VideoFrame, stale: impl Fn(i32) -> bool) -> bool {
        let mut g = self.inner.lock();
        while g.len() >= self.cap {
            if self.abort.load(Ordering::Acquire) || stale(f.serial) {
                return false;
            }
            self.cv.wait_for(&mut g, Duration::from_millis(20));
        }
        if self.abort.load(Ordering::Acquire) {
            return false;
        }
        g.push_back(f);
        drop(g);
        self.cv.notify_all();
        true
    }

    pub fn with_front<R>(&self, f: impl FnOnce(Option<&VideoFrame>, Option<&VideoFrame>) -> R) -> R {
        let g = self.inner.lock();
        f(g.front(), g.get(1))
    }

    pub fn pop(&self) -> Option<VideoFrame> {
        let mut g = self.inner.lock();
        let f = g.pop_front();
        drop(g);
        self.cv.notify_all();
        f
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Waits up to `timeout` for a frame to arrive.
    pub fn wait(&self, timeout: Duration) {
        let mut g = self.inner.lock();
        if g.is_empty() && !self.abort.load(Ordering::Acquire) {
            self.cv.wait_for(&mut g, timeout);
        }
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
        self.cv.notify_all();
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        let _g = self.inner.lock();
        self.cv.notify_all();
    }
}

#[derive(Clone, Copy)]
struct ClockInner {
    pts: f64,
    pts_drift: f64,
    last_updated: f64,
    speed: f64,
    serial: i32,
    paused: bool,
}

/// A media clock that advances in real time (scaled by `speed`) between updates.
pub struct Clock {
    inner: Mutex<ClockInner>,
}

impl Clock {
    pub fn new() -> Self {
        let t = now();
        Self {
            inner: Mutex::new(ClockInner {
                pts: f64::NAN,
                pts_drift: f64::NAN,
                last_updated: t,
                speed: 1.0,
                serial: -1,
                paused: false,
            }),
        }
    }

    /// Current value, or NaN if the clock belongs to an obsolete serial.
    pub fn get(&self, queue_serial: i32) -> f64 {
        let c = *self.inner.lock();
        if c.serial != queue_serial {
            return f64::NAN;
        }
        if c.paused {
            c.pts
        } else {
            let t = now();
            c.pts_drift + t - (t - c.last_updated) * (1.0 - c.speed)
        }
    }

    /// Current value regardless of serial.
    pub fn get_any(&self) -> f64 {
        let s = self.inner.lock().serial;
        self.get(s)
    }

    pub fn serial(&self) -> i32 {
        self.inner.lock().serial
    }

    pub fn set_at(&self, pts: f64, serial: i32, time: f64) {
        let mut c = self.inner.lock();
        c.pts = pts;
        c.last_updated = time;
        c.pts_drift = pts - time;
        c.serial = serial;
    }

    pub fn set(&self, pts: f64, serial: i32) {
        self.set_at(pts, serial, now());
    }

    pub fn set_speed(&self, speed: f64) {
        let v = self.get_any();
        let s = self.serial();
        if v.is_finite() {
            self.set(v, s);
        }
        self.inner.lock().speed = speed;
    }

    pub fn set_paused(&self, paused: bool) {
        let v = self.get_any();
        let s = self.serial();
        let mut c = self.inner.lock();
        if c.paused == paused {
            return;
        }
        c.paused = paused;
        drop(c);
        if v.is_finite() {
            self.set(v, s);
        } else {
            self.inner.lock().last_updated = now();
        }
    }
}
