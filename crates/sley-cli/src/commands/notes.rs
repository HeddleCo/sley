//! `git notes` (add/append/show/list/remove/copy/get-ref) over a notes tree.
//!
//! Notes attach a free-form blob to an arbitrary object. The mapping
//! object-oid -> note-blob is stored as a tree reachable from a notes ref
//! (`refs/notes/commits` by default). Each entry's path is the hex of the
//! annotated object; large note trees fan that path out into nested
//! two-hex-digit subtrees. We read notes from any fanout depth and, like a
//! fresh small repository, write a flat (un-fanned) tree which git reads back
//! identically.

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;
use sley_object::TreeEntries;

/// Default notes ref when none is selected via `--ref`, `GIT_NOTES_REF`, or
/// `core.notesRef`.
const DEFAULT_NOTES_REF: &str = "refs/notes/commits";

pub(crate) fn cmd_notes(args: &[String]) -> Result<()> {
    // Parse the global `--ref <ref>` / `--no-ref` option, which may appear
    // before the subcommand. Everything after the (optional) subcommand is
    // handed to that subcommand's own parser.
    let mut ref_override: Option<String> = None;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        match arg.as_str() {
            "--ref" => {
                let Some(value) = args.get(idx + 1) else {
                    return notes_option_requires_value_error("ref");
                };
                ref_override = Some(value.clone());
                idx += 2;
            }
            "--no-ref" => {
                ref_override = None;
                idx += 1;
            }
            value if value.starts_with("--ref=") => {
                ref_override = Some(value["--ref=".len()..].to_string());
                idx += 1;
            }
            "--" => {
                idx += 1;
                break;
            }
            // A leading non-option token is the subcommand; stop global parsing.
            value if !value.starts_with('-') => break,
            // An unrecognized option in the global position is reported as an
            // unknown option/switch, matching git's parse-options front-end.
            value => return Err(notes_unknown_option(value, NotesUsage::TopLevel)),
        }
    }

    let rest = &args[idx..];
    let (subcommand, sub_args) = match rest.split_first() {
        Some((subcommand, sub_args)) => (subcommand.as_str(), sub_args),
        // `git notes` with no subcommand behaves like `git notes list`.
        None => ("list", &[][..]),
    };

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let notes_ref = resolve_notes_ref(&git_dir, ref_override.as_deref())?;

    match subcommand {
        "list" => notes_list(&git_dir, format, &notes_ref, sub_args),
        "add" => notes_add(&git_dir, format, &notes_ref, sub_args),
        "append" => notes_append(&git_dir, format, &notes_ref, sub_args),
        "show" => notes_show(&git_dir, format, &notes_ref, sub_args),
        "remove" => notes_remove(&git_dir, format, &notes_ref, sub_args),
        "copy" => notes_copy(&git_dir, format, &notes_ref, sub_args),
        "get-ref" => notes_get_ref(&notes_ref, sub_args),
        other => notes_unknown_subcommand_error(other),
    }
}

/// Resolve the notes ref using git's precedence: `--ref` flag, then
/// `GIT_NOTES_REF`, then `core.notesRef`, then the built-in default. A name
/// without a `refs/notes/` prefix is qualified into the `refs/notes/`
/// namespace (matching git's `expand_notes_ref`).
fn resolve_notes_ref(git_dir: &Path, ref_override: Option<&str>) -> Result<String> {
    if let Some(value) = ref_override {
        return Ok(expand_notes_ref(value));
    }
    if let Ok(value) = env::var("GIT_NOTES_REF")
        && !value.is_empty()
    {
        return Ok(expand_notes_ref(&value));
    }
    if let Ok(config) = read_repo_config(git_dir)
        && let Some(value) = config.get("core", None, "notesRef")
        && !value.is_empty()
    {
        return Ok(expand_notes_ref(value));
    }
    Ok(DEFAULT_NOTES_REF.to_string())
}

/// Qualify a notes ref name. Only an already-`refs/notes/`-prefixed name is
/// used verbatim; every other spelling is placed under `refs/notes/` (so
/// `commits` -> `refs/notes/commits`, `refs/heads/x` -> `refs/notes/refs/heads/x`).
fn expand_notes_ref(name: &str) -> String {
    if name.starts_with("refs/notes/") {
        name.to_string()
    } else {
        format!("refs/notes/{name}")
    }
}

