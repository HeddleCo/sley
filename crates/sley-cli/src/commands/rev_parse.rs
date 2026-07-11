//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_index, sley_odb, sley_rev, sley_worktree};
use std::sync::OnceLock;
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

#[derive(Clone, Copy)]
enum RevParsePathFormat {
    Default,
    Absolute,
    Relative,
}

struct RevParseRepository<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    replace_objects: bool,
    objects: OnceLock<FileObjectDatabase>,
    refs: OnceLock<FileRefStore>,
}

impl<'a> RevParseRepository<'a> {
    fn new(git_dir: &'a Path, format: ObjectFormat, replace_objects: bool) -> Self {
        Self {
            git_dir,
            format,
            replace_objects,
            objects: OnceLock::new(),
            refs: OnceLock::new(),
        }
    }

    fn objects(&self) -> Result<&FileObjectDatabase> {
        if let Some(objects) = self.objects.get() {
            return Ok(objects);
        }
        let objects = crate::repository::open_object_database(
            self.git_dir,
            self.format,
            self.replace_objects,
        )?;
        let _ = self.objects.set(objects);
        Ok(self
            .objects
            .get()
            .expect("rev-parse object database should be initialized"))
    }

    fn refs(&self) -> &FileRefStore {
        self.refs
            .get_or_init(|| FileRefStore::new(self.git_dir, self.format))
    }
}

pub(crate) fn cmd_rev_parse(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "--parseopt") {
        return rev_parse_parseopt(&args[1..]);
    }
    if rev_parse_args_need_no_repository(cli_session.cwd(), args)? {
        return Ok(());
    }
    let cwd = cli_session.cwd().to_path_buf();
    let setup = setup::setup_git_directory(cli_session);
    let git_dir = match cli_session.git_dir() {
        Ok(git_dir) => git_dir,
        Err(GitError::NotFound(_)) => {
            if args.is_empty() {
                return Err(GitError::Command("rev-parse requires <rev>...".into()));
            }
            return rev_parse_not_git_repository();
        }
        Err(err) => return Err(err),
    };
    // git's repository setup validates the repository format (version vs
    // extensions) before rev-parse processes any argument; a bare `rev-parse`
    // in a malformed repository must still die (t0001 #60/#62/#64).
    let format = verify_repository_format(cli_session, &git_dir)?;
    let repo_config = read_repo_config(&git_dir).ok();
    let repository = RevParseRepository::new(&git_dir, format, cli_session.replace_objects());
    if args.is_empty() {
        validate_bare_rev_parse_setup(cli_session, &setup)?;
        return Err(GitError::Command("rev-parse requires <rev>...".into()));
    }
    let mut short: Option<usize> = None;
    let mut short_revs = 0usize;
    let mut verify = false;
    let mut verified_revs = 0usize;
    let mut quiet = false;
    let mut abbrev_ref = false;
    let mut symbolic = false;
    let mut symbolic_full_name = false;
    let mut revs_only = false;
    let mut path_format = RevParsePathFormat::Default;
    let mut end_of_options = false;
    let mut seen_path_arg = false;
    // `--prefix <p>` makes rev-parse behave as if it were invoked from the <p>
    // subdirectory: trailing pathspecs (after `--`) and disambiguated filenames
    // are emitted with the prefix prepended, and `<tree>:./path` / `:./path`
    // relative object names resolve against the prefix. Mirrors git's
    // `startup_info->prefix` + `output_prefix` (builtin/rev-parse.c).
    let mut output_prefix: Option<String> = None;
    let mut default_rev: Option<String> = None;
    let mut verified_output: Option<String> = None;
    let dashdash = args.iter().position(|arg| arg == "--");
    // Pseudo-ref options (`--all`, `--glob=`, `--branches[=]`, `--exclude=`, …)
    // resolve refs and print their OIDs interleaved with positional args, the
    // same way git's `handle_revision_pseudo_opt` feeds `add_pending_object`.
    // Lazily built (and config loaded) only when the first such option appears.
    let mut pseudo: Option<sley_rev::PseudoRefResolver> = None;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if !verify && sley_rev::PseudoRefResolver::is_pseudo_ref_arg(arg) {
            let resolver = match pseudo.as_mut() {
                Some(resolver) => resolver,
                None => {
                    pseudo = Some(sley_rev::PseudoRefResolver::new(
                        &git_dir,
                        format,
                        repo_config.as_ref(),
                    )?);
                    pseudo.as_mut().expect("just inserted")
                }
            };
            if let Some(matched) = resolver.feed(arg)? {
                for matched_ref in matched {
                    let oid = matched_ref.oid.to_hex();
                    if let Some(len) = short {
                        println!("{}", &oid[..len.min(oid.len())]);
                    } else {
                        println!("{oid}");
                    }
                }
            }
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                if verify {
                    break;
                }
                println!("--");
                for path in &args[idx + 1..] {
                    match &output_prefix {
                        Some(prefix) => println!("{}", rev_parse_prefix_filename(prefix, path)),
                        None => println!("{path}"),
                    }
                }
                break;
            }
            "--prefix" => {
                idx += 1;
                let prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--prefix requires an argument".into()))?;
                output_prefix = Some(prefix.clone());
            }
            "--end-of-options" if !verify => {
                println!("--end-of-options");
                end_of_options = true;
            }
            "--end-of-options" if verify => end_of_options = true,
            "--git-dir" => println!(
                "{}",
                display_git_dir(cli_session, &cwd, &git_dir, path_format)?
            ),
            "--absolute-git-dir" => println!("{}", fs::canonicalize(&git_dir)?.display()),
            "--git-common-dir" => {
                println!(
                    "{}",
                    display_git_common_dir(cli_session, &cwd, &git_dir, path_format)?
                );
            }
            "--shared-index-path" => {
                println!(
                    "{}",
                    display_shared_index_path(cli_session, &cwd, &git_dir, path_format)?
                );
            }
            "--git-path" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_git_path_requires_argument_error)?;
                println!(
                    "{}",
                    display_git_path(cli_session, &cwd, &git_dir, path_format, path)?
                );
            }
            "--resolve-git-dir" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_resolve_git_dir_requires_argument_error)?;
                println!("{}", resolve_git_dir_arg(&cwd, path)?);
            }
            "--show-toplevel" => {
                if !is_inside_work_tree(cli_session, &cwd, &git_dir, setup.as_ref())? {
                    return rev_parse_requires_work_tree();
                }
                let root = rev_parse_worktree_root(cli_session, &git_dir, setup.as_ref())?;
                match path_format {
                    RevParsePathFormat::Default | RevParsePathFormat::Absolute => {
                        println!("{}", root.display());
                    }
                    RevParsePathFormat::Relative => {
                        println!("{}", relative_path_from(&cwd, &root)?)
                    }
                }
            }
            "--show-prefix" => {
                if is_inside_work_tree(cli_session, &cwd, &git_dir, setup.as_ref())? {
                    println!(
                        "{}",
                        worktree_prefix(cli_session, &cwd, &git_dir, setup.as_ref())?
                    );
                } else {
                    println!();
                }
            }
            "--show-cdup" => {
                if is_inside_work_tree(cli_session, &cwd, &git_dir, setup.as_ref())? {
                    println!(
                        "{}",
                        worktree_cdup(cli_session, &cwd, &git_dir, setup.as_ref())?
                    );
                } else if let Some(root) = setup.as_ref().and_then(|setup| setup.worktree.as_ref())
                {
                    println!("{}", root.display());
                }
            }
            "--show-superproject-working-tree" => {
                if let Some(root) = superproject_working_tree(&git_dir)? {
                    println!("{}", root.display());
                }
            }
            "--show-object-format"
            | "--show-object-format=storage"
            | "--show-object-format=input"
            | "--show-object-format=output" => println!("{}", format.name()),
            "--show-ref-format" => {
                println!("{}", repository_ref_storage_format(cli_session, &git_dir)?)
            }
            "--local-env-vars" => print_local_env_vars(),
            "--sq-quote" => {
                print_rev_parse_sq_quote(&args[idx + 1..])?;
                break;
            }
            "--path-format=absolute" => path_format = RevParsePathFormat::Absolute,
            "--path-format=relative" => path_format = RevParsePathFormat::Relative,
            "--path-format" => return rev_parse_path_format_requires_argument(),
            "--is-inside-work-tree" => {
                println!(
                    "{}",
                    is_inside_work_tree(cli_session, &cwd, &git_dir, setup.as_ref())?
                );
            }
            "--is-inside-git-dir" => {
                println!("{}", is_inside_git_dir(&cwd, &git_dir, setup.as_ref())?)
            }
            "--is-bare-repository" => println!(
                "{}",
                is_bare_repository(cli_session, &git_dir, setup.as_ref())?
            ),
            "--is-shallow-repository" => println!("{}", is_shallow_repository(&git_dir)),
            "--short" => short = repository_abbrev(&git_dir, format)?,
            value if value.starts_with("--disambiguate=") && !verify => {
                let prefix = &value["--disambiguate=".len()..];
                for oid in sley_rev::object_ids_with_prefix(&git_dir, format, prefix)? {
                    println!("{}", oid.to_hex());
                }
            }
            "--default" => {
                idx += 1;
                default_rev = Some(
                    args.get(idx)
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?
                        .clone(),
                );
            }
            "--verify" => verify = true,
            "--quiet" | "-q" => quiet = true,
            "--revs-only" => revs_only = true,
            "--abbrev-ref" | "--abbrev-ref=strict" | "--abbrev-ref=loose" => abbrev_ref = true,
            "--symbolic" => symbolic = true,
            "--symbolic-full-name" => symbolic_full_name = true,
            "--bisect" => rev_parse_bisect(&repository, symbolic_full_name)?,
            value if value.starts_with('-') && !end_of_options => {
                if let Some(value) = value.strip_prefix("--short=") {
                    short = Some(parse_abbrev(value)?.max(4));
                    idx += 1;
                    continue;
                }
                // Date-bound options are rewritten the way `git log` consumes
                // them: `--since=`/`--after=` lower-bound the date (an upper bound
                // on age, `--max-age=`), `--before=`/`--until=` do the reverse.
                // The date is parsed to a Unix timestamp; `--max-age=`/`--min-age=`
                // are already in that form and pass through verbatim.
                if let Some(date) = value
                    .strip_prefix("--since=")
                    .or_else(|| value.strip_prefix("--after="))
                {
                    println!("--max-age={}", log_parse_date_cutoff(date)?);
                    idx += 1;
                    continue;
                }
                if let Some(date) = value
                    .strip_prefix("--before=")
                    .or_else(|| value.strip_prefix("--until="))
                {
                    println!("--min-age={}", log_parse_date_cutoff(date)?);
                    idx += 1;
                    continue;
                }
                if value.starts_with("--max-age=") || value.starts_with("--min-age=") {
                    println!("{value}");
                    idx += 1;
                    continue;
                }
                if let Some(value) = value.strip_prefix("--path-format=") {
                    return rev_parse_unknown_path_format(value);
                }
                if let Some(value) = value.strip_prefix("--show-object-format=") {
                    return rev_parse_unknown_show_object_format(value);
                }
                return Err(GitError::Command(format!(
                    "unsupported rev-parse option {value}"
                )));
            }
            rev => {
                if verify {
                    verified_revs += 1;
                    if verified_revs > 1 {
                        return rev_parse_needed_single_revision(quiet);
                    }
                }
                // A leading `^` marks an excluded revision (rev-list's "not this
                // one"). git resolves the remainder exactly like a positive arg
                // and prefixes the rendered output with `^`; the same applies to
                // --abbrev-ref / --symbolic-full-name / --short rendering.
                let (rev, negate) = match rev.strip_prefix('^') {
                    Some(rest) => (rest, true),
                    None => (rev, false),
                };
                if !verify
                    && let Some(rendered) =
                        rev_parse_render_parent_expansion(&repository, rev, negate, symbolic)?
                {
                    for line in rendered {
                        println!("{line}");
                    }
                    idx += 1;
                    continue;
                }
                if symbolic && !verify {
                    if let Some(rendered) = rev_parse_render_symbolic_range(rev, negate) {
                        for line in rendered {
                            println!("{line}");
                        }
                    } else {
                        rev_parse_print_positional(rev, negate);
                    }
                    idx += 1;
                    continue;
                }
                if let Some(rendered) = rev_parse_render_range(&repository, rev, negate)? {
                    for line in rendered {
                        println!("{line}");
                    }
                    idx += 1;
                    continue;
                }
                if abbrev_ref {
                    let rendered = rev_parse_abbrev_ref(&repository, rev)?;
                    rev_parse_print_positional(&rendered, negate);
                    idx += 1;
                    continue;
                }
                if symbolic_full_name {
                    if let Some(name) = rev_parse_symbolic_full_name(&git_dir, format, rev)? {
                        rev_parse_print_positional(&name, negate);
                    }
                    idx += 1;
                    continue;
                }
                if seen_path_arg && sley_rev::split_rev_path_spec(rev).is_some() {
                    println!("{rev}");
                    rev_parse_no_such_worktree_path(rev);
                    return Err(GitError::Exit(128));
                }
                let normalized_rev = rev_parse_normalize_revision_arg(
                    cli_session,
                    &cwd,
                    &git_dir,
                    rev,
                    output_prefix.as_deref(),
                )?;
                let oid = match rev_parse_resolve_revision_arg(
                    &repository,
                    repo_config.as_ref(),
                    &normalized_rev,
                ) {
                    Ok(oid) => oid,
                    Err(_) if revs_only => {
                        idx += 1;
                        continue;
                    }
                    Err(_) if verify && quiet => return Err(GitError::Exit(1)),
                    Err(err) if verify && rev_parse_is_selector_error(rev) => {
                        eprintln!("fatal: {}", rev_parse_error_message(&err));
                        return Err(GitError::Exit(128));
                    }
                    Err(err) if verify => {
                        rev_parse_maybe_print_ambiguity(&git_dir, format, &normalized_rev, &err)?;
                        return rev_parse_needed_single_revision(false);
                    }
                    Err(err) => {
                        if !before_dashdash(dashdash, idx) && !rev.contains(':') {
                            if let Some(prefix) = output_prefix.as_deref() {
                                if rev_parse_prefixed_path_exists(
                                    cli_session,
                                    &git_dir,
                                    prefix,
                                    rev,
                                )? {
                                    println!("{}", rev_parse_prefix_filename(prefix, rev));
                                    seen_path_arg = true;
                                    idx += 1;
                                    continue;
                                }
                            } else if rev_parse_path_exists_on_disk(
                                cli_session,
                                &cwd,
                                &git_dir,
                                rev,
                            )? {
                                println!("{rev}");
                                seen_path_arg = true;
                                idx += 1;
                                continue;
                            }
                        }
                        return rev_parse_diagnose_arg_failure(
                            cli_session,
                            &cwd,
                            &git_dir,
                            format,
                            rev,
                            &normalized_rev,
                            err,
                            before_dashdash(dashdash, idx),
                            seen_path_arg,
                        );
                    }
                };
                if verify {
                    let oid = oid.to_hex();
                    let rendered = if let Some(len) = short {
                        oid[..len.min(oid.len())].to_string()
                    } else {
                        oid
                    };
                    verified_output = Some(if negate {
                        format!("^{rendered}")
                    } else {
                        rendered
                    });
                    idx += 1;
                    continue;
                }
                if let Some(len) = short {
                    short_revs += 1;
                    if short_revs > 1 {
                        return Err(GitError::Command("needed a single revision".into()));
                    }
                    let oid = oid.to_hex();
                    rev_parse_print_positional(&oid[..len.min(oid.len())], negate);
                } else {
                    rev_parse_print_positional(&oid.to_hex(), negate);
                }
            }
        }
        idx += 1;
    }
    if verify
        && verified_revs == 0
        && let Some(default_rev) = default_rev
    {
        let oid = match resolve_revision(
            &git_dir,
            format,
            &default_rev,
            cli_session.replace_objects(),
        ) {
            Ok(oid) => oid,
            Err(_) if quiet => return Err(GitError::Exit(1)),
            Err(_) => return rev_parse_needed_single_revision(false),
        };
        verified_output = Some(oid.to_hex());
        verified_revs = 1;
    }
    if verify && verified_revs != 1 {
        return rev_parse_needed_single_revision(quiet);
    }
    if verify && let Some(output) = verified_output {
        println!("{output}");
    }
    Ok(())
}

