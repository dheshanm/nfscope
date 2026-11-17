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

/// How long a request has been running: `850µs`, `4.2 ms`, `120 ms`, `1.24 s`, `2m30s`.
pub fn latency(us: u64) -> String {
    match us {
        0..1_000 => format!("{us}µs"),
        1_000..9_950 => format!("{:.1} ms", us as f64 / 1e3),
        9_950..999_500 => format!("{} ms", (us as f64 / 1e3).round() as u64),
        999_500..59_995_000 => format!("{:.2} s", us as f64 / 1e6),
        _ => duration(us as f64 / 1e6),
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

/// Average count: one decimal below 10 (`0.4`, `2.5`), whole numbers above (`14`).
pub fn avg(n: f64) -> String {
    let n = n.max(0.0);
    if n < 9.95 { format!("{n:.1}") } else { format!("{}", n.round() as u64) }
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
    fn averages() {
        assert_eq!(avg(0.44), "0.4");
        assert_eq!(avg(14.2), "14");
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
    fn latencies() {
        assert_eq!(latency(850), "850µs");
        assert_eq!(latency(4_240), "4.2 ms");
        assert_eq!(latency(119_600), "120 ms");
        assert_eq!(latency(999_600), "1.00 s");
        assert_eq!(latency(1_244_000), "1.24 s");
        assert_eq!(latency(150_000_000), "2m30s");
    }

    #[test]
    fn trend_line() {
        let h: std::collections::VecDeque<Point> =
            [0.0, 1e6, 2e6, 4e6].iter().map(|&r| Point { t: 0.0, read: r, write: 0.0 }).collect();
        assert_eq!(trend(&h, 6), "   ▂▄█");
    }
}
