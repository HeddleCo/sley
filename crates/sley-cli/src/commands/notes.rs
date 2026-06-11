//! `git notes` (add/append/show/list/remove/copy/get-ref) over a notes tree.

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;
use sley_notes::{
    NotesCommitIdentity, NotesRef, list_notes, notes_ref_expected, read_note, remove_note,
    resolve_notes_ref, upsert_note, write_notes,
};

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
    let notes_ref = resolve_notes_ref(&git_dir, ref_override.as_deref())?
        .as_str()
        .to_string();

    // git refuses to write notes outside of refs/notes/. The check uses the
    // *resolved* ref name: `--ref` is expanded (bare names → refs/notes/<name>),
    // but GIT_NOTES_REF / core.notesRef are taken verbatim, so a fully-qualified
    // non-notes ref like `refs/heads/bogus` from the environment is rejected.
    let raw_write_ref = raw_notes_ref(&git_dir, ref_override.as_deref());
    let refuse_outside = |verb: &str| -> Result<()> {
        if !raw_write_ref.starts_with("refs/notes/") {
            eprintln!(
                "fatal: refusing to {verb} notes in {raw_write_ref} (outside of refs/notes/)"
            );
            return Err(GitError::Exit(128));
        }
        Ok(())
    };

    match subcommand {
        "list" => notes_list(&git_dir, format, &notes_ref, sub_args),
        "add" => {
            refuse_outside("add")?;
            notes_add(&git_dir, format, &notes_ref, sub_args)
        }
        "edit" => {
            refuse_outside("edit")?;
            notes_edit(&git_dir, format, &notes_ref, sub_args)
        }
        "append" => {
            refuse_outside("append")?;
            notes_append(&git_dir, format, &notes_ref, sub_args)
        }
        "show" => notes_show(&git_dir, format, &notes_ref, sub_args),
        "remove" => {
            refuse_outside("remove")?;
            notes_remove(&git_dir, format, &notes_ref, sub_args)
        }
        "copy" => {
            refuse_outside("copy")?;
            notes_copy(&git_dir, format, &notes_ref, sub_args)
        }
        "get-ref" => notes_get_ref(&notes_ref, sub_args),
        other => notes_unknown_subcommand_error(other),
    }
}

fn notes_ref_handle(notes_ref: &str) -> NotesRef {
    NotesRef::expand(notes_ref)
}

/// The notes ref name as git's `init_notes_check` sees it for the
/// outside-refs/notes refusal. `--ref` is run through `expand_notes_ref` (bare
/// names gain a `refs/notes/` prefix), but `GIT_NOTES_REF` / `core.notesRef` are
/// taken verbatim — a fully-qualified non-notes ref from the environment must
/// be rejected rather than silently re-homed under `refs/notes/`.
pub(crate) fn raw_notes_ref(git_dir: &Path, ref_override: Option<&str>) -> String {
    if let Some(value) = ref_override {
        return NotesRef::expand(value).as_str().to_string();
    }
    if let Ok(value) = env::var("GIT_NOTES_REF")
        && !value.is_empty()
    {
        return value;
    }
    if let Ok(config) = read_repo_config(git_dir)
        && let Some(value) = config.get("core", None, "notesRef")
        && !value.is_empty()
    {
        return value.to_string();
    }
    "refs/notes/commits".to_string()
}

fn notes_commit_identity() -> Result<NotesCommitIdentity> {
    Ok(NotesCommitIdentity {
        author: commit_identity_from_env("AUTHOR")?,
        committer: commit_identity_from_env("COMMITTER")?,
    })
}

/// Resolve `git`'s editor command for `git notes`, mirroring git's precedence:
/// `GIT_EDITOR`, then `core.editor`, then `VISUAL`/`EDITOR`, then the built-in
/// default. `false`/empty disables editing (handled by the caller).
fn note_editor_command() -> Option<String> {
    if let Ok(value) = env::var("GIT_EDITOR") {
        return Some(value);
    }
    if let Ok(Some(value)) = global_config_value("core.editor") {
        return Some(value);
    }
    if let Some(config) = identity_effective_config()
        && let Some(value) = config.get("core", None, "editor")
    {
        return Some(value.to_string());
    }
    if let Ok(value) = env::var("VISUAL")
        && !value.is_empty()
    {
        return Some(value);
    }
    if let Ok(value) = env::var("EDITOR")
        && !value.is_empty()
    {
        return Some(value);
    }
    None
}