// ---------------------------------------------------------------------------
// Notes tree model
// ---------------------------------------------------------------------------

/// A single note: which object it annotates and the note blob's oid.
struct NoteEntry {
    annotated: ObjectId,
    blob: ObjectId,
}

/// Read every note reachable from the notes ref, traversing any fanout layout.
/// Returns entries sorted by annotated-object hex (git's on-tree order). An
/// absent notes ref yields an empty set.
fn read_all_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &str,
) -> Result<Vec<NoteEntry>> {
    let Some(tree_oid) = notes_tree_oid(git_dir, format, store, notes_ref)? else {
        return Ok(Vec::new());
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut out = Vec::new();
    collect_notes(&db, format, &tree_oid, "", &mut out)?;
    out.sort_by_key(|entry| entry.annotated.to_hex());
    Ok(out)
}

/// Recursively walk a notes (sub)tree. `prefix` is the hex accumulated from
/// enclosing fanout directories. Leaf blob entries whose assembled path is a
/// valid object id of the repository's hash become notes; anything else
/// (non-hex names, stray files) is ignored, matching git's tolerant reader.
fn collect_notes(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut Vec<NoteEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let Ok(name) = std::str::from_utf8(entry.name) else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if tree_entry_object_type(entry.mode) == ObjectType::Tree {
            let mut nested = prefix.to_string();
            nested.push_str(name);
            collect_notes(db, format, &entry.oid, &nested, out)?;
        } else {
            let mut hex = prefix.to_string();
            hex.push_str(name);
            if hex.len() != format.hex_len() {
                continue;
            }
            let Ok(annotated) = ObjectId::from_hex(format, &hex) else {
                continue;
            };
            out.push(NoteEntry {
                annotated,
                blob: entry.oid,
            });
        }
    }
    Ok(())
}

/// The note blob oid attached to `target`, if any, across any fanout depth.
fn read_note(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &str,
    target: &ObjectId,
) -> Result<Option<ObjectId>> {
    let target_hex = target.to_hex();
    Ok(read_all_notes(git_dir, format, store, notes_ref)?
        .into_iter()
        .find(|entry| entry.annotated.to_hex() == target_hex)
        .map(|entry| entry.blob))
}

/// Peel the notes ref to its root tree oid. Returns None when the ref is
/// absent (no notes have ever been written to it).
fn notes_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &str,
) -> Result<Option<ObjectId>> {
    let Some(target) = store.read_ref(notes_ref)? else {
        return Ok(None);
    };
    let commit_oid = match target {
        RefTarget::Direct(oid) => oid,
        RefTarget::Symbolic(name) => match store.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => return Ok(None),
        },
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&commit_oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(Some(Commit::parse_ref(format, &object.body)?.tree)),
        ObjectType::Tree => Ok(Some(commit_oid)),
        _ => Ok(None),
    }
}

/// Rewrite the notes tree to exactly `notes` (a flat tree keyed by full hex)
/// and advance the notes ref to a new commit recording it. The new commit's
/// parent is the prior notes commit (if any) and both its message and reflog
/// entry use the supplied subject string. An empty `notes` set still records a
/// commit pointing at the empty tree, matching git (which keeps the ref live
/// rather than deleting it when the last note is removed).
fn write_notes(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &str,
    notes: &[NoteEntry],
    message: &str,
) -> Result<()> {
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);

    let parent = match store.read_ref(notes_ref)? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
    };

    let mut entries: Vec<TreeEntry> = notes
        .iter()
        .map(|note| TreeEntry {
            mode: 0o100644,
            name: note.annotated.to_hex().into_bytes(),
            oid: note.blob.clone(),
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let tree = Tree { entries };
    let tree_oid = db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))?;

    let parents = parent.iter().cloned().collect();
    let committer = commit_identity_from_env("COMMITTER")?;
    let author = commit_identity_from_env("AUTHOR")?;
    let commit_oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree: tree_oid,
            parents,
            author,
            committer: committer.clone(),
            message: format!("{message}\n").into_bytes(),
        },
    )?;

    let old_oid = parent.clone().unwrap_or(zero_oid(format)?);
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: notes_ref.to_string(),
        expected: parent.map(RefTarget::Direct),
        new: RefTarget::Direct(commit_oid.clone()),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: commit_oid,
            committer,
            message: message.as_bytes().to_vec(),
        }),
    });
    tx.commit()?;
    Ok(())
}

