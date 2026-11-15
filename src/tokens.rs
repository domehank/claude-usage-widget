//! Token totals for the tooltip, counted from Claude Code's local session logs
//! (~/.claude/projects/**/*.jsonl), where every reply records the tokens it used.
//!
//! Counted: input + output + cache-write tokens. Cache reads are left out: each reply re-reads
//! the whole conversation from cache, so they would dwarf everything else.
//! Only this PC's Claude Code sessions are counted, not claude.ai or other computers.
//!
//! The logs are read incrementally: each call only reads what was appended since the last one.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

#[derive(Default)]
pub struct Counter {
    read: HashMap<PathBuf, u64>, // bytes already read per log file
    seen: HashSet<String>,       // message id + request id: a reply can be logged more than once
    entries: Vec<(i64, u64)>,    // (unix time, tokens) per reply
}

impl Counter {
    /// Reads new log lines; keeps only replies from `since` (unix time) on.
    pub fn update(&mut self, projects: &Path, since: i64) {
        let mut dirs = vec![projects.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            let Ok(list) = std::fs::read_dir(&dir) else { continue };
            for e in list.flatten() {
                let path = e.path();
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                let modified = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
                if modified.is_none_or(|d| (d.as_secs() as i64) < since) {
                    continue; // not written to during the period
                }
                let done = self.read.get(&path).copied().unwrap_or(0);
                let start = if md.len() < done { 0 } else { done }; // rewritten: start over
                if md.len() == start {
                    continue;
                }
                let used = self.read_from(&path, start, since);
                self.read.insert(path, start + used);
            }
        }
        self.entries.retain(|&(t, _)| t >= since);
    }

    /// Total tokens of the replies from `since` on.
    pub fn total(&self, since: i64) -> u64 {
        self.entries.iter().filter(|&&(t, _)| t >= since).map(|&(_, n)| n).sum()
    }

    /// Parses complete lines from byte `start`; returns how many bytes were consumed.
    fn read_from(&mut self, path: &Path, start: u64, since: i64) -> u64 {
        let mut buf = Vec::new();
        let ok = std::fs::File::open(path)
            .and_then(|mut f| f.seek(SeekFrom::Start(start)).and_then(|_| f.read_to_end(&mut buf)))
            .is_ok();
        // Stop at the last newline: the final line may still be being written
        let Some(end) = buf.iter().rposition(|&b| b == b'\n').filter(|_| ok) else { return 0 };
        for line in buf[..end].split(|&b| b == b'\n') {
            let Ok(line) = std::str::from_utf8(line) else { continue };
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let Some(usage) = v.pointer("/message/usage") else { continue };
            let Some(t) = v.get("timestamp").and_then(Value::as_str).and_then(parse_time) else { continue };
            if t < since {
                continue;
            }
            let id = format!(
                "{}:{}",
                v.pointer("/message/id").and_then(Value::as_str).unwrap_or(""),
                v.get("requestId").and_then(Value::as_str).unwrap_or("")
            );
            if id != ":" && !self.seen.insert(id) {
                continue;
            }
            let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
            self.entries.push((t, n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens")));
        }
        end as u64 + 1
    }
}

/// "2026-11-15T11:21:36.141Z" (UTC) -> unix seconds
fn parse_time(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days since 1970-01-01 (Howard Hinnant's days_from_civil)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// 950, 15.7k, 113k, 3.4M
pub fn format(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..100_000 => format!("{:.1}k", n as f64 / 1e3),
        100_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn time_and_format() {
        assert_eq!(super::parse_time("1970-01-02T00:00:01.000Z"), Some(86_401));
        assert_eq!(super::parse_time("2026-11-15T11:21:36.141Z"), Some(1_794_741_696));
        assert_eq!(super::format(15_740), "15.7k");
        assert_eq!(super::format(113_000), "113k");
        assert_eq!(super::format(3_436_903), "3.4M");
    }
}
