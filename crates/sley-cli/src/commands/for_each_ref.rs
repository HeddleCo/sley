//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_for_each_ref(args: &[String]) -> Result<()> {
    for_each_ref_core(args, "git for-each-ref")
}

/// The `-h` usage banner, matching git's parse_options output byte-for-byte.
/// Only the program name on the first line differs between `for-each-ref` and
/// `refs list`; continuation lines are indented to a fixed column (25 literal
/// spaces in COMMON_USAGE_FOR_EACH_REF + 7 for the stripped `usage: ` prefix).
fn print_for_each_ref_usage(usage_cmd: &str) {
    eprint!(
        "usage: {usage_cmd} [--count=<count>] [--shell|--perl|--python|--tcl]
                                [(--sort=<key>)...] [--format=<format>]
                                [--include-root-refs] [--points-at=<object>]
                                [--merged[=<object>]] [--no-merged[=<object>]]
                                [--contains[=<object>]] [--no-contains[=<object>]]
                                [(--exclude=<pattern>)...] [--start-after=<marker>]
                                [ --stdin | (<pattern>...)]

    -s, --[no-]shell      quote placeholders suitably for shells
    -p, --[no-]perl       quote placeholders suitably for perl
    --[no-]python         quote placeholders suitably for python
    --[no-]tcl            quote placeholders suitably for Tcl
    --[no-]omit-empty     do not output a newline after empty formatted refs

    --[no-]count <n>      show only <n> matched refs
    --[no-]format <format>
                          format to use for the output
    --[no-]start-after <marker>
                          start iteration after the provided marker
    --[no-]color[=<when>] respect format colors
    --[no-]exclude <pattern>
                          exclude refs which match pattern
    --[no-]sort <key>     field name to sort on
    --[no-]points-at <object>
                          print only refs which points at the given object
    --merged <commit>     print only refs that are merged
    --no-merged <commit>  print only refs that are not merged
    --contains <commit>   print only refs which contain the commit
    --no-contains <commit>
                          print only refs which don't contain the commit
    --[no-]ignore-case    sorting and filtering are case insensitive
    --[no-]stdin          read reference patterns from stdin
    --[no-]include-root-refs
                          also include HEAD ref and pseudorefs

"
    );
}