fn before_dashdash(dashdash: Option<usize>, idx: usize) -> bool {
    dashdash.is_some_and(|dashdash| idx < dashdash)
}

fn rev_parse_resolve_revision_arg(
    repository: &RevParseRepository<'_>,
    config: Option<&GitConfig>,
    rev: &str,
) -> Result<ObjectId> {
    rev_parse_warn_ambiguous_refname_for_object_prefix(repository, rev);
    let objects = repository.objects()?;
    if let Some(disambiguation) = rev_parse_core_disambiguate(config) {
        // `core.disambiguate` narrows a bare prefix to the configured type, but a
        // ref still wins over a same-spelled prefix (git consults refs before
        // get_short_oid). Route through the ref-first resolver.
        return sley_rev::resolve_revision_with_reader_and_disambiguation(
            repository.git_dir,
            repository.format,
            objects,
            rev,
            disambiguation,
        );
    }
    sley_rev::resolve_revision_with_reader(repository.git_dir, repository.format, objects, rev)
}

fn rev_parse_warn_ambiguous_refname_for_object_prefix(
    repository: &RevParseRepository<'_>,
    rev: &str,
) {
    if rev.len() < 4
        || rev.len() > repository.format.hex_len()
        || !rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return;
    }
    let refs = repository.refs();
    let ref_exists = refs
        .read_ref(&format!("refs/heads/{rev}"))
        .ok()
        .flatten()
        .is_some()
        || refs
            .read_ref(&format!("refs/tags/{rev}"))
            .ok()
            .flatten()
            .is_some();
    if !ref_exists {
        return;
    }
    let Ok(objects) = repository.objects() else {
        return;
    };
    if matches!(
        objects.resolve_prefix(rev),
        Ok(sley_odb::ObjectPrefixResolution::Unique(_)
            | sley_odb::ObjectPrefixResolution::Ambiguous(_))
    ) {
        eprintln!("warning: refname '{rev}' is ambiguous.");
    }
}

fn rev_parse_core_disambiguate(
    config: Option<&GitConfig>,
) -> Option<sley_rev::ObjectDisambiguation> {
    let config = config?;
    match config.get("core", None, "disambiguate")? {
        "commit" => Some(sley_rev::ObjectDisambiguation::Commit),
        "committish" => Some(sley_rev::ObjectDisambiguation::Commitish),
        "tree" => Some(sley_rev::ObjectDisambiguation::Tree),
        "treeish" => Some(sley_rev::ObjectDisambiguation::Treeish),
        "blob" => Some(sley_rev::ObjectDisambiguation::Blob),
        "tag" => Some(sley_rev::ObjectDisambiguation::Tag),
        _ => None,
    }
}

fn rev_parse_render_range(
    repository: &RevParseRepository<'_>,
    rev: &str,
    negate: bool,
) -> Result<Option<Vec<String>>> {
    let Some(range) = sley_rev::parse_revision_range(rev) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    match range {
        sley_rev::RevisionRange::Asymmetric { start, end } => {
            let start_oid = rev_parse_resolve_commitish(repository, &start)?;
            let end_oid = rev_parse_resolve_commitish(repository, &end)?;
            if negate {
                out.push(format!("^{}", end_oid.to_hex()));
                out.push(start_oid.to_hex());
            } else {
                out.push(end_oid.to_hex());
                out.push(format!("^{}", start_oid.to_hex()));
            }
        }
        sley_rev::RevisionRange::Symmetric { left, right } => {
            let left_oid = rev_parse_resolve_commitish(repository, &left)?;
            let right_oid = rev_parse_resolve_commitish(repository, &right)?;
            let db = repository.objects()?;
            let left_commit = sley_rev::peel_to_commit(db, repository.format, &left_oid)?;
            let right_commit = sley_rev::peel_to_commit(db, repository.format, &right_oid)?;
            let bases = sley_rev::merge_bases(
                repository.git_dir,
                repository.format,
                db,
                &left_commit,
                &right_commit,
            )?;
            if negate {
                out.push(format!("^{}", left_oid.to_hex()));
                out.push(format!("^{}", right_oid.to_hex()));
                out.extend(bases.into_iter().map(|oid| oid.to_hex()));
            } else {
                out.push(left_oid.to_hex());
                out.push(right_oid.to_hex());
                out.extend(bases.into_iter().map(|oid| format!("^{}", oid.to_hex())));
            }
        }
    }
    Ok(Some(out))
}

