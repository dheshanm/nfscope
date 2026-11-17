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
    fn trend_line() {
        let h: std::collections::VecDeque<Point> =
            [0.0, 1e6, 2e6, 4e6].iter().map(|&r| Point { t: 0.0, read: r, write: 0.0 }).collect();
        assert_eq!(trend(&h, 6), "   ▂▄█");
    }
}
