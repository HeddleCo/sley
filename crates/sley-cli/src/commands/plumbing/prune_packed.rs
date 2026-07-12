//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;

pub(crate) fn cmd_prune_packed(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut dry_run = false;
    let mut positional = 0usize;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" | "--no-quiet" => {}
            "--" => {
                positional += iter.count();
                break;
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown switch `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            _ => positional += 1,
        }
    }
    if positional > 0 {
        eprintln!("fatal: too many arguments");
        eprintln!();
        return prune_packed_usage();
    }

    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let objects_dir = repository_objects_dir(&git_dir);
    let packed = prune_packed_object_ids(&objects_dir.join("pack"), format)?;
    if packed.is_empty() {
        return Ok(());
    }
    for (oid, path) in prune_packed_loose_object_paths(&objects_dir, format)? {
        if !packed.contains(&oid) {
            continue;
        }
        if dry_run {
            println!("rm -f {}", prune_packed_display_path(&path)?);
        } else {
            fs::remove_file(&path)?;
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    if !dry_run {
        prune_empty_loose_object_dirs(&objects_dir)?;
    }
    Ok(())
}

fn prune_empty_loose_object_dirs(objects_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let _ = fs::remove_dir(entry.path());
        }
    }
    Ok(())
}

fn prune_packed_usage<T>() -> Result<T> {
    eprintln!("usage: git prune-packed [-n | --dry-run] [-q | --quiet]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    dry run");
    eprintln!("    -q, --[no-]quiet      be quiet");
    eprintln!();
    Err(GitError::Exit(129))
}

fn prune_packed_object_ids(pack_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let mut packed = HashSet::new();
    if !pack_dir.exists() {
        return Ok(packed);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        packed.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(packed)
}

fn prune_packed_loose_object_paths(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut objects = Vec::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let suffix = object_entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let oid = ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?;
            objects.push((oid, object_entry.path()));
        }
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(objects)
}

fn prune_packed_display_path(path: &Path) -> Result<String> {
    let cwd = env::current_dir()?;
    let display = path.strip_prefix(&cwd).unwrap_or(path);
    Ok(display.to_string_lossy().replace('\\', "/"))
}