fn rev_parse_render_symbolic_range(rev: &str, negate: bool) -> Option<Vec<String>> {
    let range = sley_rev::parse_revision_range(rev)?;
    let mut out = Vec::new();
    match range {
        sley_rev::RevisionRange::Asymmetric { start, end } => {
            if negate {
                out.push(format!("^{end}"));
                out.push(start);
            } else {
                out.push(end);
                out.push(format!("^{start}"));
            }
        }
        sley_rev::RevisionRange::Symmetric { left, right } => {
            if negate {
                out.push(format!("^{left}"));
                out.push(format!("^{right}"));
            } else {
                out.push(left);
                out.push(right);
            }
        }
    }
    Some(out)
}

fn rev_parse_render_parent_expansion(
    repository: &RevParseRepository<'_>,
    rev: &str,
    negate: bool,
    symbolic: bool,
) -> Result<Option<Vec<String>>> {
    if let Some(base) = rev.strip_suffix("^@") {
        let parents = rev_parse_parent_oids(repository, base)?;
        let mut out = Vec::with_capacity(parents.len());
        for (idx, oid) in parents.into_iter().enumerate() {
            let rendered = if symbolic {
                format!("{base}^{}", idx + 1)
            } else {
                oid.to_hex()
            };
            out.push(if negate {
                format!("^{rendered}")
            } else {
                rendered
            });
        }
        return Ok(Some(out));
    }
    if let Some(base) = rev.strip_suffix("^!") {
        let parents = rev_parse_parent_oids(repository, base)?;
        let base_oid = rev_parse_resolve_commitish(repository, base)?;
        let mut out = Vec::with_capacity(parents.len() + 1);
        let rendered_base = if symbolic {
            base.to_string()
        } else {
            base_oid.to_hex()
        };
        out.push(if negate {
            format!("^{rendered_base}")
        } else {
            rendered_base
        });
        for (idx, oid) in parents.into_iter().enumerate() {
            let rendered = if symbolic {
                format!("{base}^{}", idx + 1)
            } else {
                oid.to_hex()
            };
            out.push(if negate {
                rendered
            } else {
                format!("^{rendered}")
            });
        }
        return Ok(Some(out));
    }
    Ok(None)
}

fn rev_parse_parent_oids(repository: &RevParseRepository<'_>, rev: &str) -> Result<Vec<ObjectId>> {
    let db = repository.objects()?;
    let base = rev_parse_resolve_commitish(repository, rev)?;
    let commit_oid = sley_rev::peel_to_commit(db, repository.format, &base)?;
    let object = db.read_object(&commit_oid)?;
    let commit = Commit::parse(repository.format, &object.body)?;
    Ok(sley_odb::grafted_parents(db, &commit_oid, commit.parents))
}

fn rev_parse_resolve_commitish(repository: &RevParseRepository<'_>, rev: &str) -> Result<ObjectId> {
    // Ref-first resolution: a ref named like a short hex prefix (a range
    // endpoint such as `added...HEAD`) must resolve to the ref, with the
    // commit-ish disambiguation narrowing only a genuine bare prefix.
    sley_rev::resolve_revision_commitish_with_reader(
        repository.git_dir,
        repository.format,
        repository.objects()?,
        rev,
    )
}

fn rev_parse_split_range(rev: &str) -> Option<(&str, &str, bool)> {
    if let Some(colon) = rev.find(':') {
        let dots = rev.find("..")?;
        if colon < dots {
            return None;
        }
    }
    if let Some(pos) = rev.find("...") {
        return Some((&rev[..pos], &rev[pos + 3..], true));
    }
    rev.find("..")
        .map(|pos| (&rev[..pos], &rev[pos + 2..], false))
}

fn rev_parse_normalize_revision_arg(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    rev: &str,
    output_prefix: Option<&str>,
) -> Result<String> {
    if rev.starts_with(":/") {
        return Ok(rev.to_string());
    }
    if let Some(rest) = rev.strip_prefix(':') {
        let (stage, path) = rev_parse_index_stage_path(rest);
        let path = rev_parse_resolve_relative_path(cli_session, cwd, git_dir, path, output_prefix)?;
        return Ok(match stage {
            Some(stage) => format!(":{stage}:{path}"),
            None => format!(":{path}"),
        });
    }
    if let Some((base, path)) = sley_rev::split_rev_path_spec(rev) {
        let path = rev_parse_resolve_relative_path(cli_session, cwd, git_dir, path, output_prefix)?;
        return Ok(format!("{base}:{path}"));
    }
    Ok(rev.to_string())
}

/// Resolve a `./` / `../` relative path inside an object name. With an explicit
/// `--prefix`, git resolves the relative path against the prefix string itself
/// (pure lexical `prefix_path()` normalisation, no filesystem); otherwise it is
/// resolved against the current working directory.
fn rev_parse_resolve_relative_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
    output_prefix: Option<&str>,
) -> Result<String> {
    match output_prefix {
        Some(prefix) => Ok(rev_parse_apply_prefix_path(prefix, path)),
        None => rev_parse_normalize_relative_path(cli_session, cwd, git_dir, path),
    }
}

/// git's `resolve_relative_path()` only rewrites paths that begin with `./` or
/// `../`; a bare `<tree>:top` ignores the prefix entirely. When it does apply,
/// the prefix is prepended and the result is lexically normalised (collapsing
/// `.` and `..`), matching `prefix_path()` in setup.c.
fn rev_parse_apply_prefix_path(prefix: &str, path: &str) -> String {
    if !(path.starts_with("./") || path.starts_with("../")) {
        return path.to_string();
    }
    rev_parse_lexical_normalize(&format!("{prefix}{path}"))
}

/// Collapse `.` and `..` components of a slash-separated path lexically.
fn rev_parse_lexical_normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => match out.last() {
                Some(&"..") | None => out.push(".."),
                Some(_) => {
                    out.pop();
                }
            },
            normal => out.push(normal),
        }
    }
    out.join("/")
}

/// `prefix_filename()` from setup.c: prepend the prefix to a (relative) path
/// without normalising `..`, leaving absolute paths untouched.
fn rev_parse_prefix_filename(prefix: &str, arg: &str) -> String {
    if prefix.is_empty() || Path::new(arg).is_absolute() {
        return arg.to_string();
    }
    format!("{prefix}{arg}")
}

/// Existence probe for a disambiguated filename relative to an explicit prefix
/// (`<worktree-root>/<prefix><path>`), mirroring git's `verify_filename()`.
fn rev_parse_prefixed_path_exists(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    prefix: &str,
    path: &str,
) -> Result<bool> {
    if path.is_empty() {
        return Ok(false);
    }
    let root = worktree_root_for_git_dir(cli_session, git_dir)?;
    Ok(root.join(format!("{prefix}{path}")).exists())
}

fn rev_parse_index_stage_path(rest: &str) -> (Option<u8>, &str) {
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && matches!(bytes[0], b'0'..=b'3') {
        return (Some(bytes[0] - b'0'), &rest[2..]);
    }
    (None, rest)
}

fn rev_parse_normalize_relative_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
) -> Result<String> {
    if !(path.starts_with("./") || path.starts_with("../")) {
        return Ok(path.to_string());
    }
    if !is_inside_work_tree(cli_session, cwd, git_dir, None)? {
        eprintln!("fatal: relative path syntax can't be used outside working tree");
        return Err(GitError::Exit(128));
    }
    let root = fs::canonicalize(worktree_root_for_git_dir(cli_session, git_dir)?)?;
    let cwd = fs::canonicalize(cwd)?;
    let mut normalized = cwd;
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized == root {
                    eprintln!(
                        "fatal: '{path}' is outside repository at '{}'",
                        root.display()
                    );
                    return Err(GitError::Exit(128));
                }
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
            _ => {}
        }
    }
    if !normalized.starts_with(&root) {
        eprintln!(
            "fatal: '{path}' is outside repository at '{}'",
            root.display()
        );
        return Err(GitError::Exit(128));
    }
    let relative = normalized
        .strip_prefix(&root)
        .map_err(|err| GitError::InvalidPath(err.to_string()))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn rev_parse_diagnose_arg_failure(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    normalized_rev: &str,
    err: GitError,
    before_dashdash: bool,
    seen_path_arg: bool,
) -> Result<()> {
    if before_dashdash {
        eprintln!("fatal: bad revision '{rev}'");
        return Err(GitError::Exit(128));
    }
    if rev_parse_has_unescaped_wildcard(rev) {
        println!("{rev}");
        return Ok(());
    }
    if let Some((base, path)) = sley_rev::split_rev_path_spec(normalized_rev) {
        println!("{rev}");
        if let Some((_, original_path)) = sley_rev::split_rev_path_spec(rev) {
            return rev_parse_tree_path_error(
                cli_session,
                cwd,
                git_dir,
                format,
                base,
                path,
                original_path,
                seen_path_arg,
                err,
            );
        }
    }
    if let Some(rest) = normalized_rev.strip_prefix(':')
        && !normalized_rev.starts_with(":/")
    {
        println!("{rev}");
        let (stage, path) = rev_parse_index_stage_path(rest);
        return rev_parse_index_path_error(
            cli_session,
            cwd,
            git_dir,
            format,
            stage.unwrap_or(0),
            path,
            err,
        );
    }
    if rev_parse_is_selector_error(rev) {
        eprintln!("fatal: {}", rev_parse_error_message(&err));
        return Err(GitError::Exit(128));
    }
    rev_parse_maybe_print_ambiguity(git_dir, format, normalized_rev, &err)?;
    println!("{rev}");
    Err(sley_rev::ambiguous_argument_error(rev))
}

