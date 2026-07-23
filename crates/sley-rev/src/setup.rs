use super::*;
use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevisionOrder {
    #[default]
    Default,
    Topo,
    Date,
    AuthorDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoWalkMode {
    #[default]
    Walk,
    Sorted,
    Unsorted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionTip {
    pub oid: ObjectId,
    pub rev: String,
    pub source_name: Option<String>,
    pub from_ref_selector: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSymmetricRange {
    pub left: ObjectId,
    pub right: ObjectId,
    pub negated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RevisionOptions {
    pub positives: Vec<RevisionTip>,
    pub negatives: Vec<ObjectId>,
    pub symmetric_ranges: Vec<RevisionSymmetricRange>,
    pub order: RevisionOrder,
    pub first_parent: bool,
    /// `--exclude-first-parent-only`: when marking the `^`-excluded (negative)
    /// tips' history UNINTERESTING, follow only the first parent of each merge.
    /// Unlike `--first-parent`, this affects the *negative* walk only.
    pub exclude_first_parent_only: bool,
    pub max_count: Option<usize>,
    pub skip: usize,
    pub reverse: bool,
    pub no_walk: NoWalkMode,
    pub date_window: RevWalkDateWindow,
    pub full_history: bool,
    pub sparse: bool,
    pub remove_empty: bool,
    pub simplify_merges: bool,
    pub show_pulls: bool,
    pub ancestry_path: bool,
    pub ignore_missing: bool,
    pub had_ref_selector: bool,
    pub author_patterns: Vec<String>,
}

impl RevisionOptions {
    pub fn has_revisions(&self) -> bool {
        !self.positives.is_empty() || !self.negatives.is_empty() || self.had_ref_selector
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupRevisions {
    pub options: RevisionOptions,
    pub pathspecs: Vec<String>,
    pub leftovers: Vec<String>,
}

pub struct RevisionSetupContext<'a, R> {
    pub git_dir: &'a Path,
    pub worktree_root: Option<&'a Path>,
    pub cwd: &'a Path,
    pub format: ObjectFormat,
    pub reader: &'a R,
    pub config: Option<&'a GitConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefSelectorKind {
    All,
    Glob,
    Branches,
    Tags,
    Remotes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefSelector {
    kind: RefSelectorKind,
    not: bool,
    pattern: Option<String>,
    include_all: bool,
    excludes: Vec<String>,
    hidden: Option<HiddenRefsSection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenRefsSection {
    Fetch,
    Receive,
    Uploadpack,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct HiddenRefs {
    transfer: Vec<String>,
    fetch: Vec<String>,
    receive: Vec<String>,
    uploadpack: Vec<String>,
}

impl HiddenRefs {
    fn from_config(config: Option<&GitConfig>) -> Self {
        let Some(config) = config else {
            return Self::default();
        };
        // git's parse_hide_refs_config strips trailing '/' from each value, so
        // `transfer.hideRefs=refs/heads/subspace/` hides `refs/heads/subspace/*`
        // via the `*p == '/'` clause in ref_is_hidden.
        let section = |sec| {
            config_section_values(config, sec, None, "hideRefs")
                .into_iter()
                .map(|value| value.trim_end_matches('/').to_string())
                .collect()
        };
        Self {
            transfer: section("transfer"),
            fetch: section("fetch"),
            receive: section("receive"),
            uploadpack: section("uploadpack"),
        }
    }

    fn section_patterns(&self, section: HiddenRefsSection) -> &[String] {
        match section {
            HiddenRefsSection::Fetch => &self.fetch,
            HiddenRefsSection::Receive => &self.receive,
            HiddenRefsSection::Uploadpack => &self.uploadpack,
        }
    }
}

pub fn setup_revisions<R>(
    args: &[String],
    ctx: &RevisionSetupContext<'_, R>,
) -> Result<SetupRevisions>
where
    R: ObjectReader,
{
    let mut parser = SetupRevisionsParser::new(ctx);
    parser.parse(args)?;
    parser.finish()
}

pub fn setup_revisions_os<R>(
    args: &[OsString],
    ctx: &RevisionSetupContext<'_, R>,
) -> Result<SetupRevisions>
where
    R: ObjectReader,
{
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    setup_revisions(&args, ctx)
}

struct SetupRevisionsParser<'a, R> {
    ctx: &'a RevisionSetupContext<'a, R>,
    setup: SetupRevisions,
    ref_selectors: Vec<RefSelector>,
    pending_excludes: Vec<String>,
    pending_hidden: Option<HiddenRefsSection>,
    not: bool,
    default_revision: Option<String>,
    /// `--reflog`: add every reflog entry (old + new oid, across HEAD and all
    /// refs) as a pending positive tip. Expanded in `finish`.
    reflog: bool,
}

impl<'a, R> SetupRevisionsParser<'a, R>
where
    R: ObjectReader,
{
    fn new(ctx: &'a RevisionSetupContext<'a, R>) -> Self {
        Self {
            ctx,
            setup: SetupRevisions::default(),
            ref_selectors: Vec::new(),
            pending_excludes: Vec::new(),
            pending_hidden: None,
            not: false,
            default_revision: None,
            reflog: false,
        }
    }

    fn parse(&mut self, args: &[String]) -> Result<()> {
        let mut positional_only = false;
        let mut end_of_options = false;
        // When an explicit `--` is present, every positional before it is a
        // revision, even if it happens to carry pathspec magic. This decision
        // must be known before parsing the earlier tokens.
        let has_pathspec_separator = args.iter().any(|arg| arg == "--");
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            if positional_only {
                self.setup.pathspecs.push(arg.clone());
                continue;
            }
            if end_of_options {
                self.add_positional(arg, has_pathspec_separator)?;
                continue;
            }
            match arg.as_str() {
                "--" => positional_only = true,
                "--end-of-options" => end_of_options = true,
                "--not" => self.not = !self.not,
                "--default" => {
                    self.default_revision = Some(
                        iter.next()
                            .ok_or_else(|| GitError::Command("--default requires a value".into()))?
                            .clone(),
                    );
                }
                "--full-history" => self.setup.options.full_history = true,
                "--sparse" => self.setup.options.sparse = true,
                "--dense" => self.setup.options.sparse = false,
                "--remove-empty" => self.setup.options.remove_empty = true,
                "--simplify-merges" => {
                    self.setup.options.simplify_merges = true;
                    // git: --simplify-merges implies topo-order + parent rewriting.
                    self.setup.options.order = RevisionOrder::Topo;
                }
                "--show-pulls" => self.setup.options.show_pulls = true,
                "--ancestry-path" => self.setup.options.ancestry_path = true,
                "--reverse" => self.setup.options.reverse = true,
                "--first-parent" => self.setup.options.first_parent = true,
                "--no-first-parent" => self.setup.options.first_parent = false,
                "--exclude-first-parent-only" => {
                    self.setup.options.exclude_first_parent_only = true
                }
                "--no-exclude-first-parent-only" => {
                    self.setup.options.exclude_first_parent_only = false
                }
                "--topo-order" => self.setup.options.order = RevisionOrder::Topo,
                "--date-order" => self.setup.options.order = RevisionOrder::Date,
                "--author-date-order" => self.setup.options.order = RevisionOrder::AuthorDate,
                "--no-walk" | "--no-walk=sorted" => {
                    self.setup.options.no_walk = NoWalkMode::Sorted;
                }
                "--no-walk=unsorted" => self.setup.options.no_walk = NoWalkMode::Unsorted,
                "--do-walk" => self.setup.options.no_walk = NoWalkMode::Walk,
                "--ignore-missing" => self.setup.options.ignore_missing = true,
                "--no-ignore-missing" => self.setup.options.ignore_missing = false,
                "-n" | "--max-count" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?;
                    self.setup.options.max_count = Some(parse_max_count(value)?);
                }
                value if value.starts_with("--max-count=") => {
                    self.setup.options.max_count =
                        Some(parse_max_count(&value["--max-count=".len()..])?);
                }
                "--skip" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| GitError::Command("--skip requires a value".into()))?;
                    self.setup.options.skip = parse_skip(value)?;
                }
                value if value.starts_with("--skip=") => {
                    self.setup.options.skip = parse_skip(&value["--skip=".len()..])?;
                }
                "--max-age" => {
                    let value = iter.next().ok_or_else(max_age_requires_value_error)?;
                    self.setup.options.date_window.min_time = Some(parse_timestamp(value)?);
                }
                value if value.starts_with("--max-age=") => {
                    self.setup.options.date_window.min_time =
                        Some(parse_timestamp(&value["--max-age=".len()..])?);
                }
                "--min-age" => {
                    let value = iter.next().ok_or_else(min_age_requires_value_error)?;
                    self.setup.options.date_window.max_time = Some(parse_timestamp(value)?);
                }
                value if value.starts_with("--min-age=") => {
                    self.setup.options.date_window.max_time =
                        Some(parse_timestamp(&value["--min-age=".len()..])?);
                }
                "--since" | "--after" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| date_cutoff_requires_value_error(arg))?;
                    self.setup.options.date_window.min_time = Some(parse_date_cutoff(value)?);
                }
                value if value.starts_with("--since=") => {
                    self.setup.options.date_window.min_time =
                        Some(parse_date_cutoff(&value["--since=".len()..])?);
                }
                value if value.starts_with("--after=") => {
                    self.setup.options.date_window.min_time =
                        Some(parse_date_cutoff(&value["--after=".len()..])?);
                }
                "--until" | "--before" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| date_cutoff_requires_value_error(arg))?;
                    self.setup.options.date_window.max_time = Some(parse_date_cutoff(value)?);
                }
                value if value.starts_with("--until=") => {
                    self.setup.options.date_window.max_time =
                        Some(parse_date_cutoff(&value["--until=".len()..])?);
                }
                value if value.starts_with("--before=") => {
                    self.setup.options.date_window.max_time =
                        Some(parse_date_cutoff(&value["--before=".len()..])?);
                }
                "--author" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| GitError::Command("--author requires a value".into()))?;
                    self.setup.options.author_patterns.push(value.clone());
                }
                value if value.starts_with("--author=") => {
                    self.setup
                        .options
                        .author_patterns
                        .push(value["--author=".len()..].to_string());
                }
                value if value.starts_with("-n") && value.len() > 2 => {
                    self.setup.options.max_count = Some(parse_max_count(&value[2..])?);
                }
                value
                    if value.starts_with('-')
                        && value
                            .as_bytes()
                            .get(1)
                            .is_some_and(|byte| byte.is_ascii_digit()) =>
                {
                    self.setup.options.max_count = Some(parse_max_count(&value[1..])?);
                }
                "--all" => self.add_ref_selector(RefSelectorKind::All, None, true)?,
                "--no-all" => self
                    .ref_selectors
                    .retain(|selector| selector.kind != RefSelectorKind::All),
                "--reflog" => self.reflog = true,
                "--no-reflog" => self.reflog = false,
                "--glob" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| GitError::Command("--glob requires a value".into()))?;
                    self.add_ref_selector(RefSelectorKind::Glob, Some(value.clone()), false)?;
                }
                value if value.starts_with("--glob=") => self.add_ref_selector(
                    RefSelectorKind::Glob,
                    Some(value["--glob=".len()..].to_string()),
                    false,
                )?,
                "--branches" => self.add_ref_selector(RefSelectorKind::Branches, None, true)?,
                value if value.starts_with("--branches=") => self.add_ref_selector(
                    RefSelectorKind::Branches,
                    Some(value["--branches=".len()..].to_string()),
                    false,
                )?,
                "--tags" => self.add_ref_selector(RefSelectorKind::Tags, None, true)?,
                value if value.starts_with("--tags=") => self.add_ref_selector(
                    RefSelectorKind::Tags,
                    Some(value["--tags=".len()..].to_string()),
                    false,
                )?,
                "--remotes" => self.add_ref_selector(RefSelectorKind::Remotes, None, true)?,
                value if value.starts_with("--remotes=") => self.add_ref_selector(
                    RefSelectorKind::Remotes,
                    Some(value["--remotes=".len()..].to_string()),
                    false,
                )?,
                "--exclude" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| GitError::Command("--exclude requires a value".into()))?;
                    self.pending_excludes.push(value.clone());
                }
                value if value.starts_with("--exclude=") => {
                    self.pending_excludes
                        .push(value["--exclude=".len()..].to_string());
                }
                "--exclude-hidden" => {
                    let value = iter.next().ok_or_else(|| {
                        GitError::Command("--exclude-hidden requires a value".into())
                    })?;
                    self.set_pending_hidden(parse_exclude_hidden(value)?)?;
                }
                value if value.starts_with("--exclude-hidden=") => {
                    self.set_pending_hidden(parse_exclude_hidden(
                        &value["--exclude-hidden=".len()..],
                    )?)?;
                }
                value if value.starts_with('-') => self.setup.leftovers.push(value.to_string()),
                value => self.add_positional(value, has_pathspec_separator)?,
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<SetupRevisions> {
        if !self.setup.options.has_revisions()
            && self.ref_selectors.is_empty()
            && !self.reflog
            && let Some(default_revision) = self.default_revision.take()
        {
            let saved_not = self.not;
            self.not = false;
            self.add_revision_arg(&default_revision)?;
            self.not = saved_not;
        }
        self.expand_ref_selectors()?;
        if self.reflog {
            self.expand_reflog()?;
        }
        Ok(self.setup)
    }

    /// `--reflog`: add every reflog entry (old + new oid) across HEAD and all
    /// refs as a positive tip, mirroring git's `add_reflogs_to_pending` /
    /// `add_one_commit`. Entries are read oldest-first per reflog; the first
    /// occurrence of each commit oid wins (later duplicates are dropped), and
    /// the zero oid and any oid that is not an existing commit are skipped.
    fn expand_reflog(&mut self) -> Result<()> {
        self.setup.options.had_ref_selector = true;
        let mut seen: HashSet<ObjectId> = HashSet::new();
        for (selector, oid) in reflog_entry_oids(self.ctx.git_dir, self.ctx.format) {
            if !seen.insert(oid) {
                continue;
            }
            // Only real commits become tips; reflogs can name the zero oid
            // (already filtered) or, after pruning, a missing object.
            if peel_to_commit(self.ctx.reader, self.ctx.format, &oid).is_err() {
                continue;
            }
            self.setup.options.positives.push(RevisionTip {
                oid,
                rev: selector.clone(),
                source_name: Some(selector),
                from_ref_selector: true,
            });
        }
        Ok(())
    }

    fn add_ref_selector(
        &mut self,
        kind: RefSelectorKind,
        pattern: Option<String>,
        include_all: bool,
    ) -> Result<()> {
        if self.pending_hidden.is_some()
            && matches!(
                kind,
                RefSelectorKind::Branches | RefSelectorKind::Tags | RefSelectorKind::Remotes
            )
        {
            eprintln!(
                "error: options '--exclude-hidden' and '{}' cannot be used together",
                selector_option_name(kind)
            );
            return Err(GitError::Exit(129));
        }
        self.ref_selectors.push(RefSelector {
            kind,
            not: self.not,
            pattern,
            include_all,
            excludes: std::mem::take(&mut self.pending_excludes),
            hidden: self.pending_hidden.take(),
        });
        Ok(())
    }

    fn set_pending_hidden(&mut self, section: HiddenRefsSection) -> Result<()> {
        // git dies on a second --exclude-hidden before a consuming pseudo-ref.
        if self.pending_hidden.is_some() {
            eprintln!("fatal: --exclude-hidden= passed more than once");
            return Err(GitError::Exit(128));
        }
        self.pending_hidden = Some(section);
        Ok(())
    }

    fn add_positional(&mut self, value: &str, revision_only: bool) -> Result<()> {
        match self.add_revision_arg(value) {
            Ok(()) => {
                // Without `--`, an argument that resolves both as a revision
                // and as an existing worktree path is ambiguous. Pathspec magic
                // is stripped before probing (`:/a` means `<worktree>/a`).
                if !revision_only && positional_matches_worktree_path(self.ctx, value)? {
                    return ambiguous_argument(value);
                }
                Ok(())
            }
            Err(_) if self.setup.options.ignore_missing => Ok(()),
            Err(_) if revision_only => ambiguous_argument(value),
            Err(_) if positional_matches_worktree_path(self.ctx, value)? => {
                self.setup.pathspecs.push(value.to_string());
                Ok(())
            }
            Err(_) if looks_like_nonmagic_pathspec(value) => {
                self.setup.pathspecs.push(value.to_string());
                Ok(())
            }
            Err(_) => ambiguous_argument(value),
        }
    }

    fn add_revision_arg(&mut self, value: &str) -> Result<()> {
        if let Some(exclude) = value.strip_prefix('^')
            && !exclude.is_empty()
        {
            self.add_revision_token(exclude, !self.not)?;
            return Ok(());
        }
        if let Some(base) = value.strip_suffix("^@") {
            self.add_parents(base, self.not)?;
            return Ok(());
        }
        if let Some(base) = value.strip_suffix("^!") {
            self.add_revision_token(base, self.not)?;
            self.add_parents(base, !self.not)?;
            return Ok(());
        }
        if let Some(range) = parse_revision_range(value) {
            self.add_range(range, self.not)?;
            return Ok(());
        }
        self.add_revision_token(value, self.not)
    }

    fn add_revision_token(&mut self, rev: &str, negated: bool) -> Result<()> {
        // The bare `:/text` spelling has repository-wide plumbing semantics in
        // the general resolver, but revision setup scopes it to this worktree's
        // current HEAD. In particular it must not find an orphaned commit, a
        // linked worktree's detached HEAD, or a matching commit on another ref.
        let scoped;
        let rev = if let Some(text) = rev.strip_prefix(":/") {
            scoped = format!("HEAD^{{/{text}}}");
            scoped.as_str()
        } else {
            rev
        };
        let oid = resolve_revision_commitish_with_config_optional(
            self.ctx.git_dir,
            self.ctx.format,
            self.ctx.reader,
            rev,
            self.ctx.config,
        )?;
        if negated {
            self.setup.options.negatives.push(peel_to_commit(
                self.ctx.reader,
                self.ctx.format,
                &oid,
            )?);
        } else {
            self.setup.options.positives.push(RevisionTip {
                oid,
                rev: rev.to_string(),
                source_name: Some(rev.to_string()),
                from_ref_selector: false,
            });
        }
        Ok(())
    }

    fn add_parents(&mut self, rev: &str, negated: bool) -> Result<()> {
        let oid = resolve_revision_with_config_optional(
            self.ctx.git_dir,
            self.ctx.format,
            self.ctx.reader,
            rev,
            self.ctx.config,
        )?;
        let commit = read_commit(
            self.ctx.reader,
            self.ctx.format,
            &peel_to_commit(self.ctx.reader, self.ctx.format, &oid)?,
        )?;
        for parent in commit.parents {
            if negated {
                self.setup.options.negatives.push(parent);
            } else {
                self.setup.options.positives.push(RevisionTip {
                    oid: parent,
                    rev: format!("{rev}^@"),
                    source_name: Some(format!("{rev}^@")),
                    from_ref_selector: false,
                });
            }
        }
        Ok(())
    }

    fn add_range(&mut self, range: RevisionRange, negated: bool) -> Result<()> {
        match range {
            RevisionRange::Asymmetric { start, end } => {
                let start_oid = self.resolve_commit(&start)?;
                let end_oid = self.resolve_commit(&end)?;
                if negated {
                    self.setup.options.positives.push(RevisionTip {
                        oid: start_oid,
                        rev: start.clone(),
                        source_name: Some(start),
                        from_ref_selector: false,
                    });
                    self.setup.options.negatives.push(end_oid);
                } else {
                    self.setup.options.negatives.push(start_oid);
                    self.setup.options.positives.push(RevisionTip {
                        oid: end_oid,
                        rev: end.clone(),
                        source_name: Some(end),
                        from_ref_selector: false,
                    });
                }
            }
            RevisionRange::Symmetric { left, right } => {
                let left_oid = self.resolve_commit(&left)?;
                let right_oid = self.resolve_commit(&right)?;
                let bases = merge_bases(
                    self.ctx.git_dir,
                    self.ctx.format,
                    self.ctx.reader,
                    &left_oid,
                    &right_oid,
                )?;
                if negated {
                    self.setup.options.negatives.push(left_oid);
                    self.setup.options.negatives.push(right_oid);
                    self.setup
                        .options
                        .positives
                        .extend(bases.iter().map(|oid| RevisionTip {
                            oid: *oid,
                            rev: "merge-base".to_string(),
                            source_name: None,
                            from_ref_selector: false,
                        }));
                } else {
                    self.setup.options.positives.push(RevisionTip {
                        oid: left_oid,
                        rev: left.clone(),
                        source_name: Some(left),
                        from_ref_selector: false,
                    });
                    self.setup.options.positives.push(RevisionTip {
                        oid: right_oid,
                        rev: right.clone(),
                        source_name: Some(right),
                        from_ref_selector: false,
                    });
                    self.setup.options.negatives.extend(bases);
                }
                self.setup
                    .options
                    .symmetric_ranges
                    .push(RevisionSymmetricRange {
                        left: left_oid,
                        right: right_oid,
                        negated,
                    });
            }
        }
        Ok(())
    }

    fn resolve_commit(&self, rev: &str) -> Result<ObjectId> {
        let oid = resolve_revision_commitish_with_config_optional(
            self.ctx.git_dir,
            self.ctx.format,
            self.ctx.reader,
            rev,
            self.ctx.config,
        )?;
        peel_to_commit(self.ctx.reader, self.ctx.format, &oid)
    }

    fn expand_ref_selectors(&mut self) -> Result<()> {
        if self.ref_selectors.is_empty() {
            return Ok(());
        }
        self.setup.options.had_ref_selector = true;
        let hidden_refs = HiddenRefs::from_config(self.ctx.config);
        let store = FileRefStore::new(self.ctx.git_dir, self.ctx.format);
        for reference in store.list_refs()? {
            let (include_ref, exclude_ref) =
                ref_selection(&reference.name, &self.ref_selectors, &hidden_refs);
            if !include_ref && !exclude_ref {
                continue;
            }
            let RefTarget::Direct(oid) = reference.target else {
                continue;
            };
            if include_ref {
                self.setup.options.positives.push(RevisionTip {
                    oid,
                    rev: reference.name.clone(),
                    source_name: Some(reference.name.clone()),
                    from_ref_selector: true,
                });
            }
            if exclude_ref
                && let Ok(commit) = peel_to_commit(self.ctx.reader, self.ctx.format, &oid)
            {
                self.setup.options.negatives.push(commit);
            }
        }
        // `--all` is wider than `refs/*`: revision.c enumerates the main ref
        // namespace and then calls `refs_head_ref`, so a detached HEAD remains
        // a pending tip even though no branch names it. Run HEAD through the
        // same selector/exclusion state as ordinary refs; duplicate symbolic
        // HEADs are harmless because the walk deduplicates object ids.
        let (include_head, exclude_head) = ref_selection("HEAD", &self.ref_selectors, &hidden_refs);
        if (include_head || exclude_head)
            && let Some(oid) = sley_refs::resolve_ref_peeled(&store, "HEAD")?
        {
            if include_head {
                self.setup.options.positives.push(RevisionTip {
                    oid,
                    rev: "HEAD".to_string(),
                    source_name: Some("HEAD".to_string()),
                    from_ref_selector: true,
                });
            }
            if exclude_head
                && let Ok(commit) = peel_to_commit(self.ctx.reader, self.ctx.format, &oid)
            {
                self.setup.options.negatives.push(commit);
            }
        }
        Ok(())
    }
}

