// Standalone tool - compile with: rustc -O span-stats.rs
// Usage: cat server.log | ./span-stats

use std::collections::HashMap;
use std::io::{self, BufRead};

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_time_us(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(val) = s.strip_suffix("µs").or_else(|| s.strip_suffix("us")) {
        val.parse().ok()
    } else if let Some(val) = s.strip_suffix("ms") {
        val.parse::<f64>().ok().map(|v| v * 1000.0)
    } else if let Some(val) = s.strip_suffix('s') {
        val.parse::<f64>().ok().map(|v| v * 1_000_000.0)
    } else {
        s.parse().ok()
    }
}

fn main() {
    let stdin = io::stdin();
    let mut ops: HashMap<String, Vec<f64>> = HashMap::new();
    let mut total_busy = 0.0;
    let mut total_idle = 0.0;

    for line in stdin.lock().lines().map_while(Result::ok) {
        let line = strip_ansi(&line);

        for op in ["open", "close", "read", "write", "stat", "lstat", "fstat",
                   "opendir", "readdir", "mkdir", "rmdir", "remove", "rename"] {
            let pattern = format!(" {op}{{");
            if line.contains(&pattern) {
                if let Some(busy_start) = line.find("time.busy=") {
                    let rest = &line[busy_start + 10..];
                    if let Some(end) = rest.find(' ').or(Some(rest.len())) {
                        if let Some(us) = parse_time_us(&rest[..end]) {
                            ops.entry(op.to_string()).or_default().push(us);
                            total_busy += us;
                        }
                    }
                }
                if let Some(idle_start) = line.find("time.idle=") {
                    let rest = &line[idle_start + 10..];
                    if let Some(end) = rest.find(' ').or(Some(rest.len())) {
                        if let Some(us) = parse_time_us(&rest[..end]) {
                            total_idle += us;
                        }
                    }
                }
                break;
            }
        }
    }

    println!("=== SFTP Operation Latency Stats ===\n");

    let op_order = ["open", "close", "read", "write", "stat", "lstat", "fstat",
                    "opendir", "readdir", "mkdir", "rmdir", "remove", "rename"];

    let mut total_ops = 0usize;

    for op in op_order {
        if let Some(vals) = ops.get_mut(op) {
            if vals.is_empty() { continue; }
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap());

            let count = vals.len();
            total_ops += count;
            let sum: f64 = vals.iter().sum();
            let min = vals[0];
            let max = vals[count - 1];
            let p50 = vals[count / 2];
            let p99 = vals[(count * 99 / 100).max(1) - 1];

            println!("--- {op} ---");
            println!("  count: {count}");
            println!("  total: {:.2}ms", sum / 1000.0);
            println!("  avg:   {:.2}µs", sum / count as f64);
            println!("  min:   {:.2}µs", min);
            println!("  max:   {:.2}µs", max);
            println!("  p50:   {:.2}µs", p50);
            println!("  p99:   {:.2}µs", p99);
            println!();
        }
    }

    println!("--- Overall ---");
    println!("  total operations: {total_ops}");
    println!("  total busy:  {:.2}ms (time spent in handlers)", total_busy / 1000.0);
    println!("  total idle:  {:.2}ms (time waiting for requests)", total_idle / 1000.0);
    let total = total_busy + total_idle;
    if total > 0.0 {
        println!("  busy ratio:  {:.1}% busy, {:.1}% idle",
                 total_busy / total * 100.0, total_idle / total * 100.0);
    }
    println!();
    println!("Note: 'time.idle' shows time between handler completing and next request arriving");
}