/// git's `note_template` comment block written into `NOTES_EDITMSG` before the
/// editor runs.
const NOTE_TEMPLATE: &str = "\nWrite/edit the notes for the following object:\n";

/// Run the editor flow for a note (`prepare_note_data` in git): seed
/// `$GIT_DIR/NOTES_EDITMSG` with the prior buffer (or the old note for `edit`),
/// append the commented template, launch the editor, read the result back,
/// stripspace it (dropping comment lines), and unlink the file. Returns the
/// edited note body. `seed` is the pre-editor buffer (concatenated -m/-F/-c/-C
/// content); `old_note` supplies the initial body for a bare `edit` with no
/// content sources.
fn launch_note_editor(git_dir: &Path, seed: &[u8], old_note: Option<&[u8]>) -> Result<Vec<u8>> {
    let edit_path = git_dir.join("NOTES_EDITMSG");

    let mut template = Vec::new();
    if !seed.is_empty() {
        template.extend_from_slice(seed);
    } else if let Some(note) = old_note {
        template.extend_from_slice(note);
    }
    // Commented template block (matches git's strbuf_add_commented_lines output:
    // a leading blank, then each template line prefixed with "# ").
    template.push(b'\n');
    for line in format!("\n{NOTE_TEMPLATE}\n").split_inclusive('\n') {
        if line == "\n" {
            template.extend_from_slice(b"#\n");
        } else {
            template.extend_from_slice(b"# ");
            template.extend_from_slice(line.as_bytes());
        }
    }
    fs::write(&edit_path, &template)?;

    let Some(editor) = note_editor_command() else {
        let _ = fs::remove_file(&edit_path);
        eprintln!("fatal: please supply the note contents using either -m or -F option");
        return Err(GitError::Exit(128));
    };
    if editor == "false" || editor == ":" {
        let _ = fs::remove_file(&edit_path);
        eprintln!("fatal: please supply the note contents using either -m or -F option");
        return Err(GitError::Exit(128));
    }

    // git runs the editor via the shell as `<editor> <path>`.
    let status = ProcessCommand::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\"", editor = editor))
        .arg(&editor)
        .arg(&edit_path)
        .status();
    let ok = matches!(status, Ok(status) if status.success());
    if !ok {
        let _ = fs::remove_file(&edit_path);
        eprintln!("fatal: please supply the note contents using either -m or -F option");
        return Err(GitError::Exit(128));
    }

    let edited = fs::read(&edit_path).unwrap_or_default();
    let _ = fs::remove_file(&edit_path);
    // Default stripspace strips comment lines and normalizes whitespace.
    Ok(tag_stripspace_message(&edited, true))
}

// ---------------------------------------------------------------------------
// Note-content sources (-m / -F / -c / -C)
// ---------------------------------------------------------------------------

/// A single source of note content, in the order it appeared on the command
/// line, mirroring git's `struct note_msg`. `-m`/`-F` content is stripspaced
/// when concatenated; `-c`/`-C` (reuse) content is taken from a blob verbatim
/// (`NO_STRIPSPACE`).
struct NoteContent {
    bytes: Vec<u8>,
    /// Whether this source participates in stripspace under the default
    /// (unspecified) stripspace setting. `-m`/`-F` → true; `-c`/`-C` → false.
    stripspace: bool,
}

impl NoteContent {
    fn message(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            stripspace: true,
        }
    }
    fn reuse(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            stripspace: false,
        }
    }
}

/// The `--separator` / `--no-separator` setting. git's `separator` global
/// defaults to `"\n"`; `--no-separator` clears it to `None`; `--separator[=X]`
/// sets it to `X` (or `"\n"` when given without an argument).
#[derive(Clone)]
struct Separator(Option<Vec<u8>>);

impl Default for Separator {
    fn default() -> Self {
        Separator(Some(b"\n".to_vec()))
    }
}

