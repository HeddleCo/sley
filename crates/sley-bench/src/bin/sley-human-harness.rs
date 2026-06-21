use sley_core::{GitError, ObjectFormat, ObjectId, cli_exit_code};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader, repository_objects_dir};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use sley_rev::{RevWalk, RevisionResolver};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

#[derive(Debug)]
struct Options {
    repo: PathBuf,
    repeat: usize,
    warmup: usize,
    out: Option<PathBuf>,
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

    for _ in 0..options.warmup {
        let mut sink = io::sink();
        run_once(&options, &mut sink)?;
    }

    let mut timings = Vec::with_capacity(options.repeat);
    for index in 0..options.repeat {
        let mut sink = io::sink();
        let start = Instant::now();
        run_once(&options, &mut sink)?;
        timings.push(RunTiming {
            index,
            ns: start.elapsed().as_nanos(),
        });
    }

    if let Some(out) = options.out.as_ref() {
        write_json(out, &options, &timings).map_err(|err| GitError::Io(err.to_string()))?;
    } else if options.repeat == 0 {
        let mut stdout = io::stdout().lock();
        run_once(&options, &mut stdout)?;
    }
    Ok(())
}

fn run_once(options: &Options, stdout: &mut impl Write) -> Result<(), GitError> {
    let repo = HarnessRepo::open(&options.repo)?;
    match command_key(&options.command).as_deref() {
        Some("status_short") => repo.status_short(stdout),
        Some("log_oneline_100") => repo.log_oneline(stdout, 100),
        Some("branch_list") => repo.branch_list(stdout),
        Some("tag_list") => repo.tag_list(stdout),
        Some("rev_parse_short_head") => repo.rev_parse_short_head(stdout),
        Some("branch_force_write") => repo.branch_force_write("sley-bench-write", "HEAD"),
        Some("tag_force_write") => repo.tag_force_write("sley-bench-write", "HEAD"),
        _ => Err(GitError::Command(format!(
            "unsupported harness command: {}",
            options.command.join(" ")
        ))),
    }
}

struct HarnessRepo {
    root: PathBuf,
    git_dir: PathBuf,
    format: ObjectFormat,
    refs: FileRefStore,
    db: FileObjectDatabase,
}

impl HarnessRepo {
    fn open(root: &Path) -> Result<Self, GitError> {
        let git_dir = root.join(".git");
        if !git_dir.is_dir() {
            return Err(GitError::Command(format!(
                "harness expects a non-bare repo root with .git: {}",
                root.display()
            )));
        }
        let format = ObjectFormat::Sha1;
        let refs = FileRefStore::new(&git_dir, format);
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        Ok(Self {
            root: root.to_path_buf(),
            git_dir,
            format,
            refs,
            db,
        })
    }

    fn resolver(&self) -> RevisionResolver<'_, FileObjectDatabase> {
        RevisionResolver::new(&self.git_dir, self.format, &self.db)
    }

    fn head_oid(&self) -> Result<ObjectId, GitError> {
        self.resolver().resolve("HEAD")
    }

    fn abbrev_width(&self) -> Result<usize, GitError> {
        repository_auto_abbrev_width(&self.git_dir, self.format)
    }

    fn abbrev_oid(&self, oid: &ObjectId) -> Result<String, GitError> {
        let hex = oid.to_hex();
        let width = self.abbrev_width()?.min(hex.len());
        Ok(hex[..width].to_owned())
    }

    fn status_short(&self, out: &mut impl Write) -> Result<(), GitError> {
        sley_worktree::stream_short_status(&self.root, &self.git_dir, self.format, |entry| {
            writeln!(
                out,
                "{}{} {}",
                entry.index as char,
                entry.worktree as char,
                String::from_utf8_lossy(entry.path)
            )
            .map_err(|err| GitError::Io(err.to_string()))?;
            Ok(sley_worktree::StreamControl::Continue)
        })
    }

    fn log_oneline(&self, out: &mut impl Write, count: usize) -> Result<(), GitError> {
        let head = self.head_oid()?;
        let abbrev = self.abbrev_width()?;
        let mut walk =
            RevWalk::new(&self.git_dir, self.format, &self.db, [head]).max_count(Some(count));
        while let Some(metadata) = walk.try_next()? {
            let object = self.db.read_object(&metadata.oid)?;
            if object.object_type != ObjectType::Commit {
                return Err(GitError::InvalidObject(format!(
                    "{} is not a commit",
                    metadata.oid
                )));
            }
            let commit = Commit::parse_ref(self.format, &object.body)?;
            let subject = commit
                .message
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or(b"");
            let hex = metadata.oid.to_hex();
            let width = abbrev.min(hex.len());
            writeln!(
                out,
                "{} {}",
                &hex[..width],
                String::from_utf8_lossy(subject)
            )
            .map_err(|err| GitError::Io(err.to_string()))?;
        }
        Ok(())
    }

    fn branch_list(&self, out: &mut impl Write) -> Result<(), GitError> {
        let current = self.refs.current_branch()?;
        let names = self.refs.list_short_ref_names_with_prefix("refs/heads/")?;
        for name in names {
            let marker = if current.as_deref() == Some(name.as_str()) {
                '*'
            } else {
                ' '
            };
            writeln!(out, "{marker} {name}").map_err(|err| GitError::Io(err.to_string()))?;
        }
        Ok(())
    }

    fn tag_list(&self, out: &mut impl Write) -> Result<(), GitError> {
        let names = self.refs.list_short_ref_names_with_prefix("refs/tags/")?;
        for name in names {
            writeln!(out, "{name}").map_err(|err| GitError::Io(err.to_string()))?;
        }
        Ok(())
    }

    fn rev_parse_short_head(&self, out: &mut impl Write) -> Result<(), GitError> {
        writeln!(out, "{}", self.abbrev_oid(&self.head_oid()?)?)
            .map_err(|err| GitError::Io(err.to_string()))
    }

    fn branch_force_write(&self, branch: &str, start: &str) -> Result<(), GitError> {
        let name = sley_refs::branch_ref_name(branch)?;
        let new_oid = self.resolver().resolve(start)?;
        let previous = self.refs.read_ref(&name)?;
        let reflog = match previous {
            Some(RefTarget::Direct(old_oid)) if old_oid == new_oid => None,
            Some(RefTarget::Direct(old_oid)) => Some(ReflogEntry {
                old_oid,
                new_oid,
                committer: reflog_identity(),
                message: format!("branch: Reset to {start}").into_bytes(),
            }),
            Some(_) => Some(ReflogEntry {
                old_oid: ObjectId::null(self.format),
                new_oid,
                committer: reflog_identity(),
                message: format!("branch: Reset to {start}").into_bytes(),
            }),
            None => Some(ReflogEntry {
                old_oid: ObjectId::null(self.format),
                new_oid,
                committer: reflog_identity(),
                message: format!("branch: Created from {start}").into_bytes(),
            }),
        };
        let mut tx = self.refs.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(new_oid),
            reflog,
        });
        tx.commit()
    }

    fn tag_force_write(&self, tag: &str, start: &str) -> Result<(), GitError> {
        let name = sley_refs::tag_ref_name(tag)?;
        let oid = self.resolver().resolve(start)?;
        let mut tx = self.refs.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit()
    }
}