/// The shared core of `git for-each-ref` and its clone `git refs list` (see
/// builtin/refs.c::cmd_refs_list, which calls for_each_ref_core). The only
/// per-command difference is the program name printed in the `-h` usage banner.
pub(crate) fn for_each_ref_core(args: &[String], usage_cmd: &str) -> Result<()> {
    let mut format_spec = "%(objectname) %(objecttype)\t%(refname)".to_string();
    let mut count = None;
    let mut omit_empty = false;
    let mut include_root_refs = false;
    let mut ignore_case = false;
    let mut color = false;
    let mut quote = ForEachRefQuoteMode::None;
    // git rejects more than one quoting style (HAS_MULTI_BITS on quote_style).
    let mut quote_styles = 0u32;
    let mut read_stdin = false;
    let mut sorts = Vec::new();
    let mut sort_explicit = false;
    let mut start_after = None;
    let mut points_at_revs = Vec::new();
    let mut contains_revs = Vec::new();
    let mut no_contains_revs = Vec::new();
    let mut merged_filter = None;
    let mut excludes = Vec::new();
    let mut patterns = Vec::new();
    // git's parse_options only *records* --sort strings (validation is deferred to
    // ref_sorting_options), so a `-h` anywhere on the line is reached and prints the
    // usage banner even alongside an invalid --sort=bogus. Mirror that by handling
    // -h before eager sort validation can error.
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_for_each_ref_usage(usage_cmd);
        return Err(GitError::Exit(129));
    }
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            value if value.starts_with("--format=") => {
                format_spec = value
                    .strip_prefix("--format=")
                    .expect("prefix checked by match guard")
                    .to_string();
            }
            "--format" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--format requires a value".into()));
                };
                format_spec = value.to_string();
            }
            "--omit-empty" => omit_empty = true,
            "--no-omit-empty" => omit_empty = false,
            "--include-root-refs" => include_root_refs = true,
            "--no-include-root-refs" => include_root_refs = false,
            "--color" => color = true,
            "--no-color" => color = false,
            "--color=always" => color = true,
            "--color=never" | "--color=auto" => color = false,
            "--shell" | "-s" => {
                quote = ForEachRefQuoteMode::Shell;
                quote_styles += 1;
            }
            "--python" => {
                quote = ForEachRefQuoteMode::Python;
                quote_styles += 1;
            }
            "--perl" | "-p" => {
                quote = ForEachRefQuoteMode::Perl;
                quote_styles += 1;
            }
            "--tcl" => {
                quote = ForEachRefQuoteMode::Tcl;
                quote_styles += 1;
            }
            "--ignore-case" => ignore_case = true,
            "--no-ignore-case" => ignore_case = false,
            "--stdin" => read_stdin = true,
            "--no-stdin" => read_stdin = false,
            "--count" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--count requires a value".into()));
                };
                count = Some(parse_for_each_ref_count(value)?);
            }
            "--no-count" => count = None,
            value if value.starts_with("--count=") => {
                let value = value
                    .strip_prefix("--count=")
                    .expect("prefix checked by match guard");
                count = Some(parse_for_each_ref_count(value)?);
            }
            "--sort" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--sort requires a value".into()));
                };
                sorts.push(parse_for_each_ref_sort(value)?);
                sort_explicit = true;
            }
            "--no-sort" => {
                sorts.clear();
                sort_explicit = false;
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .expect("prefix checked by match guard");
                sorts.push(parse_for_each_ref_sort(value)?);
                sort_explicit = true;
            }
            "--start-after" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--start-after requires a value".into()));
                };
                start_after = Some(value.to_string());
            }
            "--no-start-after" => start_after = None,
            value if value.starts_with("--start-after=") => {
                let value = value
                    .strip_prefix("--start-after=")
                    .expect("prefix checked by match guard");
                start_after = Some(value.to_string());
            }
            "--exclude" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--exclude requires a value".into()));
                };
                excludes.push(value.to_string());
            }
            "--no-exclude" => excludes.clear(),
            value if value.starts_with("--exclude=") => {
                let value = value
                    .strip_prefix("--exclude=")
                    .expect("prefix checked by match guard");
                excludes.push(value.to_string());
            }
            "--points-at" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--points-at requires a value".into()));
                };
                points_at_revs.push(value.to_string());
            }
            value if value.starts_with("--points-at=") => {
                let value = value
                    .strip_prefix("--points-at=")
                    .expect("prefix checked by match guard");
                points_at_revs.push(value.to_string());
            }
            "--contains" => {
                if let Some(value) = args.get(idx + 1) {
                    idx += 1;
                    contains_revs.push(value.to_string());
                } else {
                    contains_revs.push("HEAD".to_string());
                }
            }
            value if value.starts_with("--contains=") => {
                let value = value
                    .strip_prefix("--contains=")
                    .expect("prefix checked by match guard");
                contains_revs.push(value.to_string());
            }
            "--no-contains" => {
                if let Some(value) = args.get(idx + 1) {
                    idx += 1;
                    no_contains_revs.push(value.to_string());
                } else {
                    no_contains_revs.push("HEAD".to_string());
                }
            }
            value if value.starts_with("--no-contains=") => {
                let value = value
                    .strip_prefix("--no-contains=")
                    .expect("prefix checked by match guard");
                no_contains_revs.push(value.to_string());
            }
            "--merged" => {
                if let Some(value) = args.get(idx + 1) {
                    idx += 1;
                    merged_filter = Some((value.to_string(), true));
                } else {
                    merged_filter = Some(("HEAD".to_string(), true));
                }
            }
            value if value.starts_with("--merged=") => {
                let value = value
                    .strip_prefix("--merged=")
                    .expect("prefix checked by match guard");
                merged_filter = Some((value.to_string(), true));
            }
            "--no-merged" => {
                if let Some(value) = args.get(idx + 1) {
                    idx += 1;
                    merged_filter = Some((value.to_string(), false));
                } else {
                    merged_filter = Some(("HEAD".to_string(), false));
                }
            }
            value if value.starts_with("--no-merged=") => {
                let value = value
                    .strip_prefix("--no-merged=")
                    .expect("prefix checked by match guard");
                merged_filter = Some((value.to_string(), false));
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported for-each-ref option {value}"
                )));
            }
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }
    if read_stdin {
        if !patterns.is_empty() {
            return Err(GitError::Command(
                "unknown arguments supplied with --stdin".into(),
            ));
        }
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        patterns.extend(
            input
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| line.to_string()),
        );
    }
    if start_after.is_some() && sort_explicit {
        return Err(GitError::Command(
            "cannot use --start-after with custom sort options".into(),
        ));
    }
    if start_after.is_some() && !patterns.is_empty() {
        return Err(GitError::Command(
            "cannot use --start-after with patterns".into(),
        ));
    }
    if sorts.is_empty() {
        sorts.push(ForEachRefSort::Refname);
    }
    // git: `if (HAS_MULTI_BITS(format.quote_style))` -> error + usage (exit 129).
    if quote_styles > 1 {
        eprintln!("error: more than one quoting style?");
        return Err(GitError::Exit(129));
    }
    let format_spec = ForEachRefFormat::parse(&format_spec)?;
    // git: a bare %(raw) atom is incompatible with --python/--shell/--tcl.
    if matches!(
        quote,
        ForEachRefQuoteMode::Python | ForEachRefQuoteMode::Shell | ForEachRefQuoteMode::Tcl
    ) && for_each_ref_format_has_bare_raw(&format_spec)
    {
        eprintln!("fatal: --format=%(raw) cannot be used with --python, --shell, --tcl");
        return Err(GitError::Exit(128));
    }
    let needs = ForEachRefNeeds::analyze(&format_spec);
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let objectname_abbrev = repository_abbrev(&git_dir, format)?;
    let points_at = points_at_revs
        .iter()
        .map(|rev| resolve_revision(&git_dir, format, rev))
        .collect::<Result<Vec<_>>>()?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    // The abbreviation candidate set is only needed by `%(objectname:short...)`;
    // enumerating every object id is otherwise pure overhead.
    let objectname_candidates = if needs.candidates {
        cat_file_all_object_ids(&git_dir, format)?
    } else {
        Vec::new()
    };
    let contains_targets = contains_revs
        .iter()
        .map(|rev| {
            let oid = resolve_revision(&git_dir, format, rev)?;
            sley_rev::peel_to_commit(&db, format, &oid)
        })
        .collect::<Result<Vec<_>>>()?;
    let no_contains_targets = no_contains_revs
        .iter()
        .map(|rev| {
            let oid = resolve_revision(&git_dir, format, rev)?;
            sley_rev::peel_to_commit(&db, format, &oid)
        })
        .collect::<Result<Vec<_>>>()?;
    let merged_filter = merged_filter
        .map(|(rev, include)| {
            let oid = resolve_revision(&git_dir, format, &rev)?;
            let commit = sley_rev::peel_to_commit(&db, format, &oid)?;
            let reachable = sley_rev::walk_commits(&db, format, [commit])?
                .into_iter()
                .map(|record| record.oid)
                .collect::<HashSet<_>>();
            Ok::<_, GitError>((reachable, include))
        })
        .transpose()?;
    let store = FileRefStore::new(&git_dir, format);
    let head_ref = store.current_branch_ref()?;
    // Discover worktree paths once instead of re-scanning $GIT_DIR/worktrees per ref.
    let worktree_paths = if needs.worktree {
        for_each_ref_worktree_paths(&git_dir, head_ref.as_deref())?
    } else {
        HashMap::new()
    };
    let config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    // git resolves `%(...:mailmap)` atoms against the repository .mailmap plus
    // mailmap.{file,blob} config; load it once up front (cheap when absent).
    let mailmap = commands::utility::Mailmap::load_default(&git_dir, format)?;
    let mut stdout = io::stdout();
    let mut emitted = 0usize;
    let mut refs = store.list_refs()?;
    if include_root_refs && let Some(target) = store.read_ref("HEAD")? {
        refs.push(sley_refs::Ref {
            name: "HEAD".to_string(),
            target,
        });
    }
    sort_for_each_refs(
        &mut refs,
        &sorts,
        ForEachRefSortContext {
            ignore_case,
            store: &store,
            config: &config,
            db: &db,
            git_dir: &git_dir,
            head_ref: head_ref.as_deref(),
            format,
        },
    )?;
    for reference in refs {
        let Some((oid, symref)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        if start_after
            .as_deref()
            .is_some_and(|marker| reference.name.as_str() <= marker)
        {
            continue;
        }
        if !points_at.is_empty() && !for_each_ref_points_at(&db, format, &oid, &points_at)? {
            continue;
        }
        if let Some((reachable, include)) = &merged_filter {
            let merged = sley_rev::peel_to_commit(&db, format, &oid)
                .map(|tip| reachable.contains(&tip))
                .unwrap_or(false);
            if merged != *include {
                continue;
            }
        }
        if !contains_targets.is_empty() || !no_contains_targets.is_empty() {
            let reachable = sley_rev::peel_to_commit(&db, format, &oid)
                .ok()
                .map(|tip| {
                    sley_rev::walk_commits(&db, format, [tip]).map(|records| {
                        records
                            .into_iter()
                            .map(|record| record.oid)
                            .collect::<HashSet<_>>()
                    })
                })
                .transpose()?;
            let Some(reachable) = reachable else {
                continue;
            };
            if !contains_targets.is_empty()
                && !contains_targets
                    .iter()
                    .any(|target| reachable.contains(target))
            {
                continue;
            }
            if no_contains_targets
                .iter()
                .any(|target| reachable.contains(target))
            {
                continue;
            }
        }
        if !patterns.is_empty()
            && !patterns
                .iter()
                .any(|pattern| for_each_ref_pattern_matches(&reference.name, pattern, ignore_case))
        {
            continue;
        }
        if excludes
            .iter()
            .any(|pattern| for_each_ref_exclude_matches(&reference.name, pattern, ignore_case))
        {
            continue;
        }
        if count.is_some_and(|limit| limit != 0 && emitted >= limit) {
            break;
        }
        let upstream = for_each_ref_upstream(&config, &reference.name);
        let push = for_each_ref_push(&config, &reference.name);
        let upstream_track = upstream
            .as_ref()
            .map(|upstream| {
                for_each_ref_upstream_track(&store, &db, format, &oid, &upstream.refname)
            })
            .transpose()?
            .flatten();
        let push_track = push
            .as_ref()
            .and_then(|push| push.refname.as_deref())
            .map(|push_ref| for_each_ref_upstream_track(&store, &db, format, &oid, push_ref))
            .transpose()?
            .flatten();
        // Only decode the ref object when the format references an atom that needs
        // it (git's used_atom analysis). Formats like %(objectname)/%(refname) read
        // nothing here.
        let object = if needs.object {
            Some(db.read_object(&oid)?)
        } else {
            None
        };
        let contents = object
            .as_ref()
            .map(|object| for_each_ref_contents(format, object))
            .transpose()?
            .flatten();
        // The peeled tag target is only read when a %(*...) atom references it.
        let peeled_oid = if needs.peeled {
            contents.as_ref().and_then(|contents| contents.tag_object)
        } else {
            None
        };
        let peeled_encoded_object = match peeled_oid {
            Some(peeled_oid) => Some(db.read_object(&peeled_oid)?),
            None => None,
        };
        let peeled_object = if let (Some(peeled_oid), Some(peeled_encoded_object)) =
            (peeled_oid, peeled_encoded_object.as_ref())
        {
            let object_disk_size = if needs.peeled_disk {
                for_each_ref_loose_object_disk_size(&git_dir, &peeled_oid)?
            } else {
                None
            };
            let (tree, parents, message, author, committer, creator) =
                if peeled_encoded_object.object_type == ObjectType::Commit {
                    let commit = Commit::parse_ref(format, &peeled_encoded_object.body)?;
                    (
                        Some(commit.tree),
                        commit.parents,
                        Some(Cow::Borrowed(commit.message)),
                        Some(Cow::Borrowed(commit.author)),
                        Some(Cow::Borrowed(commit.committer)),
                        Some(Cow::Borrowed(commit.committer)),
                    )
                } else {
                    (None, Vec::new(), None, None, None, None)
                };
            Some(ForEachRefPeeledObject {
                oid: peeled_oid,
                object_type: peeled_encoded_object.object_type,
                object_size: peeled_encoded_object.body.len(),
                object_disk_size,
                object_body: Cow::Borrowed(&peeled_encoded_object.body),
                tree,
                parents,
                message,
                author,
                committer,
                creator,
            })
        } else {
            None
        };
        let object_disk_size = if needs.object_disk {
            for_each_ref_loose_object_disk_size(&git_dir, &oid)?
        } else {
            None
        };
        let deltabase = zero_oid(format)?;
        // `%(worktreepath)` reads from the hoisted map; the placeholder is the empty
        // path for refs not checked out anywhere, matching git.
        let worktree_path = worktree_paths.get(reference.name.as_str()).map(String::as_str);
        // When the format needs no object, these fields are never observed (every
        // atom that reads them is gated behind `needs.object`); the placeholders are
        // therefore unobservable.
        let object_type = object
            .as_ref()
            .map(|object| object.object_type)
            .unwrap_or(ObjectType::Commit);
        let object_body: &[u8] = object.as_ref().map(|object| object.body.as_ref()).unwrap_or(&[]);
        let format_context = ForEachRefFormatContext {
            git_dir: &git_dir,
            db: &db,
            format,
            refname: &reference.name,
            oid: &oid,
            deltabase: &deltabase,
            object_type,
            object_body,
            object_size: object_body.len(),
            object_disk_size,
            color,
            quote,
            objectname_abbrev,
            objectname_candidates: &objectname_candidates,
            worktree_path,
            is_head: head_ref.as_deref() == Some(reference.name.as_str()),
            symref: symref.as_deref(),
            upstream,
            push,
            upstream_track,
            push_track,
            contents,
            peeled_object,
            mailmap: &mailmap,
        };
        let mut line = Vec::new();
        print_for_each_ref_format(&mut line, &format_spec, &format_context)?;
        if !omit_empty || !line.is_empty() {
            stdout.write_all(&line)?;
            stdout.write_all(b"\n")?;
        }
        emitted += 1;
    }
    stdout.flush()?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ForEachRefSort {
    Refname,
    RefnameDescending,
    Identity(ForEachRefIdentitySortField),
    IdentityDescending(ForEachRefIdentitySortField),
    ObjectName,
    ObjectNameDescending,
    ObjectType,
    ObjectTypeDescending,
    ObjectSize,
    ObjectSizeDescending,
    ObjectSizeDisk,
    ObjectSizeDiskDescending,
    Upstream,
    UpstreamDescending,
    Push,
    PushDescending,
    Symref,
    SymrefDescending,
    WorktreePath,
    WorktreePathDescending,
    Tag,
    TagDescending,
    Type,
    TypeDescending,
    Object,
    ObjectDescending,
    Subject,
    SubjectDescending,
    Body,
    BodyDescending,
    ContentsSize,
    ContentsSizeDescending,
    Raw,
    RawDescending,
    RawSize,
    RawSizeDescending,
    PeeledSubject,
    PeeledSubjectDescending,
    PeeledBody,
    PeeledBodyDescending,
    PeeledContentsSize,
    PeeledContentsSizeDescending,
    PeeledObjectName,
    PeeledObjectNameDescending,
    PeeledObjectType,
    PeeledObjectTypeDescending,
    PeeledObjectSize,
    PeeledObjectSizeDescending,
    PeeledObjectSizeDisk,
    PeeledObjectSizeDiskDescending,
    PeeledDeltabase,
    PeeledDeltabaseDescending,
    PeeledRawSize,
    PeeledRawSizeDescending,
    Tree,
    TreeDescending,
    Parent,
    ParentDescending,
    NumParent,
    NumParentDescending,
    PeeledTree,
    PeeledTreeDescending,
    PeeledParent,
    PeeledParentDescending,
    PeeledNumParent,
    PeeledNumParentDescending,
    AuthorDate,
    AuthorDateDescending,
    CommitterDate,
    CommitterDateDescending,
    TaggerDate,
    TaggerDateDescending,
    CreatorDate,
    CreatorDateDescending,
    PeeledAuthorDate,
    PeeledAuthorDateDescending,
    PeeledCommitterDate,
    PeeledCommitterDateDescending,
    PeeledTaggerDate,
    PeeledTaggerDateDescending,
    PeeledCreatorDate,
    PeeledCreatorDateDescending,
    VersionRefname,
    VersionRefnameDescending,
}

/// Which per-ref work the parsed `--format` actually requires (git's `used_atom`
/// analysis). Computed once up front so the per-ref loop can skip object reads,
/// the peeled-tag read, disk-size stats, the abbreviation candidate scan, and the
/// worktree probe whenever the format never references the corresponding atom.
#[derive(Default, Clone, Copy)]
struct ForEachRefNeeds {
    /// The ref's own object must be decoded (object body / type / size / contents).
    object: bool,
    /// The peeled tag target must be read (any `*`-prefixed object/contents atom).
    /// Implies `object`, since the tag pointer comes from decoding the ref object.
    peeled: bool,
    /// `%(objectsize:disk)` — the loose-object on-disk size for the ref object.
    object_disk: bool,
    /// `%(*objectsize:disk)` — the loose-object on-disk size for the peeled object.
    peeled_disk: bool,
    /// `%(worktreepath)` — the per-ref worktree probe.
    worktree: bool,
    /// `%(objectname:short...)` / `%(*objectname:short...)` — needs the ambiguity
    /// candidate set (the full object-id enumeration).
    candidates: bool,
}

impl ForEachRefNeeds {
    fn analyze(format_spec: &ForEachRefFormat) -> Self {
        let mut needs = ForEachRefNeeds::default();
        for segment in format_spec.segments() {
            let ForEachRefFormatSegment::Atom(atom) = segment else {
                continue;
            };
            match atom {
                ForEachRefAtom::Raw(placeholder) => needs.note_raw(placeholder),
                ForEachRefAtom::Color(_) => {}
                ForEachRefAtom::RefName { .. } => {}
                ForEachRefAtom::ObjectName { peeled, abbrev } => {
                    if abbrev.is_some() {
                        needs.candidates = true;
                    }
                    if *peeled {
                        needs.peeled = true;
                    }
                    // The direct objectname comes straight from the ref target; it
                    // never needs the object decoded.
                }
                ForEachRefAtom::Identity { peeled, .. }
                | ForEachRefAtom::ContentsLines { peeled, .. } => {
                    if *peeled {
                        needs.peeled = true;
                    } else {
                        needs.object = true;
                    }
                }
            }
        }
        // Reading the peeled tag target requires first decoding the ref object to
        // discover the tag pointer; the disk-size stats follow their reads.
        if needs.peeled {
            needs.object = true;
        }
        if needs.peeled_disk {
            needs.peeled = true;
            needs.object = true;
        }
        needs
    }

    fn note_raw(&mut self, placeholder: &str) {
        // Strip a leading `*` (peeled) marker, classifying the peeled need first.
        let (base, peeled) = placeholder
            .strip_prefix('*')
            .map(|rest| (rest, true))
            .unwrap_or((placeholder, false));
        // Atoms that consult the ref object body (or, when peeled, the tag target).
        let consumes_object = match base {
            "objectsize" | "objecttype" | "raw" | "raw:size" | "subject" | "contents"
            | "contents:subject" | "contents:body" | "contents:size" | "body" | "author"
            | "authorname" | "authoremail" | "authordate" | "committer" | "committername"
            | "committeremail" | "committerdate" | "tagger" | "taggername" | "taggeremail"
            | "taggerdate" | "creator" | "creatordate" | "tree" | "parent" | "numparent"
            | "tag" | "type" | "object" => true,
            "objectsize:disk" => {
                if peeled {
                    self.peeled_disk = true;
                } else {
                    self.object_disk = true;
                }
                false
            }
            "objectname" | "deltabase" => peeled,
            "worktreepath" => {
                self.worktree = true;
                false
            }
            other => {
                if other == "objectname:short" || other.starts_with("objectname:short=") {
                    self.candidates = true;
                    // The direct objectname needs no read; peeled needs the tag target.
                    peeled
                } else if other == "tree:short"
                    || other.starts_with("tree:short=")
                    || other == "parent:short"
                    || other.starts_with("parent:short=")
                {
                    // Abbreviating the tree/parent oid needs both the decoded
                    // object (to read the oids) and the ambiguity candidate set.
                    self.candidates = true;
                    true
                } else if other.starts_with("authordate:")
                    || other.starts_with("committerdate:")
                    || other.starts_with("taggerdate:")
                    || other.starts_with("creatordate:")
                    || other.starts_with("authoremail:")
                    || other.starts_with("committeremail:")
                    || other.starts_with("taggeremail:")
                    || other.starts_with("authorname:")
                    || other.starts_with("committername:")
                    || other.starts_with("taggername:")
                    || other.starts_with("creatorname:")
                    || other.starts_with("subject:")
                    || other.starts_with("contents:")
                    || other == "trailers"
                    || other.starts_with("trailers:")
                {
                    // subject:sanitize, contents:{signature,body,subject,lines,
                    // size,trailers}, name/email/date option variants — all read
                    // the object (or peeled target).
                    true
                } else {
                    // refname*, symref*, upstream*, push*, color:, HEAD, ahead-behind:,
                    // and any unsupported placeholder need no object read here.
                    false
                }
            }
        };
        if consumes_object {
            if peeled {
                self.peeled = true;
            } else {
                self.object = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// %(trailers) / %(contents:trailers) — a focused port of git's
// format_trailers_from_commit (trailer.c) restricted to the for-each-ref atom
// option set: only, unfold, keyonly, valueonly, key, separator,
// key_value_separator.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct ForEachRefTrailerOptions {
    only: bool,
    unfold: bool,
    key_only: bool,
    value_only: bool,
    /// `Some` when any `key=` filter was given; lookups are case-insensitive.
    filter: Option<Vec<String>>,
    separator: Option<String>,
    key_value_separator: Option<String>,
}

/// Parse the `%(trailers:...)` option string (the part after the colon, with a
/// synthetic trailing `)` removed). `Err(None)` => `expected %(trailers:key=...)`;
/// `Err(Some(arg))` => `unknown %(trailers) argument: arg`.
pub(crate) fn parse_for_each_ref_trailer_options(
    arg: &str,
) -> std::result::Result<ForEachRefTrailerOptions, Option<String>> {
    let mut options = ForEachRefTrailerOptions::default();
    let mut rest = arg;
    loop {
        if rest.is_empty() {
            break;
        }
        if let Some((value, tail)) = for_each_ref_match_arg_value(rest, "key") {
            // git: a `key` with no `=value` is an error (-1 -> expected ...).
            let Some(value) = value else {
                return Err(None);
            };
            let value = value.strip_suffix(':').unwrap_or(value);
            options
                .filter
                .get_or_insert_with(Vec::new)
                .push(value.to_string());
            options.only = true;
            rest = tail;
        } else if let Some((value, tail)) = for_each_ref_match_arg_value(rest, "separator") {
            options.separator = Some(for_each_ref_expand_string_arg(value.unwrap_or("")));
            rest = tail;
        } else if let Some((value, tail)) =
            for_each_ref_match_arg_value(rest, "key_value_separator")
        {
            options.key_value_separator = Some(for_each_ref_expand_string_arg(value.unwrap_or("")));
            rest = tail;
        } else if let Some(tail) = for_each_ref_match_bool_arg(rest, "only", &mut options.only) {
            rest = tail;
        } else if let Some(tail) = for_each_ref_match_bool_arg(rest, "unfold", &mut options.unfold) {
            rest = tail;
        } else if let Some(tail) =
            for_each_ref_match_bool_arg(rest, "keyonly", &mut options.key_only)
        {
            rest = tail;
        } else if let Some(tail) =
            for_each_ref_match_bool_arg(rest, "valueonly", &mut options.value_only)
        {
            rest = tail;
        } else {
            // git: invalid_arg = up to the next ',' or ')'.
            let len = rest
                .find([',', ')'])
                .unwrap_or(rest.len());
            return Err(Some(rest[..len].to_string()));
        }
    }
    Ok(options)
}

/// git `match_placeholder_arg_value`: match `candidate` at the start of `to_parse`
/// followed by `=value` (until `,`/`)`), or bare (followed by `,`/end). Returns
/// `(value, remainder)` on a match. The input has no trailing `)` (we operate on
/// the comma-joined option list directly), so end-of-string acts like `)`.
fn for_each_ref_match_arg_value<'a>(
    to_parse: &'a str,
    candidate: &str,
) -> Option<(Option<&'a str>, &'a str)> {
    let p = to_parse.strip_prefix(candidate)?;
    if let Some(after_eq) = p.strip_prefix('=') {
        let len = after_eq.find([',', ')']).unwrap_or(after_eq.len());
        let value = &after_eq[..len];
        let p = &after_eq[len..];
        let tail = p.strip_prefix(',').unwrap_or(p);
        Some((Some(value), tail))
    } else if let Some(tail) = p.strip_prefix(',') {
        Some((None, tail))
    } else if p.is_empty() || p.starts_with(')') {
        Some((None, p.strip_prefix(')').unwrap_or(p)))
    } else {
        None
    }
}

/// git `match_placeholder_bool_arg` for the value-less boolean options used by
/// for-each-ref (`only`/`unfold`/`keyonly`/`valueonly`), incl. `=yes/no/...`.
fn for_each_ref_match_bool_arg<'a>(
    to_parse: &'a str,
    candidate: &str,
    out: &mut bool,
) -> Option<&'a str> {
    let (value, tail) = for_each_ref_match_arg_value(to_parse, candidate)?;
    match value {
        None => {
            *out = true;
            Some(tail)
        }
        Some(value) => match for_each_ref_parse_maybe_bool(value) {
            Some(v) => {
                *out = v;
                Some(tail)
            }
            // git returns 0 here (no match) so the option falls through to the
            // unknown-argument path.
            None => None,
        },
    }
}

fn for_each_ref_parse_maybe_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" => Some(true),
        "0" | "no" | "false" => Some(false),
        _ => None,
    }
}

