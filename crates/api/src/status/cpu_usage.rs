//! CPU usage for the dashboard's System tile, from `/proc/stat`.
//!
//! Usage is the share of CPU time spent busy across all cores between two
//! readings of the aggregate `cpu` line. Time waiting on I/O (`iowait`) counts
//! as idle: during an archive the Pi waits on the SSD and network a lot, and
//! counting that as load would make the CPU look pegged when it isn't.
//!
//! Each status request compares against the previous reading, so the figure
//! covers the time since the dashboard last polled. Requests less than a
//! second apart (several open dashboards) reuse the last figure instead of
//! measuring a near-zero interval. When there is no recent reading (the first
//! request, or none for 30 s) the request takes two readings 250 ms apart, so
//! it never shows usage averaged over minutes of idle history.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(1);
const STALE_AFTER: Duration = Duration::from_secs(30);
const FRESH_WINDOW: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Times {
    busy: u64,
    total: u64,
}

#[derive(Clone, Copy)]
struct State {
    times: Times,
    taken_at: Instant,
    percent: Option<f64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Busy and total jiffies from the aggregate `cpu` line:
/// `cpu user nice system idle iowait irq softirq steal guest guest_nice`.
/// Guest time is already included in user/nice, so it isn't added again.
fn parse(stat: &str) -> Option<Times> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let f: Vec<u64> = line.split_whitespace().skip(1).take(8).map(|v| v.parse().ok()).collect::<Option<_>>()?;
    if f.len() < 4 {
        return None;
    }
    let field = |i: usize| f.get(i).copied().unwrap_or(0);
    let idle = field(3) + field(4);
    let total: u64 = f.iter().sum();
    Some(Times { busy: total.saturating_sub(idle), total })
}

/// Percentage of `prev`→`cur` spent busy, or `None` when no time passed.
fn percent(prev: Times, cur: Times) -> Option<f64> {
    let total = cur.total.checked_sub(prev.total)?;
    if total == 0 {
        return None;
    }
    let busy = cur.busy.saturating_sub(prev.busy).min(total);
    Some((busy as f64 * 1000.0 / total as f64).round() / 10.0)
}

fn read() -> Option<Times> {
    parse(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// Current CPU usage in percent (0–100, one decimal), or `None` when
/// `/proc/stat` can't be read.
pub async fn get() -> Option<f64> {
    let now = Instant::now();
    let prev = *STATE.lock().ok()?;
    let base = match prev {
        Some(s) if now.duration_since(s.taken_at) < MIN_INTERVAL => return s.percent,
        Some(s) if now.duration_since(s.taken_at) < STALE_AFTER => s.times,
        _ => {
            let first = read()?;
            tokio::time::sleep(FRESH_WINDOW).await;
            first
        }
    };
    let cur = read()?;
    let pct = percent(base, cur);
    if let Ok(mut state) = STATE.lock() {
        *state = Some(State { times: cur, taken_at: Instant::now(), percent: pct });
    }
    pct
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 25 0 12 200 12 0 0 0 0 0\nintr 1 2 3\n";

    #[test]
    fn parses_the_aggregate_line_with_iowait_as_idle() {
        assert_eq!(parse(STAT), Some(Times { busy: 150, total: 1000 }));
        assert_eq!(parse("cpu0 1 2 3 4\n"), None, "per-core lines alone aren't the aggregate");
        assert_eq!(parse("cpu  1 2\n"), None);
        assert_eq!(parse("garbage"), None);
    }

    #[test]
    fn percent_is_the_busy_share_of_elapsed_time() {
        let a = Times { busy: 150, total: 1000 };
        assert_eq!(percent(a, Times { busy: 175, total: 1100 }), Some(25.0));
        assert_eq!(percent(a, Times { busy: 250, total: 1100 }), Some(100.0));
        assert_eq!(percent(a, Times { busy: 150, total: 1300 }), Some(0.0));
        assert_eq!(percent(a, Times { busy: 183, total: 1300 }), Some(11.0));
        assert_eq!(percent(a, a), None, "no time passed");
        assert_eq!(percent(Times { busy: 10, total: 2000 }, a), None, "counter went backwards");
    }
}