fn rev_parse_maybe_print_ambiguity(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    err: &GitError,
) -> Result<()> {
    let Some((prefix, disambiguation)) = rev_parse_ambiguity_context(format, rev, err) else {
        return Ok(());
    };
    eprintln!("error: short object ID {prefix} is ambiguous");
    let hints =
        match sley_rev::ambiguous_short_object_id_hint(git_dir, format, &prefix, disambiguation) {
            Ok(hints) => hints,
            Err(GitError::InvalidObject(message)) if message.starts_with("unknown object type") => {
                eprintln!("fatal: invalid object type");
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        };
    if !hints.is_empty() {
        eprintln!("hint: The candidates are:");
        for hint in hints {
            eprintln!("hint:   {hint}");
        }
    }
    Ok(())
}

fn rev_parse_ambiguity_context(
    format: ObjectFormat,
    rev: &str,
    err: &GitError,
) -> Option<(String, sley_rev::ObjectDisambiguation)> {
    if !sley_rev::is_short_object_id_ambiguous_error(err) {
        return None;
    }
    let GitError::InvalidObjectId(message) = err else {
        return None;
    };
    let prefix = message
        .strip_prefix("short object ID ")?
        .strip_suffix(" is ambiguous")?;
    let disambiguation = rev_parse_disambiguation_for_rev(rev, prefix);
    Some((prefix.to_string(), disambiguation))
}

fn rev_parse_disambiguation_for_rev(rev: &str, prefix: &str) -> sley_rev::ObjectDisambiguation {
    if let Some((base, _)) = sley_rev::split_rev_path_spec(rev)
        && base == prefix
    {
        return sley_rev::ObjectDisambiguation::Treeish;
    }
    if rev == prefix {
        return sley_rev::ObjectDisambiguation::Any;
    }
    if rev == format!("{prefix}^0")
        || rev == format!("{prefix}^")
        || rev.starts_with(&format!("{prefix}~"))
        || rev.starts_with(&format!("{prefix}^{{/"))
        || rev.starts_with(&format!("{prefix}^{{commit}}"))
    {
        return sley_rev::ObjectDisambiguation::Commitish;
    }
    if rev.starts_with(&format!("{prefix}^{{tree}}")) {
        return sley_rev::ObjectDisambiguation::Treeish;
    }
    if rev.starts_with(&format!("{prefix}^{{blob}}")) {
        return sley_rev::ObjectDisambiguation::Blob;
    }
    if rev.starts_with(&format!("{prefix}^{{tag}}")) {
        return sley_rev::ObjectDisambiguation::Tag;
    }
    sley_rev::ObjectDisambiguation::Any
}

fn rev_parse_tree_path_error(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    base: &str,
    path: &str,
    original_path: &str,
    seen_path_arg: bool,
    err: GitError,
) -> Result<()> {
    rev_parse_maybe_print_ambiguity(git_dir, format, &format!("{base}:{path}"), &err)?;
    if rev_parse_error_message(&err).starts_with("revision ") {
        eprintln!("fatal: invalid object name '{base}'.");
        return Err(GitError::Exit(128));
    }
    if seen_path_arg {
        rev_parse_no_such_worktree_path(&format!("{base}:{original_path}"));
        return Err(GitError::Exit(128));
    }
    if let Some(prefixed) = rev_parse_prefixed_path(cli_session, cwd, git_dir, original_path)?
        && rev_parse_tree_contains(
            git_dir,
            format,
            base,
            &prefixed,
            cli_session.replace_objects(),
        )
    {
        eprintln!("fatal: path '{prefixed}' exists, but not '{original_path}'");
        eprintln!("hint: Did you mean '{base}:{prefixed}' aka '{base}:./{original_path}'?");
        return Err(GitError::Exit(128));
    }
    if rev_parse_path_exists_on_disk(cli_session, cwd, git_dir, original_path)? {
        eprintln!("fatal: path '{path}' exists on disk, but not in '{base}'");
    } else {
        eprintln!("fatal: path '{path}' does not exist in '{base}'");
    }
    let _ = format;
    Err(GitError::Exit(128))
}

fn rev_parse_index_path_error(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stage: u8,
    path: &str,
    err: GitError,
) -> Result<()> {
    if rev_parse_error_message(&err).contains("but not at stage") {
        eprintln!("fatal: path '{path}' is in the index, but not at stage {stage}");
        if stage != 0 {
            eprintln!("hint: Did you mean ':0:{path}'?");
        }
        return Err(GitError::Exit(128));
    }
    if let Some(prefixed) = rev_parse_prefixed_path(cli_session, cwd, git_dir, path)?
        && rev_parse_index_contains(git_dir, format, &prefixed)?
    {
        eprintln!("fatal: path '{prefixed}' is in the index, but not '{path}'");
        eprintln!("hint: Did you mean ':0:{prefixed}' aka ':0:./{path}'?");
        return Err(GitError::Exit(128));
    }
    let in_index = rev_parse_index_contains(git_dir, format, path)?;
    let on_disk = rev_parse_path_exists_on_disk(cli_session, cwd, git_dir, path)?;
    match (on_disk, in_index) {
        (true, false) => eprintln!("fatal: path '{path}' exists on disk, but not in the index"),
        (false, false) => {
            eprintln!("fatal: path '{path}' does not exist (neither on disk nor in the index)")
        }
        _ => eprintln!("fatal: path '{path}' is not in the index"),
    }
    Err(GitError::Exit(128))
}

fn rev_parse_no_such_worktree_path(path: &str) {
    eprintln!("fatal: {path}: no such path in the working tree.");
    eprintln!("Use 'git <command> -- <path>...' to specify paths that do not exist locally.");
}

fn rev_parse_path_exists_on_disk(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
) -> Result<bool> {
    if path.is_empty() {
        return Ok(false);
    }
    let direct = cwd.join(path);
    if direct.exists() {
        return Ok(true);
    }
    if let Ok(root) = worktree_root_for_git_dir(cli_session, git_dir) {
        return Ok(root.join(path).exists());
    }
    Ok(false)
}

fn rev_parse_prefixed_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
) -> Result<Option<String>> {
    if path.starts_with("./") || path.starts_with("../") || Path::new(path).is_absolute() {
        return Ok(None);
    }
    if !is_inside_work_tree(cli_session, cwd, git_dir, None)? {
        return Ok(None);
    }
    let prefix = worktree_prefix(cli_session, cwd, git_dir, None)?;
    if prefix.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("{prefix}{path}")))
}

fn rev_parse_tree_contains(
    git_dir: &Path,
    format: ObjectFormat,
    base: &str,
    path: &str,
    replace_objects: bool,
) -> bool {
    let Ok(db) = crate::repository::open_object_database(git_dir, format, replace_objects) else {
        return false;
    };
    sley_rev::resolve_rev_path(git_dir, format, &db, base, path).is_ok()
}

fn rev_parse_index_contains(git_dir: &Path, format: ObjectFormat, path: &str) -> Result<bool> {
    let bytes = match fs::read(rev_parse_repository_index_path(git_dir)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let index = sley_index::Index::parse(&bytes, format)?;
    Ok(index
        .entries
        .iter()
        .any(|entry| !entry.is_sparse_dir() && entry.path == path.as_bytes()))
}

fn rev_parse_repository_index_path(git_dir: &Path) -> PathBuf {
    env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.join("index"))
}

fn rev_parse_has_unescaped_wildcard(value: &str) -> bool {
    let mut escaped = false;
    for byte in value.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        if matches!(byte, b'*' | b'?' | b'[') {
            return true;
        }
    }
    false
}

fn rev_parse_is_selector_error(rev: &str) -> bool {
    rev.contains("@{")
}

fn rev_parse_error_message(err: &GitError) -> String {
    match err {
        GitError::NotFound(kind) => kind.to_string(),
        GitError::InvalidFormat(msg)
        | GitError::InvalidObjectId(msg)
        | GitError::InvalidObject(msg)
        | GitError::InvalidPath(msg)
        | GitError::Unsupported(msg)
        | GitError::Command(msg)
        | GitError::Io(msg)
        | GitError::Transaction(msg)
        | GitError::Cli(_, msg) => msg.clone(),
        GitError::Exit(code) => format!("exit {code}"),
    }
}

fn rev_parse_args_need_no_repository(cwd: &Path, args: &[String]) -> Result<bool> {
    let mut idx = 0;
    let mut handled = false;
    while idx < args.len() {
        match args[idx].as_str() {
            "--parseopt" => {
                rev_parse_parseopt(&args[idx + 1..])?;
                return Ok(true);
            }
            "--sq-quote" => {
                print_rev_parse_sq_quote(&args[idx + 1..])?;
                return Ok(true);
            }
            "--local-env-vars" => {
                print_local_env_vars();
                handled = true;
            }
            "--resolve-git-dir" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_resolve_git_dir_requires_argument_error)?;
                println!("{}", resolve_git_dir_arg(cwd, path)?);
                handled = true;
            }
            _ => return Ok(false),
        }
        idx += 1;
    }
    Ok(handled)
}

#[derive(Clone, Debug)]
struct RevParseParseOptSpec {
    short: Option<char>,
    long: Option<String>,
    help: String,
    arg_hint: Option<String>,
    takes_arg: bool,
    optional_arg: bool,
    no_neg: bool,
    hidden: bool,
    group: bool,
}

#[derive(Clone, Copy, Debug)]
struct RevParseParseOptFlags {
    keep_dashdash: bool,
    stop_at_non_option: bool,
    stuck_long: bool,
}

#[derive(Clone, Copy, Debug)]
struct RevParseParseOptMatch {
    spec_index: usize,
    unset: bool,
}

