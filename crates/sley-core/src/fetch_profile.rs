//! Low-overhead, feature-gated timing support for the fetch/index-pack profiler.
//!
//! Timers report *exclusive* wall time. When one profiled stage calls another
//! (inflate -> sideband demux -> socket read, for example), the parent is
//! paused until the child completes. This keeps the stage totals additive.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const STAGE_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    SocketRead = 0,
    PktLineSideband = 1,
    Inflate = 2,
    DeltaResolution = 3,
    OidHash = 4,
    ObjectStoreWrite = 5,
}

impl Stage {
    pub const ALL: [Self; STAGE_COUNT] = [
        Self::SocketRead,
        Self::PktLineSideband,
        Self::Inflate,
        Self::DeltaResolution,
        Self::OidHash,
        Self::ObjectStoreWrite,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::SocketRead => "socket read",
            Self::PktLineSideband => "pkt-line / sideband-64k demux",
            Self::Inflate => "inflate (zlib-rs)",
            Self::DeltaResolution => "delta resolution",
            Self::OidHash => "OID / SHA hashing",
            Self::ObjectStoreWrite => "object-store write",
        }
    }
}

static NANOS: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];
static COUNTS: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];
static BYTES: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];
static FSYNCS: AtomicU64 = AtomicU64::new(0);

struct ActiveStage {
    stage: Stage,
    resumed_at: Instant,
    exclusive: Duration,
}

thread_local! {
    static ACTIVE: RefCell<Vec<ActiveStage>> = const { RefCell::new(Vec::new()) };
}

#[must_use]
pub struct Span {
    stage: Stage,
}

impl Span {
    pub fn enter(stage: Stage) -> Self {
        let now = Instant::now();
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(parent) = active.last_mut() {
                parent.exclusive += now.saturating_duration_since(parent.resumed_at);
            }
            active.push(ActiveStage {
                stage,
                resumed_at: now,
                exclusive: Duration::ZERO,
            });
        });
        Self { stage }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let now = Instant::now();
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            let Some(mut completed) = active.pop() else {
                return;
            };
            completed.exclusive += now.saturating_duration_since(completed.resumed_at);
            if completed.stage == self.stage {
                add_duration(completed.stage, completed.exclusive);
            }
            if let Some(parent) = active.last_mut() {
                parent.resumed_at = now;
            }
        });
    }
}

fn add_duration(stage: Stage, duration: Duration) {
    let nanos = duration.as_nanos().min(u128::from(u64::MAX)) as u64;
    NANOS[stage as usize].fetch_add(nanos, Ordering::Relaxed);
}

pub fn add_count(stage: Stage, count: u64) {
    COUNTS[stage as usize].fetch_add(count, Ordering::Relaxed);
}

pub fn add_bytes(stage: Stage, bytes: u64) {
    BYTES[stage as usize].fetch_add(bytes, Ordering::Relaxed);
}

pub fn add_fsync() {
    FSYNCS.fetch_add(1, Ordering::Relaxed);
}

pub fn reset() {
    for stage in Stage::ALL {
        NANOS[stage as usize].store(0, Ordering::Relaxed);
        COUNTS[stage as usize].store(0, Ordering::Relaxed);
        BYTES[stage as usize].store(0, Ordering::Relaxed);
    }
    FSYNCS.store(0, Ordering::Relaxed);
    ACTIVE.with(|active| active.borrow_mut().clear());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSnapshot {
    pub stage: Stage,
    pub duration: Duration,
    pub count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub stages: Vec<StageSnapshot>,
    pub fsyncs: u64,
}

pub fn snapshot() -> Snapshot {
    let stages = Stage::ALL
        .into_iter()
        .map(|stage| StageSnapshot {
            stage,
            duration: Duration::from_nanos(NANOS[stage as usize].load(Ordering::Relaxed)),
            count: COUNTS[stage as usize].load(Ordering::Relaxed),
            bytes: BYTES[stage as usize].load(Ordering::Relaxed),
        })
        .collect();
    Snapshot {
        stages,
        fsyncs: FSYNCS.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_spans_are_exclusive() {
        reset();
        {
            let _outer = Span::enter(Stage::Inflate);
            std::thread::sleep(Duration::from_millis(2));
            {
                let _inner = Span::enter(Stage::SocketRead);
                std::thread::sleep(Duration::from_millis(4));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let measured = snapshot();
        let inflate = measured.stages[Stage::Inflate as usize].duration;
        let socket = measured.stages[Stage::SocketRead as usize].duration;
        assert!(inflate >= Duration::from_millis(3));
        assert!(inflate < socket + Duration::from_millis(3));
        assert!(socket >= Duration::from_millis(3));
    }
}
