use sley::plumbing::sley_core::fetch_profile::{self, Stage};
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: fetch-profile [--report <path>] [--flamegraph <path>] \
    [--frequency <hz>] [--checkpoint-seconds <seconds>] -- <sley command> [args...]";

struct Options {
    report: PathBuf,
    flamegraph: PathBuf,
    frequency: i32,
    checkpoint: Option<Duration>,
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("fetch-profile: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8, Box<dyn Error>> {
    let options = parse_options(std::env::args().skip(1))?;
    ensure_parent(&options.report)?;
    ensure_parent(&options.flamegraph)?;

    fetch_profile::reset();
    let guard = pprof::ProfilerGuard::new(options.frequency)?;
    let started = Instant::now();
    let command_result = if let Some(interval) = options.checkpoint {
        run_with_checkpoints(&options, &guard, started, interval)?
    } else {
        sley_cli::run(options.command.clone())
    };
    let elapsed = started.elapsed();

    write_outputs(&options, &guard, elapsed)?;

    eprintln!("fetch profile report: {}", options.report.display());
    eprintln!("fetch profile flamegraph: {}", options.flamegraph.display());
    match command_result {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("sley: {err}");
            Ok(1)
        }
    }
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut report = PathBuf::from("fetch-profile.txt");
    let mut flamegraph = PathBuf::from("fetch-profile.svg");
    let mut frequency = 100i32;
    let mut checkpoint = None;
    let mut command = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--" {
            command.extend(args);
            break;
        }
        match arg.as_str() {
            "--report" => {
                report = PathBuf::from(
                    args.next()
                        .ok_or_else(|| format!("--report requires a path\n{USAGE}"))?,
                );
            }
            "--flamegraph" => {
                flamegraph = PathBuf::from(
                    args.next()
                        .ok_or_else(|| format!("--flamegraph requires a path\n{USAGE}"))?,
                );
            }
            "--frequency" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--frequency requires a value\n{USAGE}"))?;
                frequency = value
                    .parse::<i32>()
                    .map_err(|_| format!("invalid sampling frequency {value:?}"))?;
                if frequency <= 0 {
                    return Err("sampling frequency must be positive".into());
                }
            }
            "--checkpoint-seconds" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--checkpoint-seconds requires a value\n{USAGE}"))?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid checkpoint interval {value:?}"))?;
                if seconds == 0 {
                    return Err("checkpoint interval must be positive".into());
                }
                checkpoint = Some(Duration::from_secs(seconds));
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown profiling option {arg:?}\n{USAGE}")),
        }
    }
    if command.is_empty() {
        return Err(format!("missing sley command\n{USAGE}"));
    }
    Ok(Options {
        report,
        flamegraph,
        frequency,
        checkpoint,
        command,
    })
}

fn run_with_checkpoints(
    options: &Options,
    guard: &pprof::ProfilerGuard<'_>,
    started: Instant,
    interval: Duration,
) -> Result<Result<(), sley::GitError>, Box<dyn Error>> {
    let command = options.command.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("sley-fetch-profile".into())
        .spawn(move || {
            let result = sley_cli::run(command);
            let _ = sender.send(result);
        })?;
    let command_result = loop {
        match receiver.recv_timeout(interval) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(err) = write_outputs(options, guard, started.elapsed()) {
                    eprintln!("fetch-profile: checkpoint failed: {err}");
                } else {
                    eprintln!(
                        "fetch-profile: checkpoint at {:.1}s",
                        started.elapsed().as_secs_f64()
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("profile worker disconnected").into());
            }
        }
    };
    worker
        .join()
        .map_err(|_| std::io::Error::other("profile worker panicked"))?;
    Ok(command_result)
}