/// Replace (or insert) the note for `target` with blob `blob` inside `notes`.
fn upsert_note(notes: &mut Vec<NoteEntry>, target: &ObjectId, blob: ObjectId) {
    let target_hex = target.to_hex();
    if let Some(existing) = notes
        .iter_mut()
        .find(|entry| entry.annotated.to_hex() == target_hex)
    {
        existing.blob = blob;
    } else {
        notes.push(NoteEntry {
            annotated: target.clone(),
            blob,
        });
    }
}

// ---------------------------------------------------------------------------
// Note-content sources (-m / -F / -c / -C)
// ---------------------------------------------------------------------------

/// A single source of note content, in the order it appeared on the command
/// line. `-m`/`-F` content is stripspace-cleaned and paragraph-joined;
/// `-c`/`-C` content is taken from a blob verbatim.
enum NoteContent {
    Message(Vec<u8>),
    File(Vec<u8>),
    Reuse(Vec<u8>),
}

/// Build the final note body from collected content sources. Returns None when
/// no `-m/-F/-c/-C` was given (the caller decides whether that is an error).
/// `-m` and `-F` paragraphs are concatenated with a blank line between them and
/// run through stripspace (trailing whitespace trimmed, blank-line runs
/// collapsed, a single trailing newline ensured); reuse sources are appended
/// verbatim.
fn build_note_body(sources: &[NoteContent]) -> Option<Vec<u8>> {
    if sources.is_empty() {
        return None;
    }
    let mut message_paragraphs: Vec<Vec<u8>> = Vec::new();
    let mut reuse: Vec<u8> = Vec::new();
    let mut have_reuse = false;
    for source in sources {
        match source {
            NoteContent::Message(bytes) | NoteContent::File(bytes) => {
                message_paragraphs.push(bytes.clone());
            }
            NoteContent::Reuse(bytes) => {
                have_reuse = true;
                reuse.extend_from_slice(bytes);
            }
        }
    }
    let mut body = Vec::new();
    if !message_paragraphs.is_empty() {
        let mut joined = Vec::new();
        for (i, paragraph) in message_paragraphs.iter().enumerate() {
            if i != 0 {
                joined.extend_from_slice(b"\n\n");
            }
            joined.extend_from_slice(paragraph);
        }
        body.extend_from_slice(&tag_stripspace_message(&joined, false));
    }
    if have_reuse {
        body.extend_from_slice(&reuse);
    }
    Some(body)
}

/// Read a blob object's bytes for `-c`/`-C`. Non-blob objects are rejected the
/// way git does, echoing the user's original spelling of the object.
fn read_note_blob_content(git_dir: &Path, format: ObjectFormat, spec: &str) -> Result<Vec<u8>> {
    let oid = resolve_note_object(git_dir, format, spec)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Blob {
        eprintln!("fatal: cannot read note data from non-blob object '{spec}'.");
        return Err(GitError::Exit(128));
    }
    Ok(object.body.clone())
}

/// Parsed flags shared by `add` and `append`.
struct EditOptions {
    contents: Vec<NoteContent>,
    force: bool,
    allow_empty: bool,
    object: Option<String>,
}