/// git `expand_string_arg`: only `%%` and `%x##` literal escapes are expanded;
/// any other `%` is emitted verbatim.
fn for_each_ref_expand_string_arg(arg: &str) -> String {
    let bytes = arg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] != b'%' {
            out.push(bytes[idx]);
            idx += 1;
            continue;
        }
        if bytes.get(idx + 1) == Some(&b'%') {
            out.push(b'%');
            idx += 2;
        } else if bytes.get(idx + 1) == Some(&b'x')
            && let (Some(h), Some(l)) = (
                bytes.get(idx + 2).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(idx + 3).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            out.push((h * 16 + l) as u8);
            idx += 4;
        } else {
            out.push(b'%');
            idx += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct ForEachRefTrailerItem {
    /// `Some(token)` for a real trailer; `None` for a preserved non-trailer line.
    token: Option<String>,
    value: String,
}

/// Render `%(trailers)` for `message` under `options`, mirroring git's
/// `format_trailers_from_commit` + `format_trailers`.
pub(crate) fn for_each_ref_format_trailers(message: &[u8], options: &ForEachRefTrailerOptions) -> Vec<u8> {
    let text = String::from_utf8_lossy(message);
    let block = for_each_ref_trailer_block(&text);
    // Fast path: unmodified whole block.
    if !options.only
        && !options.unfold
        && options.filter.is_none()
        && options.separator.is_none()
        && !options.key_only
        && !options.value_only
        && options.key_value_separator.is_none()
    {
        return block.as_bytes().to_vec();
    }
    let items = for_each_ref_parse_trailer_items(&block, options);
    let mut out = String::new();
    let orig_len = out.len();
    for item in &items {
        match &item.token {
            Some(token) => {
                let mut value = item.value.clone();
                if options.unfold {
                    value = for_each_ref_unfold(&value);
                }
                if let Some(filter) = &options.filter
                    && !filter.iter().any(|key| key.eq_ignore_ascii_case(token))
                {
                    continue;
                }
                if let Some(sep) = &options.separator
                    && out.len() != orig_len
                {
                    out.push_str(sep);
                }
                if !options.value_only {
                    out.push_str(token);
                }
                if !options.key_only && !options.value_only {
                    if let Some(kvsep) = &options.key_value_separator {
                        out.push_str(kvsep);
                    } else {
                        // git appends "%c " using separators[0] (':') only when
                        // the token doesn't already end with a separator char.
                        let last = token.trim_end().chars().last();
                        if last != Some(':') {
                            out.push_str(": ");
                        }
                    }
                }
                if !options.key_only {
                    out.push_str(&value);
                }
                if options.separator.is_none() {
                    out.push('\n');
                }
            }
            None => {
                if options.only {
                    continue;
                }
                if let Some(sep) = &options.separator
                    && out.len() != orig_len
                {
                    out.push_str(sep);
                }
                out.push_str(&item.value);
                if options.separator.is_some() {
                    while out.ends_with([' ', '\t', '\n', '\r']) {
                        out.pop();
                    }
                } else {
                    out.push('\n');
                }
            }
        }
    }
    out.into_bytes()
}

/// The trailer block text (`[start, end)`) of a message, with `no_divider=1`
/// (the whole message is the log region).
fn for_each_ref_trailer_block(message: &str) -> String {
    let bytes = message.as_bytes();
    let len = bytes.len();
    let start = for_each_ref_find_trailer_block_start(message, len);
    message[start..].to_string()
}

/// Port of trailer.c `find_trailer_block_start` (no comment prefix; default
/// `:` separator; `Signed-off-by: ` / `(cherry picked from commit ` prefixes).
fn for_each_ref_find_trailer_block_start(buf: &str, len: usize) -> usize {
    let bytes = buf.as_bytes();
    // Skip the title paragraph up to the first blank line.
    let mut s = 0usize;
    while s < len {
        if for_each_ref_is_blank_line(bytes, s) {
            break;
        }
        s = for_each_ref_next_line(bytes, s, len);
    }
    let end_of_title = s;

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i64;
    let mut non_trailer_lines = 0i64;
    let mut possible_continuation = 0i64;

    let mut maybe_l = for_each_ref_last_line(bytes, len);
    while let Some(l) = maybe_l {
        if l < end_of_title {
            break;
        }
        if for_each_ref_is_blank_line(bytes, l) {
            if only_spaces {
                // trailing blank; keep scanning upward
            } else {
                non_trailer_lines += possible_continuation;
                if (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
                    || (trailer_lines > 0 && non_trailer_lines == 0)
                {
                    return for_each_ref_next_line(bytes, l, len);
                }
                return len;
            }
        } else {
            only_spaces = false;
            let line = for_each_ref_line_text(buf, l, len);
            if line.starts_with("Signed-off-by: ")
                || line.starts_with("(cherry picked from commit ")
            {
                trailer_lines += 1;
                possible_continuation = 0;
                recognized_prefix = true;
            } else if for_each_ref_find_separator(line).is_some_and(|pos| pos >= 1)
                && !bytes[l].is_ascii_whitespace()
            {
                trailer_lines += 1;
                possible_continuation = 0;
            } else if bytes[l].is_ascii_whitespace() {
                possible_continuation += 1;
            } else {
                non_trailer_lines += 1;
                non_trailer_lines += possible_continuation;
                possible_continuation = 0;
            }
        }
        if l == 0 {
            break;
        }
        maybe_l = for_each_ref_last_line(bytes, l);
    }
    len
}

/// Parse the trailer block into items, joining continuation lines (git's
/// `trailer_block_get` split + `parse_trailers`).
fn for_each_ref_parse_trailer_items(
    block: &str,
    options: &ForEachRefTrailerOptions,
) -> Vec<ForEachRefTrailerItem> {
    // Split on '\n' keeping each line; fold continuation lines (leading
    // whitespace) into the previous line *only if it had a separator*.
    let mut lines: Vec<String> = Vec::new();
    let mut last_had_sep = false;
    for raw in block.split_inclusive('\n') {
        if last_had_sep
            && raw.starts_with([' ', '\t'])
            && let Some(prev) = lines.last_mut()
        {
            prev.push_str(raw);
            continue;
        }
        let has_sep = for_each_ref_find_separator(raw).is_some_and(|pos| pos >= 1);
        last_had_sep = has_sep;
        lines.push(raw.to_string());
    }

    let mut items = Vec::new();
    for line in &lines {
        // Trim a single trailing newline for separator analysis / raw value.
        let trimmed_nl = line.strip_suffix('\n').unwrap_or(line);
        match for_each_ref_find_separator(line).filter(|pos| *pos >= 1) {
            Some(sep) => {
                let token = line[..sep].trim().to_string();
                let value = line[sep + 1..].trim().to_string();
                items.push(ForEachRefTrailerItem {
                    token: Some(token),
                    value,
                });
            }
            None => {
                if !options.only {
                    items.push(ForEachRefTrailerItem {
                        token: None,
                        value: trimmed_nl.to_string(),
                    });
                }
            }
        }
    }
    items
}

/// git `find_separator` restricted to the default `:` separator.
fn for_each_ref_find_separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut whitespace_found = false;
    for (idx, &c) in bytes.iter().enumerate() {
        if c == b':' {
            return Some(idx);
        }
        if !whitespace_found && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if idx != 0 && (c == b' ' || c == b'\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

/// git `unfold_value`: a newline plus following whitespace run collapses to one
/// space; result is trimmed.
fn for_each_ref_unfold(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn for_each_ref_is_blank_line(bytes: &[u8], pos: usize) -> bool {
    let mut idx = pos;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        if !bytes[idx].is_ascii_whitespace() {
            return false;
        }
        idx += 1;
    }
    true
}

fn for_each_ref_next_line(bytes: &[u8], pos: usize, len: usize) -> usize {
    match bytes[pos..len].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel + 1,
        None => len,
    }
}

/// The byte offset of the start of the last line within `bytes[..len]`.
fn for_each_ref_last_line(bytes: &[u8], len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // If the region ends with '\n', that newline terminates the prior line.
    let end = if bytes[len - 1] == b'\n' { len - 1 } else { len };
    if end == 0 {
        return Some(0);
    }
    match bytes[..end].iter().rposition(|&b| b == b'\n') {
        Some(nl) => Some(nl + 1),
        None => Some(0),
    }
}

fn for_each_ref_line_text(buf: &str, pos: usize, len: usize) -> &str {
    let bytes = buf.as_bytes();
    let end = match bytes[pos..len].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel,
        None => len,
    };
    &buf[pos..end]
}

/// Whether the format contains a bare `%(raw)` / `%(*raw)` atom (not
/// `raw:size`), which git forbids under `--python`/`--shell`/`--tcl`.
fn for_each_ref_format_has_bare_raw(format_spec: &ForEachRefFormat) -> bool {
    format_spec.segments().iter().any(|segment| {
        matches!(
            segment,
            ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(placeholder))
                if placeholder == "raw" || placeholder == "*raw"
        )
    })
}

fn parse_for_each_ref_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid for-each-ref count {value}")))
}

