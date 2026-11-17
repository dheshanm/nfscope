//! Number formatting helpers.

use humansize::{BINARY, FormatSizeOptions, format_size};

use crate::model::Point;

fn opts() -> FormatSizeOptions {
    FormatSizeOptions::from(BINARY).decimal_places(1).decimal_zeroes(1)
}

pub fn bytes(n: u64) -> String {
    format_size(n, opts())
}

pub fn rate(bps: f64) -> String {
    format!("{}/s", bytes(bps.max(0.0).round() as u64))
}

/// Compact duration: `45s`, `2m30s`, `1h05m`.
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 if s.is_multiple_of(60) => format!("{}m", s / 60),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Compact count: `950`, `12.3k`, `1.2M`.
pub fn count(n: f64) -> String {
    let n = n.max(0.0);
    match n {
        n if n < 999.5 => format!("{}", n.round() as u64),
        n if n < 999_950.0 => format!("{:.1}k", n / 1e3),
        n => format!("{:.1}M", n / 1e6),
    }
}

/// Percentage with one decimal below 10%: `0.4%`, `2.3%`, `45%`.
pub fn percent(p: f64) -> String {
    let p = p.clamp(0.0, 100.0);
    if p < 9.95 { format!("{p:.1}%") } else { format!("{}%", p.round() as u64) }
}

const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Tiny text sparkline of total throughput for the last `n` points.
/// Scaled to the window's max, with a floor so idle noise stays flat.
pub fn trend(h: &std::collections::VecDeque<Point>, n: usize) -> String {
    let start = h.len().saturating_sub(n);
    let pts: Vec<f64> = h.iter().skip(start).map(Point::total).collect();
    let max = pts.iter().copied().fold(64.0 * 1024.0, f64::max);
    let mut s: String = std::iter::repeat_n(' ', n - pts.len()).collect();
    for v in pts {
        let idx = if v <= 0.0 { 0 } else { ((v / max) * 8.0).ceil().clamp(1.0, 8.0) as usize };
        s.push(BARS[idx]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(rate(0.0), "0.0 B/s");
        assert_eq!(rate(111.1 * 1048576.0), "111.1 MiB/s");
        assert_eq!(bytes(458_900_000_000), "427.4 GiB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(950.2), "950");
        assert_eq!(count(999.7), "1.0k");
        assert_eq!(count(12_345.0), "12.3k");
        assert_eq!(count(1_234_567.0), "1.2M");
    }

    #[test]
    fn percents() {
        assert_eq!(percent(2.31), "2.3%");
        assert_eq!(percent(45.4), "45%");
        assert_eq!(percent(120.0), "100%");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(4.6), "5s");
        assert_eq!(duration(120.0), "2m");
        assert_eq!(duration(150.0), "2m30s");
        assert_eq!(duration(3900.0), "1h05m");
    }

    #[test]
    fn trend_line() {
        let h: std::collections::VecDeque<Point> =
            [0.0, 1e6, 2e6, 4e6].iter().map(|&r| Point { t: 0.0, read: r, write: 0.0 }).collect();
        assert_eq!(trend(&h, 6), "   ▂▄█");
    }
}