/// Parse the option/positional grammar common to `add` and `append`.
/// `allow_force` controls whether `-f`/`--force` is accepted (only `add`/`copy`
/// take it). Content options preserve relative order so paragraph joining and
/// verbatim reuse interleave exactly as on the command line.
fn parse_edit_options(
    git_dir: &Path,
    format: ObjectFormat,
    args: &[String],
    allow_force: bool,
    usage: NotesUsage,
) -> Result<EditOptions> {
    let mut contents = Vec::new();
    let mut force = false;
    let mut allow_empty = false;
    let mut object = None;
    let mut iter = args.iter();
    let mut positional_only = false;
    while let Some(arg) = iter.next() {
        if positional_only {
            object = Some(set_single_object(object, arg, usage)?);
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-m" | "--message" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                contents.push(NoteContent::Message(value.as_bytes().to_vec()));
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                contents.push(NoteContent::Message(value.as_bytes()[2..].to_vec()));
            }
            value if value.starts_with("--message=") => {
                contents.push(NoteContent::Message(
                    value.as_bytes()["--message=".len()..].to_vec(),
                ));
            }
            "-F" | "--file" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                contents.push(NoteContent::File(read_commit_message_file(value)?));
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                contents.push(NoteContent::File(read_commit_message_file(&value[2..])?));
            }
            value if value.starts_with("--file=") => {
                contents.push(NoteContent::File(read_commit_message_file(
                    &value["--file=".len()..],
                )?));
            }
            "-C" | "--reuse-message" | "-c" | "--reedit-message" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                contents.push(NoteContent::Reuse(read_note_blob_content(
                    git_dir, format, value,
                )?));
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                contents.push(NoteContent::Reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value[2..],
                )?));
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                contents.push(NoteContent::Reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value[2..],
                )?));
            }
            value if value.starts_with("--reuse-message=") => {
                contents.push(NoteContent::Reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value["--reuse-message=".len()..],
                )?));
            }
            value if value.starts_with("--reedit-message=") => {
                contents.push(NoteContent::Reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value["--reedit-message=".len()..],
                )?));
            }
            "-f" | "--force" if allow_force => force = true,
            "--allow-empty" => allow_empty = true,
            "--no-allow-empty" => allow_empty = false,
            // Accepted, no-op flags so common invocations parse cleanly.
            "-e" | "--edit" | "--no-edit" => {}
            "--stripspace" | "--no-stripspace" => {}
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, usage));
            }
            value => object = Some(set_single_object(object, value, usage)?),
        }
    }
    Ok(EditOptions {
        contents,
        force,
        allow_empty,
        object,
    })
}

/// Enforce that at most one positional object is given; a second one is the
/// `error: too many arguments` usage failure git reports (exit 129).
fn set_single_object(
    existing: Option<String>,
    candidate: &str,
    usage: NotesUsage,
) -> Result<String> {
    if existing.is_some() {
        return Err(notes_too_many_arguments(usage));
    }
    Ok(candidate.to_string())
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn notes_list(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let object = parse_optional_single_object(args, NotesUsage::List)?;
    let store = FileRefStore::new(git_dir, format);
    if let Some(spec) = object {
        // `list <object>` prints just the note blob oid, or errors if absent.
        let target = resolve_note_object(git_dir, format, &spec)?;
        match read_note(git_dir, format, &store, notes_ref, &target)? {
            Some(blob) => {
                println!("{}", blob.to_hex());
                Ok(())
            }
            None => {
                eprintln!("error: no note found for object {}.", target.to_hex());
                Err(GitError::Exit(1))
            }
        }
    } else {
        // `list` (all) prints "<note-blob> <annotated-object>" per note.
        for note in read_all_notes(git_dir, format, &store, notes_ref)? {
            println!("{} {}", note.blob.to_hex(), note.annotated.to_hex());
        }
        Ok(())
    }
}

fn notes_show(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let spec =
        parse_optional_single_object(args, NotesUsage::Show)?.unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);
    let Some(blob) = read_note(git_dir, format, &store, notes_ref, &target)? else {
        eprintln!("error: no note found for object {}.", target.to_hex());
        return Err(GitError::Exit(1));
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&blob)?;
    io::stdout().write_all(&object.body)?;
    io::stdout().flush()?;
    Ok(())
}

fn notes_add(git_dir: &Path, format: ObjectFormat, notes_ref: &str, args: &[String]) -> Result<()> {
    let options = parse_edit_options(git_dir, format, args, true, NotesUsage::Add)?;
    let spec = options.object.unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);

    let existing = read_note(git_dir, format, &store, notes_ref, &target)?;
    if existing.is_some() && !options.force {
        eprintln!(
            "error: Cannot add notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
            target.to_hex()
        );
        return Err(GitError::Exit(1));
    }
    if existing.is_some() && options.force {
        eprintln!("Overwriting existing notes for object {}", target.to_hex());
    }

    let Some(body) = build_note_body(&options.contents) else {
        // No -m/-F/-c/-C: git would open an editor. We do not run editors, so
        // surface a clear, non-zero failure rather than silently doing nothing.
        return Err(GitError::Command(
            "git notes add without -m/-F/-c/-C is not supported (editor unavailable)".into(),
        ));
    };

    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    if body.is_empty() && !options.allow_empty {
        // Without --allow-empty, empty content removes any existing note (git
        // emits a "Removing note" line keyed by the resolved oid) and is a
        // no-op when there was nothing to remove.
        if existing.is_some() {
            eprintln!("Removing note for object {}", target.to_hex());
            let mut notes = read_all_notes(git_dir, format, &store, notes_ref)?;
            let target_hex = target.to_hex();
            notes.retain(|entry| entry.annotated.to_hex() != target_hex);
            write_notes(
                git_dir,
                format,
                &store,
                notes_ref,
                &notes,
                "Notes removed by 'git notes add'",
            )?;
        }
        return Ok(());
    }
    let blob = db.write_object(EncodedObject::new(ObjectType::Blob, body))?;
    let mut notes = read_all_notes(git_dir, format, &store, notes_ref)?;
    upsert_note(&mut notes, &target, blob);
    write_notes(
        git_dir,
        format,
        &store,
        notes_ref,
        &notes,
        "Notes added by 'git notes add'",
    )
}