fn command_key(command: &[String]) -> Option<String> {
    let args = command.iter().map(String::as_str).collect::<Vec<_>>();
    match args.as_slice() {
        ["status", "--short"] => Some("status_short".to_owned()),
        ["log", "--oneline", "-100"] => Some("log_oneline_100".to_owned()),
        ["branch", "--list"] => Some("branch_list".to_owned()),
        ["tag", "--list"] => Some("tag_list".to_owned()),
        ["rev-parse", "--short", "HEAD"] => Some("rev_parse_short_head".to_owned()),
        ["branch", "-f", "sley-bench-write", "HEAD"] => Some("branch_force_write".to_owned()),
        ["tag", "-f", "sley-bench-write", "HEAD"] => Some("tag_force_write".to_owned()),
        _ => None,
    }
}

fn reflog_identity() -> Vec<u8> {
    let name = env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "Git Rs".to_owned());
    let email =
        env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "sley@example.invalid".to_owned());
    let date = env::var("GIT_COMMITTER_DATE")
        .ok()
        .and_then(|raw| normalize_reflog_date(&raw))
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("{now} +0000")
        });
    format!("{name} <{email}> {date}").into_bytes()
}

fn normalize_reflog_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    let (seconds, tz) = raw.split_once(' ')?;
    if seconds.parse::<i64>().is_ok()
        && tz.len() == 5
        && (tz.starts_with('+') || tz.starts_with('-'))
        && tz[1..].chars().all(|ch| ch.is_ascii_digit())
    {
        Some(format!("{seconds} {tz}"))
    } else {
        None
    }
}

fn repository_auto_abbrev_width(git_dir: &Path, format: ObjectFormat) -> Result<usize, GitError> {
    let object_count = repository_approx_object_count(git_dir, format)?;
    if object_count == 0 {
        return Ok(7.min(format.hex_len()));
    }
    let bits = u64::BITS as usize - object_count.saturating_sub(1).leading_zeros() as usize;
    Ok(((bits + 1) / 2).max(7).min(format.hex_len()))
}

fn repository_approx_object_count(git_dir: &Path, _format: ObjectFormat) -> Result<u64, GitError> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("idx")) {
            count = count.saturating_add(u64::from(pack_index_object_count(&path)?));
        }
    }
    Ok(count)
}

fn pack_index_object_count(path: &Path) -> Result<u32, GitError> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 8 + 256 * 4];
    file.read_exact(&mut header[..8]).map_err(|_| {
        GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
    })?;
    let fanout_offset = if &header[..4] == b"\xfftOc" {
        let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        file.read_exact(&mut header[8..]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        8
    } else {
        file.read_exact(&mut header[8..256 * 4]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        0
    };
    let offset = fanout_offset + 255 * 4;
    Ok(u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ]))
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
    if repeat == 0 {
        if out.is_some() {
            return Err(GitError::Command(
                "--repeat must be greater than zero when --out is used".into(),
            ));
        }
    } else if out.is_none() {
        return Err(GitError::Command(
            "--out is required when --repeat is greater than zero".into(),
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

fn write_json(out: &Path, options: &Options, timings: &[RunTiming]) -> io::Result<()> {
    let mut file = File::create(out)?;
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