impl Separator {
    /// Append the separator to `message`, matching git's `append_separator`:
    /// nothing when unset; the separator verbatim when it ends in `\n`;
    /// otherwise the separator followed by a single `\n`.
    fn append_to(&self, message: &mut Vec<u8>) {
        let Some(sep) = &self.0 else { return };
        if sep.last() == Some(&b'\n') {
            message.extend_from_slice(sep);
        } else {
            message.extend_from_slice(sep);
            message.push(b'\n');
        }
    }
}

/// Concatenate collected content sources exactly like git's `concat_messages`:
/// each source is preceded by the separator when the running buffer is
/// non-empty, and the whole buffer is stripspaced after a source whose
/// stripspace flag is set (the default-unspecified stripspace path). Returns
/// None when no `-m/-F/-c/-C` was given.
fn build_note_body(sources: &[NoteContent], separator: &Separator) -> Option<Vec<u8>> {
    if sources.is_empty() {
        return None;
    }
    let mut body: Vec<u8> = Vec::new();
    for source in sources {
        if !body.is_empty() {
            separator.append_to(&mut body);
        }
        body.extend_from_slice(&source.bytes);
        if source.stripspace {
            body = tag_stripspace_message(&body, false);
        }
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

/// Parsed flags shared by `add`, `append`, and `edit`.
struct EditOptions {
    contents: Vec<NoteContent>,
    force: bool,
    allow_empty: bool,
    object: Option<String>,
    separator: Separator,
    /// Set by `-e`/`--edit` and implicitly by `-c`/`--reedit-message`.
    use_editor: bool,
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
    let mut separator = Separator::default();
    let mut use_editor = false;
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
                contents.push(NoteContent::message(value.as_bytes().to_vec()));
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                contents.push(NoteContent::message(value.as_bytes()[2..].to_vec()));
            }
            value if value.starts_with("--message=") => {
                contents.push(NoteContent::message(
                    value.as_bytes()["--message=".len()..].to_vec(),
                ));
            }
            "-F" | "--file" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                contents.push(NoteContent::message(read_commit_message_file(value)?));
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                contents.push(NoteContent::message(read_commit_message_file(&value[2..])?));
            }
            value if value.starts_with("--file=") => {
                contents.push(NoteContent::message(read_commit_message_file(
                    &value["--file=".len()..],
                )?));
            }
            // `-C`/`--reuse-message` reuses a blob verbatim. `-c`/`--reedit-message`
            // additionally turns on the editor (it is "reuse and edit").
            "-C" | "--reuse-message" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir, format, value,
                )?));
            }
            "-c" | "--reedit-message" => {
                let Some(value) = iter.next() else {
                    return notes_message_requires_value_error(arg);
                };
                use_editor = true;
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir, format, value,
                )?));
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value[2..],
                )?));
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                use_editor = true;
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value[2..],
                )?));
            }
            value if value.starts_with("--reuse-message=") => {
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value["--reuse-message=".len()..],
                )?));
            }
            value if value.starts_with("--reedit-message=") => {
                use_editor = true;
                contents.push(NoteContent::reuse(read_note_blob_content(
                    git_dir,
                    format,
                    &value["--reedit-message=".len()..],
                )?));
            }
            "-f" | "--force" if allow_force => force = true,
            "--allow-empty" => allow_empty = true,
            "--no-allow-empty" => allow_empty = false,
            "-e" | "--edit" => use_editor = true,
            "--no-edit" => use_editor = false,
            // `--separator` takes an optional argument (PARSE_OPT_OPTARG): only the
            // stuck `--separator=<x>` form supplies it; the bare flag defaults to a
            // single newline and never consumes a following token.
            "--separator" => separator = Separator(Some(b"\n".to_vec())),
            value if value.starts_with("--separator=") => {
                separator = Separator(Some(value.as_bytes()["--separator=".len()..].to_vec()));
            }
            "--no-separator" => separator = Separator(None),
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
        separator,
        use_editor,
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
        match read_note(
            git_dir,
            format,
            &store,
            &notes_ref_handle(notes_ref),
            &target,
        )? {
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
        for note in list_notes(git_dir, format, &store, &notes_ref_handle(notes_ref))? {
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
    let Some(blob) = read_note(
        git_dir,
        format,
        &store,
        &notes_ref_handle(notes_ref),
        &target,
    )?
    else {
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
    let has_messages = !options.contents.is_empty();
    let spec = options.object.clone().unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);

    let existing = read_note(
        git_dir,
        format,
        &store,
        &notes_ref_handle(notes_ref),
        &target,
    )?;
    if existing.is_some() && !options.force {
        if has_messages {
            eprintln!(
                "error: Cannot add notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
                target.to_hex()
            );
            return Err(GitError::Exit(1));
        }
        // No -m/-F/-c/-C and no -f: git redirects to the `edit` subcommand.
        return notes_edit(git_dir, format, notes_ref, args);
    }
    if existing.is_some() && options.force {
        eprintln!("Overwriting existing notes for object {}", target.to_hex());
    }

    // Concatenate content sources, then run the editor when requested (or when
    // no content was supplied at all, which is git's default add path).
    let mut body = build_note_body(&options.contents, &options.separator).unwrap_or_default();
    if options.use_editor || !has_messages {
        body = launch_note_editor(git_dir, &body, None)?;
    }

    write_note_or_remove(
        git_dir,
        format,
        &store,
        notes_ref,
        &target,
        &spec,
        body,
        options.allow_empty,
        existing.is_some(),
        "add",
    )
}

/// Store `body` as the note for `target`, or remove the existing note when the
/// body is empty and `--allow-empty` was not given (git's add/append/edit
/// shared tail). Emits the matching "Removing note" diagnostic and commit
/// message verb.
#[allow(clippy::too_many_arguments)]
fn write_note_or_remove(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    notes_ref: &str,
    target: &ObjectId,
    spec: &str,
    body: Vec<u8>,
    allow_empty: bool,
    had_existing: bool,
    verb: &str,
) -> Result<()> {
    let handle = notes_ref_handle(notes_ref);
    if body.is_empty() && !allow_empty {
        if had_existing {
            eprintln!("Removing note for object {spec}");
            let mut notes = list_notes(git_dir, format, store, &handle)?;
            remove_note(&mut notes, target);
            write_notes(
                git_dir,
                format,
                store,
                &handle,
                &notes,
                &format!("Notes removed by 'git notes {verb}'"),
                &notes_commit_identity()?,
                notes_ref_expected(store, &handle)?,
            )?;
        }
        return Ok(());
    }
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let blob = db.write_object(EncodedObject::new(ObjectType::Blob, body))?;
    let mut notes = list_notes(git_dir, format, store, &handle)?;
    upsert_note(&mut notes, target, blob);
    write_notes(
        git_dir,
        format,
        store,
        &handle,
        &notes,
        &format!("Notes added by 'git notes {verb}'"),
        &notes_commit_identity()?,
        notes_ref_expected(store, &handle)?,
    )
}

/// `git notes edit [<object>]`: replace the note for `<object>` with the result
/// of editing it (or the supplied -m/-F/-c/-C content) in the editor.
fn notes_edit(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let options = parse_edit_options(git_dir, format, args, false, NotesUsage::Edit)?;
    let has_messages = !options.contents.is_empty();
    if has_messages {
        eprintln!(
            "The -m/-F/-c/-C options have been deprecated for the 'edit' subcommand.\nPlease use 'git notes add -f -m/-F/-c/-C' instead."
        );
    }
    let spec = options.object.clone().unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);
    let existing = read_note(
        git_dir,
        format,
        &store,
        &notes_ref_handle(notes_ref),
        &target,
    )?;

    // edit always opens the editor (use_editor || !msg_nr is always true here
    // because edit has no non-editor path). Seed with concatenated content, or
    // with the prior note when there was no content.
    let seed = build_note_body(&options.contents, &options.separator).unwrap_or_default();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let old_note = match &existing {
        Some(blob) => Some(db.read_object(blob)?.body.clone()),
        None => None,
    };
    let body = launch_note_editor(git_dir, &seed, old_note.as_deref())?;

    write_note_or_remove(
        git_dir,
        format,
        &store,
        notes_ref,
        &target,
        &spec,
        body,
        options.allow_empty,
        existing.is_some(),
        "edit",
    )
}

