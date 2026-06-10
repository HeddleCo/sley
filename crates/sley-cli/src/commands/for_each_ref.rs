//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_for_each_ref(args: &[String]) -> Result<()> {
    let mut format_spec = "%(objectname) %(objecttype)\t%(refname)".to_string();
    let mut count = None;
    let mut omit_empty = false;
    let mut include_root_refs = false;
    let mut ignore_case = false;
    let mut color = false;
    let mut quote = ForEachRefQuoteMode::None;
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
            "--shell" | "-s" => quote = ForEachRefQuoteMode::Shell,
            "--python" => quote = ForEachRefQuoteMode::Python,
            "--perl" | "-p" => quote = ForEachRefQuoteMode::Perl,
            "--tcl" => quote = ForEachRefQuoteMode::Tcl,
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
    let format_spec = ForEachRefFormat::parse(&format_spec)?;
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
                } else if other.starts_with("authordate:")
                    || other.starts_with("committerdate:")
                    || other.starts_with("taggerdate:")
                    || other.starts_with("creatordate:")
                    || other.starts_with("authoremail:")
                    || other.starts_with("committeremail:")
                    || other.starts_with("taggeremail:")
                    || other.starts_with("contents:lines=")
                {
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
}

impl Ord for ForEachRefSortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (ForEachRefSortKey::Number(left), ForEachRefSortKey::Number(right)) => left.cmp(right),
            (ForEachRefSortKey::Text(left), ForEachRefSortKey::Text(right)) => left.cmp(right),
            (ForEachRefSortKey::Version(left), ForEachRefSortKey::Version(right)) => {
                version_sort_cmp(left, right)
            }
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