fn notes_append(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let options = parse_edit_options(git_dir, format, args, false, NotesUsage::Append)?;
    let spec = options.object.unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);

    let Some(appended) = build_note_body(&options.contents) else {
        return Err(GitError::Command(
            "git notes append without -m/-F/-c/-C is not supported (editor unavailable)".into(),
        ));
    };

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let existing = read_note(git_dir, format, &store, notes_ref, &target)?;
    let mut body = Vec::new();
    if let Some(blob) = &existing {
        let object = db.read_object(blob)?;
        body.extend_from_slice(&object.body);
    }
    if !body.is_empty() && !appended.is_empty() {
        // Separate prior content from the new paragraph with a blank line, as
        // git does, normalizing the existing trailing newline first.
        while body.last() == Some(&b'\n') {
            body.pop();
        }
        body.extend_from_slice(b"\n\n");
    }
    body.extend_from_slice(&appended);

    if body.is_empty() {
        return Ok(());
    }

    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let blob = db.write_object(EncodedObject::new(ObjectType::Blob, body))?;
    let mut notes = read_all_notes(git_dir, format, &store, notes_ref)?;
    upsert_note(&mut notes, &target, blob);
    write_notes(
        git_dir,
        format,
        &store,
        notes_ref,
        &notes,
        "Notes added by 'git notes append'",
    )
}

fn notes_remove(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let mut ignore_missing = false;
    let mut from_stdin = false;
    let mut specs: Vec<String> = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            specs.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--ignore-missing" => ignore_missing = true,
            "--no-ignore-missing" => ignore_missing = false,
            "--stdin" => from_stdin = true,
            "--no-stdin" => from_stdin = false,
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, NotesUsage::Remove));
            }
            value => specs.push(value.to_string()),
        }
    }
    if from_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        for line in input.split_whitespace() {
            specs.push(line.to_string());
        }
    }
    if specs.is_empty() {
        specs.push("HEAD".to_string());
    }

    let store = FileRefStore::new(git_dir, format);
    let mut notes = read_all_notes(git_dir, format, &store, notes_ref)?;
    let mut any_missing = false;
    let mut removed_any = false;
    for spec in &specs {
        let target = resolve_note_object(git_dir, format, spec)?;
        let target_hex = target.to_hex();
        let had_note = notes
            .iter()
            .any(|entry| entry.annotated.to_hex() == target_hex);
        if had_note {
            // git echoes the user's spelling of the object, not the full oid.
            eprintln!("Removing note for object {spec}");
            notes.retain(|entry| entry.annotated.to_hex() != target_hex);
            removed_any = true;
        } else {
            eprintln!("Object {spec} has no note");
            if !ignore_missing {
                any_missing = true;
            }
        }
    }
    if removed_any {
        write_notes(
            git_dir,
            format,
            &store,
            notes_ref,
            &notes,
            "Notes removed by 'git notes remove'",
        )?;
    }
    if any_missing {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn notes_copy(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let mut force = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            positionals.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, NotesUsage::Copy));
            }
            value => positionals.push(value.to_string()),
        }
    }
    // 0 args is a usage error; 1 arg copies onto HEAD; 2 args copy from->to;
    // anything more is "too many arguments". (--stdin batch copy is not
    // supported and is rejected as an unknown option above.)
    let (from_spec, to_spec) = match positionals.as_slice() {
        [from] => (from.clone(), "HEAD".to_string()),
        [from, to] => (from.clone(), to.clone()),
        [] => {
            eprintln!("error: too few arguments");
            print_notes_usage(NotesUsage::Copy);
            return Err(GitError::Exit(129));
        }
        _ => return Err(notes_too_many_arguments(NotesUsage::Copy)),
    };

    let from = resolve_note_object(git_dir, format, &from_spec)?;
    let to = resolve_note_object(git_dir, format, &to_spec)?;
    let store = FileRefStore::new(git_dir, format);

    // git checks the destination's existing-note guard before reading the
    // source note, so mirror that ordering for matching error precedence.
    let existing = read_note(git_dir, format, &store, notes_ref, &to)?;
    if existing.is_some() && !force {
        eprintln!(
            "error: Cannot copy notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
            to.to_hex()
        );
        return Err(GitError::Exit(1));
    }
    let Some(source_blob) = read_note(git_dir, format, &store, notes_ref, &from)? else {
        eprintln!(
            "error: missing notes on source object {}. Cannot copy.",
            from.to_hex()
        );
        return Err(GitError::Exit(1));
    };
    if existing.is_some() && force {
        eprintln!("Overwriting existing notes for object {}", to.to_hex());
    }

    let mut notes = read_all_notes(git_dir, format, &store, notes_ref)?;
    upsert_note(&mut notes, &to, source_blob);
    write_notes(
        git_dir,
        format,
        &store,
        notes_ref,
        &notes,
        "Notes added by 'git notes copy'",
    )
}