/// Enumerate `(selector, oid)` for every reflog entry across HEAD and all refs,
/// oldest entry first, old oid then new oid — git's `add_reflogs_to_pending`
/// traversal. HEAD's reflog is read first, then per-ref reflogs in sorted path
/// order so the result is independent of directory-iteration order (and so
/// `rev-list --reflog` and `log --reflog`, two separate processes, agree).
fn reflog_entry_oids(git_dir: &Path, format: ObjectFormat) -> Vec<(String, ObjectId)> {
    let logs_dir = git_dir.join("logs");
    let zero = "0".repeat(format.hex_len());
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let head_log = logs_dir.join("HEAD");
    if head_log.is_file() {
        files.push(("HEAD".to_string(), head_log));
    }
    let mut ref_files: Vec<(String, PathBuf)> = Vec::new();
    collect_reflog_files(&logs_dir.join("refs"), &logs_dir, &mut ref_files);
    ref_files.sort_by(|a, b| a.0.cmp(&b.0));
    files.extend(ref_files);

    let mut out = Vec::new();
    for (name, path) in files {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            // `<old> <new> <ident> <ts> <tz>\t<message>`: the first two
            // space-separated tokens are the old and new oids.
            let mut fields = line.split(' ');
            for hex in [fields.next(), fields.next()].into_iter().flatten() {
                if hex != zero
                    && let Ok(oid) = ObjectId::from_hex(format, hex)
                {
                    out.push((name.clone(), oid));
                }
            }
        }
    }
    out
}