fn rev_parse_parseopt(args: &[String]) -> Result<()> {
    let mut flags = RevParseParseOptFlags {
        keep_dashdash: false,
        stop_at_non_option: false,
        stuck_long: false,
    };
    let mut idx = 0;
    while let Some(arg) = args.get(idx) {
        match arg.as_str() {
            "--" => {
                idx += 1;
                break;
            }
            "--keep-dashdash" => flags.keep_dashdash = true,
            "--stop-at-non-option" => flags.stop_at_non_option = true,
            "--stuck-long" => flags.stuck_long = true,
            "-h" | "--help" => {
                print_rev_parse_parseopt_usage_stdout();
                return Err(GitError::Exit(129));
            }
            other => {
                eprintln!("error: unknown option `{}`", other.trim_start_matches("--"));
                print_rev_parse_parseopt_usage_stderr();
                return Err(GitError::Exit(129));
            }
        }
        idx += 1;
    }
    if idx == 0 || args.get(idx.saturating_sub(1)).map(String::as_str) != Some("--") {
        print_rev_parse_parseopt_usage_stderr();
        return Err(GitError::Exit(129));
    }

    let script_args = &args[idx..];
    let (usage, specs) = read_rev_parse_parseopt_spec()?;
    match parse_rev_parse_parseopt_args(script_args, &specs, flags) {
        Ok((parsed, rest)) => {
            let mut out = parsed;
            out.push_str(" --");
            for arg in rest {
                out.push(' ');
                push_shell_sq_quote(&mut out, arg);
            }
            println!("{out}");
            Ok(())
        }
        Err(RevParseParseOptError::Help { full }) => {
            print!(
                "{}",
                render_rev_parse_parseopt_usage(&usage, &specs, full, true)
            );
            Err(GitError::Exit(129))
        }
        Err(RevParseParseOptError::Usage { message }) => {
            eprintln!("error: {message}");
            eprint!(
                "{}",
                render_rev_parse_parseopt_usage(&usage, &specs, false, false)
            );
            Err(GitError::Exit(129))
        }
    }
}

fn print_rev_parse_parseopt_usage_stdout() {
    print!(
        "usage: git rev-parse --parseopt [<options>] -- [<args>...]\n\n    --[no-]keep-dashdash    keep the `--` passed as an arg\n    --[no-]stop-at-non-option\n                          stop parsing after the first non-option argument\n    --[no-]stuck-long      output in stuck long form\n\n"
    );
}

fn print_rev_parse_parseopt_usage_stderr() {
    eprint!(
        "usage: git rev-parse --parseopt [<options>] -- [<args>...]\n\n    --[no-]keep-dashdash    keep the `--` passed as an arg\n    --[no-]stop-at-non-option\n                          stop parsing after the first non-option argument\n    --[no-]stuck-long      output in stuck long form\n\n"
    );
}

fn read_rev_parse_parseopt_spec() -> Result<(Vec<String>, Vec<RevParseParseOptSpec>)> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut lines = input.lines();
    let mut usage = Vec::new();
    loop {
        let Some(line) = lines.next() else {
            eprintln!("fatal: premature end of input");
            return Err(GitError::Exit(128));
        };
        if line == "--" {
            if usage.is_empty() {
                eprintln!("fatal: no usage string given before the `--' separator");
                return Err(GitError::Exit(128));
            }
            break;
        }
        usage.push(line.to_string());
    }

    let mut specs = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        specs.push(parse_rev_parse_parseopt_spec_line(line)?);
    }
    Ok((usage, specs))
}

fn parse_rev_parse_parseopt_spec_line(line: &str) -> Result<RevParseParseOptSpec> {
    let Some(help_start) = line.find(char::is_whitespace) else {
        return Ok(RevParseParseOptSpec {
            short: None,
            long: None,
            help: line.trim_start().to_string(),
            arg_hint: None,
            takes_arg: false,
            optional_arg: false,
            no_neg: false,
            hidden: false,
            group: true,
        });
    };
    if help_start == 0 {
        return Ok(RevParseParseOptSpec {
            short: None,
            long: None,
            help: line.trim_start().to_string(),
            arg_hint: None,
            takes_arg: false,
            optional_arg: false,
            no_neg: false,
            hidden: false,
            group: true,
        });
    }

    let mut spec = RevParseParseOptSpec {
        short: None,
        long: None,
        help: line[help_start..].trim_start().to_string(),
        arg_hint: None,
        takes_arg: false,
        optional_arg: false,
        no_neg: false,
        hidden: false,
        group: false,
    };
    let optspec = &line[..help_start];
    let flags_start = optspec.find(['*', '=', '?', '!']).unwrap_or(optspec.len());
    if flags_start == 0 {
        eprintln!("fatal: missing opt-spec before option flags");
        return Err(GitError::Exit(128));
    }
    let names = &optspec[..flags_start];
    if names.chars().count() == 1 {
        spec.short = names.chars().next();
    } else if names.as_bytes().get(1) != Some(&b',') {
        spec.long = Some(names.to_string());
    } else {
        spec.short = names.chars().next();
        spec.long = Some(names[2..].to_string());
    }

    let mut flags = &optspec[flags_start..];
    while let Some(ch) = flags.chars().next() {
        match ch {
            '=' => spec.takes_arg = true,
            '?' => {
                spec.takes_arg = true;
                spec.optional_arg = true;
            }
            '!' => spec.no_neg = true,
            '*' => spec.hidden = true,
            _ => break,
        }
        flags = &flags[ch.len_utf8()..];
    }
    if !flags.is_empty() {
        spec.arg_hint = Some(flags.to_string());
    }
    Ok(spec)
}

enum RevParseParseOptError {
    Help { full: bool },
    Usage { message: String },
}

fn parse_rev_parse_parseopt_args<'a>(
    args: &'a [String],
    specs: &[RevParseParseOptSpec],
    flags: RevParseParseOptFlags,
) -> std::result::Result<(String, Vec<&'a str>), RevParseParseOptError> {
    let mut parsed = String::from("set --");
    let mut positionals = Vec::new();
    let mut idx = 0;
    while let Some(arg) = args.get(idx).map(String::as_str) {
        if arg == "--" {
            let start = if flags.keep_dashdash { idx } else { idx + 1 };
            positionals.extend(args[start..].iter().map(String::as_str));
            return Ok((parsed, positionals));
        }
        if arg == "-h" || arg == "--help" {
            return Err(RevParseParseOptError::Help { full: false });
        }
        if arg == "--help-all" {
            return Err(RevParseParseOptError::Help { full: true });
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, attached) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            let matched = resolve_rev_parse_parseopt_long(name, specs)?;
            dump_rev_parse_parseopt_match(&mut parsed, specs, matched, attached, flags)?;
            idx += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            parse_rev_parse_parseopt_short_bundle(&mut parsed, args, &mut idx, specs, flags)?;
            idx += 1;
            continue;
        }
        if flags.stop_at_non_option {
            positionals.extend(args[idx..].iter().map(String::as_str));
            return Ok((parsed, positionals));
        }
        positionals.push(arg);
        idx += 1;
    }
    Ok((parsed, positionals))
}

fn parse_rev_parse_parseopt_short_bundle<'a>(
    parsed: &mut String,
    args: &'a [String],
    idx: &mut usize,
    specs: &[RevParseParseOptSpec],
    flags: RevParseParseOptFlags,
) -> std::result::Result<(), RevParseParseOptError> {
    let mut rest = args[*idx]
        .strip_prefix('-')
        .expect("caller checked leading dash");
    while let Some(short) = rest.chars().next() {
        rest = &rest[short.len_utf8()..];
        let Some(spec_index) = specs
            .iter()
            .position(|spec| !spec.group && spec.short == Some(short))
        else {
            return Err(RevParseParseOptError::Usage {
                message: format!("unknown switch `{short}'"),
            });
        };
        let spec = &specs[spec_index];
        let matched = RevParseParseOptMatch {
            spec_index,
            unset: false,
        };
        if spec.takes_arg {
            if !rest.is_empty() {
                dump_rev_parse_parseopt_match(parsed, specs, matched, Some(rest), flags)?;
            } else if spec.optional_arg {
                dump_rev_parse_parseopt_match(parsed, specs, matched, None, flags)?;
            } else {
                *idx += 1;
                let Some(value) = args.get(*idx).map(String::as_str) else {
                    return Err(RevParseParseOptError::Usage {
                        message: format!("switch `{short}' requires a value"),
                    });
                };
                dump_rev_parse_parseopt_match(parsed, specs, matched, Some(value), flags)?;
            }
            return Ok(());
        }
        dump_rev_parse_parseopt_match(parsed, specs, matched, None, flags)?;
    }
    Ok(())
}

fn resolve_rev_parse_parseopt_long(
    name: &str,
    specs: &[RevParseParseOptSpec],
) -> std::result::Result<RevParseParseOptMatch, RevParseParseOptError> {
    let mut candidates = Vec::new();
    for (spec_index, spec) in specs.iter().enumerate() {
        let Some(long) = spec.long.as_deref() else {
            continue;
        };
        candidates.push((long.to_string(), spec_index, false));
        if !spec.no_neg {
            candidates.push((format!("no-{long}"), spec_index, true));
            if let Some(positive) = long.strip_prefix("no-") {
                candidates.push((positive.to_string(), spec_index, true));
            }
        }
    }

    for (candidate, spec_index, unset) in &candidates {
        if candidate == name {
            return Ok(RevParseParseOptMatch {
                spec_index: *spec_index,
                unset: *unset,
            });
        }
    }

    let matches: Vec<_> = candidates
        .iter()
        .filter(|(candidate, _, _)| candidate.starts_with(name))
        .collect();
    match matches.as_slice() {
        [] => Err(RevParseParseOptError::Usage {
            message: format!("unknown option `{name}'"),
        }),
        [(candidate, spec_index, unset)] => {
            let spec = &specs[*spec_index];
            if name.starts_with("no-") && *unset && spec.no_neg {
                return Err(RevParseParseOptError::Usage {
                    message: format!("unknown option `{name}'"),
                });
            }
            let _ = candidate;
            Ok(RevParseParseOptMatch {
                spec_index: *spec_index,
                unset: *unset,
            })
        }
        [first, second, ..] => Err(RevParseParseOptError::Usage {
            message: format!(
                "ambiguous option: {name} (could be --{} or --{})",
                first.0, second.0
            ),
        }),
    }
}

