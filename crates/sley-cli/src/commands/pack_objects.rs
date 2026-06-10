//! `git pack-objects`: read object ids from standard input and write a pack
//! (with its `.idx` and `.rev` companions) named after the pack checksum.
//!
//! Implements the `<base-name>` form upstream documents as
//! `git pack-objects [<options>] <base-name> [< <object-list>]` (see
//! `read_object_list_from_stdin` in upstream `builtin/pack-objects.c`):
//! each input line carries an object id, optionally followed by a name hint
//! that git only uses as a delta heuristic; lines starting with `-` name edge
//! (preferred-base) objects, which never become pack members. The revision
//! traversal modes (`--revs`, `--all`, ...) and `--stdout` are not supported
//! yet and are reported as such.

use std::io::BufRead;

use sley_pack::{PackInput, PackIndexEntry, PackReverseIndex};

use crate::*;

pub(crate) fn cmd_pack_objects(args: &[String]) -> Result<()> {
    let mut base_name = None::<String>;
    let mut iter = args.iter();
    let mut saw_dashdash = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" if !saw_dashdash => saw_dashdash = true,
            // Progress-meter toggles: sley never draws a progress meter, so
            // these are accepted as no-ops (they have no on-disk effect).
            "-q" | "--quiet" | "--no-quiet" | "--progress" | "--no-progress" | "--all-progress"
            | "--no-all-progress" | "--all-progress-implied" | "--no-all-progress-implied"
                if !saw_dashdash => {}
            value if !saw_dashdash && value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported pack-objects option {value}"
                )));
            }
            value => {
                if base_name.is_some() {
                    return pack_objects_usage();
                }
                base_name = Some(value.to_string());
            }
        }
    }
    let Some(base_name) = base_name else {
        return pack_objects_usage();
    };

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let oids = read_pack_objects_stdin(format)?;

    let database = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let mut objects = Vec::with_capacity(oids.len());
    for oid in &oids {
        match database.read_object(oid) {
            Ok(object) => objects.push(object),
            Err(GitError::NotFound(_)) => {
                eprintln!("fatal: unable to read {oid}");
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        }
    }

    let inputs: Vec<PackInput<'_>> = oids
        .iter()
        .zip(&objects)
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let positions = pack_order_positions(&written.entries);
    let reverse_index = PackReverseIndex::write(format, &positions, &written.checksum)?;

    // Write the pack before its lookup companions so no reader ever sees an
    // index that points at a missing or incomplete pack.
    let checksum = written.checksum.to_hex();
    fs::write(format!("{base_name}-{checksum}.pack"), &written.pack)?;
    fs::write(format!("{base_name}-{checksum}.rev"), &reverse_index)?;
    fs::write(format!("{base_name}-{checksum}.idx"), &written.index)?;
    println!("{checksum}");
    Ok(())
}

/// Read the object list from standard input, mirroring upstream's
/// `read_object_list_from_stdin`: one object id per line with an optional
/// name hint after it, `-<oid>` edge lines validated then skipped (preferred
/// bases are delta heuristics, never pack members), and garbage rejected with
/// git's exact message and exit code. Duplicate ids collapse to their first
/// occurrence, as `add_object_entry` does.
fn read_pack_objects_stdin(format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let hex_len = format.raw_len() * 2;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    let mut seen = HashSet::new();
    let mut oids = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.first() == Some(&b'-') {
            if parse_pack_objects_oid(&line[1..], hex_len, format).is_none() {
                return pack_objects_garbage("expected edge object ID", &line);
            }
            // Edge (preferred-base) objects are only delta-base hints; they
            // are never added to the pack, and sley's pack writer picks its
            // own delta bases, so a validated edge line is simply skipped.
            continue;
        }
        let Some(oid) = parse_pack_objects_oid(&line, hex_len, format) else {
            return pack_objects_garbage("expected object ID", &line);
        };
        if seen.insert(oid) {
            oids.push(oid);
        }
    }
    Ok(oids)
}

/// Parse the leading `hex_len` bytes of `line` as an object id, returning
/// `None` when the line is too short or not hex — the caller reports git's
/// "got garbage" error. Anything after the id (a name hint) is ignored.
fn parse_pack_objects_oid(line: &[u8], hex_len: usize, format: ObjectFormat) -> Option<ObjectId> {
    let hex = line.get(..hex_len)?;
    let hex = std::str::from_utf8(hex).ok()?;
    ObjectId::from_hex(format, hex).ok()
}

/// Report a garbage input line exactly like upstream's
/// `die(_("expected [edge ]object ID, got garbage:\n %s"), line)`: the raw
/// line keeps its trailing newline (when present) and `die` appends one more.
fn pack_objects_garbage<T>(what: &str, line: &[u8]) -> Result<T> {
    eprint!(
        "fatal: {what}, got garbage:\n {}\n",
        String::from_utf8_lossy(line)
    );
    Err(GitError::Exit(128))
}

/// Build the `.rev` table for a freshly written pack: index positions (the
/// rank of each object in the oid-sorted `.idx`) listed in pack order
/// (ascending pack offset), as upstream `write_rev_file` lays them out.
fn pack_order_positions(entries: &[PackIndexEntry]) -> Vec<u32> {
    let mut oid_sorted: Vec<usize> = (0..entries.len()).collect();
    oid_sorted.sort_by(|&a, &b| entries[a].oid.as_bytes().cmp(entries[b].oid.as_bytes()));
    let mut index_position = vec![0u32; entries.len()];
    for (position, &entry) in oid_sorted.iter().enumerate() {
        index_position[entry] = position as u32;
    }
    let mut by_offset: Vec<usize> = (0..entries.len()).collect();
    by_offset.sort_by_key(|&entry| entries[entry].offset);
    by_offset
        .into_iter()
        .map(|entry| index_position[entry])
        .collect()
}

fn pack_objects_usage<T>() -> Result<T> {
    eprintln!("usage: git pack-objects --stdout [<options>] [< <ref-list> | < <object-list>]");
    eprintln!("   or: git pack-objects [<options>] <base-name> [< <ref-list> | < <object-list>]");
    Err(GitError::Exit(129))
}