fn parse_for_each_ref_sort(value: &str) -> Result<ForEachRefSort> {
    match value {
        "refname" => Ok(ForEachRefSort::Refname),
        "-refname" => Ok(ForEachRefSort::RefnameDescending),
        "objectname" => Ok(ForEachRefSort::ObjectName),
        "-objectname" => Ok(ForEachRefSort::ObjectNameDescending),
        "objecttype" => Ok(ForEachRefSort::ObjectType),
        "-objecttype" => Ok(ForEachRefSort::ObjectTypeDescending),
        "objectsize" => Ok(ForEachRefSort::ObjectSize),
        "-objectsize" => Ok(ForEachRefSort::ObjectSizeDescending),
        "objectsize:disk" => Ok(ForEachRefSort::ObjectSizeDisk),
        "-objectsize:disk" => Ok(ForEachRefSort::ObjectSizeDiskDescending),
        "upstream" => Ok(ForEachRefSort::Upstream),
        "-upstream" => Ok(ForEachRefSort::UpstreamDescending),
        "push" => Ok(ForEachRefSort::Push),
        "-push" => Ok(ForEachRefSort::PushDescending),
        "symref" => Ok(ForEachRefSort::Symref),
        "-symref" => Ok(ForEachRefSort::SymrefDescending),
        "worktreepath" => Ok(ForEachRefSort::WorktreePath),
        "-worktreepath" => Ok(ForEachRefSort::WorktreePathDescending),
        "tag" => Ok(ForEachRefSort::Tag),
        "-tag" => Ok(ForEachRefSort::TagDescending),
        "type" => Ok(ForEachRefSort::Type),
        "-type" => Ok(ForEachRefSort::TypeDescending),
        "object" => Ok(ForEachRefSort::Object),
        "-object" => Ok(ForEachRefSort::ObjectDescending),
        "subject" | "contents:subject" => Ok(ForEachRefSort::Subject),
        "-subject" | "-contents:subject" => Ok(ForEachRefSort::SubjectDescending),
        "body" | "contents:body" => Ok(ForEachRefSort::Body),
        "-body" | "-contents:body" => Ok(ForEachRefSort::BodyDescending),
        "contents:size" => Ok(ForEachRefSort::ContentsSize),
        "-contents:size" => Ok(ForEachRefSort::ContentsSizeDescending),
        "raw" => Ok(ForEachRefSort::Raw),
        "-raw" => Ok(ForEachRefSort::RawDescending),
        "raw:size" => Ok(ForEachRefSort::RawSize),
        "-raw:size" => Ok(ForEachRefSort::RawSizeDescending),
        "*subject" | "*contents:subject" => Ok(ForEachRefSort::PeeledSubject),
        "-*subject" | "-*contents:subject" => Ok(ForEachRefSort::PeeledSubjectDescending),
        "*body" | "*contents:body" => Ok(ForEachRefSort::PeeledBody),
        "-*body" | "-*contents:body" => Ok(ForEachRefSort::PeeledBodyDescending),
        "*contents:size" => Ok(ForEachRefSort::PeeledContentsSize),
        "-*contents:size" => Ok(ForEachRefSort::PeeledContentsSizeDescending),
        "*objectname" => Ok(ForEachRefSort::PeeledObjectName),
        "-*objectname" => Ok(ForEachRefSort::PeeledObjectNameDescending),
        "*objecttype" => Ok(ForEachRefSort::PeeledObjectType),
        "-*objecttype" => Ok(ForEachRefSort::PeeledObjectTypeDescending),
        "*objectsize" => Ok(ForEachRefSort::PeeledObjectSize),
        "-*objectsize" => Ok(ForEachRefSort::PeeledObjectSizeDescending),
        "*objectsize:disk" => Ok(ForEachRefSort::PeeledObjectSizeDisk),
        "-*objectsize:disk" => Ok(ForEachRefSort::PeeledObjectSizeDiskDescending),
        "*deltabase" => Ok(ForEachRefSort::PeeledDeltabase),
        "-*deltabase" => Ok(ForEachRefSort::PeeledDeltabaseDescending),
        "*raw:size" => Ok(ForEachRefSort::PeeledRawSize),
        "-*raw:size" => Ok(ForEachRefSort::PeeledRawSizeDescending),
        "tree" => Ok(ForEachRefSort::Tree),
        "-tree" => Ok(ForEachRefSort::TreeDescending),
        "parent" => Ok(ForEachRefSort::Parent),
        "-parent" => Ok(ForEachRefSort::ParentDescending),
        "numparent" => Ok(ForEachRefSort::NumParent),
        "-numparent" => Ok(ForEachRefSort::NumParentDescending),
        "*tree" => Ok(ForEachRefSort::PeeledTree),
        "-*tree" => Ok(ForEachRefSort::PeeledTreeDescending),
        "*parent" => Ok(ForEachRefSort::PeeledParent),
        "-*parent" => Ok(ForEachRefSort::PeeledParentDescending),
        "*numparent" => Ok(ForEachRefSort::PeeledNumParent),
        "-*numparent" => Ok(ForEachRefSort::PeeledNumParentDescending),
        "authordate" => Ok(ForEachRefSort::AuthorDate),
        "-authordate" => Ok(ForEachRefSort::AuthorDateDescending),
        "committerdate" => Ok(ForEachRefSort::CommitterDate),
        "-committerdate" => Ok(ForEachRefSort::CommitterDateDescending),
        "taggerdate" => Ok(ForEachRefSort::TaggerDate),
        "-taggerdate" => Ok(ForEachRefSort::TaggerDateDescending),
        "creatordate" => Ok(ForEachRefSort::CreatorDate),
        "-creatordate" => Ok(ForEachRefSort::CreatorDateDescending),
        "*authordate" => Ok(ForEachRefSort::PeeledAuthorDate),
        "-*authordate" => Ok(ForEachRefSort::PeeledAuthorDateDescending),
        "*committerdate" => Ok(ForEachRefSort::PeeledCommitterDate),
        "-*committerdate" => Ok(ForEachRefSort::PeeledCommitterDateDescending),
        "*taggerdate" => Ok(ForEachRefSort::PeeledTaggerDate),
        "-*taggerdate" => Ok(ForEachRefSort::PeeledTaggerDateDescending),
        "*creatordate" => Ok(ForEachRefSort::PeeledCreatorDate),
        "-*creatordate" => Ok(ForEachRefSort::PeeledCreatorDateDescending),
        "version:refname" | "v:refname" => Ok(ForEachRefSort::VersionRefname),
        "-version:refname" | "-v:refname" => Ok(ForEachRefSort::VersionRefnameDescending),
        other => {
            if let Some((field, descending)) = parse_for_each_ref_identity_sort(other) {
                Ok(if descending {
                    ForEachRefSort::IdentityDescending(field)
                } else {
                    ForEachRefSort::Identity(field)
                })
            } else {
                Err(GitError::Command(format!(
                    "unsupported for-each-ref sort key {other}"
                )))
            }
        }
    }
}

