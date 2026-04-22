//! Monotonic timestamps anchored at agent start.
//!
//! We use QueryPerformanceCounter because wall-clock time can drift during
//! a long meeting (NTP nudges, timezone/DST) and we need all three capture
//! pipelines to land on a single consistent timeline.

use once_cell::sync::OnceCell;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

static QPC_FREQ: OnceCell<i64> = OnceCell::new();
static QPC_START: OnceCell<i64> = OnceCell::new();

pub fn init() {
    let mut f: i64 = 0;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    let _ = QPC_FREQ.set(f);

    let mut c: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    let _ = QPC_START.set(c);
}

fn qpc() -> i64 {
    let mut c: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    c
}

/// Milliseconds since `init()` was called.
pub fn now_ms() -> u64 {
    let freq = *QPC_FREQ.get().unwrap_or(&10_000_000);
    let start = *QPC_START.get().unwrap_or(&0);
    let delta = qpc() - start;
    if freq <= 0 {
        return 0;
    }
    ((delta as i128 * 1000) / freq as i128).max(0) as u64
}

/// Current local wall-clock with timezone offset attached.
///
/// We pick local over UTC for the `wall` field of timeline events because
/// it's the time the user actually sees on their PC and on the Teams meeting
/// invite. The offset (e.g. `+09:00`) is preserved in the rfc3339 output so
/// the timestamp stays unambiguous even when files are shared across zones.
pub fn now_local() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}

/// Microseconds since `init()` — used for sub-second audio timestamps.
#[allow(dead_code)]
pub fn now_us() -> u64 {
    let freq = *QPC_FREQ.get().unwrap_or(&10_000_000);
    let start = *QPC_START.get().unwrap_or(&0);
    let delta = qpc() - start;
    if freq <= 0 {
        return 0;
    }
    ((delta as i128 * 1_000_000) / freq as i128).max(0) as u64
}
