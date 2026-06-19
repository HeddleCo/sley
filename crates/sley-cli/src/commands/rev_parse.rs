//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

#[derive(Clone, Copy)]
enum RevParsePathFormat {
    Default,
    Absolute,
    Relative,
}

pub(crate) fn cmd_rev_parse(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "--parseopt") {
        return rev_parse_parseopt(&args[1..]);
    }
    if rev_parse_args_need_no_repository(args)? {
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
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
    verify_repository_format(&git_dir)?;
    if args.is_empty() {
        return Err(GitError::Command("rev-parse requires <rev>...".into()));
    }
    let format = repository_object_format(&git_dir)?;
    let mut short: Option<usize> = None;
    let mut short_revs = 0usize;
    let mut verify = false;
    let mut verified_revs = 0usize;
    let mut quiet = false;
    let mut abbrev_ref = false;
    let mut symbolic_full_name = false;
    let mut revs_only = false;
    let mut path_format = RevParsePathFormat::Default;
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
                    let config = read_repo_config(&git_dir).ok();
                    pseudo = Some(sley_rev::PseudoRefResolver::new(
                        &git_dir,
                        format,
                        config.as_ref(),
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
            "--" => break,
            "--end-of-options" if verify => {}
            "--git-dir" => println!("{}", display_git_dir(&cwd, &git_dir, path_format)?),
            "--absolute-git-dir" => println!("{}", fs::canonicalize(&git_dir)?.display()),
            "--git-common-dir" => {
                println!("{}", display_git_common_dir(&cwd, &git_dir, path_format)?);
            }
            "--git-path" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_git_path_requires_argument_error)?;
                println!("{}", display_git_path(&cwd, &git_dir, path_format, path)?);
            }
            "--resolve-git-dir" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_resolve_git_dir_requires_argument_error)?;
                println!("{}", resolve_git_dir_arg(&cwd, path)?);
            }
            "--show-toplevel" => {
                if !is_inside_work_tree(&cwd, &git_dir)? {
                    return rev_parse_requires_work_tree();
                }
                let root = worktree_root_for_git_dir(&git_dir)?;
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
                if is_inside_work_tree(&cwd, &git_dir)? {
                    println!("{}", worktree_prefix(&cwd, &git_dir)?);
                } else {
                    println!();
                }
            }
            "--show-cdup" => {
                if is_inside_work_tree(&cwd, &git_dir)? {
                    println!("{}", worktree_cdup(&cwd, &git_dir)?);
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
            "--show-ref-format" => println!("{}", repository_ref_storage_format(&git_dir)?),
            "--local-env-vars" => print_local_env_vars(),
            "--sq-quote" => {
                print_rev_parse_sq_quote(&args[idx + 1..])?;
                break;
            }
            "--path-format=absolute" => path_format = RevParsePathFormat::Absolute,
            "--path-format=relative" => path_format = RevParsePathFormat::Relative,
            "--path-format" => return rev_parse_path_format_requires_argument(),
            "--is-inside-work-tree" => {
                println!("{}", is_inside_work_tree(&cwd, &git_dir)?);
            }
            "--is-inside-git-dir" => println!("{}", is_inside_git_dir(&cwd, &git_dir)?),
            "--is-bare-repository" => println!("{}", is_bare_repository(&git_dir)?),
            "--is-shallow-repository" => println!("{}", is_shallow_repository(&git_dir)),
            "--short" => short = repository_abbrev(&git_dir, format)?,
            "--verify" => verify = true,
            "--quiet" | "-q" => quiet = true,
            "--revs-only" => revs_only = true,
            "--abbrev-ref" | "--abbrev-ref=strict" | "--abbrev-ref=loose" => abbrev_ref = true,
            "--symbolic-full-name" => symbolic_full_name = true,
            "--bisect" => rev_parse_bisect(&git_dir, format, symbolic_full_name)?,
            value if value.starts_with('-') => {
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
                if abbrev_ref {
                    let rendered = rev_parse_abbrev_ref(&git_dir, format, rev)?;
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
                let oid = match resolve_revision(&git_dir, format, rev) {
                    Ok(oid) => oid,
                    Err(_) if revs_only => {
                        idx += 1;
                        continue;
                    }
                    Err(_) if verify && quiet => return Err(GitError::Exit(1)),
                    Err(_) if verify => {
                        return rev_parse_needed_single_revision(false);
                    }
                    Err(err) => return Err(err),
                };
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
    if verify && verified_revs != 1 {
        return rev_parse_needed_single_revision(quiet);
    }
    Ok(())
}

fn rev_parse_args_need_no_repository(args: &[String]) -> Result<bool> {
    let cwd = env::current_dir()?;
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
                println!("{}", resolve_git_dir_arg(&cwd, path)?);
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
            print!("{}", render_rev_parse_parseopt_usage(&usage, &specs, full, true));
            Err(GitError::Exit(129))
        }
        Err(RevParseParseOptError::Usage { message }) => {
            eprintln!("error: {message}");
            eprint!("{}", render_rev_parse_parseopt_usage(&usage, &specs, false, false));
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

fn rev_parse_abbrev_ref(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
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

fn rev_parse_bisect(git_dir: &Path, format: ObjectFormat, symbolic_full_name: bool) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    let terms = sley_rev::read_bisect_terms(git_dir)?;
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

fn worktree_cdup(cwd: &Path, git_dir: &Path) -> Result<String> {
    let prefix = worktree_prefix(cwd, git_dir)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok("../".repeat(depth))
}

fn display_git_dir(cwd: &Path, git_dir: &Path, path_format: RevParsePathFormat) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_dir_default(cwd, git_dir),
        RevParsePathFormat::Absolute => Ok(fs::canonicalize(git_dir)?.display().to_string()),
        RevParsePathFormat::Relative => relative_path_from(cwd, git_dir),
    }
}

fn display_git_dir_default(cwd: &Path, git_dir: &Path) -> Result<String> {
    if let Some(git_dir) = explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    if global_bare() {
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
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_common_dir_default(cwd, git_dir),
        RevParsePathFormat::Absolute => {
            Ok(common_git_dir_for_git_dir(git_dir)?.display().to_string())
        }
        RevParsePathFormat::Relative => {
            relative_path_from_absolute(cwd, &common_git_dir_for_git_dir(git_dir)?)
        }
    }
}

fn display_git_common_dir_default(cwd: &Path, git_dir: &Path) -> Result<String> {
    if let Some(git_dir) = explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    // A linked worktree's git dir (`…/worktrees/<id>`) carries a `commondir`
    // file pointing at the shared repository. git's `--git-common-dir`
    // (DEFAULT_RELATIVE_IF_SHARED) prints that common dir, not the per-worktree
    // git dir, so resolve it before any `.git`-suffix heuristics.
    if git_dir.join("commondir").is_file() {
        return Ok(common_git_dir_for_git_dir(git_dir)?.display().to_string());
    }
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return display_git_dir_default(cwd, git_dir);
    }
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    if cwd == git_dir {
        return Ok(".".into());
    }
    if cwd.starts_with(&git_dir) {
        return Ok(git_dir.display().to_string());
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if cwd == fs::canonicalize(worktree_root)? {
        return Ok(".git".into());
    }
    let prefix = worktree_prefix(&cwd, &git_dir)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok(format!("{}.git", "../".repeat(depth)))
}

fn display_git_path(
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<String> {
    if let Some(path) = display_git_path_env_override(cwd, path_format, path)? {
        return Ok(path);
    }
    match path_format {
        RevParsePathFormat::Default => Ok(join_display_path(
            &display_git_common_dir_default(cwd, git_dir)?,
            path,
        )),
        RevParsePathFormat::Absolute => {
            Ok(fs::canonicalize(git_dir)?.join(path).display().to_string())
        }
        RevParsePathFormat::Relative => {
            let target = fs::canonicalize(git_dir)?.join(path);
            relative_path_from_absolute(cwd, &target)
        }
    }
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

fn is_inside_git_dir(cwd: &Path, git_dir: &Path) -> Result<bool> {
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    Ok(cwd.starts_with(git_dir))
}

fn is_inside_work_tree(cwd: &Path, git_dir: &Path) -> Result<bool> {
    if let Some(work_tree) = explicit_work_tree() {
        let root = fs::canonicalize(resolve_cli_path(
            &env::current_dir()?,
            work_tree.to_string_lossy().as_ref(),
        ))?;
        let cwd = fs::canonicalize(cwd)?;
        return Ok(cwd.starts_with(root));
    }
    // A bare repository has no work tree, so we are never inside one. This
    // covers `core.bare = true` set on a `.git`-named directory, which the
    // directory-layout probe below would otherwise treat as having a worktree.
    if is_bare_repository(git_dir)? {
        return Ok(false);
    }
    if worktree_root_for_git_dir(git_dir).is_err() {
        return Ok(false);
    }
    Ok(!is_inside_git_dir(cwd, git_dir)?)
}

fn is_bare_repository(git_dir: &Path) -> Result<bool> {
    if explicit_work_tree().is_some() {
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
    if explicit_git_dir().is_some() {
        return Ok(false);
    }
    Ok(git_dir.file_name().and_then(|name| name.to_str()) != Some(".git"))
}

fn is_shallow_repository(git_dir: &Path) -> bool {
    sley_worktree::is_shallow_repository(git_dir)
}

/// `check_repository_format_gently`.
fn verify_repository_format(git_dir: &Path) -> Result<()> {
    repository_ref_storage_format(git_dir)?;
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config_path = common_git_dir.join("config");
    let Ok(config) = GitConfig::read(&config_path) else {
        return Ok(());
    };
    let Some(version_value) = config.get("core", None, "repositoryformatversion") else {
        return Ok(());
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
    Ok(())
}

fn repository_ref_storage_format(git_dir: &Path) -> Result<&'static str> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
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
        // Git compares the backend name with `strcmp` (case-sensitive): only the
        // exact lowercase `files`/`reftable` are valid; anything else is rejected.
        if value == "files" || value == "reftable" {
            continue;
        }
        eprintln!("error: invalid value for 'extensions.refstorage': '{value}'");
        let line = refstorage_invalid_value_line(&bytes).unwrap_or(0);
        eprintln!(
            "fatal: bad config line {line} in file {}",
            ref_storage_config_display_path(git_dir, &common_git_dir)
        );
        return Err(GitError::Exit(128));
    }
    Ok(match config.get("extensions", None, "refStorage") {
        // Validation above guarantees any surviving value is exactly `files` or
        // `reftable`; only the latter selects the reftable backend.
        Some("reftable") => RefStorageFormat::Reftable.name(),
        _ => RefStorageFormat::Files.name(),
    })
}

fn ref_storage_config_display_path(git_dir: &Path, common_git_dir: &Path) -> String {
    if explicit_git_dir().is_some() {
        return common_git_dir.join("config").display().to_string();
    }
    // Discovery anchors at the worktree toplevel. When the common dir is the
    // toplevel's `.git`, git prints the relative `.git/config`.
    if let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
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
            return Ok(Some(fs::canonicalize(worktree_root_for_git_dir(
                super_git_dir,
            )?)?));
        }
    }
    Ok(None)
}