struct ForEachRefSortContext<'a> {
    ignore_case: bool,
    store: &'a FileRefStore,
    config: &'a GitConfig,
    db: &'a FileObjectDatabase,
    git_dir: &'a Path,
    head_ref: Option<&'a str>,
    format: ObjectFormat,
}

fn sort_for_each_refs(
    refs: &mut Vec<sley_refs::Ref>,
    sorts: &[ForEachRefSort],
    context: ForEachRefSortContext<'_>,
) -> Result<()> {
    let mut keyed = Vec::with_capacity(refs.len());
    for reference in refs.drain(..) {
        let keys = sorts
            .iter()
            .map(|sort| for_each_ref_sort_key(&reference, *sort, &context))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((reference, keys));
    }
    keyed.sort_by(|left, right| compare_for_each_ref_sort_keys(sorts, &left.1, &right.1));
    refs.extend(keyed.into_iter().map(|(reference, _)| reference));
    Ok(())
}

fn compare_for_each_ref_sort_keys(
    sorts: &[ForEachRefSort],
    left: &[ForEachRefSortKey],
    right: &[ForEachRefSortKey],
) -> std::cmp::Ordering {
    for idx in (0..sorts.len()).rev() {
        let ordering = if sorts[idx].descending() {
            right[idx].cmp(&left[idx])
        } else {
            left[idx].cmp(&right[idx])
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

impl ForEachRefSort {
    fn descending(self) -> bool {
        matches!(
            self,
            ForEachRefSort::RefnameDescending
                | ForEachRefSort::IdentityDescending(_)
                | ForEachRefSort::ObjectNameDescending
                | ForEachRefSort::ObjectTypeDescending
                | ForEachRefSort::ObjectSizeDescending
                | ForEachRefSort::ObjectSizeDiskDescending
                | ForEachRefSort::UpstreamDescending
                | ForEachRefSort::PushDescending
                | ForEachRefSort::SymrefDescending
                | ForEachRefSort::WorktreePathDescending
                | ForEachRefSort::TagDescending
                | ForEachRefSort::TypeDescending
                | ForEachRefSort::ObjectDescending
                | ForEachRefSort::SubjectDescending
                | ForEachRefSort::BodyDescending
                | ForEachRefSort::ContentsSizeDescending
                | ForEachRefSort::RawDescending
                | ForEachRefSort::RawSizeDescending
                | ForEachRefSort::PeeledSubjectDescending
                | ForEachRefSort::PeeledBodyDescending
                | ForEachRefSort::PeeledContentsSizeDescending
                | ForEachRefSort::PeeledObjectNameDescending
                | ForEachRefSort::PeeledObjectTypeDescending
                | ForEachRefSort::PeeledObjectSizeDescending
                | ForEachRefSort::PeeledObjectSizeDiskDescending
                | ForEachRefSort::PeeledDeltabaseDescending
                | ForEachRefSort::PeeledRawSizeDescending
                | ForEachRefSort::TreeDescending
                | ForEachRefSort::ParentDescending
                | ForEachRefSort::NumParentDescending
                | ForEachRefSort::PeeledTreeDescending
                | ForEachRefSort::PeeledParentDescending
                | ForEachRefSort::PeeledNumParentDescending
                | ForEachRefSort::AuthorDateDescending
                | ForEachRefSort::CommitterDateDescending
                | ForEachRefSort::TaggerDateDescending
                | ForEachRefSort::CreatorDateDescending
                | ForEachRefSort::PeeledAuthorDateDescending
                | ForEachRefSort::PeeledCommitterDateDescending
                | ForEachRefSort::PeeledTaggerDateDescending
                | ForEachRefSort::PeeledCreatorDateDescending
                | ForEachRefSort::VersionRefnameDescending
        )
    }
}

fn for_each_ref_sort_key(
    reference: &sley_refs::Ref,
    sort: ForEachRefSort,
    context: &ForEachRefSortContext<'_>,
) -> Result<ForEachRefSortKey> {
    let key = match sort {
        ForEachRefSort::Refname | ForEachRefSort::RefnameDescending => {
            ForEachRefSortKey::Text(reference.name.clone())
        }
        ForEachRefSort::Identity(field) | ForEachRefSort::IdentityDescending(field) => {
            let contents = match field.source {
                ForEachRefIdentitySource::Direct => for_each_ref_sort_contents(reference, context)?,
                ForEachRefIdentitySource::Peeled => {
                    for_each_ref_sort_peeled_contents(reference, context)?
                }
            };
            ForEachRefSortKey::Text(for_each_ref_sort_identity_key(contents.as_ref(), field))
        }
        ForEachRefSort::VersionRefname | ForEachRefSort::VersionRefnameDescending => {
            ForEachRefSortKey::Version(reference.name.clone())
        }
        ForEachRefSort::Upstream | ForEachRefSort::UpstreamDescending => ForEachRefSortKey::Text(
            for_each_ref_upstream(context.config, &reference.name)
                .map(|upstream| upstream.refname)
                .unwrap_or_default(),
        ),
        ForEachRefSort::Push | ForEachRefSort::PushDescending => ForEachRefSortKey::Text(
            for_each_ref_push(context.config, &reference.name)
                .and_then(|push| push.refname)
                .unwrap_or_default(),
        ),
        ForEachRefSort::Symref | ForEachRefSort::SymrefDescending => ForEachRefSortKey::Text(
            resolve_for_each_ref_target(context.store, reference)?
                .and_then(|(_, symref)| symref)
                .unwrap_or_default(),
        ),
        ForEachRefSort::WorktreePath | ForEachRefSort::WorktreePathDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_worktree_path(context.git_dir, context.head_ref, &reference.name)?
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::Tag | ForEachRefSort::TagDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_tag_contents(reference, context)?
                .and_then(|contents| contents.tag)
                .map(|tag| String::from_utf8_lossy(&tag).into_owned())
                .unwrap_or_default(),
        ),
        ForEachRefSort::Type | ForEachRefSort::TypeDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_tag_contents(reference, context)?
                .and_then(|contents| contents.tag_object_type)
                .map(|object_type| object_type.as_str().to_string())
                .unwrap_or_default(),
        ),
        ForEachRefSort::Object | ForEachRefSort::ObjectDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_tag_contents(reference, context)?
                .and_then(|contents| contents.tag_object)
                .map(|object| object.to_hex())
                .unwrap_or_default(),
        ),
        ForEachRefSort::Subject | ForEachRefSort::SubjectDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_contents(reference, context)?
                .map(|contents| commit_subject(&contents.message))
                .unwrap_or_default(),
        ),
        ForEachRefSort::Body | ForEachRefSort::BodyDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_contents(reference, context)?
                .map(|contents| {
                    String::from_utf8_lossy(commit_body(&contents.message)).into_owned()
                })
                .unwrap_or_default(),
        ),
        ForEachRefSort::ContentsSize | ForEachRefSort::ContentsSizeDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_contents(reference, context)?
                    .map(|contents| contents.message.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::Raw | ForEachRefSort::RawDescending => ForEachRefSortKey::Bytes(
            if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                context.db.read_object(&oid)?.body.clone()
            } else {
                Vec::new()
            },
        ),
        ForEachRefSort::RawSize | ForEachRefSort::RawSizeDescending => ForEachRefSortKey::Number(
            if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                context.db.read_object(&oid)?.body.len() as i128
            } else {
                0
            },
        ),
        ForEachRefSort::PeeledSubject | ForEachRefSort::PeeledSubjectDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .map(|contents| commit_subject(&contents.message))
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledBody | ForEachRefSort::PeeledBodyDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .map(|contents| {
                        String::from_utf8_lossy(commit_body(&contents.message)).into_owned()
                    })
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledContentsSize | ForEachRefSort::PeeledContentsSizeDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .map(|contents| contents.message.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::PeeledObjectName | ForEachRefSort::PeeledObjectNameDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_object(reference, context)?
                    .map(|(oid, _)| oid.to_hex())
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledObjectType | ForEachRefSort::PeeledObjectTypeDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_object(reference, context)?
                    .map(|(_, object)| object.object_type.as_str().to_string())
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledObjectSize | ForEachRefSort::PeeledObjectSizeDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_peeled_object(reference, context)?
                    .map(|(_, object)| object.body.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::PeeledObjectSizeDisk | ForEachRefSort::PeeledObjectSizeDiskDescending => {
            ForEachRefSortKey::Number(
                if let Some((oid, _)) = for_each_ref_sort_peeled_object(reference, context)? {
                    for_each_ref_loose_object_disk_size(context.git_dir, &oid)?
                        .map(i128::from)
                        .unwrap_or(0)
                } else {
                    0
                },
            )
        }
        ForEachRefSort::PeeledDeltabase | ForEachRefSort::PeeledDeltabaseDescending => {
            ForEachRefSortKey::Text(
                if for_each_ref_sort_peeled_object(reference, context)?.is_some() {
                    zero_oid(context.format)?.to_hex()
                } else {
                    String::new()
                },
            )
        }
        ForEachRefSort::PeeledRawSize | ForEachRefSort::PeeledRawSizeDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_peeled_object(reference, context)?
                    .map(|(_, object)| object.body.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::Tree | ForEachRefSort::TreeDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_contents(reference, context)?
                .and_then(|contents| contents.tree)
                .map(|tree| tree.to_hex())
                .unwrap_or_default(),
        ),
        ForEachRefSort::Parent | ForEachRefSort::ParentDescending => ForEachRefSortKey::Text(
            for_each_ref_sort_contents(reference, context)?
                .map(|contents| {
                    contents
                        .parents
                        .iter()
                        .map(ObjectId::to_hex)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
        ),
        ForEachRefSort::NumParent | ForEachRefSort::NumParentDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_contents(reference, context)?
                    .filter(|contents| contents.tree.is_some())
                    .map(|contents| contents.parents.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::PeeledTree | ForEachRefSort::PeeledTreeDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .and_then(|contents| contents.tree)
                    .map(|tree| tree.to_hex())
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledParent | ForEachRefSort::PeeledParentDescending => {
            ForEachRefSortKey::Text(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .map(|contents| {
                        contents
                            .parents
                            .iter()
                            .map(ObjectId::to_hex)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::PeeledNumParent | ForEachRefSort::PeeledNumParentDescending => {
            ForEachRefSortKey::Number(
                for_each_ref_sort_peeled_contents(reference, context)?
                    .filter(|contents| contents.tree.is_some())
                    .map(|contents| contents.parents.len() as i128)
                    .unwrap_or(0),
            )
        }
        ForEachRefSort::ObjectName | ForEachRefSort::ObjectNameDescending => {
            ForEachRefSortKey::Text(
                resolve_for_each_ref_target(context.store, reference)?
                    .map(|(oid, _)| oid.to_hex())
                    .unwrap_or_default(),
            )
        }
        ForEachRefSort::ObjectType | ForEachRefSort::ObjectTypeDescending => {
            ForEachRefSortKey::Text(
                if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                    context
                        .db
                        .read_object(&oid)?
                        .object_type
                        .as_str()
                        .to_string()
                } else {
                    String::new()
                },
            )
        }
        ForEachRefSort::ObjectSize | ForEachRefSort::ObjectSizeDescending => {
            ForEachRefSortKey::Number(
                if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                    context.db.read_object(&oid)?.body.len() as i128
                } else {
                    0
                },
            )
        }
        ForEachRefSort::ObjectSizeDisk | ForEachRefSort::ObjectSizeDiskDescending => {
            ForEachRefSortKey::Number(
                if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                    for_each_ref_loose_object_disk_size(context.git_dir, &oid)?
                        .map(i128::from)
                        .unwrap_or(0)
                } else {
                    0
                },
            )
        }
        ForEachRefSort::AuthorDate | ForEachRefSort::AuthorDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_contents(reference, context)?,
                ForEachRefDateSortField::Author,
            ))
        }
        ForEachRefSort::CommitterDate | ForEachRefSort::CommitterDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_contents(reference, context)?,
                ForEachRefDateSortField::Committer,
            ))
        }
        ForEachRefSort::TaggerDate | ForEachRefSort::TaggerDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_contents(reference, context)?,
                ForEachRefDateSortField::Tagger,
            ))
        }
        ForEachRefSort::CreatorDate | ForEachRefSort::CreatorDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_contents(reference, context)?,
                ForEachRefDateSortField::Creator,
            ))
        }
        ForEachRefSort::PeeledAuthorDate | ForEachRefSort::PeeledAuthorDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_peeled_contents(reference, context)?,
                ForEachRefDateSortField::Author,
            ))
        }
        ForEachRefSort::PeeledCommitterDate | ForEachRefSort::PeeledCommitterDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_peeled_contents(reference, context)?,
                ForEachRefDateSortField::Committer,
            ))
        }
        ForEachRefSort::PeeledTaggerDate | ForEachRefSort::PeeledTaggerDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_peeled_contents(reference, context)?,
                ForEachRefDateSortField::Tagger,
            ))
        }
        ForEachRefSort::PeeledCreatorDate | ForEachRefSort::PeeledCreatorDateDescending => {
            ForEachRefSortKey::Number(for_each_ref_sort_date_key(
                for_each_ref_sort_peeled_contents(reference, context)?,
                ForEachRefDateSortField::Creator,
            ))
        }
    };
    Ok(match (key, context.ignore_case) {
        (ForEachRefSortKey::Text(value), true) => {
            ForEachRefSortKey::Text(value.to_ascii_lowercase())
        }
        (ForEachRefSortKey::Version(value), true) => {
            ForEachRefSortKey::Version(value.to_ascii_lowercase())
        }
        (key, _) => key,
    })
}

