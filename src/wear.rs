//! Days to wear-out (#23): a straight line through the drive's wear trend.
//!
//! The monitor records `wear_pct` (NVMe Percentage Used, the SSD's own
//! estimate of its rated endurance consumed) whenever it changes and at
//! least daily (`inventory.trends`). This fits a least-squares line through
//! the last year of those samples and says when it reaches 100 %. A drive
//! keeps working past 100 %, but that is the vendor's rated life — the
//! point to have a replacement on the shelf.
//!
//! No projection without enough to go on: fewer than two distinct wear
//! values, less than a week of samples, or wear that is not growing.

use serde::{Deserialize, Serialize};

use crate::inventory::TrendSample;

const DAY: f64 = 86_400.0;
/// Samples older than this do not steer the line: a drive's write load
/// changes with what it holds.
pub const WINDOW_SECS: u64 = 365 * 86_400;
/// Too little to fit a line through.
pub const MIN_SPAN_SECS: u64 = 7 * 86_400;
/// A projection past this is "not in the drive's service life".
pub const MAX_DAYS: u64 = 100 * 365;
/// Under this many days the drive needs a replacement planned: the
/// `monitor.wear_out_warn_days` default, and the feed's warn tone.
pub const WARN_DAYS: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WearProjection {
    /// Percentage points of rated endurance used per day, by the fit.
    pub rate_pct_per_day: f64,
    /// From `computed_unix` to 100 % (0 when already there).
    pub days_left: u64,
    pub wear_out_unix: u64,
    pub computed_unix: u64,
    /// What the line was fitted to.
    pub samples: usize,
    pub span_days: u64,
}

/// Fit the samples within [`WINDOW_SECS`] of `now`. None when there is not
/// enough to go on, or the wear is not growing.
pub fn project(trend: &[TrendSample], now: u64) -> Option<WearProjection> {
    let since = now.saturating_sub(WINDOW_SECS);
    let pts: Vec<(f64, f64)> = trend
        .iter()
        .filter(|s| s.unix_secs >= since && s.unix_secs <= now)
        .filter_map(|s| s.wear_pct.map(|w| (s.unix_secs as f64, f64::from(w))))
        .collect();
    let first = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let last = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let mut distinct: Vec<i64> = pts.iter().map(|p| p.1 as i64).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if pts.len() < 2 || distinct.len() < 2 || last - first < MIN_SPAN_SECS as f64 {
        return None;
    }
    // Least squares on time relative to the first sample (keeps the sums
    // small).
    let n = pts.len() as f64;
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |(a, b), (t, w)| (a + (t - first), b + w));
    let (mx, my) = (sx / n, sy / n);
    let (sxy, sxx) = pts.iter().fold((0.0, 0.0), |(a, b), (t, w)| {
        let dx = t - first - mx;
        (a + dx * (w - my), b + dx * dx)
    });
    if sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx; // pct per second
    if slope <= 0.0 {
        return None;
    }
    let now_rel = now as f64 - first;
    let fitted_now = my + slope * (now_rel - mx);
    let remaining = 100.0 - fitted_now;
    let days_left = if remaining <= 0.0 { 0 } else { ((remaining / slope) / DAY + 1e-6).floor().min(MAX_DAYS as f64) as u64 };
    Some(WearProjection {
        rate_pct_per_day: slope * DAY,
        days_left,
        wear_out_unix: now + days_left * 86_400,
        computed_unix: now,
        samples: pts.len(),
        span_days: ((last - first) / DAY) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: u64 = 86_400;
    const T0: u64 = 1_790_000_000;

    fn s(day: u64, wear: Option<u8>) -> TrendSample {
        TrendSample { unix_secs: T0 + day * D, wear_pct: wear, media_errors: 0 }
    }

    #[test]
    fn a_steady_point_a_month_reaches_100_where_the_line_says() {
        // 10 % → 13 % over 90 days: 1 point every 30 days.
        let t = [s(0, Some(10)), s(30, Some(11)), s(60, Some(12)), s(90, Some(13))];
        let p = project(&t, T0 + 90 * D).unwrap();
        assert!((p.rate_pct_per_day - 1.0 / 30.0).abs() < 1e-9, "{p:?}");
        assert_eq!(p.days_left, 87 * 30, "87 points left at 1 per 30 days");
        assert_eq!(p.wear_out_unix, T0 + 90 * D + p.days_left * D);
        assert_eq!((p.samples, p.span_days), (4, 90));
    }

    #[test]
    fn not_enough_to_go_on_is_no_projection() {
        assert_eq!(project(&[], T0), None);
        assert_eq!(project(&[s(0, Some(5)), s(30, Some(5)), s(60, Some(5))], T0 + 60 * D), None, "one distinct value: not wearing");
        assert_eq!(project(&[s(0, Some(5)), s(3, Some(6))], T0 + 3 * D), None, "under a week");
        assert_eq!(project(&[s(0, None), s(30, None)], T0 + 30 * D), None, "no wear reported (an HDD)");
        assert_eq!(project(&[s(0, Some(9)), s(30, Some(8))], T0 + 30 * D), None, "going down is no wear-out");
    }

    #[test]
    fn worn_out_already_is_zero_days_and_slow_wear_is_capped() {
        let t = [s(0, Some(98)), s(10, Some(100)), s(20, Some(102))];
        assert_eq!(project(&t, T0 + 20 * D).unwrap().days_left, 0);
        // Flapping between 1 and 2 all year: a near-flat line, capped.
        let flat = [s(0, Some(1)), s(7, Some(2)), s(357, Some(1)), s(364, Some(2))];
        assert_eq!(project(&flat, T0 + 364 * D).unwrap().days_left, MAX_DAYS);
    }

    #[test]
    fn only_the_last_year_steers_the_line() {
        // A burst two years ago (0 → 50 in a month) then a quiet year (50 → 52).
        let mut t = vec![s(0, Some(0)), s(30, Some(50))];
        t.extend((0..=12).map(|m| s(365 + 30 + m * 30, Some(50 + (m / 6) as u8))));
        let now = T0 + (365 + 30 + 360) * D;
        let p = project(&t, now).unwrap();
        assert!(p.rate_pct_per_day < 0.01, "the old burst is out of the window: {p:?}");
        assert!(p.days_left > 3000, "{p:?}");
    }
}
