//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;

use super::plumbing_options::setup_replace_options;

#[derive(Debug)]
pub(super) enum ReplaceMode {
    Create {
        object: String,
        replacement: String,
    },
    List {
        pattern: Option<String>,
    },
    Delete {
        objects: Vec<String>,
    },
    Edit {
        object: String,
    },
    Graft {
        object: String,
        parents: Vec<String>,
    },
    ConvertGraftFile,
}

#[derive(Debug)]
pub(super) struct ReplaceOptions {
    pub(crate) force: bool,
    pub(crate) format: ReplaceListFormat,
    pub(crate) raw: bool,
    pub(crate) mode: ReplaceMode,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ReplaceListFormat {
    Short,
    Medium,
    Long,
}

pub(crate) fn cmd_replace(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_replace_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    match options.mode {
        ReplaceMode::List { pattern } => {
            replace_list(&store, &db, format, pattern.as_deref(), options.format)
        }
        ReplaceMode::Delete { objects } => replace_delete(
            &store,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
            &objects,
        ),
        ReplaceMode::Edit { object } => replace_edit(
            &store,
            &db,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
            &object,
            options.force,
            options.raw,
        ),
        ReplaceMode::Graft { object, parents } => replace_graft(
            &store,
            &db,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
            &object,
            &parents,
            options.force,
        ),
        ReplaceMode::ConvertGraftFile => {
            replace_convert_graft_file(&store, &db, &common_git_dir, format)
        }
        ReplaceMode::Create {
            object,
            replacement,
        } => replace_create(
            &store,
            &db,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
            &object,
            &replacement,
            options.force,
        ),
    }
}

fn replace_list(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    object_format: ObjectFormat,
    pattern: Option<&str>,
    format: ReplaceListFormat,
) -> Result<()> {
    for reference in store.list_refs()? {
        let Some(object) = reference.name.strip_prefix("refs/replace/") else {
            continue;
        };
        if pattern.is_some_and(|pattern| !refname_pattern_matches(pattern, object)) {
            continue;
        }
        let RefTarget::Direct(replacement) = reference.target else {
            continue;
        };
        match format {
            ReplaceListFormat::Short => println!("{object}"),
            ReplaceListFormat::Medium => println!("{object} -> {replacement}"),
            ReplaceListFormat::Long => {
                let object_type = replace_object_type(db, object_format, object)?;
                let replacement_type = db
                    .read_object_header(&replacement)?
                    .map(|(object_type, _)| object_type.as_str())
                    .unwrap_or("unknown");
                println!("{object} ({object_type}) -> {replacement} ({replacement_type})");
            }
        }
    }
    Ok(())
}

fn replace_delete(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    objects: &[String],
) -> Result<()> {
    let mut failed = false;
    for object in objects {
        let oid = match ObjectId::from_hex(format, object) {
            Ok(oid) => oid,
            Err(_) => match resolve_revision(git_dir, format, object, replace_objects) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: failed to resolve '{object}' as a valid ref");
                    failed = true;
                    continue;
                }
            },
        };
        let name = format!("refs/replace/{oid}");
        match store.delete_ref(&name) {
            Ok(_) => println!("Deleted replace ref '{oid}'"),
            Err(_) => {
                eprintln!("error: replace ref '{oid}' not found");
                failed = true;
            }
        }
    }
    if failed {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

fn replace_create(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    object: &str,
    replacement: &str,
    force: bool,
) -> Result<()> {
    let object_oid = resolve_revision(git_dir, format, object, replace_objects)?;
    let replacement_oid = resolve_revision(git_dir, format, replacement, replace_objects)?;
    let object_type = db
        .read_object_header(&object_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(object_oid))?;
    let replacement_type = db
        .read_object_header(&replacement_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(replacement_oid))?;
    if !force && object_type != replacement_type {
        eprintln!("error: Objects must be of the same type.");
        eprintln!(
            "'{object}' points to a replaced object of type '{}'",
            object_type.as_str()
        );
        eprintln!(
            "while '{replacement}' points to a replacement object of type '{}'.",
            replacement_type.as_str()
        );
        return Err(GitError::Exit(255));
    }
    let name = format!("refs/replace/{object_oid}");
    write_replace_ref(store, &name, replacement_oid, force)
}

fn write_replace_ref(
    store: &FileRefStore,
    name: &str,
    replacement_oid: ObjectId,
    force: bool,
) -> Result<()> {
    let precondition = if force {
        RefPrecondition::Any
    } else {
        RefPrecondition::MustNotExist
    };
    let mut tx = store.transaction();
    tx.update_to(
        name.to_string(),
        RefTarget::Direct(replacement_oid),
        precondition,
        None,
    );
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(_) if !force => {
            eprintln!("error: replace ref '{name}' already exists");
            Err(GitError::Exit(255))
        }
        Err(err) => Err(err),
    }
}