fn notes_append(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    args: &[String],
) -> Result<()> {
    let options = parse_edit_options(git_dir, format, args, false, NotesUsage::Append)?;
    let has_messages = !options.contents.is_empty();
    let spec = options.object.clone().unwrap_or_else(|| "HEAD".to_string());
    let target = resolve_note_object(git_dir, format, &spec)?;
    let store = FileRefStore::new(git_dir, format);

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let existing = read_note(
        git_dir,
        format,
        &store,
        &notes_ref_handle(notes_ref),
        &target,
    )?;

    // Concatenate the new content, then run the editor when requested (or when
    // no content was supplied). For append, the editor is seeded only with the
    // new content (not the prior note); the prior note is prepended afterwards.
    let mut appended =
        build_note_body(&options.contents, &options.separator).unwrap_or_default();
    if options.use_editor || !has_messages {
        appended = launch_note_editor(git_dir, &appended, None)?;
    }

    // Prepend the existing note, separated from the new content with the
    // separator when both are non-empty (git's `append_separator`).
    let mut body = Vec::new();
    if let Some(blob) = &existing {
        let object = db.read_object(blob)?;
        body.extend_from_slice(&object.body);
        if !appended.is_empty() && !object.body.is_empty() {
            options.separator.append_to(&mut body);
        }
    }
    body.extend_from_slice(&appended);

    write_note_or_remove(
        git_dir,
        format,
        &store,
        notes_ref,
        &target,
        &spec,
        body,
        options.allow_empty,
        existing.is_some(),
        "append",
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
    let mut notes = list_notes(git_dir, format, &store, &notes_ref_handle(notes_ref))?;
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
            remove_note(&mut notes, &target);
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
            &notes_ref_handle(notes_ref),
            &notes,
            "Notes removed by 'git notes remove'",
            &notes_commit_identity()?,
            notes_ref_expected(&store, &notes_ref_handle(notes_ref))?,
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
    let mut from_stdin = false;
    let mut rewrite_cmd: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            positionals.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "--stdin" => from_stdin = true,
            "--no-stdin" => from_stdin = false,
            "--for-rewrite" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `for-rewrite' requires a value");
                    return Err(GitError::Exit(129));
                };
                rewrite_cmd = Some(value.clone());
            }
            value if value.starts_with("--for-rewrite=") => {
                rewrite_cmd = Some(value["--for-rewrite=".len()..].to_string());
            }
            value if value.starts_with('-') && value.len() > 1 && value != "-" => {
                return Err(notes_unknown_option(value, NotesUsage::Copy));
            }
            value => positionals.push(value.to_string()),
        }
    }

    // `--stdin` / `--for-rewrite` batch-copy from stdin; any positionals are a
    // usage error in that mode.
    if from_stdin || rewrite_cmd.is_some() {
        if !positionals.is_empty() {
            return Err(notes_too_many_arguments(NotesUsage::Copy));
        }
        return notes_copy_from_stdin(git_dir, format, notes_ref, force, rewrite_cmd.as_deref());
    }

    // 0 args is a usage error; 1 arg copies onto HEAD; 2 args copy from->to;
    // anything more is "too many arguments".
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
    let existing = read_note(git_dir, format, &store, &notes_ref_handle(notes_ref), &to)?;
    if existing.is_some() && !force {
        eprintln!(
            "error: Cannot copy notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
            to.to_hex()
        );
        return Err(GitError::Exit(1));
    }
    let Some(source_blob) =
        read_note(git_dir, format, &store, &notes_ref_handle(notes_ref), &from)?
    else {
        eprintln!(
            "error: missing notes on source object {}. Cannot copy.",
            from.to_hex()
        );
        return Err(GitError::Exit(1));
    };
    if existing.is_some() && force {
        eprintln!("Overwriting existing notes for object {}", to.to_hex());
    }

    let mut notes = list_notes(git_dir, format, &store, &notes_ref_handle(notes_ref))?;
    upsert_note(&mut notes, &to, source_blob);
    write_notes(
        git_dir,
        format,
        &store,
        &notes_ref_handle(notes_ref),
        &notes,
        "Notes added by 'git notes copy'",
        &notes_commit_identity()?,
        notes_ref_expected(&store, &notes_ref_handle(notes_ref))?,
    )
}

/// How two notes are combined when copying onto an object that already has a
/// note, mirroring git's `combine_notes_*` family.
#[derive(Clone, Copy, PartialEq)]
enum CombineMode {
    Overwrite,
    Ignore,
    Concatenate,
}

impl CombineMode {
    /// Parse a `notes.rewriteMode` / `GIT_NOTES_REWRITE_MODE` value. Returns
    /// None for an unrecognized mode (git errors, but the tests only use the
    /// three modes here plus the default).
    fn parse(value: &str) -> Option<CombineMode> {
        match value {
            "overwrite" => Some(CombineMode::Overwrite),
            "ignore" => Some(CombineMode::Ignore),
            "concatenate" | "cat_sort_uniq" => Some(CombineMode::Concatenate),
            _ => None,
        }
    }
}

/// Combine `cur` (existing note bytes, if any) and `new` (incoming note bytes)
/// under `mode`, returning the resulting bytes. Mirrors git's combiners:
/// overwrite returns `new`; ignore keeps `cur`; concatenate joins them with a
/// blank line (stripping one trailing newline from `cur` first).
fn combine_notes(mode: CombineMode, cur: Option<&[u8]>, new: &[u8]) -> Vec<u8> {
    match mode {
        CombineMode::Overwrite => new.to_vec(),
        CombineMode::Ignore => cur.map(|c| c.to_vec()).unwrap_or_default(),
        CombineMode::Concatenate => {
            let Some(cur) = cur.filter(|c| !c.is_empty()) else {
                return new.to_vec();
            };
            if new.is_empty() {
                return cur.to_vec();
            }
            let mut cur = cur.to_vec();
            if cur.last() == Some(&b'\n') {
                cur.pop();
            }
            cur.extend_from_slice(b"\n\n");
            cur.extend_from_slice(new);
            cur
        }
    }
}

/// Resolve the rewrite configuration for `git notes copy --for-rewrite=<cmd>`:
/// the combine mode (env `GIT_NOTES_REWRITE_MODE`, else `notes.rewriteMode`,
/// else concatenate), the enabled flag (`notes.rewrite.<cmd>`, default true),
/// and the target notes refs (env `GIT_NOTES_REWRITE_REF` colon-list, else
/// `notes.rewriteRef`, glob-expanded). Returns None when disabled or no refs.
fn resolve_rewrite_config(store: &FileRefStore, cmd: &str) -> Result<Option<(CombineMode, Vec<String>)>> {
    let config = identity_effective_config();

    // Mode: env wins, then config, then concatenate.
    let mode_from_env;
    let mut mode = CombineMode::Concatenate;
    if let Ok(value) = env::var("GIT_NOTES_REWRITE_MODE") {
        mode_from_env = true;
        if let Some(parsed) = CombineMode::parse(&value) {
            mode = parsed;
        }
    } else {
        mode_from_env = false;
    }
    if !mode_from_env
        && let Some(config) = &config
        && let Some(value) = config.get("notes", None, "rewriteMode")
        && let Some(parsed) = CombineMode::parse(value)
    {
        mode = parsed;
    }

    // Enabled: notes.rewrite.<cmd> bool (default true). git reads the flattened
    // key `notes.rewrite.<cmd>` (section `notes`, dotted key `rewrite.<cmd>`).
    let mut enabled = true;
    if let Some(config) = &config
        && let Some(value) = config.get("notes", None, &format!("rewrite.{cmd}"))
    {
        enabled = value != "false" && value != "0" && value != "no" && value != "off";
    }
    if !enabled {
        return Ok(None);
    }

    // Refs: env colon-list wins, else config (glob-expanded).
    let mut ref_globs: Vec<String> = Vec::new();
    if let Ok(value) = env::var("GIT_NOTES_REWRITE_REF") {
        ref_globs.extend(value.split(':').filter(|s| !s.is_empty()).map(String::from));
    } else if let Some(config) = &config {
        for value in config.get_all("notes", None, "rewriteRef").into_iter().flatten() {
            if value.starts_with("refs/notes/") {
                ref_globs.push(value.to_string());
            }
        }
    }

    let refs = expand_notes_ref_globs(store, &ref_globs)?;
    if refs.is_empty() {
        return Ok(None);
    }
    Ok(Some((mode, refs)))
}

/// Expand a list of notes-ref globs (each either an exact `refs/notes/...` ref
/// or a `refs/notes/*`-style glob) against the existing refs, de-duplicated and
/// in first-seen order.
fn expand_notes_ref_globs(store: &FileRefStore, globs: &[String]) -> Result<Vec<String>> {
    let all_refs = store.list_refs()?;
    let mut out: Vec<String> = Vec::new();
    for glob in globs {
        if glob.contains('*') {
            // Prefix match for the common `refs/notes/*` shape.
            let prefix = glob.trim_end_matches('*');
            for entry in &all_refs {
                if entry.name.starts_with(prefix) && !out.contains(&entry.name) {
                    out.push(entry.name.clone());
                }
            }
        } else if all_refs.iter().any(|entry| entry.name == *glob) && !out.contains(glob) {
            out.push(glob.clone());
        }
    }
    Ok(out)
}

/// `git notes copy --stdin` / `--for-rewrite=<cmd>`: read `<from> <to>` lines
/// from stdin and copy each note. Plain `--stdin` writes the current notes ref
/// with overwrite semantics; `--for-rewrite` applies the configured mode across
/// every configured notes ref.
fn notes_copy_from_stdin(
    git_dir: &Path,
    format: ObjectFormat,
    notes_ref: &str,
    force: bool,
    rewrite_cmd: Option<&str>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);

    // Determine the (mode, refs) set. `--stdin` uses overwrite on the single
    // current ref (honouring -f); `--for-rewrite` reads config.
    let (mode, refs) = if let Some(cmd) = rewrite_cmd {
        match resolve_rewrite_config(&store, cmd)? {
            Some(resolved) => resolved,
            // Disabled or no configured refs: a silent no-op (git returns 0).
            None => return Ok(()),
        }
    } else {
        // Plain `--stdin` always overwrites (git uses combine_notes_overwrite).
        let _ = force;
        (CombineMode::Overwrite, vec![notes_ref.to_string()])
    };

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    for handle_name in &refs {
        let handle = notes_ref_handle(handle_name);
        let mut notes = list_notes(git_dir, format, &store, &handle)?;
        let mut changed = false;

        for line in input.lines() {
            let mut parts = line.split_whitespace();
            let (Some(from_spec), Some(to_spec)) = (parts.next(), parts.next()) else {
                if line.trim().is_empty() {
                    continue;
                }
                eprintln!("fatal: malformed input line: '{line}'.");
                return Err(GitError::Exit(128));
            };
            let from = resolve_note_object(git_dir, format, from_spec)?;
            let to = resolve_note_object(git_dir, format, to_spec)?;

            let Some(from_blob) = read_note(git_dir, format, &store, &handle, &from)? else {
                // No source note: nothing to copy (git's copy_note returns 0).
                continue;
            };
            let new_bytes = db.read_object(&from_blob)?.body.clone();

            let cur_blob = read_note(git_dir, format, &store, &handle, &to)?;
            let cur_bytes = match &cur_blob {
                Some(blob) => Some(db.read_object(blob)?.body.clone()),
                None => None,
            };
            let combined = combine_notes(mode, cur_bytes.as_deref(), &new_bytes);

            let mut db_w = FileObjectDatabase::from_git_dir(git_dir, format);
            let blob = if combined == new_bytes {
                from_blob
            } else {
                db_w.write_object(EncodedObject::new(ObjectType::Blob, combined))?
            };
            if cur_blob.as_ref() != Some(&blob) {
                upsert_note(&mut notes, &to, blob);
                changed = true;
            }
        }

        if changed || rewrite_cmd.is_none() {
            write_notes(
                git_dir,
                format,
                &store,
                &handle,
                &notes,
                "Notes added by 'git notes copy'",
                &notes_commit_identity()?,
                notes_ref_expected(&store, &handle)?,
            )?;
        }
    }

    Ok(())
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
    Edit,
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
        NotesUsage::Edit => "usage: git notes edit [--allow-empty] [<object>]\n\n",
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
