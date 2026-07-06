//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;

use super::plumbing_options::setup_rerere_options;

struct MergeRrEntry {
    hash: String,
    variant: u32,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RerereSubcommand {
    Clear,
    Forget,
    Gc,
    Status,
}

#[derive(Debug)]
pub(super) struct RerereOptions {
    pub(super) subcommand: Option<RerereSubcommand>,
    pub(super) paths: Vec<String>,
}

pub(crate) fn cmd_rerere(args: &[String]) -> Result<()> {
    let options = setup_rerere_options(args)?;
    let git_dir = crate::session::cli_git_dir()?;
    match options.subcommand {
        None => Ok(()),
        Some(RerereSubcommand::Status) => rerere_status(&git_dir),
        Some(RerereSubcommand::Clear) => rerere_clear(&git_dir),
        Some(RerereSubcommand::Forget) => rerere_forget(&git_dir, &options.paths),
        Some(RerereSubcommand::Gc) => rerere_gc(&git_dir),
    }
}

fn is_rerere_enabled(git_dir: &Path) -> Result<bool> {
    let config = read_repo_config(git_dir)?;
    if let Some(value) = config.get("rerere", None, "enabled") {
        return Ok(matches!(value, "true" | "1" | "yes" | "on"));
    }
    Ok(git_dir.join("rr-cache").is_dir())
}

fn read_merge_rr(git_dir: &Path) -> Result<Vec<MergeRrEntry>> {
    let path = git_dir.join("MERGE_RR");
    let Ok(data) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for record in data
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(GitError::Command("corrupt MERGE_RR".into()));
        };
        let id = std::str::from_utf8(&record[..tab])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let (hash, variant) = parse_merge_rr_id(id)?;
        entries.push(MergeRrEntry {
            hash,
            variant,
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn parse_merge_rr_id(id: &str) -> Result<(String, u32)> {
    let Some(dot) = id.find('.') else {
        return Ok((id.to_string(), 0));
    };
    let hash = &id[..dot];
    let variant = id[dot + 1..]
        .parse::<u32>()
        .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
    Ok((hash.to_string(), variant))
}

fn rerere_cache_file_path(cache_dir: &Path, variant: u32, name: &str) -> PathBuf {
    if variant == 0 {
        cache_dir.join(name)
    } else {
        cache_dir.join(format!("{name}.{variant}"))
    }
}

fn rerere_has_resolution(rr_cache: &Path, entry: &MergeRrEntry) -> bool {
    let cache_dir = rr_cache.join(&entry.hash);
    rerere_cache_file_path(&cache_dir, entry.variant, "preimage").is_file()
        && rerere_cache_file_path(&cache_dir, entry.variant, "postimage").is_file()
}

fn remove_rr_cache_entry(rr_cache: &Path, entry: &MergeRrEntry) -> Result<()> {
    let cache_dir = rr_cache.join(&entry.hash);
    if !cache_dir.is_dir() {
        return Ok(());
    }
    for file in fs::read_dir(&cache_dir)? {
        let path = file?.path();
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    match fs::remove_dir(&cache_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(GitError::Io(err.to_string())),
    }
    Ok(())
}

fn rerere_status(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    for entry in read_merge_rr(git_dir)? {
        println!("{}", entry.path);
    }
    Ok(())
}

pub(crate) fn rerere_clear(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    for entry in read_merge_rr(git_dir)? {
        if !rerere_has_resolution(&rr_cache, &entry) {
            remove_rr_cache_entry(&rr_cache, &entry)?;
        }
    }
    let merge_rr = git_dir.join("MERGE_RR");
    if merge_rr.is_file() {
        fs::remove_file(merge_rr)?;
    }
    Ok(())
}

fn rerere_gc(git_dir: &Path) -> Result<()> {
    let rr_cache = git_dir.join("rr-cache");
    if !rr_cache.exists() {
        return Ok(());
    }
    rerere_gc_dir(&rr_cache, true)
}

fn rerere_gc_dir(path: &Path, keep_root: bool) -> Result<()> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            rerere_gc_dir(&child, false)?;
        } else {
            match fs::remove_file(&child) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    if !keep_root {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn rerere_path_matches(path: &str, pattern: &str) -> bool {
    path == pattern || path.ends_with(&format!("/{pattern}"))
}

fn rerere_forget(git_dir: &Path, paths: &[String]) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    if paths.is_empty() {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    let entries = read_merge_rr(git_dir)?;
    for pattern in paths {
        let mut matched = false;
        for entry in entries
            .iter()
            .filter(|entry| rerere_path_matches(&entry.path, pattern))
        {
            matched = true;
            let cache_dir = rr_cache.join(&entry.hash);
            let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
            if !postimage.is_file() {
                eprintln!("error: no remembered resolution for '{pattern}'");
                continue;
            }
            fs::remove_file(&postimage)?;
            if let Ok(thisimage) = fs::read(rerere_cache_file_path(
                &cache_dir,
                entry.variant,
                "thisimage",
            )) {
                fs::write(
                    rerere_cache_file_path(&cache_dir, entry.variant, "preimage"),
                    thisimage,
                )?;
                eprintln!("Updated preimage for '{pattern}'");
            }
            eprintln!("Forgot resolution for '{pattern}'");
        }
        if !matched {
            eprintln!("error: no remembered resolution for '{pattern}'");
        }
    }
    Ok(())
}