fn replace_edit(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    object: &str,
    force: bool,
    _raw: bool,
) -> Result<()> {
    let object_oid = resolve_revision(git_dir, format, object, replace_objects)?;
    let ref_name = format!("refs/replace/{object_oid}");
    let existing = store.read_ref(&ref_name)?;
    if existing.is_some() && !force {
        eprintln!("error: replace ref '{ref_name}' already exists");
        return Err(GitError::Exit(255));
    }
    // `--force --edit` replaces an existing replacement by editing the
    // original object again, rather than recursively editing the current
    // replacement target.
    let original = db.read_object_without_replacement(&object_oid)?;
    let edit_path = git_dir.join("REPLACE_EDITOBJ");
    fs::write(&edit_path, &original.body)?;
    let editor_result = commands::replay::launch_editor(git_dir, &edit_path);
    if let Err(err) = editor_result {
        let _ = fs::remove_file(&edit_path);
        return Err(err);
    }
    let edited = fs::read(&edit_path)?;
    let _ = fs::remove_file(&edit_path);
    if edited == original.body {
        eprintln!("error: new object is the same as the old one");
        return Err(GitError::Exit(1));
    }
    let replacement_oid = db.write_object(EncodedObject::new(original.object_type, edited))?;
    write_replace_ref(store, &ref_name, replacement_oid, force)
}

fn replace_graft(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    object: &str,
    parents: &[String],
    force: bool,
) -> Result<()> {
    let object_oid = resolve_revision(git_dir, format, object, replace_objects)?;
    let commit_oid = sley_rev::peel_to_commit(db, format, &object_oid)?;
    let mut parent_oids = Vec::with_capacity(parents.len());
    for parent in parents {
        let oid = resolve_revision(git_dir, format, parent, replace_objects)?;
        parent_oids.push(sley_rev::peel_to_commit(db, format, &oid)?);
    }
    replace_graft_oids(store, db, format, commit_oid, parent_oids, force)
}

fn replace_graft_oids(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: ObjectId,
    parents: Vec<ObjectId>,
    force: bool,
) -> Result<()> {
    let object = db.read_object_without_replacement(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "object {commit_oid} is not a commit"
        )));
    }
    for mergetag in commit_mergetag_targets(format, &object.body)? {
        if !parents.contains(&mergetag) {
            eprintln!("error: new commit is missing mergetag parent {mergetag}");
            return Err(GitError::Exit(1));
        }
    }
    let mut commit = Commit::parse(format, &object.body)?;
    commit.parents = parents;
    let replacement_oid =
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))?;
    let ref_name = format!("refs/replace/{commit_oid}");
    write_replace_ref(store, &ref_name, replacement_oid, force)
}

fn commit_mergetag_targets(format: ObjectFormat, body: &[u8]) -> Result<Vec<ObjectId>> {
    let header_end = body
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap_or(body.len());
    let mut targets = Vec::new();
    for line in body[..header_end].split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"mergetag object ") {
            let value = std::str::from_utf8(value)
                .map_err(|err| GitError::InvalidObject(err.to_string()))?;
            targets.push(ObjectId::from_hex(format, value)?);
        }
    }
    Ok(targets)
}

fn replace_convert_graft_file(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let graft_path = git_dir.join("info").join("grafts");
    let contents = fs::read_to_string(&graft_path)?;
    let mut grafts = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let parse_oid = |value: &str| {
            ObjectId::from_hex(format, value).map_err(|_| {
                eprintln!("error: malformed graft data: {line}");
                GitError::Exit(1)
            })
        };
        let commit = parse_oid(fields[0])?;
        let parents = fields[1..]
            .iter()
            .map(|value| parse_oid(value))
            .collect::<Result<Vec<_>>>()?;
        if db
            .read_object_header_without_replacement(&commit)?
            .map(|(kind, _)| kind)
            != Some(ObjectType::Commit)
            || parents.iter().any(|parent| {
                db.read_object_header_without_replacement(parent)
                    .ok()
                    .flatten()
                    .map(|(kind, _)| kind)
                    != Some(ObjectType::Commit)
            })
        {
            eprintln!("error: malformed graft data: {line}");
            return Err(GitError::Exit(1));
        }
        grafts.push((commit, parents));
    }
    for (commit, parents) in grafts {
        replace_graft_oids(store, db, format, commit, parents, true)?;
    }
    fs::remove_file(graft_path)?;
    Ok(())
}

fn replace_object_type(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    object: &str,
) -> Result<&'static str> {
    let oid = ObjectId::from_hex(format, object)?;
    Ok(db
        .read_object_header(&oid)?
        .map(|(object_type, _)| object_type.as_str())
        .unwrap_or("unknown"))
}