fn dump_rev_parse_parseopt_match(
    out: &mut String,
    specs: &[RevParseParseOptSpec],
    matched: RevParseParseOptMatch,
    attached: Option<&str>,
    flags: RevParseParseOptFlags,
) -> std::result::Result<(), RevParseParseOptError> {
    let spec = &specs[matched.spec_index];
    if matched.unset && attached.is_some() {
        let long = spec.long.as_deref().unwrap_or_default();
        return Err(RevParseParseOptError::Usage {
            message: format!("option `no-{long}' takes no value"),
        });
    }
    if attached.is_some() && !spec.takes_arg {
        let name = spec
            .long
            .as_deref()
            .map(|long| format!("option `{long}'"))
            .or_else(|| spec.short.map(|short| format!("switch `{short}'")))
            .unwrap_or_else(|| "option".to_string());
        return Err(RevParseParseOptError::Usage {
            message: format!("{name} takes no value"),
        });
    }
    if !matched.unset && spec.takes_arg && !spec.optional_arg && attached.is_none() {
        let name = spec
            .long
            .as_deref()
            .map(|long| format!("option `{long}'"))
            .or_else(|| spec.short.map(|short| format!("switch `{short}'")))
            .unwrap_or_else(|| "option".to_string());
        return Err(RevParseParseOptError::Usage {
            message: format!("{name} requires a value"),
        });
    }

    if matched.unset {
        out.push_str(" --no-");
        out.push_str(spec.long.as_deref().unwrap_or_default());
    } else if let Some(short) = spec.short
        && (spec.long.is_none() || !flags.stuck_long)
    {
        out.push_str(" -");
        out.push(short);
    } else if let Some(long) = spec.long.as_deref() {
        out.push_str(" --");
        out.push_str(long);
    }

    if let Some(value) = attached {
        if !flags.stuck_long {
            out.push(' ');
        } else if spec.long.is_some() {
            out.push('=');
        }
        push_shell_sq_quote(out, value);
    }
    Ok(())
}

fn render_rev_parse_parseopt_usage(
    usage: &[String],
    specs: &[RevParseParseOptSpec],
    full: bool,
    shell_eval: bool,
) -> String {
    let mut out = String::new();
    if shell_eval {
        out.push_str("cat <<\\EOF\n");
    }
    let mut saw_empty_line = false;
    let mut first_usage = true;
    for usage_line in usage {
        if !saw_empty_line && usage_line.is_empty() {
            saw_empty_line = true;
        }
        if saw_empty_line {
            if usage_line.is_empty() {
                out.push('\n');
            } else {
                out.push_str("    ");
                out.push_str(usage_line);
                out.push('\n');
            }
        } else if first_usage {
            out.push_str("usage: ");
            out.push_str(usage_line);
            out.push('\n');
        } else {
            out.push_str("   or: ");
            out.push_str(usage_line);
            out.push('\n');
        }
        first_usage = false;
    }

    let mut need_newline = true;
    for spec in specs {
        if spec.group {
            out.push('\n');
            need_newline = false;
            if !spec.help.is_empty() {
                out.push_str(&spec.help);
                out.push('\n');
            }
            continue;
        }
        if spec.hidden && !full {
            continue;
        }
        if need_newline {
            out.push('\n');
            need_newline = false;
        }
        let option = rev_parse_parseopt_usage_option(spec);
        out.push_str(&option);
        usage_padding_string(&mut out, option.chars().count());
        out.push_str(&spec.help);
        out.push('\n');

        if !spec.no_neg
            && let Some(long) = spec.long.as_deref()
            && let Some(positive) = long.strip_prefix("no-")
            && !specs
                .iter()
                .any(|candidate| candidate.long.as_deref() == Some(positive))
        {
            let opposite = format!("    --{positive}");
            out.push_str(&opposite);
            usage_padding_string(&mut out, opposite.chars().count());
            out.push_str("opposite of --");
            out.push_str(long);
            out.push('\n');
        }
    }
    out.push('\n');
    if shell_eval {
        out.push_str("EOF\n");
    }
    out
}

fn rev_parse_parseopt_usage_option(spec: &RevParseParseOptSpec) -> String {
    let mut out = String::from("    ");
    if let Some(short) = spec.short {
        out.push('-');
        out.push(short);
    }
    if let Some(long) = spec.long.as_deref() {
        if spec.short.is_some() {
            out.push_str(", ");
        }
        if spec.no_neg || long.starts_with("no-") {
            out.push_str("--");
            out.push_str(long);
        } else {
            out.push_str("--[no-]");
            out.push_str(long);
        }
    }
    if spec.takes_arg || spec.arg_hint.is_some() {
        out.push_str(&rev_parse_parseopt_arg_hint(spec));
    }
    out
}

fn rev_parse_parseopt_arg_hint(spec: &RevParseParseOptSpec) -> String {
    let hint = spec.arg_hint.as_deref().unwrap_or("...");
    let literal = spec.arg_hint.is_none() || hint.chars().any(|ch| "()<>[]|".contains(ch));
    if spec.optional_arg {
        if spec.long.is_some() {
            if literal {
                format!("[={hint}]")
            } else {
                format!("[=<{hint}>]")
            }
        } else if literal {
            format!("[{hint}]")
        } else {
            format!("[<{hint}>]")
        }
    } else if literal {
        format!(" {hint}")
    } else {
        format!(" <{hint}>")
    }
}

fn usage_padding_string(out: &mut String, width: usize) {
    if width < 26 {
        out.push_str(&" ".repeat(26 - width));
    } else {
        out.push('\n');
        out.push_str(&" ".repeat(26));
    }
}

fn push_shell_sq_quote(out: &mut String, value: &str) {
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
}

fn print_rev_parse_sq_quote(args: &[String]) -> Result<()> {
    let mut stdout = io::stdout();
    for arg in args {
        stdout.write_all(b" '")?;
        for byte in arg.as_bytes() {
            if *byte == b'\'' {
                stdout.write_all(b"'\\''")?;
            } else {
                stdout.write_all(&[*byte])?;
            }
        }
        stdout.write_all(b"'")?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn print_local_env_vars() {
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        println!("{name}");
    }
}

fn rev_parse_needed_single_revision(quiet: bool) -> Result<()> {
    if quiet {
        return Err(GitError::Exit(1));
    }
    eprintln!("fatal: Needed a single revision");
    Err(GitError::Exit(128))
}

fn rev_parse_path_format_requires_argument() -> Result<()> {
    eprintln!("fatal: --path-format requires an argument");
    Err(GitError::Exit(128))
}

fn rev_parse_git_path_requires_argument_error() -> GitError {
    eprintln!("fatal: --git-path requires an argument");
    GitError::Exit(128)
}

fn rev_parse_resolve_git_dir_requires_argument_error() -> GitError {
    eprintln!("fatal: --resolve-git-dir requires an argument");
    GitError::Exit(128)
}

fn rev_parse_not_git_repository() -> Result<()> {
    eprintln!("fatal: not a git repository (or any of the parent directories): .git");
    Err(GitError::Exit(128))
}

fn rev_parse_unknown_path_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown argument to --path-format: {value}");
    Err(GitError::Exit(128))
}

fn rev_parse_unknown_show_object_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown mode for --show-object-format: {value}");
    Err(GitError::Exit(128))
}

fn rev_parse_not_gitdir(path: &str) -> Result<String> {
    eprintln!("fatal: not a gitdir '{path}'");
    Err(GitError::Exit(128))
}

fn rev_parse_requires_work_tree() -> Result<()> {
    eprintln!("fatal: this operation must be run in a work tree");
    Err(GitError::Exit(128))
}

fn rev_parse_abbrev_ref(repository: &RevParseRepository<'_>, rev: &str) -> Result<String> {
    let store = repository.refs();
    if rev == "HEAD" {
        return store
            .current_branch()?
            .ok_or_else(|| GitError::reference_not_found("symbolic HEAD"));
    }
    if let Some(name) = rev.strip_prefix("refs/heads/")
        && store.read_ref(rev)?.is_some()
    {
        return Ok(name.into());
    }
    if let Some(name) = rev.strip_prefix("refs/tags/")
        && store.read_ref(rev)?.is_some()
    {
        return Ok(name.into());
    }
    if store.read_ref(&format!("refs/heads/{rev}"))?.is_some() {
        return Ok(rev.into());
    }
    if store.read_ref(&format!("refs/tags/{rev}"))?.is_some() {
        return Ok(rev.into());
    }
    Err(GitError::not_found(format!("revision {rev}")))
}

/// Render a positional rev-parse line, prefixing `^` for an excluded (`^rev`)
/// argument. Mirrors the `^{rendered}` form `rev_parse_bisect` emits for good
/// refs.
fn rev_parse_print_positional(rendered: &str, negate: bool) {
    if negate {
        println!("^{rendered}");
    } else {
        println!("{rendered}");
    }
}

fn rev_parse_bisect(repository: &RevParseRepository<'_>, symbolic_full_name: bool) -> Result<()> {
    let store = repository.refs();
    let refs = store.list_refs()?;
    let terms = sley_rev::read_bisect_terms(repository.git_dir)?;
    let emit = |reference: &Ref, negate: bool| -> Result<()> {
        let rendered = if symbolic_full_name {
            reference.name.clone()
        } else {
            match resolve_ref_peeled(&store, &reference.name)? {
                Some(oid) => oid.to_hex(),
                None => return Ok(()),
            }
        };
        rev_parse_print_positional(&rendered, negate);
        Ok(())
    };
    // `list_refs` already returns refs in name order, so a single forward pass
    // per prefix preserves git's sorted output.
    for reference in &refs {
        if terms.is_bad_ref(&reference.name) {
            emit(reference, false)?;
        }
    }
    for reference in &refs {
        if terms.is_good_ref(&reference.name) {
            emit(reference, true)?;
        }
    }
    Ok(())
}

