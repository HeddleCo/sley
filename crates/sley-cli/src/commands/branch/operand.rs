//! Branch name/ref resolution shared across branch subcommands.

use crate::*;

#[derive(Clone, Copy)]
pub(super) enum BranchOperandKind {
    Existing,
    UpdateOrCreate,
}

pub(super) fn branch_resolve_local_branch_operand(
    git_dir: &Path,
    format: ObjectFormat,
    _store: &FileRefStore,
    branch: &str,
    kind: BranchOperandKind,
) -> Result<(String, String)> {
    if branch.contains("@{") {
        let Some(refname) = sley_rev::resolve_revision_symbolic_full_name(git_dir, format, branch)?
        else {
            eprintln!("fatal: '{branch}' does not name a branch");
            return Err(GitError::Exit(128));
        };
        let Some(local) = refname.strip_prefix("refs/heads/") else {
            eprintln!("fatal: '{branch}' does not name a local branch");
            return Err(GitError::Exit(128));
        };
        return Ok((local.to_string(), refname));
    }
    let refname = match kind {
        BranchOperandKind::Existing => validate_branch_source_name(branch)?,
        BranchOperandKind::UpdateOrCreate => validate_branch_creation_name(branch)?,
    };
    Ok((branch.to_string(), refname))
}
pub(super) fn validate_branch_creation_name(branch: &str) -> Result<String> {
    // git's strbuf_check_branch_ref rejects "HEAD" as a branch name even
    // though refs/heads/HEAD passes check_refname_format (t3200 #10). A literal
    // "@" is still a valid local branch operand here (t3204).
    if branch == "HEAD" {
        eprintln!("fatal: '{branch}' is not a valid branch name");
        print_branch_ref_syntax_hint();
        return Err(GitError::Exit(128));
    }
    match branch_ref_name(branch)
        .and_then(|refname| sley_refs::check_refname_format(&refname, false).map(|()| refname))
    {
        Ok(refname) => Ok(refname),
        Err(GitError::InvalidPath(_)) => {
            eprintln!("fatal: '{branch}' is not a valid branch name");
            print_branch_ref_syntax_hint();
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

pub(super) fn validate_branch_source_name(branch: &str) -> Result<String> {
    match sley_refs::branch_ref_name_for_source(branch) {
        Ok(refname) => Ok(refname),
        Err(GitError::InvalidPath(_)) => {
            eprintln!("fatal: invalid branch name: '{branch}'");
            print_branch_ref_syntax_hint();
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

pub(super) fn print_branch_ref_syntax_hint() {
    eprintln!("hint: See 'git help check-ref-format'");
    eprintln!("hint: Disable this message with \"git config set advice.refSyntax false\"");
}
