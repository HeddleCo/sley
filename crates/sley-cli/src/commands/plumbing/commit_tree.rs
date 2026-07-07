//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;
use sley::plumbing::{sley_rev};

pub(crate) fn cmd_commit_tree(args: &[String]) -> Result<()> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut message_chunks = Vec::new();
    let mut gpg_sign = false;
    let mut gpg_sign_key: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" => {
                let Some(parent) = iter.next() else {
                    return commit_tree_parent_requires_value_error();
                };
                parents.push(parent.to_string());
            }
            value if value.starts_with("-p") && value.len() > 2 => {
                parents.push(value[2..].to_string());
            }
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                let mut chunk = value.as_bytes()[2..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "-F" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                message_chunks.push(read_commit_message_file(path)?);
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                message_chunks.push(read_commit_message_file(&value[2..])?);
            }
            "-S" | "--gpg-sign" => {
                gpg_sign = true;
                gpg_sign_key = None;
            }
            value if value.starts_with("-S") && value.len() > 2 => {
                gpg_sign = true;
                gpg_sign_key = Some(value[2..].to_string());
            }
            value if value.starts_with("--gpg-sign=") => {
                gpg_sign = true;
                gpg_sign_key = Some(value["--gpg-sign=".len()..].to_string());
            }
            "--no-gpg-sign" => {
                gpg_sign = false;
                gpg_sign_key = None;
            }
            value if tree.is_none() => tree = Some(value.to_string()),
            value if !value.starts_with('-') => return commit_tree_requires_one_tree_error(),
            value => {
                return Err(GitError::Command(format!(
                    "unexpected commit-tree argument {value}"
                )));
            }
        }
    }
    let Some(tree) = tree else {
        return commit_tree_requires_one_tree_error();
    };
    let git_dir = crate::session::cli_git_dir()?;
    let format = repository_object_format(&git_dir)?;
    // git resolves the tree and each `-p` parent as a revision-ish (so a tag,
    // branch, `HEAD^`, abbreviated oid, or `<rev>^{tree}` all work), peeling the
    // tree argument to a tree and each parent to a commit. A *full-length* hex
    // oid is taken verbatim without an existence check (matching git, which
    // accepts e.g. the empty-tree hash `4b825d...` even when it is absent from
    // the object store); shorter names go through revision resolution + peel.
    let db_resolve = FileObjectDatabase::from_git_dir(&git_dir, format);
    let tree = match ObjectId::from_hex(format, &tree) {
        Ok(oid) => oid,
        Err(_) => {
            let tree_rev = resolve_revision_treeish(&git_dir, format, &tree)?;
            sley_rev::peel_to_tree(&db_resolve, format, &tree_rev)?
        }
    };
    let parents = parents
        .iter()
        .map(|parent| match ObjectId::from_hex(format, parent) {
            Ok(oid) => Ok(oid),
            Err(_) => {
                let resolved = resolve_revision_commitish(&git_dir, format, parent)?;
                sley_rev::peel_to_commit(&db_resolve, format, &resolved)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let message = if message_chunks.is_empty() {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        message
    } else {
        commit_message_from_prepared_chunks(&message_chunks)
    };
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let config = read_repo_config(&git_dir).ok();
    let signature = if gpg_sign {
        let unsigned = Commit {
            tree,
            parents: parents.clone(),
            author: author.clone(),
            committer: committer.clone(),
            encoding: None,
            message: message.clone(),
        };
        let key =
            commands::signing::signing_key(config.as_ref(), gpg_sign_key.as_deref(), &committer);
        Some(commands::signing::sign_payload(
            config.as_ref(),
            &unsigned.write(),
            key.as_deref(),
        )?)
    } else {
        None
    };
    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer,
            message,
            encoding: None,
            signature,
        },
    )?;
    println!("{oid}");
    Ok(())
}

fn commit_tree_parent_requires_value_error() -> Result<()> {
    eprintln!("error: switch `p' requires a value");
    Err(GitError::Exit(129))
}

fn commit_tree_requires_one_tree_error() -> Result<()> {
    eprintln!("fatal: must give exactly one tree");
    Err(GitError::Exit(128))
}