fn write_outputs(
    options: &Options,
    guard: &pprof::ProfilerGuard<'_>,
    elapsed: Duration,
) -> Result<(), Box<dyn Error>> {
    let snapshot = fetch_profile::snapshot();
    let rendered = render_report(elapsed, &snapshot, &options.flamegraph);
    fs::write(&options.report, rendered)?;
    let profile = guard.report().build()?;
    let temporary = options.flamegraph.with_extension("svg.tmp");
    profile.flamegraph(File::create(&temporary)?)?;
    fs::rename(temporary, &options.flamegraph)?;
    Ok(())
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn render_report(
    elapsed: Duration,
    snapshot: &fetch_profile::Snapshot,
    flamegraph: &Path,
) -> String {
    let mut out = String::new();
    let profiled = snapshot
        .stages
        .iter()
        .map(|sample| sample.duration)
        .sum::<Duration>();
    let unattributed = elapsed.saturating_sub(profiled);
    let socket = &snapshot.stages[Stage::SocketRead as usize];
    let pack = &snapshot.stages[Stage::PktLineSideband as usize];
    let transfer_mbps = decimal_mbps(pack.bytes, elapsed);
    let active_socket_mbps = decimal_mbps(socket.bytes, socket.duration);

    let _ = writeln!(out, "sley protocol-v2 fetch/index-pack profile");
    let _ = writeln!(out, "total_wall_seconds={:.6}", elapsed.as_secs_f64());
    let _ = writeln!(out, "profiled_wall_seconds={:.6}", profiled.as_secs_f64());
    let _ = writeln!(
        out,
        "unattributed_wall_seconds={:.6}",
        unattributed.as_secs_f64()
    );
    let _ = writeln!(out, "pack_bytes={}", pack.bytes);
    let _ = writeln!(out, "socket_body_bytes={}", socket.bytes);
    let _ = writeln!(out, "end_to_end_MB_per_second={transfer_mbps:.3}");
    let _ = writeln!(out, "active_socket_MB_per_second={active_socket_mbps:.3}");
    let _ = writeln!(out, "inflate_backend=zlib-rs (flate2)");
    #[cfg(feature = "fast-sha1")]
    let _ = writeln!(
        out,
        "sha1_backend=RustCrypto sha1 (hardware-dispatched; not SHA-1DC)"
    );
    #[cfg(not(feature = "fast-sha1"))]
    let _ = writeln!(out, "sha1_backend=sley-core scalar SHA-1 (not SHA-1DC)");
    let _ = writeln!(out, "index_pack_threads=1");
    let _ = writeln!(out, "object_storage=packfile_plus_v2_idx");
    let _ = writeln!(out, "fsync_count={}", snapshot.fsyncs);
    let _ = writeln!(out, "flamegraph={}", flamegraph.display());
    let _ = writeln!(out);
    let _ = writeln!(out, "stage\twall_seconds\tpercent_total\tcount\tbytes");
    for sample in &snapshot.stages {
        let percent = if elapsed.is_zero() {
            0.0
        } else {
            sample.duration.as_secs_f64() * 100.0 / elapsed.as_secs_f64()
        };
        let _ = writeln!(
            out,
            "{}\t{:.6}\t{percent:.2}\t{}\t{}",
            sample.stage.label(),
            sample.duration.as_secs_f64(),
            sample.count,
            sample.bytes,
        );
    }
    let unattributed_percent = if elapsed.is_zero() {
        0.0
    } else {
        unattributed.as_secs_f64() * 100.0 / elapsed.as_secs_f64()
    };
    let _ = writeln!(
        out,
        "unattributed / orchestration\t{:.6}\t{unattributed_percent:.2}\t0\t0",
        unattributed.as_secs_f64(),
    );
    out
}

fn decimal_mbps(bytes: u64, duration: Duration) -> f64 {
    if duration.is_zero() {
        return 0.0;
    }
    bytes as f64 / 1_000_000.0 / duration.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_keeps_sley_arguments_after_separator() {
        let options = parse_options(
            [
                "--report",
                "out.txt",
                "--flamegraph",
                "out.svg",
                "--",
                "clone",
                "-n",
                "https://example.invalid/repo",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("profile arguments");
        assert_eq!(options.report, PathBuf::from("out.txt"));
        assert_eq!(options.flamegraph, PathBuf::from("out.svg"));
        assert_eq!(options.command[0], "clone");
        assert_eq!(options.command[1], "-n");
    }
}
