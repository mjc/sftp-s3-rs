// Standalone tool - compile with: rustc -O collapse-bpftrace-offcpu.rs
// Usage: cat offcpu-stacks.txt | ./collapse-bpftrace-offcpu > offcpu-folded.txt

use std::io::{self, BufRead};

fn clean_frame(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let (_, frame) = line.split_once(' ')?;
    let mut frame = frame.trim();
    if frame.is_empty() || frame.ends_with("([unknown])") {
        return None;
    }

    if let Some(path_start) = frame.rfind(" (") {
        if frame.ends_with(')') {
            frame = &frame[..path_start];
        }
    }

    if let Some(offset_start) = frame.rfind('+') {
        if frame[offset_start + 1..].chars().all(|c| c.is_ascii_digit()) {
            frame = &frame[..offset_start];
        }
    }

    if frame.is_empty() {
        None
    } else {
        Some(frame.to_string())
    }
}

fn count_from_line(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("]:")
        .map(str::trim)
        .filter(|count| count.chars().all(|c| c.is_ascii_digit()))
}

fn main() {
    let stdin = io::stdin();
    let mut frames: Option<Vec<String>> = None;

    for line in stdin.lock().lines().map_while(Result::ok) {
        if line.starts_with("@offcpu[") {
            frames = Some(Vec::new());
            continue;
        }

        let Some(current_frames) = frames.as_mut() else {
            continue;
        };

        if let Some(count) = count_from_line(&line) {
            if !current_frames.is_empty() {
                current_frames.reverse();
                println!("{} {}", current_frames.join(";"), count);
            }
            frames = None;
            continue;
        }

        if line.starts_with('\t') {
            if let Some(frame) = clean_frame(&line) {
                current_frames.push(frame);
            }
        }
    }
}