fn notes_get_ref(notes_ref: &str, args: &[String]) -> Result<()> {
    // get-ref takes no parameters: an unknown option is reported as such, while
    // any stray positional is "too many arguments".
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            return Err(notes_too_many_arguments(NotesUsage::GetRef));
        }
        match arg.as_str() {
            "--" => positional_only = true,
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, NotesUsage::GetRef));
            }
            _ => return Err(notes_too_many_arguments(NotesUsage::GetRef)),
        }
    }
    println!("{notes_ref}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Parse subcommands that accept at most one optional `<object>` and no flags
/// other than `--` (list/show). Extra positionals produce git's
/// `error: too many arguments` usage failure.
fn parse_optional_single_object(args: &[String], usage: NotesUsage) -> Result<Option<String>> {
    let mut object = None;
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            object = Some(set_single_object(object, arg, usage)?);
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, usage));
            }
            value => object = Some(set_single_object(object, value, usage)?),
        }
    }
    Ok(object)
}

/// Resolve an object spec for notes, mapping resolution failures to git's
/// notes-specific lowercase `failed to resolve` message (exit 128).
fn resolve_note_object(git_dir: &Path, format: ObjectFormat, spec: &str) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, spec) {
        Ok(oid) => Ok(oid),
        Err(
            GitError::NotFound(_)
            | GitError::InvalidFormat(_)
            | GitError::InvalidPath(_)
            | GitError::InvalidObjectId(_),
        ) => {
            eprintln!("fatal: failed to resolve '{spec}' as a valid ref.");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Errors / usage
// ---------------------------------------------------------------------------

/// Which usage block to print when a particular parse fails. git shows the
/// top-level synopsis for an unknown subcommand and a focused, subcommand-
/// specific block for option/argument errors within a subcommand.
#[derive(Clone, Copy)]
enum NotesUsage {
    TopLevel,
    List,
    Add,
    Append,
    Copy,
    Show,
    Remove,
    GetRef,
}

fn notes_unknown_subcommand_error(subcommand: &str) -> Result<()> {
    eprintln!("error: unknown subcommand: `{subcommand}'");
    print_notes_usage(NotesUsage::TopLevel);
    Err(GitError::Exit(129))
}

/// Build the error for an unrecognized option, distinguishing a long `--opt`
/// ("unknown option") from a short `-x` cluster ("unknown switch `x'"), as
/// git's parse-options front-end does, after printing the relevant usage.
/// Returns the `GitError` so callers in any return-type context can
/// `return Err(...)`.
fn notes_unknown_option(option: &str, usage: NotesUsage) -> GitError {
    if let Some(long) = option.strip_prefix("--") {
        eprintln!("error: unknown option `{long}'");
    } else if let Some(short) = option.strip_prefix('-') {
        // Report the first unrecognized switch character in the cluster.
        let switch = short.chars().next().unwrap_or('-');
        eprintln!("error: unknown switch `{switch}'");
    } else {
        eprintln!("error: unknown option `{option}'");
    }
    print_notes_usage(usage);
    GitError::Exit(129)
}

fn notes_option_requires_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

fn notes_message_requires_value_error(flag: &str) -> Result<EditOptions> {
    if let Some(short) = flag.strip_prefix('-').filter(|rest| rest.len() == 1) {
        eprintln!("error: switch `{short}' requires a value");
    } else {
        eprintln!(
            "error: option `{}' requires a value",
            flag.trim_start_matches('-')
        );
    }
    Err(GitError::Exit(129))
}

/// `error: too many arguments` followed by the subcommand usage (exit 129),
/// matching git's behavior for excess positionals. Returns the `GitError` so
/// callers in any return-type context can `return Err(...)`.
fn notes_too_many_arguments(usage: NotesUsage) -> GitError {
    eprintln!("error: too many arguments");
    print_notes_usage(usage);
    GitError::Exit(129)
}

fn print_notes_usage(usage: NotesUsage) {
    let text = match usage {
        NotesUsage::TopLevel => {
            r#"usage: git notes [--ref <notes-ref>] [list [<object>]]
   or: git notes [--ref <notes-ref>] add [-f] [--allow-empty] [--[no-]separator|--separator=<paragraph-break>] [--[no-]stripspace] [-m <msg> | -F <file> | (-c | -C) <object>] [<object>] [-e]
   or: git notes [--ref <notes-ref>] copy [-f] <from-object> <to-object>
   or: git notes [--ref <notes-ref>] append [--allow-empty] [--[no-]separator|--separator=<paragraph-break>] [--[no-]stripspace] [-m <msg> | -F <file> | (-c | -C) <object>] [<object>] [-e]
   or: git notes [--ref <notes-ref>] edit [--allow-empty] [<object>]
   or: git notes [--ref <notes-ref>] show [<object>]
   or: git notes [--ref <notes-ref>] merge [-v | -q] [-s <strategy>] <notes-ref>
   or: git notes merge --commit [-v | -q]
   or: git notes merge --abort [-v | -q]
   or: git notes [--ref <notes-ref>] remove [<object>...]
   or: git notes [--ref <notes-ref>] prune [-n] [-v]
   or: git notes [--ref <notes-ref>] get-ref

    --[no-]ref <notes-ref>
                          use notes from <notes-ref>

"#
        }
        NotesUsage::List => "usage: git notes [list [<object>]]\n\n",
        NotesUsage::Add => {
            r#"usage: git notes add [<options>] [<object>]

    -m, --message <message>
                          note contents as a string
    -F, --file <file>     note contents in a file
    -c, --reedit-message <object>
                          reuse and edit specified note object
    -e, --[no-]edit       edit note message in editor
    -C, --reuse-message <object>
                          reuse specified note object
    --[no-]allow-empty    allow storing empty note
    -f, --[no-]force      replace existing notes
    --[no-]separator[=<paragraph-break>]
                          insert <paragraph-break> between paragraphs
    --[no-]stripspace     remove unnecessary whitespace

"#
        }
        NotesUsage::Append => {
            r#"usage: git notes append [<options>] [<object>]

    -m, --message <message>
                          note contents as a string
    -F, --file <file>     note contents in a file
    -c, --reedit-message <object>
                          reuse and edit specified note object
    -C, --reuse-message <object>
                          reuse specified note object
    -e, --[no-]edit       edit note message in editor
    --[no-]allow-empty    allow storing empty note
    --[no-]separator[=<paragraph-break>]
                          insert <paragraph-break> between paragraphs
    --[no-]stripspace     remove unnecessary whitespace

"#
        }
        NotesUsage::Copy => {
            r#"usage: git notes copy [<options>] <from-object> <to-object>
   or: git notes copy --stdin [<from-object> <to-object>]...

    -f, --[no-]force      replace existing notes
    --[no-]stdin          read objects from stdin
    --[no-]for-rewrite <command>
                          load rewriting config for <command> (implies --stdin)

"#
        }
        NotesUsage::Show => "usage: git notes show [<object>]\n\n",
        NotesUsage::Remove => {
            r#"usage: git notes remove [<object>]

    --[no-]ignore-missing attempt to remove non-existent note is not an error
    --[no-]stdin          read object names from the standard input

"#
        }
        NotesUsage::GetRef => "usage: git notes get-ref\n\n",
    };
    eprint!("{text}");
}