fn for_each_ref_sort_tag_contents(
    reference: &sley_refs::Ref,
    context: &ForEachRefSortContext<'_>,
) -> Result<Option<ForEachRefContents<'static>>> {
    let Some(contents) = for_each_ref_sort_contents(reference, context)? else {
        return Ok(None);
    };
    if contents.tag.is_none() {
        return Ok(None);
    }
    Ok(Some(contents))
}

fn for_each_ref_sort_contents(
    reference: &sley_refs::Ref,
    context: &ForEachRefSortContext<'_>,
) -> Result<Option<ForEachRefContents<'static>>> {
    let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? else {
        return Ok(None);
    };
    let object = context.db.read_object(&oid)?;
    for_each_ref_contents_owned(context.format, &object)
}

fn for_each_ref_sort_peeled_object(
    reference: &sley_refs::Ref,
    context: &ForEachRefSortContext<'_>,
) -> Result<Option<(ObjectId, sley_object::EncodedObject)>> {
    let Some(contents) = for_each_ref_sort_tag_contents(reference, context)? else {
        return Ok(None);
    };
    let Some(oid) = contents.tag_object else {
        return Ok(None);
    };
    let object = context.db.read_object(&oid)?;
    Ok(Some((oid, (*object).clone())))
}

fn for_each_ref_sort_peeled_contents(
    reference: &sley_refs::Ref,
    context: &ForEachRefSortContext<'_>,
) -> Result<Option<ForEachRefContents<'static>>> {
    let Some((_, object)) = for_each_ref_sort_peeled_object(reference, context)? else {
        return Ok(None);
    };
    for_each_ref_contents_owned(context.format, &object)
}

