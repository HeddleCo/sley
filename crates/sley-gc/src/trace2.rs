//! Trace2 child/region/perf event helpers shared by the gc, repack, and
//! maintenance execution paths. These emit git-compatible trace2 event lines
//! to `GIT_TRACE2_EVENT` / `GIT_TRACE2_PERF` when configured.

use std::env;
use std::fs;
use std::io::Write;

pub fn child_start(args: &[&str]) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let sid = sid();
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let argv = argv
        .iter()
        .map(|arg| format!("\"{}\"", json_escape(arg)))
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        "{{\"event\":\"child_start\",\"sid\":\"{sid}\",\"child_id\":0,\"argv\":[{argv}]}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn touch() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let _ = fs::OpenOptions::new().create(true).append(true).open(path);
}

fn sid() -> String {
    let depth = sley_core::trace2::depth();
    if depth == 0 {
        "sley".to_string()
    } else {
        format!("sley/{depth}")
    }
}

pub(crate) fn region(event: &str, category: &str, label: &str) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"{}\",\"sid\":\"sley\",\"category\":\"{}\",\"label\":\"{}\"}}\n",
        json_escape(event),
        json_escape(category),
        json_escape(label)
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(crate) fn perf_data(key: &str, value: &str) {
    let Some(path) = env::var_os("GIT_TRACE2_PERF") else {
        return;
    };
    let line = format!("data: {key}:{value}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}
