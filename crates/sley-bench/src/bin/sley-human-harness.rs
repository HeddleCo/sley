use sley_core::{GitError, cli_exit_code};
use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::time::Instant;

#[derive(Debug)]
struct Options {
    repo: PathBuf,
    repeat: usize,
    warmup: usize,
    out: PathBuf,
    command: Vec<String>,
}

#[derive(Debug)]
struct RunTiming {
    index: usize,
    ns: u128,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("sley-human-harness: {err}");
            process::exit(cli_exit_code(&err));
        }
    }
}

fn run() -> Result<(), GitError> {
    let options = parse_options(env::args().skip(1).collect())?;
    let original_dir = env::current_dir().map_err(|err| GitError::Io(err.to_string()))?;

    for _ in 0..options.warmup {
        run_once(&options)?;
    }

    let mut timings = Vec::with_capacity(options.repeat);
    for index in 0..options.repeat {
        let start = Instant::now();
        run_once(&options)?;
        timings.push(RunTiming {
            index,
            ns: start.elapsed().as_nanos(),
        });
    }

    env::set_current_dir(original_dir).map_err(|err| GitError::Io(err.to_string()))?;
    write_json(&options, &timings).map_err(|err| GitError::Io(err.to_string()))
}

fn run_once(options: &Options) -> Result<(), GitError> {
    env::set_current_dir(&options.repo).map_err(|err| GitError::Io(err.to_string()))?;
    sley_cli::run(options.command.clone())
}

fn parse_options(args: Vec<String>) -> Result<Options, GitError> {
    let mut repo = None;
    let mut repeat = 1usize;
    let mut warmup = 0usize;
    let mut out = None;
    let mut command = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                repo = args.get(index).map(PathBuf::from);
            }
            "--repeat" => {
                index += 1;
                repeat = parse_usize(args.get(index), "--repeat")?;
            }
            "--warmup" => {
                index += 1;
                warmup = parse_usize(args.get(index), "--warmup")?;
            }
            "--out" => {
                index += 1;
                out = args.get(index).map(PathBuf::from);
            }
            "--" => {
                command.extend(args[index + 1..].iter().cloned());
                break;
            }
            "-h" | "--help" => {
                print_usage();
                return Err(GitError::Exit(0));
            }
            value => {
                return Err(GitError::Command(format!(
                    "unknown option {value}; pass command args after --"
                )));
            }
        }
        index += 1;
    }

    let repo = repo.ok_or_else(|| GitError::Command("--repo is required".into()))?;
    let out = out.ok_or_else(|| GitError::Command("--out is required".into()))?;
    if repeat == 0 {
        return Err(GitError::Command(
            "--repeat must be greater than zero".into(),
        ));
    }
    if command.is_empty() {
        return Err(GitError::Command("command after -- is required".into()));
    }

    Ok(Options {
        repo,
        repeat,
        warmup,
        out,
        command,
    })
}

fn parse_usize(value: Option<&String>, name: &str) -> Result<usize, GitError> {
    let Some(value) = value else {
        return Err(GitError::Command(format!("{name} requires a value")));
    };
    value
        .parse()
        .map_err(|_| GitError::Command(format!("{name} must be an integer")))
}

fn print_usage() {
    eprintln!(
        "usage: sley-human-harness --repo <path> --out <json> [--warmup n] [--repeat n] -- <sley args...>"
    );
}

fn write_json(options: &Options, timings: &[RunTiming]) -> io::Result<()> {
    let mut file = File::create(&options.out)?;
    writeln!(file, "{{")?;
    writeln!(
        file,
        "  \"repo\": {},",
        json_string(&options.repo.display().to_string())
    )?;
    writeln!(file, "  \"repeat\": {},", options.repeat)?;
    writeln!(file, "  \"warmup\": {},", options.warmup)?;
    write!(file, "  \"command\": [")?;
    for (index, arg) in options.command.iter().enumerate() {
        if index > 0 {
            write!(file, ", ")?;
        }
        write!(file, "{}", json_string(arg))?;
    }
    writeln!(file, "],")?;
    writeln!(file, "  \"runs\": [")?;
    for (index, timing) in timings.iter().enumerate() {
        let comma = if index + 1 == timings.len() { "" } else { "," };
        writeln!(
            file,
            "    {{\"index\": {}, \"ns\": {}}}{}",
            timing.index, timing.ns, comma
        )?;
    }
    writeln!(file, "  ]")?;
    writeln!(file, "}}")?;
    Ok(())
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
