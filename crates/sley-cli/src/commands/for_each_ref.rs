//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_object, sley_refs, sley_rev};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_for_each_ref(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let config = identity_effective_config_for(cli_session).unwrap_or_default();
    for_each_ref_core_with_config(cli_session, &git_dir, args, "git for-each-ref", &config)
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
pub(crate) fn for_each_ref_core_with_config(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    args: &[String],
    usage_cmd: &str,
    effective_config: &GitConfig,
) -> Result<()> {
    let git_dir = git_dir.to_path_buf();
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
        eprintln!("fatal: cannot use --start-after with custom sort options");
        return Err(GitError::Exit(128));
    }
    if start_after.is_some() && !patterns.is_empty() {
        eprintln!("fatal: cannot use --start-after with patterns");
        return Err(GitError::Exit(128));
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
    let is_base_targets = for_each_ref_is_base_targets(&format_spec)?;
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
    let format = repository_object_format(&git_dir)?;
    let objectname_abbrev = repository_abbrev(&git_dir, format)?;
    let db =
        crate::repository::open_object_database(&git_dir, format, cli_session.replace_objects())?;
    let points_at = points_at_revs
        .iter()
        .map(|rev| for_each_ref_resolve_revision(&git_dir, format, &db, rev))
        .collect::<Result<Vec<_>>>()?;
    let mut reachability = sley_rev::CommitReachability::new(&git_dir, format, &db);
    for_each_ref_validate_ahead_behind(&format_spec, &git_dir, format, &db)?;
    for_each_ref_validate_describe(&format_spec)?;
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
            let oid = for_each_ref_resolve_revision(&git_dir, format, &db, rev)?;
            sley_rev::peel_to_commit(&db, format, &oid)
        })
        .collect::<Result<Vec<_>>>()?;
    let no_contains_targets = no_contains_revs
        .iter()
        .map(|rev| {
            let oid = for_each_ref_resolve_revision(&git_dir, format, &db, rev)?;
            sley_rev::peel_to_commit(&db, format, &oid)
        })
        .collect::<Result<Vec<_>>>()?;
    let contains_target_set = contains_targets.iter().copied().collect::<HashSet<_>>();
    let no_contains_target_set = no_contains_targets.iter().copied().collect::<HashSet<_>>();
    let merged_filter = merged_filter
        .map(|(rev, include)| {
            let oid = for_each_ref_resolve_revision(&git_dir, format, &db, &rev)?;
            let commit = sley_rev::peel_to_commit(&db, format, &oid)?;
            let reachable = reachability.reachable_oids([commit], false)?;
            Ok::<_, GitError>((reachable, include))
        })
        .transpose()?;
    let store = FileRefStore::new(&git_dir, format);
    let head_ref = store.current_branch_ref()?;
    let main_worktree_root = worktree_root_for_git_dir(cli_session, &git_dir).ok();
    // Discover worktree paths once instead of re-scanning $GIT_DIR/worktrees per ref.
    let worktree_paths = if needs.worktree {
        for_each_ref_worktree_paths(&git_dir, main_worktree_root.as_deref(), head_ref.as_deref())?
    } else {
        HashMap::new()
    };
    let config = if needs.config || needs.short_ref || for_each_ref_sorts_need_config(&sorts) {
        // git resolves %(upstream)/%(push)/sort keys against the *full* config
        // layering (system + global + repo + includes + command-line `-c`
        // overrides), not just the repository file — e.g. `-c push.default=simple`
        // must win over a repo-level `push.default`. The command entry point
        // supplies that layered view from its explicit invocation session.
        effective_config.clone()
    } else {
        GitConfig::default()
    };
    // git resolves `%(...:mailmap)` atoms against the repository .mailmap plus
    // mailmap.{file,blob} config. Avoid probing those paths for formats that
    // never request mailmap rewriting.
    let mailmap = if needs.mailmap {
        commands::utility::Mailmap::load_default_with_config(
            &git_dir,
            format,
            effective_config,
            cli_session.replace_objects(),
        )?
    } else {
        commands::utility::Mailmap::default()
    };
    let mut object_headers = ForEachRefObjectHeaderCache::new();
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut emitted = 0usize;
    let mut refs = store.list_refs()?;
    // The `:short` atoms (refname/symref/upstream/push) resolve via git's
    // shorten_unambiguous_ref, which probes the ref store for ambiguity; collect
    // the ref-name universe once, plus `core.warnambiguousrefs` (default true).
    let ref_names: std::collections::HashSet<String> = if needs.short_ref {
        refs.iter()
            .map(|reference| reference.name.clone())
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let warn_ambiguous_refs = config
        .get_bool("core", None, "warnambiguousrefs")
        .unwrap_or(true);
    if include_root_refs {
        append_for_each_ref_root_refs(&git_dir, &store, &mut refs)?;
    }
    let is_base_refs =
        for_each_ref_compute_is_base_refs(&refs, &is_base_targets, &store, &git_dir, format, &db)?;
    sort_for_each_refs(
        &mut refs,
        &sorts,
        ForEachRefSortContext {
            ignore_case,
            store: &store,
            config: &config,
            db: &db,
            git_dir: &git_dir,
            main_worktree_root: main_worktree_root.as_deref(),
            head_ref: head_ref.as_deref(),
            format,
        },
        &mut object_headers,
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
            let target_match = sley_rev::peel_to_commit(&db, format, &oid)
                .ok()
                .map(|tip| {
                    reachability.target_match(
                        &tip,
                        &contains_target_set,
                        &no_contains_target_set,
                        false,
                    )
                })
                .transpose()?;
            let Some(target_match) = target_match else {
                continue;
            };
            if !target_match.reached_required {
                continue;
            }
            if target_match.reached_excluded {
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
        let upstream = if needs.upstream || needs.upstream_track {
            for_each_ref_upstream(&config, &reference.name)
        } else {
            None
        };
        let push = if needs.push || needs.push_track {
            for_each_ref_push(&config, &reference.name)
        } else {
            None
        };
        let upstream_track = if needs.upstream_track {
            upstream
                .as_ref()
                .map(|upstream| {
                    for_each_ref_upstream_track(
                        &store,
                        &git_dir,
                        &db,
                        format,
                        &oid,
                        &upstream.refname,
                    )
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        let push_track = if needs.push_track {
            push.as_ref()
                .and_then(|push| push.refname.as_deref())
                .map(|push_ref| {
                    for_each_ref_upstream_track(&store, &git_dir, &db, format, &oid, push_ref)
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        // Only decode the ref object when the format references an atom that needs
        // its body (git's used_atom analysis). Type/size-only atoms can use the
        // cheaper header path below, and formats like %(objectname)/%(refname)
        // read nothing here.
        let object = if needs.object {
            Some(
                db.read_object(&oid)
                    .map_err(|err| for_each_ref_missing_object(err, &oid, &reference.name))?,
            )
        } else {
            None
        };
        let object_header = if object.is_none() && needs.object_header {
            Some(
                for_each_ref_object_header(&db, &mut object_headers, &oid)
                    .map_err(|err| for_each_ref_missing_object(err, &oid, &reference.name))?,
            )
        } else {
            None
        };
        let contents = object
            .as_ref()
            .map(|object| for_each_ref_contents(format, object))
            .transpose()?
            .flatten();
        // The peeled tag target is only read when a %(*...) atom references it.
        // git's %(*...) atoms expose `peel_object`, which follows the *whole*
        // tag chain (a tag that points at another tag is peeled all the way to
        // the underlying non-tag object), so chase nested tags to the bottom.
        let peeled_chain: Option<(ObjectId, std::sync::Arc<sley_object::EncodedObject>)> = if needs
            .peeled
        {
            if let Some(first_oid) = contents.as_ref().and_then(|contents| contents.tag_object) {
                let mut target_oid = first_oid;
                let mut target = db.read_object(&target_oid)?;
                // Validate the outer tag's recorded pointer type first.
                if let Some(contents) = contents.as_ref() {
                    for_each_ref_validate_tag_pointer(&oid, contents, &target_oid, &target)?;
                }
                // Then follow each further tag, validating its declared
                // target type against what it actually points at.
                while target.object_type == ObjectType::Tag {
                    let (next_oid, declared_type) = {
                        let tag = sley_object::Tag::parse_ref(format, &target.body)?;
                        (tag.object, tag.object_type)
                    };
                    let next = db.read_object(&next_oid)?;
                    if declared_type != next.object_type {
                        eprintln!("error: bad tag pointer to {next_oid} in {target_oid}");
                        return Err(GitError::Exit(128));
                    }
                    target_oid = next_oid;
                    target = next;
                }
                Some((target_oid, target))
            } else {
                None
            }
        } else {
            None
        };
        let peeled_object = if let Some((peeled_oid, peeled_encoded_object)) = peeled_chain.as_ref()
        {
            let peeled_oid = *peeled_oid;
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
        // %(signature[:opt]) / %(*signature[:opt]) verify the embedded GPG/SSH
        // signature of the ref object (or its peeled target), reusing the same
        // verification backend as `git verify-commit`.
        let signature = if needs.signature {
            object
                .as_ref()
                .and_then(|object| for_each_ref_object_signature(&git_dir, &config, object))
        } else {
            None
        };
        let peeled_signature = if needs.peeled_signature {
            peeled_chain
                .as_ref()
                .and_then(|(_, object)| for_each_ref_object_signature(&git_dir, &config, object))
        } else {
            None
        };
        let deltabase = ObjectId::null(format);
        // `%(worktreepath)` reads from the hoisted map; the placeholder is the empty
        // path for refs not checked out anywhere, matching git.
        let worktree_path = worktree_paths
            .get(reference.name.as_str())
            .map(String::as_str);
        // When the format needs neither the object nor its header, these fields are
        // never observed; the placeholders are therefore unobservable.
        let object_type = object
            .as_ref()
            .map(|object| object.object_type)
            .or_else(|| object_header.map(|(object_type, _)| object_type))
            .unwrap_or(ObjectType::Commit);
        let object_body: &[u8] = object
            .as_ref()
            .map(|object| object.body.as_ref())
            .unwrap_or(&[]);
        let object_size = object
            .as_ref()
            .map(|object| object.body.len())
            .or_else(|| object_header.map(|(_, size)| size))
            .unwrap_or(0);
        let format_context = ForEachRefFormatContext {
            git_dir: &git_dir,
            db: &db,
            format,
            refname: &reference.name,
            oid: &oid,
            deltabase: &deltabase,
            object_type,
            object_body,
            object_size,
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
            signature,
            peeled_signature,
            mailmap: &mailmap,
            ref_names: &ref_names,
            warn_ambiguous_refs,
        };
        let mut line = Vec::new();
        print_for_each_ref_format_with_is_bases(
            &mut line,
            &format_spec,
            &format_context,
            &is_base_refs,
        )?;
        if !omit_empty || !line.is_empty() {
            stdout.write_all(&line)?;
            stdout.write_all(b"\n")?;
        }
        emitted += 1;
    }
    stdout.flush()?;
    Ok(())
}

fn append_for_each_ref_root_refs(
    _git_dir: &std::path::Path,
    store: &FileRefStore,
    refs: &mut Vec<sley_refs::Ref>,
) -> Result<()> {
    for reference in store.list_root_refs()? {
        if !sley_ref_filter::is_for_each_ref_root_ref(&reference.name)
            || refs.iter().any(|existing| existing.name == reference.name)
        {
            continue;
        }
        refs.push(reference);
    }
    Ok(())
}

#[derive(Clone)]
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
    FormattedDate(ForEachRefDateSort),
    VersionRefname,
    VersionRefnameDescending,
}

/// Which per-ref work the parsed `--format` actually requires (git's `used_atom`
/// analysis). Computed once up front so the per-ref loop can skip object reads,
/// the peeled-tag read, disk-size stats, the abbreviation candidate scan, and the
/// worktree probe whenever the format never references the corresponding atom.
#[derive(Default, Clone, Copy)]
struct ForEachRefNeeds {
    /// The ref's own object must be decoded (object body / contents / identities).
    object: bool,
    /// The ref's own object header must be read (object type / content size).
    object_header: bool,
    /// The peeled tag target must be read (any `*`-prefixed object/contents atom).
    /// Implies `object`, since the tag pointer comes from decoding the ref object.
    peeled: bool,
    /// `%(objectsize:disk)` — the loose-object on-disk size for the ref object.
    object_disk: bool,
    /// `%(*objectsize:disk)` — the loose-object on-disk size for the peeled object.
    peeled_disk: bool,
    /// `%(worktreepath)` — the per-ref worktree probe.
    worktree: bool,
    /// `%(upstream*)` — branch upstream config is needed for formatting.
    upstream: bool,
    /// `%(upstream:track*)` — ahead/behind against the upstream is needed.
    upstream_track: bool,
    /// `%(push*)` — branch push destination config is needed for formatting.
    push: bool,
    /// `%(push:track*)` — ahead/behind against the push destination is needed.
    push_track: bool,
    /// `%(objectname:short...)` / `%(*objectname:short...)` — needs the ambiguity
    /// candidate set (the full object-id enumeration).
    candidates: bool,
    /// `%(...:mailmap)` — needs repository and configured mailmap sources.
    mailmap: bool,
    /// `%(upstream*)` / `%(push*)` — needs branch config.
    config: bool,
    /// `%(refname:short)` / `%(symref:short)` / `%(upstream:short)` /
    /// `%(push:short)` — needs the ref-name universe for shorten_unambiguous_ref.
    short_ref: bool,
    /// `%(signature*)` — the ref object's GPG/SSH signature must be verified
    /// (the verification reads the object body and consults the config).
    signature: bool,
    /// `%(*signature*)` — the peeled object's signature must be verified.
    peeled_signature: bool,
}

/// Map a missing-object read failure to git's `fatal: missing object <oid> for
/// <refname>` (ref-filter.c) when a format atom forces the ref's object to be
/// read; other errors propagate unchanged.
fn for_each_ref_missing_object(err: GitError, oid: &ObjectId, refname: &str) -> GitError {
    if matches!(err, GitError::NotFound(_)) {
        eprintln!("fatal: missing object {oid} for {refname}");
        return GitError::Exit(128);
    }
    err
}

/// git resolves every `%(ahead-behind:<committish>)` base up front, rejecting a
/// bare `%(ahead-behind)` and dying on an unresolvable base before any ref is
/// printed (builtin/for-each-ref.c + ref-filter.c's ahead/behind setup).
/// git compiles the ref format once, up front, so a malformed `%(describe:...)`
/// argument is rejected before any ref is matched (builtin/for-each-ref.c's
/// `verify_ref_format`). Validate the describe options here so a bad argument
/// fails even when the pattern matches zero refs (the per-ref render path would
/// otherwise never reach the offending atom).
fn for_each_ref_validate_describe(format_spec: &ForEachRefFormat) -> Result<()> {
    for segment in format_spec.segments() {
        let ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(placeholder)) = segment else {
            continue;
        };
        if let Some((_peeled, opts)) = crate::for_each_ref_describe_atom(placeholder) {
            crate::for_each_ref_parse_describe_opts(opts)?;
        }
    }
    Ok(())
}

fn for_each_ref_resolve_revision(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    sley_rev::RevisionResolver::new(git_dir, format, db).resolve(rev)
}

fn for_each_ref_validate_ahead_behind(
    format_spec: &ForEachRefFormat,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<()> {
    for segment in format_spec.segments() {
        let ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(placeholder)) = segment else {
            continue;
        };
        let placeholder = placeholder.strip_prefix('*').unwrap_or(placeholder);
        let base = if placeholder == "ahead-behind" {
            None
        } else if let Some(base) = placeholder.strip_prefix("ahead-behind:") {
            Some(base)
        } else {
            continue;
        };
        let Some(base) = base.filter(|base| !base.is_empty()) else {
            eprintln!("fatal: expected format: %(ahead-behind:<committish>)");
            return Err(GitError::Exit(128));
        };
        if for_each_ref_resolve_revision(git_dir, format, db, base).is_err() {
            eprintln!("fatal: failed to find '{base}'");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn for_each_ref_is_base_targets(format_spec: &ForEachRefFormat) -> Result<Vec<String>> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for segment in format_spec.segments() {
        let ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(placeholder)) = segment else {
            continue;
        };
        let target = if placeholder == "is-base" {
            None
        } else {
            placeholder.strip_prefix("is-base:")
        };
        let Some(target) = target.filter(|target| !target.is_empty()) else {
            if placeholder == "is-base" || placeholder == "is-base:" {
                eprintln!("fatal: expected format: %(is-base:<committish>)");
                return Err(GitError::Exit(128));
            }
            continue;
        };
        if seen.insert(target.to_string()) {
            targets.push(target.to_string());
        }
    }
    Ok(targets)
}

fn for_each_ref_compute_is_base_refs(
    refs: &[sley_refs::Ref],
    targets: &[String],
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<HashMap<String, String>> {
    if targets.is_empty() {
        return Ok(HashMap::new());
    }

    let mut candidate_names = Vec::new();
    let mut candidate_histories = Vec::new();
    for reference in refs {
        let Some(commit) = for_each_ref_commit_oid_gently(store, db, format, reference)? else {
            continue;
        };
        candidate_names.push(reference.name.clone());
        candidate_histories.push(for_each_ref_first_parent_history(db, format, commit)?);
    }

    let mut selected = HashMap::new();
    for target in targets {
        let tip = for_each_ref_resolve_revision(git_dir, format, db, target)
            .and_then(|oid| sley_rev::peel_to_commit(db, format, &oid));
        let tip = match tip {
            Ok(tip) => tip,
            Err(_) => {
                eprintln!("fatal: failed to find '{target}'");
                return Err(GitError::Exit(128));
            }
        };
        let tip_history = for_each_ref_first_parent_history(db, format, tip)?;
        if let Some(candidate) =
            select_for_each_ref_is_base_candidate(&tip_history, &candidate_histories)
        {
            selected.insert(target.clone(), candidate_names[candidate].clone());
        }
    }
    Ok(selected)
}

fn for_each_ref_commit_oid_gently(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &sley_refs::Ref,
) -> Result<Option<ObjectId>> {
    let Some((mut oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(None);
    };
    let mut seen = HashSet::new();
    while seen.insert(oid) {
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => return Ok(None),
        };
        match object.object_type {
            ObjectType::Commit => return Ok(Some(oid)),
            ObjectType::Tag => {
                let tag = match Tag::parse_ref(format, &object.body) {
                    Ok(tag) => tag,
                    Err(_) => return Ok(None),
                };
                let target = match db.read_object(&tag.object) {
                    Ok(target) => target,
                    Err(_) => return Ok(None),
                };
                if tag.object_type != target.object_type {
                    eprintln!(
                        "error: object {} is a {}, not a {}",
                        tag.object,
                        target.object_type.as_str(),
                        tag.object_type.as_str()
                    );
                    eprintln!("error: bad tag pointer to {} in {oid}", tag.object);
                    return Ok(None);
                }
                oid = tag.object;
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn for_each_ref_first_parent_history(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: ObjectId,
) -> Result<Vec<ObjectId>> {
    let mut history = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(start);
    while let Some(oid) = current {
        if !seen.insert(oid) {
            break;
        }
        history.push(oid);
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            break;
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        current = commit.parents.first().copied();
    }
    Ok(history)
}

impl ForEachRefNeeds {
    fn analyze(format_spec: &ForEachRefFormat) -> Self {
        let mut needs = ForEachRefNeeds::default();
        for segment in format_spec.segments() {
            let ForEachRefFormatSegment::Atom(atom) = segment else {
                continue;
            };
            match atom {
                ForEachRefAtom::Raw(placeholder) => {
                    if placeholder.contains("mailmap") {
                        needs.mailmap = true;
                    }
                    needs.note_raw(placeholder);
                }
                ForEachRefAtom::Color(_) => {}
                ForEachRefAtom::RefName { source, format } => {
                    if matches!(format, ForEachRefNameFormat::Short) {
                        needs.short_ref = true;
                    }
                    match source {
                        ForEachRefNameSource::Ref => {}
                        ForEachRefNameSource::Upstream => {
                            needs.upstream = true;
                            needs.config = true;
                        }
                        ForEachRefNameSource::Push => {
                            needs.push = true;
                            needs.config = true;
                        }
                    }
                }
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
        // `%(symref:short)` / `%(upstream:short)` / `%(push:short)` (the Raw-atom
        // forms) need the ref-name universe for shorten_unambiguous_ref.
        if matches!(
            placeholder,
            "symref:short" | "upstream:short" | "push:short"
        ) {
            self.short_ref = true;
        }
        // Strip a leading `*` (peeled) marker, classifying the peeled need first.
        let (base, peeled) = placeholder
            .strip_prefix('*')
            .map(|rest| (rest, true))
            .unwrap_or((placeholder, false));
        // Atoms that consult the ref object body (or, when peeled, the tag target).
        let consumes_object = match base {
            "objectsize" | "objecttype" if !peeled => {
                self.object_header = true;
                false
            }
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
                } else if other == "describe" || other.starts_with("describe:") {
                    // %(describe) runs the describe engine directly off the OID
                    // (no body read); the deref form needs the peeled tag target
                    // resolved so `context.peeled_object` is populated.
                    peeled
                } else if other == "signature" || other.starts_with("signature:") {
                    // %(signature[:opt]) verifies the (commit) object's embedded
                    // signature: it reads the object body and consults the config
                    // (gpg.format, allowedSigners, ...). The deref form verifies
                    // the peeled tag target instead.
                    if peeled {
                        self.peeled_signature = true;
                    } else {
                        self.signature = true;
                    }
                    self.config = true;
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
                    self.note_ref_relation_atom(other);
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

    fn note_ref_relation_atom(&mut self, atom: &str) {
        match atom {
            "upstream" | "upstream:short" | "upstream:remotename" | "upstream:remoteref" => {
                self.upstream = true;
                self.config = true;
            }
            "upstream:track"
            | "upstream:track,nobracket"
            | "upstream:nobracket,track"
            | "upstream:trackshort" => {
                self.upstream = true;
                self.upstream_track = true;
                self.config = true;
            }
            "push" | "push:short" | "push:remotename" | "push:remoteref" => {
                self.push = true;
                self.config = true;
            }
            "push:track" | "push:track,nobracket" | "push:nobracket,track" | "push:trackshort" => {
                self.push = true;
                self.push_track = true;
                self.config = true;
            }
            other
                if other.starts_with("upstream:lstrip=")
                    || other.starts_with("upstream:strip=")
                    || other.starts_with("upstream:rstrip=") =>
            {
                self.upstream = true;
                self.config = true;
            }
            other
                if other.starts_with("push:lstrip=")
                    || other.starts_with("push:strip=")
                    || other.starts_with("push:rstrip=") =>
            {
                self.push = true;
                self.config = true;
            }
            _ => {}
        }
    }
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
        // git ref-filter applies `version:`/`v:` version comparison to any
        // field. `version:tag` keys on the `tag` atom (the short name under
        // refs/tags); every ref a `git tag` listing produces shares the
        // `refs/tags/` prefix, so a version compare on the short name and on the
        // full refname yield the same order — alias to VersionRefname.
        "version:tag" | "v:tag" => Ok(ForEachRefSort::VersionRefname),
        "-version:tag" | "-v:tag" => Ok(ForEachRefSort::VersionRefnameDescending),
        other => {
            if let Some(date) = parse_for_each_ref_date_sort(other)?
                && !matches!(date.mode, DateMode::Default)
            {
                Ok(ForEachRefSort::FormattedDate(date))
            } else if let Some((field, descending)) = parse_for_each_ref_identity_sort(other) {
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
    main_worktree_root: Option<&'a Path>,
    head_ref: Option<&'a str>,
    format: ObjectFormat,
}

type ForEachRefObjectHeaderCache = HashMap<ObjectId, (ObjectType, usize)>;

fn sort_for_each_refs(
    refs: &mut Vec<sley_refs::Ref>,
    sorts: &[ForEachRefSort],
    context: ForEachRefSortContext<'_>,
    object_headers: &mut ForEachRefObjectHeaderCache,
) -> Result<()> {
    let mut keyed = Vec::with_capacity(refs.len());
    for reference in refs.drain(..) {
        let keys = sorts
            .iter()
            .map(|sort| for_each_ref_sort_key(&reference, sort, &context, object_headers))
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
    fn descending(&self) -> bool {
        if let ForEachRefSort::FormattedDate(date) = self {
            return date.descending;
        }
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

    fn needs_config(&self) -> bool {
        matches!(
            self,
            ForEachRefSort::Upstream
                | ForEachRefSort::UpstreamDescending
                | ForEachRefSort::Push
                | ForEachRefSort::PushDescending
        )
    }
}

fn for_each_ref_sorts_need_config(sorts: &[ForEachRefSort]) -> bool {
    sorts.iter().any(|sort| sort.needs_config())
}

/// Verify the embedded signature of a ref's commit/tag object for the
/// `%(signature[:opt])` atom family, mirroring git's `check_commit_signature`.
///
/// An *unsigned* commit still produces a verification result (git reports grade
/// `N` with empty key/signer/fingerprints), so a missing signature maps to a
/// default [`GpgVerification`] rather than `None`. Object types that cannot
/// carry a signature (trees, blobs) yield `None`, leaving the atoms empty.
fn for_each_ref_object_signature(
    git_dir: &Path,
    config: &GitConfig,
    object: &sley_object::EncodedObject,
) -> Option<commands::signing::GpgVerification> {
    let payload_signature = match object.object_type {
        ObjectType::Commit => commands::signing::commit_signature_payload(&object.body),
        ObjectType::Tag => commands::signing::tag_signature_payload(&object.body)
            .map(|(payload, signature)| (payload.to_vec(), signature.to_vec())),
        _ => return None,
    };
    let Some((payload, signature)) = payload_signature else {
        // Unsigned: git's check_commit_signature leaves result 'N' with empty
        // identity fields, which a default verification models exactly.
        return Some(commands::signing::GpgVerification::default());
    };
    commands::signing::verify_payload(git_dir, Some(config), &payload, &signature).ok()
}

fn for_each_ref_object_header(
    db: &FileObjectDatabase,
    cache: &mut ForEachRefObjectHeaderCache,
    oid: &ObjectId,
) -> Result<(ObjectType, usize)> {
    if let Some(header) = cache.get(oid).copied() {
        return Ok(header);
    }
    let header = for_each_ref_read_object_header(db, oid)?;
    cache.insert(*oid, header);
    Ok(header)
}

fn for_each_ref_read_object_header(
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Result<(ObjectType, usize)> {
    if let Some((object_type, size)) = db.read_object_header(oid)? {
        return Ok((object_type, size as usize));
    }
    let object = db.read_object(oid)?;
    Ok((object.object_type, object.body.len()))
}

fn for_each_ref_sort_key(
    reference: &sley_refs::Ref,
    sort: &ForEachRefSort,
    context: &ForEachRefSortContext<'_>,
    object_headers: &mut ForEachRefObjectHeaderCache,
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
            ForEachRefSortKey::Text(for_each_ref_sort_identity_key(contents.as_ref(), *field))
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
                for_each_ref_worktree_path(
                    context.git_dir,
                    context.main_worktree_root,
                    context.head_ref,
                    &reference.name,
                )?
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
                let (_, size) = for_each_ref_object_header(context.db, object_headers, &oid)?;
                size as i128
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
                    ObjectId::null(context.format).to_hex()
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
                    let (object_type, _) =
                        for_each_ref_object_header(context.db, object_headers, &oid)?;
                    object_type.as_str().to_string()
                } else {
                    String::new()
                },
            )
        }
        ForEachRefSort::ObjectSize | ForEachRefSort::ObjectSizeDescending => {
            ForEachRefSortKey::Number(
                if let Some((oid, _)) = resolve_for_each_ref_target(context.store, reference)? {
                    let (_, size) = for_each_ref_object_header(context.db, object_headers, &oid)?;
                    size as i128
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
        ForEachRefSort::FormattedDate(date) => {
            let contents = if date.peeled {
                for_each_ref_sort_peeled_contents(reference, context)?
            } else {
                for_each_ref_sort_contents(reference, context)?
            };
            let identity = contents.as_ref().and_then(|contents| match date.role {
                ForEachRefAtomIdentityRole::Author => contents.author.as_deref(),
                ForEachRefAtomIdentityRole::Committer => contents.committer.as_deref(),
                ForEachRefAtomIdentityRole::Tagger => contents.tagger.as_deref(),
                ForEachRefAtomIdentityRole::Creator => contents.creator.as_deref(),
            });
            ForEachRefSortKey::Text(
                identity
                    .and_then(|identity| for_each_ref_identity_date(identity, &date.mode))
                    .unwrap_or_default(),
            )
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
    let tag_oid = resolve_for_each_ref_target(context.store, reference)?
        .map(|(oid, _)| oid)
        .unwrap_or(oid);
    for_each_ref_validate_tag_pointer(&tag_oid, &contents, &oid, &object)?;
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
    // git's match_name_as_path: a literal pattern matches when `name` starts with
    // it AND the boundary is a path component edge — `name[plen]` is end-of-string
    // or '/', OR the pattern itself ends in '/' (a `refs/tags/` prefix pattern).
    let pattern_is_prefix = pattern.ends_with('/');
    let rest = if ignore_case {
        strip_prefix_ignore_ascii_case(name, pattern)
    } else {
        name.strip_prefix(pattern)
    };
    rest.is_some_and(|rest| pattern_is_prefix || rest.is_empty() || rest.starts_with('/'))
}

fn for_each_ref_exclude_matches(name: &str, pattern: &str, ignore_case: bool) -> bool {
    // git's filter_exclude_match uses the same match_name_as_path as the positive
    // patterns, so an exclude like `refs/tags/foo` is a path-prefix match that
    // drops `refs/tags/foo/one` &c, not just an exact wildmatch.
    for_each_ref_pattern_matches(name, pattern, ignore_case)
}

fn for_each_ref_pattern_glob_matches(name: &str, pattern: &str, ignore_case: bool) -> bool {
    fn matches_from(pattern: &[u8], name: &[u8]) -> bool {
        match pattern {
            [] => name.is_empty(),
            // `**` (git's WM_PATHNAME double-star) matches any run of bytes
            // INCLUDING '/', so paired patterns like `refs/heads/*/**` reach
            // nested refs (`refs/heads/feature/topic`) that a single `*` cannot.
            [b'*', b'*', rest @ ..] => {
                matches_from(rest, name) || (!name.is_empty() && matches_from(pattern, &name[1..]))
            }
            // A single `*` matches within one path segment only: it never
            // consumes '/', matching git's per-segment wildmatch.
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
            [b'*', b'*', rest @ ..] => {
                matches_from_ignore_case(rest, name)
                    || (!name.is_empty() && matches_from_ignore_case(pattern, &name[1..]))
            }
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