fn validate_bare_rev_parse_setup(
    cli_session: &crate::session::CliSession,
    setup: &Option<setup::SetupResult>,
) -> Result<()> {
    let Some(setup) = setup else {
        return Ok(());
    };
    let Some(worktree) = setup.worktree.as_ref() else {
        return Ok(());
    };
    if cli_session
        .explicit_work_tree()
        .as_ref()
        .is_some_and(|worktree| worktree.is_absolute())
        && fs::canonicalize(worktree).is_err()
    {
        eprintln!("fatal: cannot chdir to '{}'", worktree.display());
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn rev_parse_worktree_root(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    setup: Option<&setup::SetupResult>,
) -> Result<PathBuf> {
    if let Some(worktree) = setup.and_then(|setup| setup.worktree.as_ref()) {
        return Ok(worktree.clone());
    }
    worktree_root_for_git_dir(cli_session, git_dir)
}

fn worktree_cdup(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    setup: Option<&setup::SetupResult>,
) -> Result<String> {
    let prefix = worktree_prefix(cli_session, cwd, git_dir, setup)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok("../".repeat(depth))
}

fn worktree_prefix(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    setup: Option<&setup::SetupResult>,
) -> Result<String> {
    let root = fs::canonicalize(rev_parse_worktree_root(cli_session, git_dir, setup)?)?;
    let cwd = fs::canonicalize(cwd)?;
    let prefix = cwd.strip_prefix(&root).map_err(|_| {
        GitError::InvalidPath(format!(
            "{} is outside worktree {}",
            cwd.display(),
            root.display()
        ))
    })?;
    if prefix.as_os_str().is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{}/", prefix.to_string_lossy().replace('\\', "/")))
}

fn display_git_dir(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_dir_default(cli_session, cwd, git_dir),
        RevParsePathFormat::Absolute => Ok(fs::canonicalize(git_dir)?.display().to_string()),
        RevParsePathFormat::Relative => relative_path_from(cwd, git_dir),
    }
}

fn display_git_dir_default(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
) -> Result<String> {
    if let Some(git_dir) = cli_session.explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    if cli_session.explicit_bare() {
        return Ok(fs::canonicalize(git_dir)?.display().to_string());
    }
    if fs::canonicalize(cwd)? == fs::canonicalize(git_dir)? {
        Ok(".".into())
    } else if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git")
        && git_dir.parent() == Some(cwd)
    {
        Ok(".git".into())
    } else {
        Ok(fs::canonicalize(git_dir)?.display().to_string())
    }
}

fn display_git_common_dir(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_common_dir_default(cli_session, cwd, git_dir),
        RevParsePathFormat::Absolute => {
            Ok(cli_session.common_git_dir(git_dir)?.display().to_string())
        }
        RevParsePathFormat::Relative => {
            relative_path_from_absolute(cwd, &cli_session.common_git_dir(git_dir)?)
        }
    }
}

fn display_git_common_dir_default(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
) -> Result<String> {
    // A linked worktree's git dir (`…/worktrees/<id>`) carries a `commondir`
    // file pointing at the shared repository. git's `--git-common-dir`
    // (DEFAULT_RELATIVE_IF_SHARED) prints that common dir, not the per-worktree
    // git dir, so resolve it before any `.git`-suffix heuristics.
    if git_dir.join("commondir").is_file() {
        return Ok(cli_session.common_git_dir(git_dir)?.display().to_string());
    }
    if let Some(git_dir) = cli_session.explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return display_git_dir_default(cli_session, cwd, git_dir);
    }
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    if cwd == git_dir {
        return Ok(".".into());
    }
    if cwd.starts_with(&git_dir) {
        return Ok(git_dir.display().to_string());
    }
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    if cwd == fs::canonicalize(worktree_root)? {
        return Ok(".git".into());
    }
    let prefix = worktree_prefix(cli_session, &cwd, &git_dir, None)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok(format!("{}.git", "../".repeat(depth)))
}

fn display_shared_index_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
) -> Result<String> {
    let format = repository_object_format(git_dir)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    let bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(err.into()),
    };
    let index = sley_index::Index::parse(&bytes, format)?;
    let Some(link) = index.split_index_link(format)? else {
        return Ok(String::new());
    };
    if link.base_oid.is_null() {
        return Ok(String::new());
    }
    display_git_path(
        cli_session,
        cwd,
        git_dir,
        path_format,
        &format!("sharedindex.{}", link.base_oid),
    )
}

fn display_git_path(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<String> {
    if let Some(path) = display_git_path_env_override(cwd, path_format, path)? {
        return Ok(path);
    }
    if let Some(path) = display_git_path_hooks_override(cwd, git_dir, path_format, path)? {
        return Ok(path);
    }
    let base = git_path_base_dir(cli_session, git_dir, path)?;
    match path_format {
        RevParsePathFormat::Default => Ok(join_display_path(
            &display_git_path_default_base(cli_session, cwd, git_dir, path)?,
            path,
        )),
        RevParsePathFormat::Absolute => Ok(base.join(path).display().to_string()),
        RevParsePathFormat::Relative => {
            let target = base.join(path);
            relative_path_from_absolute(cwd, &target)
        }
    }
}

fn display_git_path_hooks_override(
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<Option<String>> {
    let suffix = if path == "hooks" {
        Some("")
    } else {
        path.strip_prefix("hooks/")
    };
    let Some(suffix) = suffix else {
        return Ok(None);
    };
    let config = commands::remote::read_effective_repo_config(git_dir, cwd)
        .map_err(report_config_setup_error)?;
    let Some(configured) = config.get("core", None, "hookspath") else {
        return Ok(None);
    };
    let base = PathBuf::from(configured);
    match path_format {
        RevParsePathFormat::Default => Ok(Some(join_display_path(configured, suffix))),
        RevParsePathFormat::Absolute => Ok(Some(
            absolute_env_git_path(cwd, &base, suffix)?
                .display()
                .to_string(),
        )),
        RevParsePathFormat::Relative => {
            let target = absolute_env_git_path(cwd, &base, suffix)?;
            Ok(Some(relative_path_from_absolute(cwd, &target)?))
        }
    }
}

fn display_git_path_default_base(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    path: &str,
) -> Result<String> {
    if git_dir.join("commondir").is_file() && !git_path_is_common(path) {
        return Ok(fs::canonicalize(git_dir)?.display().to_string());
    }
    display_git_common_dir_default(cli_session, cwd, git_dir)
}

fn git_path_base_dir(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    path: &str,
) -> Result<PathBuf> {
    if !git_dir.join("commondir").is_file() || !git_path_is_common(path) {
        return fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()));
    }
    cli_session.common_git_dir(git_dir)
}

fn git_path_is_common(path: &str) -> bool {
    if path == "config" || path == "packed-refs" || path == "shallow" {
        return true;
    }
    if git_path_component_is(path, "branches")
        || git_path_component_is(path, "hooks")
        || git_path_component_is(path, "info")
        || git_path_component_is(path, "objects")
        || git_path_component_is(path, "worktrees")
    {
        return true;
    }
    if let Some(refname) = path.strip_prefix("refs/") {
        return !git_path_is_per_worktree_ref(refname);
    }
    if let Some(logged) = path.strip_prefix("logs/") {
        return logged != "HEAD"
            && !logged
                .strip_prefix("refs/")
                .is_some_and(git_path_is_per_worktree_ref);
    }
    false
}

fn git_path_component_is(path: &str, component: &str) -> bool {
    path == component || path.starts_with(&format!("{component}/"))
}

fn git_path_is_per_worktree_ref(refname: &str) -> bool {
    refname.starts_with("bisect/")
        || refname.starts_with("worktree/")
        || refname.starts_with("rewritten/")
}

fn display_git_path_env_override(
    cwd: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<Option<String>> {
    if path == "index"
        && let Some(index) = env::var_os("GIT_INDEX_FILE")
    {
        return display_env_git_path(cwd, path_format, PathBuf::from(index), "");
    }
    let suffix = if path == "objects" {
        Some("")
    } else {
        path.strip_prefix("objects/")
    };
    if let Some(suffix) = suffix
        && let Some(objects) = env::var_os("GIT_OBJECT_DIRECTORY")
    {
        return display_env_git_path(cwd, path_format, PathBuf::from(objects), suffix);
    }
    Ok(None)
}

fn display_env_git_path(
    cwd: &Path,
    path_format: RevParsePathFormat,
    base: PathBuf,
    suffix: &str,
) -> Result<Option<String>> {
    match path_format {
        RevParsePathFormat::Default => {
            let base = base.to_string_lossy();
            Ok(Some(join_display_path(&base, suffix)))
        }
        RevParsePathFormat::Absolute => Ok(Some(
            absolute_env_git_path(cwd, &base, suffix)?
                .display()
                .to_string(),
        )),
        RevParsePathFormat::Relative => {
            let target = absolute_env_git_path(cwd, &base, suffix)?;
            Ok(Some(relative_path_from_absolute(cwd, &target)?))
        }
    }
}

fn absolute_env_git_path(cwd: &Path, base: &Path, suffix: &str) -> Result<PathBuf> {
    let resolved = if base.is_absolute() {
        base.to_path_buf()
    } else {
        cwd.join(base)
    };
    let canonical = if resolved.exists() {
        fs::canonicalize(&resolved)?
    } else if let Some(parent) = resolved.parent() {
        let file_name = resolved
            .file_name()
            .ok_or_else(|| GitError::InvalidPath(resolved.display().to_string()))?;
        fs::canonicalize(parent)?.join(file_name)
    } else {
        resolved
    };
    Ok(if suffix.is_empty() {
        canonical
    } else {
        canonical.join(suffix)
    })
}

fn join_display_path(base: &str, path: &str) -> String {
    if path.is_empty() {
        return base.to_string();
    }
    if base == "." {
        return path.to_string();
    }
    if base.is_empty() {
        return path.to_string();
    }
    format!("{base}/{path}")
}

fn resolve_git_dir_arg(cwd: &Path, path: &str) -> Result<String> {
    let candidate = cwd.join(path);
    if is_git_dir_candidate(&candidate) {
        return Ok(path.to_string());
    }
    if candidate.is_file()
        && let Ok(contents) = fs::read_to_string(&candidate)
        && let Some(target) = contents.trim().strip_prefix("gitdir:")
    {
        let target = target.trim();
        let resolved = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            candidate
                .parent()
                .map(|parent| parent.join(target))
                .unwrap_or_else(|| PathBuf::from(target))
        };
        if is_git_dir_candidate(&resolved) {
            return Ok(target.to_string());
        }
    }
    rev_parse_not_gitdir(path)
}