/// Recursively collect reflog files under `dir`, naming each by its path
/// relative to `logs_root` (so `logs/refs/heads/main` → `refs/heads/main`).
fn collect_reflog_files(dir: &Path, logs_root: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_reflog_files(&path, logs_root, out);
        } else if let Ok(rel) = path.strip_prefix(logs_root) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path));
        }
    }
}

fn resolve_revision_with_config_optional<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<ObjectId> {
    match config {
        Some(config) => resolve_revision_with_config(git_dir, format, reader, rev, config),
        None => resolve_revision_with_reader(git_dir, format, reader, rev),
    }
}

fn resolve_revision_commitish_with_config_optional<R: ObjectReader>(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &R,
    rev: &str,
    config: Option<&GitConfig>,
) -> Result<ObjectId> {
    // A ref must win over a same-spelled short hex prefix (e.g. `cherry-pick
    // added`, where `added` is both a ref and a valid hex prefix). Route through
    // the ref-first resolver; the commit-ish disambiguation only narrows a bare
    // prefix once ref lookup misses.
    match config {
        Some(config) => {
            super::resolve_revision_commitish_with_config(git_dir, format, reader, rev, config)
        }
        None => super::resolve_revision_commitish_with_reader(git_dir, format, reader, rev),
    }
}

fn read_commit<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Commit> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Commit::parse(format, &object.body)
}