#[derive(Clone, Eq, PartialEq)]
enum ForEachRefSortKey {
    Number(i128),
    Text(String),
    Version(String),
    /// Raw object bytes (`--sort=raw`): git compares with memcmp over the
    /// shared prefix, then by length — Rust's `Vec<u8>` Ord matches exactly.
    Bytes(Vec<u8>),
}

impl Ord for ForEachRefSortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (ForEachRefSortKey::Number(left), ForEachRefSortKey::Number(right)) => left.cmp(right),
            (ForEachRefSortKey::Text(left), ForEachRefSortKey::Text(right)) => left.cmp(right),
            (ForEachRefSortKey::Version(left), ForEachRefSortKey::Version(right)) => {
                version_sort_cmp(left, right, &[])
            }
            (ForEachRefSortKey::Bytes(left), ForEachRefSortKey::Bytes(right)) => left.cmp(right),
            (left, right) => {
                for_each_ref_sort_key_rank(left).cmp(&for_each_ref_sort_key_rank(right))
            }
        }
    }
}

impl PartialOrd for ForEachRefSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn for_each_ref_sort_key_rank(key: &ForEachRefSortKey) -> u8 {
    match key {
        ForEachRefSortKey::Number(_) => 0,
        ForEachRefSortKey::Text(_) => 1,
        ForEachRefSortKey::Version(_) => 2,
        ForEachRefSortKey::Bytes(_) => 3,
    }
}

