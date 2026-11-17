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