pub fn ambiguous_argument_message(value: &str) -> String {
    format!(
        "fatal: ambiguous argument '{value}': unknown revision or path not in the working tree.\n\
Use '--' to separate paths from revisions, like this:\n\
'git <command> [<revision>...] -- [<file>...]'"
    )
}

pub fn ambiguous_argument_error(value: &str) -> GitError {
    eprintln!("{}", ambiguous_argument_message(value));
    GitError::Exit(128)
}

fn ambiguous_argument(value: &str) -> Result<()> {
    Err(ambiguous_argument_error(value))
}

/// A non-magic positional with wildcard syntax remains a pathspec even when no
/// current worktree path matches it. Magic pathspecs are parsed and validated
/// separately so an unknown magic word cannot silently become an empty filter.
fn looks_like_nonmagic_pathspec(arg: &str) -> bool {
    let mut bytes = arg.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                bytes.next();
            }
            b'*' | b'?' | b'[' => return true,
            _ => {}
        }
    }
    false
}

fn positional_matches_worktree_path<R>(
    ctx: &RevisionSetupContext<'_, R>,
    value: &str,
) -> Result<bool> {
    let carries_magic = value.starts_with(":(")
        || value
            .strip_prefix(':')
            .is_some_and(|rest| matches!(rest.as_bytes().first(), Some(b'/' | b'!' | b'^')));
    if !carries_magic {
        return Ok(path_exists(ctx, value));
    }

    let element = sley_pathspec::PathspecElement::parse(
        value.as_bytes(),
        sley_pathspec::PathspecMatchMagic::default(),
    )
    .map_err(|error| {
        eprintln!("fatal: {error} in '{value}'");
        GitError::Exit(128)
    })?;
    // The empty top-level pathspec `:/` denotes the whole tree. It is not a
    // competing filename for revision search's empty-pattern spelling.
    if element.pattern().is_empty() {
        return Ok(false);
    }
    let Ok(pattern) = std::str::from_utf8(element.pattern()) else {
        return Ok(false);
    };
    let base = if element.is_top() {
        ctx.worktree_root.unwrap_or(ctx.cwd)
    } else {
        ctx.cwd
    };
    Ok(base.join(pattern).exists())
}