fn relative_path_from(cwd: &Path, target: &Path) -> Result<String> {
    let cwd = fs::canonicalize(cwd)?;
    let target = fs::canonicalize(target)?;
    relative_path_from_absolute_components(&cwd, &target)
}

fn is_inside_git_dir(
    cwd: &Path,
    git_dir: &Path,
    _setup: Option<&setup::SetupResult>,
) -> Result<bool> {
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    Ok(cwd.starts_with(git_dir))
}

fn cwd_starts_with(cwd: &Path, root: &Path) -> Result<bool> {
    let cwd = fs::canonicalize(cwd)?;
    let Ok(root) = fs::canonicalize(root) else {
        return Ok(false);
    };
    Ok(cwd.starts_with(root))
}

fn is_inside_work_tree(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    git_dir: &Path,
    setup: Option<&setup::SetupResult>,
) -> Result<bool> {
    if let Some(setup) = setup {
        let Some(worktree) = setup.worktree.as_ref() else {
            return Ok(false);
        };
        return cwd_starts_with(cwd, worktree);
    }
    // No SetupResult was threaded in (internal callers such as relative-path
    // normalization): honor an explicit `--work-tree` / `GIT_WORK_TREE` override
    // before the directory-layout probe. Without this, `rev-parse HEAD:./path`
    // run from a directory *outside* a relocated work tree mis-detects as inside
    // it and emits "outside repository" instead of git's "relative path syntax
    // can't be used outside working tree" (t1506 "relative path when cwd is
    // outside worktree").
    if let Some(work_tree) = cli_session.explicit_work_tree() {
        let root = resolve_cli_path(cwd, work_tree.to_string_lossy().as_ref());
        return cwd_starts_with(cwd, &root);
    }
    // A bare repository has no work tree, so we are never inside one. This
    // covers `core.bare = true` set on a `.git`-named directory, which the
    // directory-layout probe below would otherwise treat as having a worktree.
    if is_bare_repository(cli_session, git_dir, None)? {
        return Ok(false);
    }
    if worktree_root_for_git_dir(cli_session, git_dir).is_err() {
        return Ok(false);
    }
    Ok(!is_inside_git_dir(cwd, git_dir, None)?)
}

fn is_bare_repository(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    setup: Option<&setup::SetupResult>,
) -> Result<bool> {
    if setup.is_some_and(|setup| setup.worktree.is_some()) {
        return Ok(false);
    }
    if cli_session.explicit_work_tree().is_some() {
        return Ok(false);
    }
    let config = git_dir.join("config");
    if let Ok(config) = GitConfig::read(config)
        && let Some(bare) = config.get_bool("core", None, "bare")
    {
        return Ok(bare);
    }
    // With `core.bare` unset, git only infers bareness from the directory layout
    // during *discovery* (walking up to find a repo). When the git dir was named
    // explicitly via `--git-dir`/`GIT_DIR`, git applies no name heuristic and
    // defaults to non-bare.
    if cli_session.explicit_git_dir().is_some() {
        return Ok(false);
    }
    Ok(git_dir.file_name().and_then(|name| name.to_str()) != Some(".git"))
}

fn is_shallow_repository(git_dir: &Path) -> bool {
    sley_worktree::is_shallow_repository(git_dir)
}

/// `check_repository_format_gently`.
fn verify_repository_format(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
) -> Result<ObjectFormat> {
    repository_ref_storage_format(cli_session, git_dir)?;
    let common_git_dir = cli_session.common_git_dir(git_dir)?;
    let config_path = common_git_dir.join("config");
    let Ok(config) = GitConfig::read(&config_path) else {
        return Ok(ObjectFormat::Sha1);
    };
    let Some(version_value) = config.get("core", None, "repositoryformatversion") else {
        return Ok(config.repository_object_format()?);
    };
    let version: i64 = version_value.trim().parse().unwrap_or(0);
    if version > 1 {
        eprintln!("fatal: Expected git repo version <= 1, found {version}");
        return Err(GitError::Exit(128));
    }
    let mut v1_only = Vec::new();
    let mut unknown = Vec::new();
    for section in config.sections.iter().filter(|section| {
        section.name.eq_ignore_ascii_case("extensions") && section.subsection.is_none()
    }) {
        for entry in &section.entries {
            let ext = entry.key.to_ascii_lowercase();
            match ext.as_str() {
                // Extensions git honours even at repository version 0
                // (`handle_extension_v0`).
                "noop" | "preciousobjects" | "partialclone" | "worktreeconfig" => {}
                // v1-only extensions (`handle_extension`).
                "noop-v1"
                | "objectformat"
                | "compatobjectformat"
                | "refstorage"
                | "relativeworktrees"
                | "submodulepathconfig" => v1_only.push(ext),
                _ => unknown.push(ext),
            }
        }
    }
    if version >= 1 && !unknown.is_empty() {
        let plural = if unknown.len() == 1 {
            "extension"
        } else {
            "extensions"
        };
        eprintln!(
            "fatal: unknown repository {plural} found:\n\t{}",
            unknown.join("\n\t")
        );
        return Err(GitError::Exit(128));
    }
    if version == 0 && !v1_only.is_empty() {
        let plural = if v1_only.len() == 1 {
            "extension"
        } else {
            "extensions"
        };
        eprintln!(
            "fatal: repo version is 0, but v1-only {plural} found:\n\t{}",
            v1_only.join("\n\t")
        );
        return Err(GitError::Exit(128));
    }
    config.repository_object_format()
}

fn repository_ref_storage_format(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
) -> Result<&'static str> {
    let common_git_dir = cli_session.common_git_dir(git_dir)?;
    let config_path = common_git_dir.join("config");
    let Ok(bytes) = fs::read(&config_path) else {
        return Ok(RefStorageFormat::Files.name());
    };
    let Ok(config) = GitConfig::parse(&bytes) else {
        return Ok(RefStorageFormat::Files.name());
    };
    // Git validates `extensions.refstorage` as the config is read and aborts on
    // the first occurrence whose value is neither "files" nor "reftable" (the
    // check fires per-occurrence in file order, not just on the last-one-wins
    // value). Mirror that: report the bad value plus the physical config line.
    for value in config.get_all("extensions", None, "refStorage") {
        let Some(value) = value else { continue };
        // Git runs the value through `parse_reference_uri` first: the backend
        // name is the substring before the first `://` (the remainder is the
        // storage path payload), or the whole value when there is no `://`. The
        // *scheme* — not the raw value — is then matched with `strcmp` against
        // the lowercase `files`/`reftable`. So `files:///abs/path` is the valid
        // `files` backend, while `db://.git`, `reftable:`, and `reftable@/p` are
        // rejected. (The bad-value diagnostic still echoes the whole value.)
        if matches!(ref_storage_scheme(value), "files" | "reftable") {
            continue;
        }
        eprintln!("error: invalid value for 'extensions.refstorage': '{value}'");
        let line = refstorage_invalid_value_line(&bytes).unwrap_or(0);
        eprintln!(
            "fatal: bad config line {line} in file {}",
            ref_storage_config_display_path(cli_session, git_dir, &common_git_dir)
        );
        return Err(GitError::Exit(128));
    }
    Ok(
        match config
            .get("extensions", None, "refStorage")
            .map(ref_storage_scheme)
        {
            // Validation above guarantees any surviving scheme is exactly
            // `files` or `reftable`; only the latter selects the reftable backend.
            Some("reftable") => RefStorageFormat::Reftable.name(),
            _ => RefStorageFormat::Files.name(),
        },
    )
}

/// Git's `parse_reference_uri`: the backend name of an `extensions.refStorage`
/// (or `GIT_REFERENCE_BACKEND`) value is the substring before the first `://`;
/// the remainder is the storage-path payload. Without a `://` the whole value
/// is the backend name. Validation matches on this scheme, so a URI like
/// `files:///abs/path` selects the `files` backend rather than an unknown one.
fn ref_storage_scheme(value: &str) -> &str {
    match value.split_once("://") {
        Some((scheme, _payload)) => scheme,
        None => value,
    }
}

fn ref_storage_config_display_path(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
) -> String {
    if cli_session.explicit_git_dir().is_some() {
        return common_git_dir.join("config").display().to_string();
    }
    // Discovery anchors at the worktree toplevel. When the common dir is the
    // toplevel's `.git`, git prints the relative `.git/config`.
    if let Ok(worktree_root) = worktree_root_for_git_dir(cli_session, git_dir)
        && let Ok(worktree_root) = fs::canonicalize(&worktree_root)
        && common_git_dir == worktree_root.join(".git")
    {
        return Path::new(".git").join("config").display().to_string();
    }
    common_git_dir.join("config").display().to_string()
}

fn refstorage_invalid_value_line(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut in_extensions = false;
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if let Some(header) = line.strip_prefix('[') {
            // A section header opens a new scope. `[extensions]`, the quoted form
            // `[extensions "x"]`, and the dotted form `[extensions.x]` all begin
            // the extensions section (subsection is irrelevant for refstorage).
            let name = header
                .trim_end_matches(']')
                .split([' ', '\t', '.'])
                .next()
                .unwrap_or("")
                .trim();
            in_extensions = name.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("refstorage") {
            continue;
        }
        // Strip an inline comment, then surrounding whitespace, to recover the
        // assigned value (git-written configs never quote these tokens).
        let value = value
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        // Backend names are compared case-sensitively (git uses `strcmp`).
        if value != "files" && value != "reftable" {
            return Some(idx + 1);
        }
    }
    None
}

fn superproject_working_tree(git_dir: &Path) -> Result<Option<PathBuf>> {
    let git_dir = fs::canonicalize(git_dir)?;
    for ancestor in git_dir.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) != Some("modules") {
            continue;
        }
        let Some(super_git_dir) = ancestor.parent() else {
            continue;
        };
        if super_git_dir.file_name().and_then(|name| name.to_str()) == Some(".git")
            && is_git_dir_candidate(super_git_dir)
        {
            if let Some(worktree_root) = sley_worktree::worktree_root_for_git_dir(super_git_dir)? {
                return Ok(Some(fs::canonicalize(worktree_root)?));
            }
        }
    }
    Ok(None)
}