fn for_each_ref_points_at(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    targets: &[ObjectId],
) -> Result<bool> {
    if targets.iter().any(|target| target == oid) {
        return Ok(true);
    }
    let peeled = sley_rev::peel_tags(db, format, oid)?;
    Ok(peeled != *oid && targets.iter().any(|target| target == &peeled))
}

fn for_each_ref_pattern_matches(name: &str, pattern: &str, ignore_case: bool) -> bool {
    if pattern
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?'))
    {
        return for_each_ref_pattern_glob_matches(name, pattern, ignore_case);
    }
    if ignore_case {
        name.eq_ignore_ascii_case(pattern)
            || strip_prefix_ignore_ascii_case(name, pattern)
                .is_some_and(|rest| rest.starts_with('/'))
    } else {
        name == pattern
            || name
                .strip_prefix(pattern)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

fn for_each_ref_exclude_matches(name: &str, pattern: &str, ignore_case: bool) -> bool {
    for_each_ref_pattern_glob_matches(name, pattern, ignore_case)
}

fn for_each_ref_pattern_glob_matches(name: &str, pattern: &str, ignore_case: bool) -> bool {
    fn matches_from(pattern: &[u8], name: &[u8]) -> bool {
        match pattern {
            [] => name.is_empty(),
            [b'*', rest @ ..] => {
                matches_from(rest, name)
                    || (!name.is_empty() && name[0] != b'/' && matches_from(pattern, &name[1..]))
            }
            [b'?', rest @ ..] => {
                !name.is_empty() && name[0] != b'/' && matches_from(rest, &name[1..])
            }
            [literal, rest @ ..] => {
                matches!(name, [first, ..] if first == literal) && matches_from(rest, &name[1..])
            }
        }
    }
    fn matches_from_ignore_case(pattern: &[u8], name: &[u8]) -> bool {
        match pattern {
            [] => name.is_empty(),
            [b'*', rest @ ..] => {
                matches_from_ignore_case(rest, name)
                    || (!name.is_empty()
                        && name[0] != b'/'
                        && matches_from_ignore_case(pattern, &name[1..]))
            }
            [b'?', rest @ ..] => {
                !name.is_empty() && name[0] != b'/' && matches_from_ignore_case(rest, &name[1..])
            }
            [literal, rest @ ..] => {
                matches!(name, [first, ..] if first.eq_ignore_ascii_case(literal))
                    && matches_from_ignore_case(rest, &name[1..])
            }
        }
    }

    if ignore_case {
        matches_from_ignore_case(pattern.as_bytes(), name.as_bytes())
    } else {
        matches_from(pattern.as_bytes(), name.as_bytes())
    }
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&value[prefix.len()..])
}

fn for_each_ref_contents_owned(
    format: ObjectFormat,
    object: &sley_object::EncodedObject,
) -> Result<Option<ForEachRefContents<'static>>> {
    Ok(for_each_ref_contents(format, object)?.map(ForEachRefContents::into_owned))
}