fn path_exists<R>(ctx: &RevisionSetupContext<'_, R>, value: &str) -> bool {
    // git's `verify_non_filename` / `check_filename` only probe the work tree.
    // In a bare repository there is no work tree, so files that happen to live
    // in the git directory itself (`HEAD`, `FETCH_HEAD`, `config`, …) must not
    // make a successfully resolved revision look "ambiguous" (t5900: `git log
    // FETCH_HEAD` from a bare fetch destination).
    let Some(worktree_root) = ctx.worktree_root else {
        return false;
    };
    let path = PathBuf::from(value);
    let candidate = if path.is_absolute() {
        path
    } else {
        // Relative names are resolved from the invocation cwd (which may be a
        // subdirectory of the worktree), matching git's prefix-aware probe.
        ctx.cwd.join(path)
    };
    if candidate.exists() {
        return true;
    }
    // Also accept a worktree-rooted spelling when cwd is elsewhere (absolute
    // pathspecs and top-level names from outside the tree).
    worktree_root.join(value).exists()
}

fn parse_max_count(value: &str) -> Result<usize> {
    match value.parse::<isize>() {
        Ok(value) if value < 0 => Ok(usize::MAX),
        Ok(value) => Ok(value as usize),
        Err(_) => Err(GitError::Command(format!("{value} is not an integer"))),
    }
}

fn parse_skip(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("{value} is not an integer")))
}

