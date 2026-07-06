//! Branch-related repository config read/write helpers.

use crate::*;
use sley::plumbing::{sley_config};

pub(super) fn rename_branch_config(git_dir: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    let path = git_dir.join("config");
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let old_section = branch_config_section_name(old_branch);
    let new_section = branch_config_section_name(new_branch);
    match sley_config::raw_edit::rename_or_remove_section(
        &contents,
        &old_section,
        Some(&new_section),
    ) {
        sley_config::raw_edit::SectionEditOutcome::Changed(out) => {
            write_raw_repo_config(git_dir, out)?;
        }
        sley_config::raw_edit::SectionEditOutcome::NotFound => {}
        sley_config::raw_edit::SectionEditOutcome::LineTooLong(line) => {
            return Err(GitError::InvalidFormat(format!(
                "config line {line} is too long"
            )));
        }
    }
    Ok(())
}

pub(super) fn copy_branch_config(git_dir: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    if copy_branch_config_raw(git_dir, old_branch, new_branch)? {
        return Ok(());
    }
    let mut config = read_repo_config(git_dir)?;
    let mut copied = false;
    let mut sections = Vec::with_capacity(config.sections.len());
    for section in config.sections {
        if section.name == "branch" && section.subsection.as_deref() == Some(old_branch) {
            let mut copied_section = section.clone();
            copied_section.subsection = Some(new_branch.to_string());
            sections.push(section);
            sections.push(copied_section);
            copied = true;
        } else {
            sections.push(section);
        }
    }
    if copied {
        config.sections = sections;
        write_branch_repo_config(git_dir, &config)?;
    }
    Ok(())
}

pub(super) fn copy_branch_config_raw(git_dir: &Path, old_branch: &str, new_branch: &str) -> Result<bool> {
    let path = git_dir.join("config");
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let old_header = branch_config_section_header(old_branch);
    let new_header = branch_config_section_header(new_branch);
    let mut out = Vec::with_capacity(contents.len() + new_header.len());
    let mut pos = 0usize;
    let mut copied = false;
    while pos < contents.len() {
        let line_start = pos;
        while pos < contents.len() && contents[pos] != b'\n' {
            pos += 1;
        }
        if pos < contents.len() {
            pos += 1;
        }
        let line_end = pos;
        if !config_line_matches_header(&contents[line_start..line_end], &old_header) {
            out.extend_from_slice(&contents[line_start..line_end]);
            continue;
        }

        let body_start = line_end;
        let mut section_end = body_start;
        while section_end < contents.len() {
            let next_line = section_end;
            while section_end < contents.len() && contents[section_end] != b'\n' {
                section_end += 1;
            }
            if section_end < contents.len() {
                section_end += 1;
            }
            if config_line_starts_section(&contents[next_line..section_end]) {
                section_end = next_line;
                break;
            }
        }

        out.extend_from_slice(&contents[line_start..section_end]);
        out.extend_from_slice(new_header.as_bytes());
        out.extend_from_slice(&contents[body_start..section_end]);
        copied = true;
        pos = section_end;
    }
    if copied {
        write_raw_repo_config(git_dir, out)?;
    }
    Ok(copied)
}

pub(super) fn branch_config_section_name(branch: &str) -> String {
    format!("branch.{branch}")
}

pub(super) fn branch_config_section_header(branch: &str) -> String {
    let escaped = branch.replace('\\', "\\\\").replace('"', "\\\"");
    format!("[branch \"{escaped}\"]\n")
}

pub(super) fn config_line_matches_header(line: &[u8], header: &str) -> bool {
    trim_config_line_newline(line) == header.trim_end_matches('\n').as_bytes()
}

pub(super) fn config_line_starts_section(line: &[u8]) -> bool {
    trim_config_line_newline(line)
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'[')
}

pub(super) fn trim_config_line_newline(mut line: &[u8]) -> &[u8] {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line = &line[..line.len() - 1];
    }
    line
}

pub(super) fn write_raw_repo_config(git_dir: &Path, bytes: Vec<u8>) -> Result<()> {
    let path = git_dir.join("config");
    match sley_config::raw_edit::write_config_file_locked(
        &path,
        &bytes,
        sley_config::raw_edit::ConfigFileWriteOptions::default(),
    ) {
        Ok(()) => Ok(()),
        Err(sley_config::raw_edit::ConfigFileWriteError::ExistingLock(_)) => {
            eprintln!(
                "error: could not lock config file {}: File exists",
                branch_config_display_path(git_dir)
            );
            Err(GitError::Exit(255))
        }
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

pub(super) fn write_branch_repo_config(git_dir: &Path, config: &GitConfig) -> Result<()> {
    if git_dir.join("config.lock").exists() {
        eprintln!(
            "error: could not lock config file {}: File exists",
            branch_config_display_path(git_dir)
        );
        return Err(GitError::Exit(255));
    }
    fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
    Ok(())
}

pub(super) fn branch_config_display_path(git_dir: &Path) -> String {
    if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
        ".git/config".to_string()
    } else {
        git_dir.join("config").display().to_string()
    }
}
pub(super) fn remove_branch_config(git_dir: &Path, branch: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let before = config.sections.len();
    config.sections.retain(|section| {
        !(section.name == "branch" && section.subsection.as_deref() == Some(branch))
    });
    if config.sections.len() != before {
        write_branch_repo_config(git_dir, &config)?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
pub(super) enum AutoRebase {
    Never,
    Local,
    Remote,
    Always,
}

pub(super) fn validate_autosetuprebase(config: &GitConfig) -> Result<AutoRebase> {
    match config.get_entry("branch", None, "autosetuprebase") {
        None => Ok(AutoRebase::Never),
        Some(None) => {
            eprintln!("error: missing value for 'branch.autosetuprebase'");
            Err(GitError::Exit(128))
        }
        Some(Some("never")) => Ok(AutoRebase::Never),
        Some(Some("local")) => Ok(AutoRebase::Local),
        Some(Some("remote")) => Ok(AutoRebase::Remote),
        Some(Some("always")) => Ok(AutoRebase::Always),
        Some(Some(other)) => {
            eprintln!("error: malformed value for 'branch.autosetuprebase': {other}");
            Err(GitError::Exit(128))
        }
    }
}