fn parse_timestamp(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("fatal: '{value}': not a number of seconds since epoch");
        GitError::Exit(128)
    })
}

fn max_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--max-age' requires a value");
    GitError::Exit(128)
}

fn min_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--min-age' requires a value");
    GitError::Exit(128)
}

fn date_cutoff_requires_value_error(option: &str) -> GitError {
    eprintln!("fatal: Option '{option}' requires a value");
    GitError::Exit(128)
}

fn parse_date_cutoff(value: &str) -> Result<i64> {
    let mut parts = value.split_whitespace();
    let Some(first) = parts.next() else {
        return invalid_date_format(value);
    };
    if let Some(timestamp) = first.strip_prefix('@') {
        let Some(timezone) = parts.next() else {
            return invalid_date_format(value);
        };
        if parts.next().is_some() || parse_timezone_offset_seconds(timezone).is_none() {
            return invalid_date_format(value);
        }
        return timestamp.parse::<i64>().map_err(|_| {
            eprintln!("fatal: invalid date format: {value}");
            GitError::Exit(128)
        });
    }
    // A bare all-digit number with at least 9 digits is seconds-since-epoch
    // (date.c match_digit: `num >= 100000000 && nodate(tm)` → gmtime). Fewer
    // digits stay ambiguous with YYYYMMDD/HHMMSS and fall through to the
    // date+time parse below.
    if value.split_whitespace().nth(1).is_none()
        && first.len() >= 9
        && first.bytes().all(|b| b.is_ascii_digit())
        && let Ok(timestamp) = first.parse::<i64>()
    {
        return Ok(timestamp);
    }
    let (date, time, embedded_tz) = if let Some((date, rest)) = first.split_once('T') {
        let (time, tz) = split_embedded_timezone(rest);
        (date, time, tz)
    } else if let Some(time) = parts.next() {
        (first, time, None)
    } else if parse_date_ymd(first).is_some() {
        // A bare `YYYY-MM-DD` with no time of day: git's approxidate fills the
        // time from "now", but for the since/until window boundaries this feeds
        // (callers only compare against it), midnight UTC is precise enough.
        (first, "00:00:00", Some("+0000".to_string()))
    } else {
        return invalid_date_format(value);
    };
    let timezone = match embedded_tz {
        Some(tz) => tz,
        None => match parts.next() {
            Some(tz) => tz.to_string(),
            None => return invalid_date_format(value),
        },
    };
    if parts.next().is_some() {
        return invalid_date_format(value);
    }
    let Some((year, month, day)) = parse_date_ymd(date) else {
        return invalid_date_format(value);
    };
    let Some((hour, minute, second)) = parse_time_hms(time) else {
        return invalid_date_format(value);
    };
    let Some(timezone_offset) = parse_timezone_offset_seconds(&timezone) else {
        return invalid_date_format(value);
    };
    let days = days_from_civil(year, month, day);
    Ok(days * 86_400 + i64::from(hour * 3_600 + minute * 60 + second) - timezone_offset)
}

fn split_embedded_timezone(rest: &str) -> (&str, Option<String>) {
    if let Some(time) = rest.strip_suffix('Z') {
        return (time, Some("+0000".to_string()));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 5 {
        let tz_start = bytes.len() - 5;
        if matches!(bytes[tz_start], b'+' | b'-')
            && bytes[tz_start + 1..]
                .iter()
                .all(|byte| byte.is_ascii_digit())
        {
            return (&rest[..tz_start], Some(rest[tz_start..].to_string()));
        }
    }
    (rest, None)
}

fn parse_date_ymd(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

fn parse_time_hms(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

fn parse_timezone_offset_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes.first(), Some(b'+' | b'-'))
        || !bytes[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hours = value[1..3].parse::<i64>().ok()?;
    let minutes = value[3..5].parse::<i64>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = hours * 3_600 + minutes * 60;
    if bytes[0] == b'-' {
        Some(-offset)
    } else {
        Some(offset)
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn invalid_date_format<T>(value: &str) -> Result<T> {
    eprintln!("fatal: invalid date format: {value}");
    Err(GitError::Exit(128))
}

fn parse_exclude_hidden(value: &str) -> Result<HiddenRefsSection> {
    match value {
        "fetch" => Ok(HiddenRefsSection::Fetch),
        "receive" => Ok(HiddenRefsSection::Receive),
        "uploadpack" => Ok(HiddenRefsSection::Uploadpack),
        _ => Err(GitError::Command(format!(
            "unsupported section for hidden refs: {value}"
        ))),
    }
}

/// A single ref matched by a pseudo-ref option (`--glob`, `--branches=`, …).
///
/// `rev-parse` prints the OIDs of these in the order the selectors are seen on
/// the command line (sorted by refname within each selector); this mirrors
/// git's `add_pending_object` flow through `handle_revision_pseudo_opt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRef {
    pub name: String,
    pub oid: ObjectId,
}

/// Stateful resolver for `rev-parse`'s pseudo-ref options.
///
/// git processes `--exclude`/`--exclude-hidden` as *pending* exclusion state
/// that the next `--all`/`--branches`/`--tags`/`--remotes`/`--glob` consumes,
/// then clears (`clear_ref_exclusions`). This struct replays that protocol so
/// `rev-parse` and `rev-list` share one implementation of the matching rules.
pub struct PseudoRefResolver {
    hidden_refs: HiddenRefs,
    refs: Vec<sley_refs::Ref>,
    pending_excludes: Vec<String>,
    pending_hidden: Option<HiddenRefsSection>,
}

impl PseudoRefResolver {
    pub fn new(git_dir: &Path, format: ObjectFormat, config: Option<&GitConfig>) -> Result<Self> {
        let store = FileRefStore::new(git_dir, format);
        Ok(Self {
            hidden_refs: HiddenRefs::from_config(config),
            refs: store.list_refs()?,
            pending_excludes: Vec::new(),
            pending_hidden: None,
        })
    }

    /// Whether `arg` is a pseudo-ref option this resolver understands. Used by
    /// `rev-parse` to route the argument here instead of treating it as a rev.
    pub fn is_pseudo_ref_arg(arg: &str) -> bool {
        matches!(arg, "--all" | "--branches" | "--tags" | "--remotes")
            || arg.starts_with("--glob=")
            || arg.starts_with("--branches=")
            || arg.starts_with("--tags=")
            || arg.starts_with("--remotes=")
            || arg.starts_with("--exclude=")
            || arg.starts_with("--exclude-hidden=")
    }

    /// Feed one pseudo-ref argument. `--exclude`/`--exclude-hidden` only update
    /// pending state and return `Ok(None)`; selector options return the matched
    /// refs (sorted by name) and clear the pending exclusions.
    pub fn feed(&mut self, arg: &str) -> Result<Option<Vec<MatchedRef>>> {
        if let Some(pattern) = arg.strip_prefix("--exclude=") {
            self.pending_excludes.push(pattern.to_string());
            return Ok(None);
        }
        if let Some(section) = arg.strip_prefix("--exclude-hidden=") {
            // git: exclude_hidden_refs() dies if called twice without an
            // intervening pseudo-ref option consuming the pending state.
            if self.pending_hidden.is_some() {
                eprintln!("fatal: --exclude-hidden= passed more than once");
                return Err(GitError::Exit(128));
            }
            self.pending_hidden = Some(parse_exclude_hidden(section)?);
            return Ok(None);
        }
        let (kind, pattern, include_all) = if arg == "--all" {
            (RefSelectorKind::All, None, true)
        } else if arg == "--branches" {
            (RefSelectorKind::Branches, None, true)
        } else if arg == "--tags" {
            (RefSelectorKind::Tags, None, true)
        } else if arg == "--remotes" {
            (RefSelectorKind::Remotes, None, true)
        } else if let Some(pat) = arg.strip_prefix("--glob=") {
            (RefSelectorKind::Glob, Some(pat.to_string()), false)
        } else if let Some(pat) = arg.strip_prefix("--branches=") {
            (RefSelectorKind::Branches, Some(pat.to_string()), false)
        } else if let Some(pat) = arg.strip_prefix("--tags=") {
            (RefSelectorKind::Tags, Some(pat.to_string()), false)
        } else if let Some(pat) = arg.strip_prefix("--remotes=") {
            (RefSelectorKind::Remotes, Some(pat.to_string()), false)
        } else {
            return Err(GitError::Command(format!(
                "unsupported pseudo-ref option {arg}"
            )));
        };
        // git rejects `--exclude-hidden` paired with --branches/--tags/--remotes
        // (any namespaced selector); --all and --glob are allowed.
        if self.pending_hidden.is_some()
            && matches!(
                kind,
                RefSelectorKind::Branches | RefSelectorKind::Tags | RefSelectorKind::Remotes
            )
        {
            eprintln!(
                "error: options '--exclude-hidden' and '{}' cannot be used together",
                selector_option_name(kind)
            );
            return Err(GitError::Exit(129));
        }
        let selector = RefSelector {
            kind,
            not: false,
            pattern,
            include_all,
            excludes: std::mem::take(&mut self.pending_excludes),
            hidden: self.pending_hidden.take(),
        };
        let selectors = [selector];
        let mut matched = Vec::new();
        for reference in &self.refs {
            let (include_ref, _exclude_ref) =
                ref_selection(&reference.name, &selectors, &self.hidden_refs);
            if !include_ref {
                continue;
            }
            if let RefTarget::Direct(oid) = reference.target {
                matched.push(MatchedRef {
                    name: reference.name.clone(),
                    oid,
                });
            }
        }
        Ok(Some(matched))
    }

    /// `--exclude-hidden` requires a following pseudo-ref option per git
    /// (`die("--exclude-hidden ... must be used with another ref option")`);
    /// callers check this after the argument loop.
    pub fn has_dangling_exclude_hidden(&self) -> bool {
        self.pending_hidden.is_some()
    }
}

fn selector_option_name(kind: RefSelectorKind) -> &'static str {
    match kind {
        RefSelectorKind::All => "--all",
        RefSelectorKind::Glob => "--glob",
        RefSelectorKind::Branches => "--branches",
        RefSelectorKind::Tags => "--tags",
        RefSelectorKind::Remotes => "--remotes",
    }
}

fn config_section_values(
    config: &GitConfig,
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Vec<String> {
    config
        .sections
        .iter()
        .filter(|candidate| {
            candidate.name.eq_ignore_ascii_case(section)
                && candidate.subsection.as_deref() == subsection
        })
        .flat_map(|candidate| candidate.entries.iter())
        .filter(|entry| entry.key.eq_ignore_ascii_case(key))
        .filter_map(|entry| entry.value.clone())
        .collect()
}

fn ref_selection(
    refname: &str,
    selectors: &[RefSelector],
    hidden_refs: &HiddenRefs,
) -> (bool, bool) {
    let mut include = false;
    let mut exclude = false;
    for selector in selectors {
        let selected = match selector.kind {
            RefSelectorKind::All => {
                !ref_excluded(refname, &selector.excludes, None)
                    && !ref_hidden(refname, selector.hidden, hidden_refs)
            }
            RefSelectorKind::Glob => {
                selector
                    .pattern
                    .as_deref()
                    .is_some_and(|pattern| glob_ref_selector_matches(pattern, refname))
                    && !ref_excluded(refname, &selector.excludes, None)
                    && !ref_hidden(refname, selector.hidden, hidden_refs)
            }
            RefSelectorKind::Branches => {
                ref_selector_matches(
                    refname,
                    "refs/heads/",
                    selector.include_all,
                    selector.pattern.as_deref(),
                ) && !ref_excluded(refname, &selector.excludes, Some("refs/heads/"))
                    && !ref_hidden(refname, selector.hidden, hidden_refs)
            }
            RefSelectorKind::Tags => {
                ref_selector_matches(
                    refname,
                    "refs/tags/",
                    selector.include_all,
                    selector.pattern.as_deref(),
                ) && !ref_excluded(refname, &selector.excludes, Some("refs/tags/"))
                    && !ref_hidden(refname, selector.hidden, hidden_refs)
            }
            RefSelectorKind::Remotes => {
                ref_selector_matches(
                    refname,
                    "refs/remotes/",
                    selector.include_all,
                    selector.pattern.as_deref(),
                ) && !ref_excluded(refname, &selector.excludes, Some("refs/remotes/"))
                    && !ref_hidden(refname, selector.hidden, hidden_refs)
            }
        };
        if selected {
            if selector.not {
                exclude = true;
            } else {
                include = true;
            }
        }
    }
    (include, exclude)
}

fn ref_selector_matches(
    name: &str,
    namespace: &str,
    include_all: bool,
    pattern: Option<&str>,
) -> bool {
    if name.strip_prefix(namespace).is_none() {
        return false;
    }
    match pattern {
        None => include_all,
        Some(pattern) => {
            refname_pattern_matches(&namespaced_real_pattern(namespace, pattern), name)
        }
    }
}

/// Build git's `real_pattern` for a namespaced selector (`--branches=<pat>` &c),
/// mirroring `refs_for_each_ref_ext`: `prefix + pattern`, and when the user
/// pattern has no glob specials, complete a trailing `/` (only if absent) and
/// append `*`. The result is wildmatched against the *full* refname.
fn namespaced_real_pattern(prefix: &str, pattern: &str) -> String {
    let mut real = format!("{prefix}{pattern}");
    if !has_glob_magic(pattern) {
        if !real.ends_with('/') {
            real.push('/');
        }
        real.push('*');
    }
    real
}

fn glob_ref_selector_matches(pattern: &str, refname: &str) -> bool {
    // `--glob=<pat>`: prefix is empty, so git prepends `refs/` unless the
    // pattern already names it, then applies the same complete-`/`-and-`*`
    // rule as namespaced selectors.
    let mut real = if pattern.starts_with("refs/") {
        pattern.to_string()
    } else {
        format!("refs/{pattern}")
    };
    if !has_glob_magic(pattern) {
        if !real.ends_with('/') {
            real.push('/');
        }
        real.push('*');
    }
    refname_pattern_matches(&real, refname)
}

fn ref_excluded(refname: &str, patterns: &[String], namespace: Option<&str>) -> bool {
    // git matches `--exclude` against the name handed to `handle_one_ref`, which
    // is the *trimmed* refname for namespaced selectors (`--branches`/`--tags`/
    // `--remotes` set `trim_prefix`) and the *full* refname for `--all`/`--glob`.
    let candidate = match namespace {
        Some(namespace) => match refname.strip_prefix(namespace) {
            Some(trimmed) => trimmed,
            None => return false,
        },
        None => refname,
    };
    patterns
        .iter()
        .any(|pattern| ref_exclude_pattern_matches(pattern, candidate))
}

fn ref_exclude_pattern_matches(pattern: &str, name: &str) -> bool {
    // git: `wildmatch(item->string, name, 0)` — a non-glob pattern is an exact
    // match (no implicit prefix/`*` expansion, unlike the positive selectors).
    refname_pattern_matches(pattern, name)
}

fn ref_hidden(refname: &str, section: Option<HiddenRefsSection>, hidden_refs: &HiddenRefs) -> bool {
    let Some(section) = section else {
        return false;
    };
    let mut hidden = false;
    for pattern in hidden_refs
        .transfer
        .iter()
        .chain(hidden_refs.section_patterns(section))
    {
        if let Some(pattern) = pattern.strip_prefix('!') {
            if hidden_ref_pattern_matches(pattern, refname) {
                hidden = false;
            }
        } else if hidden_ref_pattern_matches(pattern, refname) {
            hidden = true;
        }
    }
    hidden
}

fn hidden_ref_pattern_matches(pattern: &str, refname: &str) -> bool {
    let pattern = pattern.strip_prefix('^').unwrap_or(pattern);
    !pattern.is_empty()
        && (refname == pattern
            || refname.starts_with(&format!("{pattern}/"))
            || has_glob_magic(pattern) && refname_pattern_matches(pattern, refname))
}

fn has_glob_magic(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

fn refname_pattern_matches(pattern: &str, name: &str) -> bool {
    fn matches_from(pattern: &[u8], name: &[u8]) -> bool {
        match pattern {
            [] => name.is_empty(),
            [b'*', rest @ ..] => {
                matches_from(rest, name) || (!name.is_empty() && matches_from(pattern, &name[1..]))
            }
            [b'?', rest @ ..] => !name.is_empty() && matches_from(rest, &name[1..]),
            [b'\\', escaped, rest @ ..] => {
                matches!(name, [first, ..] if first == escaped) && matches_from(rest, &name[1..])
            }
            [b'[', rest @ ..] => {
                if let Some((matched, consumed)) =
                    match_refname_pattern_class(rest, name.first().copied())
                {
                    !name.is_empty() && matched && matches_from(&rest[consumed..], &name[1..])
                } else {
                    matches!(name, [b'[', ..]) && matches_from(rest, &name[1..])
                }
            }
            [literal, rest @ ..] => {
                matches!(name, [first, ..] if first == literal) && matches_from(rest, &name[1..])
            }
        }
    }

    matches_from(pattern.as_bytes(), name.as_bytes())
}

fn match_refname_pattern_class(class: &[u8], name: Option<u8>) -> Option<(bool, usize)> {
    let mut idx = 0;
    let negated = matches!(class.first(), Some(b'!' | b'^'));
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while idx < class.len() {
        if class[idx] == b']' && saw_member {
            return Some((if negated { !matched } else { matched }, idx + 1));
        }

        let start = class[idx];
        if start == b'\\' && idx + 1 < class.len() {
            idx += 1;
        }
        let start = class[idx];
        saw_member = true;

        if idx + 2 < class.len() && class[idx + 1] == b'-' && class[idx + 2] != b']' {
            let mut end_idx = idx + 2;
            if class[end_idx] == b'\\' && end_idx + 1 < class.len() {
                end_idx += 1;
            }
            let end = class[end_idx];
            if let Some(value) = name {
                matched |= start <= value && value <= end;
            }
            idx = end_idx + 1;
        } else {
            matched |= name == Some(start);
            idx += 1;
        }
    }

    None
}
