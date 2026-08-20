//! `git config`: read and write repository configuration.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{sley_config, sley_rev};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigAction {
    Get,
    GetColor,
    GetColorBool,
    GetUrlMatch,
    GetAll,
    GetRegexp,
    List,
    Edit,
    Set,
    /// git's `ACTION_SET_ALL` — the legacy default `git config <key> <value>
    /// <value-pattern>` (3 positionals, no explicit mode). Like `--replace-all`
    /// but WITHOUT the multi-replace flag, so it refuses when several entries
    /// match the pattern.
    SetAll,
    ReplaceAll,
    Add,
    Unset,
    UnsetAll,
    RenameSection,
    RemoveSection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigValueType {
    Raw,
    Bool,
    Int,
    BoolOrInt,
    ExpiryDate,
    Color,
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigSubcommand {
    Get,
    Set,
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    /// No explicit source: reads see git's full default sequence
    /// (system → global → local → worktree → command), writes go to the
    /// repository config. Carries the discovered git dir.
    Repository(PathBuf),
    /// An explicit single-layer source (`--local` / `--global` / `--system` /
    /// `--worktree`): reads and writes use exactly this file, attributed to
    /// the given scope. Includes are not resolved unless `--includes`.
    ScopedFile {
        path: PathBuf,
        scope: sley_config::ConfigScope,
    },
    /// `--file <path>` / `GIT_CONFIG`: scope `command`.
    File(PathBuf),
    /// `--blob <spec>`: scope `command`, read-only.
    Blob(String),
    /// `--file -`: scope `command`, read-only.
    Stdin,
}

/// Scope + origin attribution for one displayed config value, mirroring git's
/// `key_value_info`. Stack entries carry their own; synthesized values
/// (`--default`) are attributed to the command line.
#[derive(Debug, Clone)]
struct ConfigValueMeta {
    scope: sley_config::ConfigScope,
    origin: sley_config::ConfigOrigin,
}

impl ConfigValueMeta {
    fn of(entry: &sley_config::ConfigStackEntry) -> Self {
        Self {
            scope: entry.scope,
            origin: entry.origin.clone(),
        }
    }

    /// git attributes `--default` fallbacks to the command line.
    fn command_line() -> Self {
        Self {
            scope: sley_config::ConfigScope::Command,
            origin: sley_config::ConfigOrigin::command_line(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConfigDisplayOptions {
    show_origin: bool,
    show_scope: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfigEntryWriteOptions {
    display: ConfigDisplayOptions,
    /// Mirror git's `omit_values`: print the key (or nothing) but never the
    /// value.
    name_only: bool,
    /// Mirror git's `show_keys`: prefix the value with its key. The classic
    /// `--list` / `--get-regexp` paths always show keys; the modern
    /// `git config get` subcommand only shows them under `--show-names`.
    show_keys: bool,
    value_type: ConfigValueType,
    null_terminate: bool,
    equals_separator: bool,
}

/// Mutually exclusive `git config` action modes. Git rejects combining these with
/// the exact `error: options 'A' and 'B' cannot be used together` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMode {
    Get,
    GetAll,
    GetRegexp,
    List,
    GetColor,
    GetColorBool,
    GetUrlMatch,
    Edit,
    Set,
    Add,
    ReplaceAll,
    Unset,
    UnsetAll,
    RenameSection,
    RemoveSection,
}

impl ConfigMode {
    fn from_action(action: ConfigAction) -> Self {
        match action {
            ConfigAction::Get => Self::Get,
            ConfigAction::GetAll => Self::GetAll,
            ConfigAction::GetRegexp => Self::GetRegexp,
            ConfigAction::List => Self::List,
            ConfigAction::GetColor => Self::GetColor,
            ConfigAction::GetColorBool => Self::GetColorBool,
            ConfigAction::GetUrlMatch => Self::GetUrlMatch,
            ConfigAction::Edit => Self::Edit,
            ConfigAction::Set | ConfigAction::SetAll => Self::Set,
            ConfigAction::Add => Self::Add,
            ConfigAction::ReplaceAll => Self::ReplaceAll,
            ConfigAction::Unset => Self::Unset,
            ConfigAction::UnsetAll => Self::UnsetAll,
            ConfigAction::RenameSection => Self::RenameSection,
            ConfigAction::RemoveSection => Self::RemoveSection,
        }
    }
}

#[derive(Debug, Default)]
struct ConfigModeTracker {
    chosen: Option<(ConfigMode, &'static str)>,
}

impl ConfigModeTracker {
    fn set_action(&mut self, action: ConfigAction, flag: &'static str) -> Result<()> {
        self.set_mode(ConfigMode::from_action(action), flag)
    }

    fn set_mode(&mut self, mode: ConfigMode, flag: &'static str) -> Result<()> {
        if let Some((existing, existing_flag)) = self.chosen {
            if existing != mode {
                eprintln!("error: options '{flag}' and '{existing_flag}' cannot be used together");
                return Err(GitError::Exit(129));
            }
        } else {
            self.chosen = Some((mode, flag));
        }
        Ok(())
    }

    fn set_action_value(
        &mut self,
        action: &mut Option<ConfigAction>,
        value: ConfigAction,
        flag: &'static str,
    ) -> Result<()> {
        self.set_action(value, flag)?;
        *action = Some(value);
        Ok(())
    }
}

pub(crate) fn cmd_config(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut action = None;
    let mut subcommand = None;
    let mut modes = ConfigModeTracker::default();
    let args = if let Some((first, rest)) = args.split_first() {
        match first.as_str() {
            "list" => {
                action = Some(ConfigAction::List);
                rest
            }
            "get" => {
                action = Some(ConfigAction::Get);
                subcommand = Some(ConfigSubcommand::Get);
                rest
            }
            "set" => {
                action = Some(ConfigAction::Set);
                subcommand = Some(ConfigSubcommand::Set);
                rest
            }
            "unset" => {
                action = Some(ConfigAction::Unset);
                subcommand = Some(ConfigSubcommand::Unset);
                rest
            }
            "rename-section" => {
                action = Some(ConfigAction::RenameSection);
                rest
            }
            "remove-section" => {
                action = Some(ConfigAction::RemoveSection);
                rest
            }
            "edit" => {
                action = Some(ConfigAction::Edit);
                rest
            }
            _ => args,
        }
    } else {
        args
    };
    let mut name_only = false;
    let mut comment = None;
    let mut config_file = None;
    let mut default_value = None;
    let mut display = ConfigDisplayOptions::default();
    let mut fixed_value = false;
    let mut null_terminate = false;
    let mut value_type = ConfigValueType::Raw;
    // git's `to_type`: 0 (Raw) means "no explicit type". Setting a *different*
    // explicit type errors ("only one type at a time"); `--no-type` resets to 0.
    let mut type_set = false;
    let mut positional = Vec::new();
    // Subcommand-mode filter options (git 2.54). `get --value=<pattern>`,
    // `set --all --value=<pattern>`, and `unset --value=<pattern>` all feed the
    // same value-pattern matcher; the classic flag form uses positionals.
    let mut subcommand_get_regexp = false;
    let mut subcommand_show_names = false;
    let mut subcommand_value_pattern: Option<String> = None;
    let mut subcommand_url: Option<String> = None;
    let mut use_local = false;
    let mut use_global = false;
    let mut use_system = false;
    let mut use_worktree = false;
    let mut blob = None;
    // `--includes` / `--no-includes`; `None` = git's default (respect includes
    // only when no explicit config file source was given).
    let mut respect_includes_opt: Option<bool> = None;
    let mut iter = args.iter();
    let mut parse_options = true;
    while let Some(arg) = iter.next() {
        if !parse_options {
            positional.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => parse_options = false,
            "--local" => use_local = true,
            "--global" => use_global = true,
            "--system" => use_system = true,
            "--worktree" => use_worktree = true,
            "--includes" => respect_includes_opt = Some(true),
            "--no-includes" => respect_includes_opt = Some(false),
            "--blob" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--blob requires a value".into()))?;
                blob = Some(value.to_string());
            }
            value if value.starts_with("--blob=") => {
                blob = Some(value["--blob=".len()..].to_string());
            }
            // `git config get --url=<url>`: route through the urlmatch lookup.
            "--url" if subcommand == Some(ConfigSubcommand::Get) => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--url requires a value".into()))?;
                subcommand_url = Some(value.to_string());
            }
            value if subcommand == Some(ConfigSubcommand::Get) && value.starts_with("--url=") => {
                subcommand_url = Some(value["--url=".len()..].to_string());
            }
            "-f" | "--file" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--file requires a value".into()))?;
                config_file = Some(value.to_string());
            }
            value if value.starts_with("--file=") => {
                config_file = Some(value["--file=".len()..].to_string());
            }
            "--get" => modes.set_action_value(&mut action, ConfigAction::Get, "--get")?,
            "--get-color" => {
                modes.set_action_value(&mut action, ConfigAction::GetColor, "--get-color")?;
                value_type = ConfigValueType::Color;
            }
            "--get-colorbool" => {
                modes.set_action_value(
                    &mut action,
                    ConfigAction::GetColorBool,
                    "--get-colorbool",
                )?;
            }
            "--get-urlmatch" => {
                modes.set_action_value(&mut action, ConfigAction::GetUrlMatch, "--get-urlmatch")?;
            }
            "--get-all" => {
                modes.set_action_value(&mut action, ConfigAction::GetAll, "--get-all")?
            }
            "--get-regexp" => {
                modes.set_action_value(&mut action, ConfigAction::GetRegexp, "--get-regexp")?
            }
            "--get-regex" => {
                modes.set_action_value(&mut action, ConfigAction::GetRegexp, "--get-regex")?
            }
            "--list" => modes.set_action_value(&mut action, ConfigAction::List, "--list")?,
            "-l" => modes.set_action_value(&mut action, ConfigAction::List, "--list")?,
            "--edit" => modes.set_action_value(&mut action, ConfigAction::Edit, "--edit")?,
            "--all" if subcommand == Some(ConfigSubcommand::Get) => {
                action = Some(ConfigAction::GetAll);
            }
            "--all" if subcommand == Some(ConfigSubcommand::Set) => {
                action = Some(ConfigAction::ReplaceAll);
            }
            "--all" if subcommand == Some(ConfigSubcommand::Unset) => {
                action = Some(ConfigAction::UnsetAll);
            }
            // `git config get --regexp <name-regex>`: the positional is a key
            // pattern rather than an exact key. Unlike the classic
            // `--get-regexp`, the subcommand only prints key names when
            // `--show-names` is given and only prints all matches with `--all`.
            "--regexp" if subcommand == Some(ConfigSubcommand::Get) => {
                subcommand_get_regexp = true;
            }
            // `--show-names` mirrors git's `show_keys`: prefix each value with
            // its key (only meaningful for the `get` subcommand).
            "--show-names" if subcommand == Some(ConfigSubcommand::Get) => {
                subcommand_show_names = true;
            }
            // `--value=<pattern>` filters matches by their value (a regexp by
            // default, an exact string under `--fixed-value`; a leading `!`
            // negates). Subcommand `get` only.
            "--value"
                if matches!(
                    subcommand,
                    Some(ConfigSubcommand::Get | ConfigSubcommand::Set | ConfigSubcommand::Unset)
                ) =>
            {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--value requires a value".into()))?;
                subcommand_value_pattern = Some(value.to_string());
            }
            value
                if matches!(
                    subcommand,
                    Some(ConfigSubcommand::Get | ConfigSubcommand::Set | ConfigSubcommand::Unset)
                ) && value.starts_with("--value=") =>
            {
                subcommand_value_pattern = Some(value["--value=".len()..].to_string());
            }
            "--name-only" => name_only = true,
            "--show-origin" => display.show_origin = true,
            "--show-scope" => display.show_scope = true,
            "--fixed-value" => fixed_value = true,
            "--no-fixed-value" => fixed_value = false,
            "--comment" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--comment requires a value".into()))?;
                comment = Some(parse_config_comment(value)?);
            }
            value if value.starts_with("--comment=") => {
                comment = Some(parse_config_comment(&value["--comment=".len()..])?);
            }
            "-z" | "--null" => null_terminate = true,
            "--default" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `default' requires a value");
                    return Err(GitError::Exit(129));
                };
                default_value = Some(value.to_string());
            }
            value if value.starts_with("--default=") => {
                default_value = Some(
                    value
                        .strip_prefix("--default=")
                        .ok_or_else(|| GitError::Command("--default requires a value".into()))?
                        .to_string(),
                );
            }
            "--bool" => set_config_type(&mut value_type, &mut type_set, ConfigValueType::Bool)?,
            "--int" => set_config_type(&mut value_type, &mut type_set, ConfigValueType::Int)?,
            "--bool-or-int" => {
                set_config_type(&mut value_type, &mut type_set, ConfigValueType::BoolOrInt)?;
            }
            "--expiry-date" => {
                set_config_type(&mut value_type, &mut type_set, ConfigValueType::ExpiryDate)?;
            }
            "--path" => set_config_type(&mut value_type, &mut type_set, ConfigValueType::Path)?,
            // git's `--no-type` resets to the untyped state.
            "--no-type" => {
                value_type = ConfigValueType::Raw;
                type_set = false;
            }
            "--type" => {
                let kind = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--type requires a value".into()))?;
                let parsed = parse_config_type_arg(kind)?;
                set_config_type(&mut value_type, &mut type_set, parsed)?;
            }
            value if value.starts_with("--type=") => {
                let kind = &value["--type=".len()..];
                let parsed = parse_config_type_arg(kind)?;
                set_config_type(&mut value_type, &mut type_set, parsed)?;
            }
            "--append" if subcommand == Some(ConfigSubcommand::Set) => {
                action = Some(ConfigAction::Add);
            }
            "--add" => modes.set_action_value(&mut action, ConfigAction::Add, "--add")?,
            "--replace-all" => {
                modes.set_action_value(&mut action, ConfigAction::ReplaceAll, "--replace-all")?;
            }
            "--unset" => modes.set_action_value(&mut action, ConfigAction::Unset, "--unset")?,
            "--unset-all" => {
                modes.set_action_value(&mut action, ConfigAction::UnsetAll, "--unset-all")?;
            }
            "--rename-section" => {
                modes.set_action_value(
                    &mut action,
                    ConfigAction::RenameSection,
                    "--rename-section",
                )?;
            }
            "--remove-section" => {
                modes.set_action_value(
                    &mut action,
                    ConfigAction::RemoveSection,
                    "--remove-section",
                )?;
            }
            value if value.starts_with("--no-") => {
                eprintln!("error: unknown option `{}'", &value[2..]);
                return Err(GitError::Exit(129));
            }
            value => {
                positional.push(value);
                parse_options = false;
            }
        }
    }
    let action = if let Some(action) = action {
        action
    } else {
        match positional.len() {
            0 => {
                eprintln!("error: no action specified");
                return Err(GitError::Exit(129));
            }
            1 if positional[0].contains('=') && !config_key_is_valid(positional[0]) => {
                ConfigAction::Set
            }
            1 => ConfigAction::Get,
            2 => ConfigAction::Set,
            // Legacy `git config <key> <value> <value-pattern>` (git's
            // ACTION_SET_ALL).
            3 => ConfigAction::SetAll,
            _ => {
                return Err(GitError::Command(
                    "config requires <key> [<value>] or an explicit action".into(),
                ));
            }
        }
    };
    match action {
        ConfigAction::List if !positional.is_empty() => {
            return Err(GitError::Command(
                "config --list does not accept positional arguments".into(),
            ));
        }
        ConfigAction::Edit if !positional.is_empty() => {
            return Err(GitError::Command(
                "config --edit does not accept positional arguments".into(),
            ));
        }
        ConfigAction::GetRegexp if !(1..=2).contains(&positional.len()) => {
            return Err(GitError::Command(
                "config --get-regexp requires <name-regex> [<value-pattern>]".into(),
            ));
        }
        ConfigAction::RenameSection if positional.len() != 2 => {
            return Err(GitError::Command(
                "config --rename-section requires <old-name> <new-name>".into(),
            ));
        }
        ConfigAction::RemoveSection if positional.len() != 1 => {
            return Err(GitError::Command(
                "config --remove-section requires <name>".into(),
            ));
        }
        // The modern `get` subcommand always takes exactly the key/pattern (any
        // value filter arrives via `--value=`), while the classic
        // `--get`/`--get-all` forms accept an optional value-pattern positional.
        ConfigAction::Get | ConfigAction::GetAll
            if subcommand == Some(ConfigSubcommand::Get) && positional.len() != 1 =>
        {
            return Err(GitError::Command(
                "config action requires exactly one key".into(),
            ));
        }
        ConfigAction::Get | ConfigAction::GetAll
            if subcommand != Some(ConfigSubcommand::Get)
                && !(1..=2).contains(&positional.len()) =>
        {
            return Err(GitError::Command(
                "config action requires <key> [<value-pattern>]".into(),
            ));
        }
        ConfigAction::GetColor if !(1..=2).contains(&positional.len()) => {
            eprintln!("error: wrong number of arguments, should be from 1 to 2");
            return Err(GitError::Exit(129));
        }
        ConfigAction::GetColorBool if !(1..=2).contains(&positional.len()) => {
            eprintln!("error: wrong number of arguments, should be from 1 to 2");
            return Err(GitError::Exit(129));
        }
        ConfigAction::GetUrlMatch if positional.len() != 2 => {
            eprintln!("error: wrong number of arguments, should be 2");
            return Err(GitError::Exit(129));
        }
        ConfigAction::Unset | ConfigAction::UnsetAll if !(1..=2).contains(&positional.len()) => {
            return Err(GitError::Command(
                "config action requires <key> [<value-pattern>]".into(),
            ));
        }
        ConfigAction::Set if positional.len() == 1 => {
            return config_missing_set_value(positional[0]);
        }
        ConfigAction::Set | ConfigAction::Add if positional.len() != 2 => {
            return Err(GitError::Command(
                "config write action requires <key> <value>".into(),
            ));
        }
        ConfigAction::ReplaceAll | ConfigAction::SetAll if !(2..=3).contains(&positional.len()) => {
            return Err(GitError::Command(
                "config --replace-all requires <key> <value> [<value-pattern>]".into(),
            ));
        }
        _ => {}
    }
    // The modern `git config get` subcommand routes through a dedicated handler
    // (`config_subcommand_get`) rather than the classic `--get`/`--get-all`/
    // `--get-regexp` paths, because its display semantics differ (keys are only
    // shown under `--show-names`, only the last match prints without `--all`).
    let is_subcommand_get = subcommand == Some(ConfigSubcommand::Get);
    let subcommand_color_default_only = is_subcommand_get
        && value_type == ConfigValueType::Color
        && default_value.is_some()
        && positional.first().is_some_and(|value| value.is_empty());
    if is_subcommand_get {
        // git's `cmd_config_get` validation order. `--fixed-value` is only
        // meaningful with a `--value=<pattern>`; `--default` cannot combine with
        // `--all`. Both abort via `die()` (exit 128).
        if fixed_value && subcommand_value_pattern.is_none() {
            eprintln!("fatal: --fixed-value only applies with 'value-pattern'");
            return Err(GitError::Exit(128));
        }
        if default_value.is_some() && (action == ConfigAction::GetAll || subcommand_url.is_some()) {
            eprintln!("fatal: --default= cannot be used with --all or --url=");
            return Err(GitError::Exit(128));
        }
        if subcommand_url.is_some()
            && (action == ConfigAction::GetAll
                || subcommand_get_regexp
                || subcommand_value_pattern.is_some())
        {
            eprintln!("fatal: --url= cannot be used with --all, --regexp or --value");
            return Err(GitError::Exit(128));
        }
    } else if default_value.is_some() && action != ConfigAction::Get {
        eprintln!("error: --default is only applicable to --get");
        return Err(GitError::Exit(129));
    }
    // git restricts `--show-origin` to the four read actions; `--show-scope`
    // carries no such check (it works with `--get-urlmatch`, see t1300).
    if matches!(
        action,
        ConfigAction::GetColor | ConfigAction::GetColorBool | ConfigAction::GetUrlMatch
    ) && display.show_origin
    {
        eprintln!(
            "error: --show-origin is only applicable to --get, --get-all, --get-regexp, and --list"
        );
        return Err(GitError::Exit(129));
    }
    if action == ConfigAction::GetUrlMatch && name_only {
        eprintln!("error: --name-only is only applicable to --list or --get-regexp");
        return Err(GitError::Exit(129));
    }
    if comment.is_some()
        && !matches!(
            action,
            ConfigAction::Set | ConfigAction::SetAll | ConfigAction::ReplaceAll | ConfigAction::Add
        )
    {
        eprintln!("error: --comment is only applicable to add/set/replace operations");
        return Err(GitError::Exit(129));
    }
    // Classic-form `--fixed-value` (git's `cmd_config_legacy`) only applies when
    // a value-pattern is supplied in the appropriate positional, and only for
    // the actions that take one. The modern `get` subcommand validates
    // `--fixed-value` against `--value=<pattern>` separately above.
    if fixed_value && !is_subcommand_get {
        let allowed = match action {
            ConfigAction::Get
            | ConfigAction::GetAll
            | ConfigAction::GetRegexp
            | ConfigAction::Unset
            | ConfigAction::UnsetAll => positional.len() > 1 || subcommand_value_pattern.is_some(),
            ConfigAction::Set | ConfigAction::SetAll | ConfigAction::ReplaceAll => {
                positional.len() > 2 || subcommand_value_pattern.is_some()
            }
            _ => false,
        };
        if !allowed {
            eprintln!("error: --fixed-value only applies with 'value-pattern'");
            return Err(GitError::Exit(129));
        }
    }
    // The value-pattern positional, parsed as git's value-pattern (a leading `!`
    // negates the match, unless `--fixed-value` requests literal comparison).
    let value_pattern_filter = match action {
        ConfigAction::SetAll | ConfigAction::ReplaceAll => subcommand_value_pattern
            .as_deref()
            .or_else(|| positional.get(2).copied())
            .map(|pattern| ConfigValuePatternFilter::parse(pattern, fixed_value)),
        ConfigAction::Unset | ConfigAction::UnsetAll => subcommand_value_pattern
            .as_deref()
            .or_else(|| positional.get(1).copied())
            .map(|pattern| ConfigValuePatternFilter::parse(pattern, fixed_value)),
        _ => None,
    };
    let value_pattern_filter = value_pattern_filter.as_ref();

    let key = if matches!(
        action,
        ConfigAction::List
            | ConfigAction::Edit
            | ConfigAction::GetColor
            | ConfigAction::GetColorBool
            | ConfigAction::GetUrlMatch
            | ConfigAction::GetRegexp
            | ConfigAction::RenameSection
            | ConfigAction::RemoveSection
    ) || (is_subcommand_get && (subcommand_get_regexp || subcommand_url.is_some()))
        || subcommand_color_default_only
    {
        // Under `git config get --regexp` the positional is a key pattern, not a
        // concrete key, and under `get --url=` it may be a bare section name, so
        // it must not be validated/parsed as one.
        None
    } else {
        Some(parse_config_key(positional[0])?)
    };
    // Source selection mirrors git's `location_options_init`: at most one of
    // the scope flags / `--file` (or the legacy `GIT_CONFIG` env var) /
    // `--blob`; `--file -` reads stdin; `--local`, `--worktree`, and `--blob`
    // require a repository.
    let git_config_env = env::var_os("GIT_CONFIG").filter(|value| !value.is_empty());
    let effective_file = match config_file {
        Some(value) => Some(value),
        None => git_config_env.map(|path| path.to_string_lossy().into_owned()),
    };
    let file_sources = usize::from(use_global)
        + usize::from(use_system)
        + usize::from(use_local)
        + usize::from(use_worktree)
        + usize::from(effective_file.is_some())
        + usize::from(blob.is_some());
    if file_sources > 1 {
        eprintln!("error: only one config file at a time");
        return Err(GitError::Exit(129));
    }
    let repo_git_dir = cli_session.git_dir();
    if repo_git_dir.is_err() {
        if use_local {
            eprintln!("fatal: --local can only be used inside a git repository");
            return Err(GitError::Exit(128));
        }
        if blob.is_some() {
            eprintln!("fatal: --blob can only be used inside a git repository");
            return Err(GitError::Exit(128));
        }
        if use_worktree {
            eprintln!("fatal: --worktree can only be used inside a git repository");
            return Err(GitError::Exit(128));
        }
    }
    let editor_repo_git_dir = match &repo_git_dir {
        Ok(git_dir) => Some(git_dir.clone()),
        Err(_) => None,
    };
    let source = if use_global {
        ConfigSource::ScopedFile {
            path: global_config_file_path()?,
            scope: sley_config::ConfigScope::Global,
        }
    } else if use_system {
        ConfigSource::ScopedFile {
            path: system_config_file_path(),
            scope: sley_config::ConfigScope::System,
        }
    } else if use_local {
        let git_dir = repo_git_dir?;
        let common = common_git_dir_for_git_dir(&git_dir).unwrap_or_else(|_| git_dir.clone());
        ConfigSource::ScopedFile {
            path: config_display_path(cli_session.cwd(), common.join("config")),
            scope: sley_config::ConfigScope::Local,
        }
    } else if use_worktree {
        // git: with the worktreeConfig extension this is `config.worktree`;
        // with a single worktree it falls back to the shared local config; with
        // multiple worktrees and no extension it refuses. The explicit-source
        // scope stays `local` (mirrors `location_options_init`).
        let git_dir = repo_git_dir?;
        let common = common_git_dir_for_git_dir(&git_dir).unwrap_or_else(|_| git_dir.clone());
        let path = if worktree_config_extension_enabled(&common) {
            git_dir.join("config.worktree")
        } else if has_multiple_worktrees(&common) {
            eprintln!(
                "fatal: --worktree cannot be used with multiple working trees unless the config\nextension worktreeConfig is enabled. Please read \"CONFIGURATION FILE\"\nsection in \"git help worktree\" for details"
            );
            return Err(GitError::Exit(128));
        } else {
            common.join("config")
        };
        ConfigSource::ScopedFile {
            path: config_display_path(cli_session.cwd(), path),
            scope: sley_config::ConfigScope::Local,
        }
    } else if let Some(value) = effective_file {
        if value == "-" {
            ConfigSource::Stdin
        } else {
            ConfigSource::File(PathBuf::from(value))
        }
    } else if let Some(spec) = blob {
        ConfigSource::Blob(spec)
    } else {
        match repo_git_dir {
            Ok(git_dir) => ConfigSource::Repository(git_dir),
            Err(err) => {
                // git's `check_write` / `show_editor`: writing the default
                // (repository) config outside a repo dies with this exact
                // message — not the generic "not a git repository" discovery
                // error. Reads still fall through to the discovery error so
                // non-write paths keep their existing diagnostic.
                let is_write = matches!(
                    action,
                    ConfigAction::Set
                        | ConfigAction::SetAll
                        | ConfigAction::Add
                        | ConfigAction::ReplaceAll
                        | ConfigAction::Unset
                        | ConfigAction::UnsetAll
                        | ConfigAction::RenameSection
                        | ConfigAction::RemoveSection
                        | ConfigAction::Edit
                );
                if is_write {
                    eprintln!("fatal: not in a git directory");
                    return Err(GitError::Exit(128));
                }
                return Err(err);
            }
        }
    };

    if action == ConfigAction::Edit {
        config_edit(cli_session, &source, editor_repo_git_dir.as_deref())?;
        return Ok(());
    }

    let is_write_action = matches!(
        action,
        ConfigAction::Set
            | ConfigAction::SetAll
            | ConfigAction::Add
            | ConfigAction::ReplaceAll
            | ConfigAction::Unset
            | ConfigAction::UnsetAll
            | ConfigAction::RenameSection
            | ConfigAction::RemoveSection
    );
    if is_write_action {
        // git parses the `-c`/`GIT_CONFIG_*` injection during startup even for
        // writes; surface a bogus entry the same way.
        if matches!(source, ConfigSource::Repository(_)) {
            crate::injected_config_parameters()?;
        }
        // Writes operate on the target file's document alone — never on the
        // merged stack, and never with includes spliced in (git edits the file
        // in place and leaves include directives untouched).
        //
        // Variable set/add/replace/unset go through the git-faithful surgical
        // editor (`config_raw_edit`), which preserves the target file
        // byte-for-byte apart from the lines it genuinely touches — exactly like
        // git's `git_config_set_multivar_in_file`. Section rename/remove still use
        // the structured document path.
        match action {
            ConfigAction::Set => {
                let key = key.expect("validated config key");
                // git's ACTION_SET: with no value-pattern, refuse if the key is
                // already multi-valued (CONFIG_NOTHING_SET → "cannot overwrite
                // multiple values with a single value").
                let value = normalize_set_value(&key, positional[1], value_type)?;
                if !config_raw_edit(&source, &key, Some(&value), comment.as_deref(), None, false)? {
                    print_config_multiple_values_error(&key);
                    return Err(GitError::Exit(5));
                }
            }
            ConfigAction::SetAll => {
                // git's ACTION_SET_ALL: legacy `<key> <value> <value-pattern>` —
                // single replace with a value-pattern (no multi-replace flag).
                let key = key.expect("validated config key");
                let value = normalize_set_value(&key, positional[1], value_type)?;
                let pred = value_pattern_filter.map(filter_predicate);
                if !config_raw_edit(
                    &source,
                    &key,
                    Some(&value),
                    comment.as_deref(),
                    pred.as_deref(),
                    false,
                )? {
                    print_config_multiple_values_error(&key);
                    return Err(GitError::Exit(5));
                }
            }
            ConfigAction::ReplaceAll => {
                let key = key.expect("validated config key");
                let value = normalize_set_value(&key, positional[1], value_type)?;
                let pred = value_pattern_filter.map(filter_predicate);
                // --replace-all: multi-replace; never errors on multiple matches.
                config_raw_edit(
                    &source,
                    &key,
                    Some(&value),
                    comment.as_deref(),
                    pred.as_deref(),
                    true,
                )?;
            }
            ConfigAction::Add => {
                let key = key.expect("validated config key");
                let value = normalize_set_value(&key, positional[1], value_type)?;
                // git's ACTION_ADD: set_multivar with CONFIG_REGEX_NONE — a
                // pattern that matches nothing, so it always appends a new line.
                let never = |_: Option<&str>| false;
                config_raw_edit(
                    &source,
                    &key,
                    Some(&value),
                    comment.as_deref(),
                    Some(&never),
                    true,
                )?;
            }
            ConfigAction::Unset => {
                let key = key.expect("validated config key");
                let pred = value_pattern_filter.map(filter_predicate);
                if !config_raw_edit(&source, &key, None, None, pred.as_deref(), false)? {
                    return Err(GitError::Exit(5));
                }
            }
            ConfigAction::UnsetAll => {
                let key = key.expect("validated config key");
                let pred = value_pattern_filter.map(filter_predicate);
                if !config_raw_edit(&source, &key, None, None, pred.as_deref(), true)? {
                    return Err(GitError::Exit(5));
                }
            }
            ConfigAction::RenameSection => {
                config_rename_or_remove_section(&source, positional[0], Some(positional[1]))?;
            }
            ConfigAction::RemoveSection => {
                config_rename_or_remove_section(&source, positional[0], None)?;
            }
            _ => unreachable!("write actions handled above"),
        }
        return Ok(());
    }

    // Read path: build the flattened, metadata-carrying config event stream.
    // For the default (repository) source this is git's full config sequence —
    // system, global (XDG then `~/.gitconfig`), local, worktree — with the
    // command-line / environment injection (`-c`, `--config-env`,
    // `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`) layered on top at `command`
    // scope. Explicit sources contribute exactly their own entries. Reads also
    // validate the injection stream here, surfacing a bogus `-c`/env entry the
    // same way git does.
    let loaded = load_read_entries(cli_session, &source, action, respect_includes_opt)?;
    let mut entries = loaded.entries;
    if matches!(source, ConfigSource::Repository(_)) {
        let parameters = crate::injected_config_parameters()?;
        let mut stack = sley_config::ConfigStack { entries };
        if respect_includes_opt == Some(false) {
            stack.push_parameters(&parameters);
        } else {
            let context = config_include_context(cli_session);
            stack
                .push_parameters_with_includes(&parameters, &context)
                .map_err(|err| report_config_parse_error(err, None))?;
        }
        entries = stack.entries;
    }
    let entries = entries;
    if is_subcommand_get {
        if subcommand_color_default_only && let Some(default) = default_value.as_deref() {
            let formatted = format_config_value(default, value_type)?;
            let mut stdout = io::stdout();
            write_config_metadata(
                &mut stdout,
                &ConfigValueMeta::command_line(),
                display,
                null_terminate,
            )?;
            if null_terminate {
                write!(stdout, "{formatted}\0")?;
            } else {
                write!(stdout, "{formatted}")?;
            }
            return Ok(());
        }
        // `git config get --url=<url>` routes through the urlmatch lookup,
        // exactly like the classic `--get-urlmatch`.
        if let Some(url) = subcommand_url.as_deref() {
            let target = parse_config_urlmatch_target(positional[0])?;
            if !config_get_urlmatch(&entries, &target, url, null_terminate, display, value_type)? {
                return Err(GitError::Exit(1));
            }
            return Ok(());
        }
        // `--all` is surfaced as the `GetAll` action by the parser; `--regexp`
        // and `--show-names` were captured separately above.
        let all = action == ConfigAction::GetAll;
        let get_key = if subcommand_get_regexp {
            SubcommandGetKey::Regexp(SimpleConfigRegex::parse(positional[0]))
        } else {
            SubcommandGetKey::Exact(key.expect("validated config key"))
        };
        let value_filter = subcommand_value_pattern
            .as_deref()
            .map(|pattern| ConfigValuePatternFilter::parse(pattern, fixed_value));
        if !config_subcommand_get(
            &entries,
            get_key,
            value_filter.as_ref(),
            display,
            all,
            subcommand_show_names,
            name_only,
            value_type,
            default_value.as_deref(),
            null_terminate,
        )? {
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    match action {
        ConfigAction::List => {
            config_list(&entries, display, name_only, null_terminate, value_type)?;
            if let Some(err) = loaded.tail_error {
                let path = match &source {
                    ConfigSource::Repository(git_dir) => Some(git_dir.join("config")),
                    ConfigSource::ScopedFile { path, .. } | ConfigSource::File(path) => {
                        Some(path.clone())
                    }
                    ConfigSource::Blob(_) | ConfigSource::Stdin => None,
                };
                if matches!(source, ConfigSource::Stdin) {
                    return Err(report_config_stdin_parse_error(err));
                }
                if let ConfigSource::Blob(spec) = &source {
                    return Err(report_config_blob_parse_error(err, spec));
                }
                return Err(report_config_parse_error(err, path.as_deref()));
            }
        }
        ConfigAction::Get => {
            let key = key.expect("validated config key");
            // Classic `--get <name> <value-pattern>` filters the (possibly
            // multi-valued) key by value and returns the last surviving match,
            // exactly as git's shared `get_value` collector does. Without a
            // value-pattern the simpler last-entry lookup below is used.
            if let Some(pattern) = positional.get(1) {
                let filter = ConfigValuePatternFilter::parse(pattern, fixed_value);
                if !config_subcommand_get(
                    &entries,
                    SubcommandGetKey::Exact(key),
                    Some(&filter),
                    display,
                    false,
                    false,
                    false,
                    value_type,
                    default_value.as_deref(),
                    null_terminate,
                )? {
                    return Err(GitError::Exit(1));
                }
                return Ok(());
            }
            let name = config_key_name(&key);
            let mut selected = None;
            for entry in entries
                .iter()
                .rev()
                .filter(|entry| entry.matches(&key.section, key.subsection.as_deref(), &key.key))
            {
                let meta = ConfigValueMeta::of(entry);
                let Some(value) = format_config_entry_value(entry, value_type, &name)? else {
                    continue;
                };
                selected = Some((meta, value));
                break;
            }
            let (meta, value) = match selected {
                Some(selected) => selected,
                None => {
                    let Some(default) = default_value.as_deref() else {
                        return Err(GitError::Exit(1));
                    };
                    let Some(value) = format_default_config_value(default, value_type)? else {
                        return Err(GitError::Exit(1));
                    };
                    (
                        ConfigValueMeta::command_line(),
                        ConfigFormattedValue::Value(value),
                    )
                }
            };
            write_config_formatted_entry(
                &mut io::stdout(),
                &meta,
                &name,
                &value,
                ConfigEntryWriteOptions {
                    display,
                    name_only: false,
                    show_keys: false,
                    value_type,
                    null_terminate,
                    equals_separator: false,
                },
            )?;
        }
        ConfigAction::GetColor => {
            if positional[0].is_empty() {
                if let Some(default) = positional.get(1) {
                    write!(
                        io::stdout(),
                        "{}",
                        format_config_default_color_value(default)?
                    )?;
                }
                return Ok(());
            }
            let key = parse_config_key(positional[0])?;
            let value = entries_get(&entries, &key).and_then(|entry| entry.value.as_deref());
            if let Some(value) = value {
                write!(io::stdout(), "{}", format_config_value(value, value_type)?)?;
            } else if let Some(default) = positional.get(1) {
                write!(
                    io::stdout(),
                    "{}",
                    format_config_default_color_value(default)?
                )?;
            }
        }
        ConfigAction::GetColorBool => {
            let key = parse_config_key(positional[0])?;
            let stdout_tty_hint = match positional.get(1) {
                Some(stdout_is_tty) => {
                    let Some(parsed) = sley_config::parse_config_bool(stdout_is_tty) else {
                        eprintln!(
                            "fatal: bad boolean config value '{stdout_is_tty}' for 'command line'"
                        );
                        return Err(GitError::Exit(128));
                    };
                    Some(parsed)
                }
                None => None,
            };
            let value = entries_get(&entries, &key)
                .and_then(|entry| entry.value.as_deref())
                .or_else(|| {
                    entries
                        .iter()
                        .rev()
                        .find(|entry| entry.matches("color", None, "ui"))
                        .and_then(|entry| entry.value.as_deref())
                });
            let setting = match value {
                Some(value) => config_colorbool_setting(&key, value)?,
                None => ConfigColorBoolSetting::Auto,
            };
            let enabled = config_colorbool_enabled(setting, stdout_tty_hint);
            if stdout_tty_hint.is_some() {
                writeln!(io::stdout(), "{enabled}")?;
            } else if !enabled {
                return Err(GitError::Exit(1));
            }
        }
        ConfigAction::GetUrlMatch => {
            let target = parse_config_urlmatch_target(positional[0])?;
            if !config_get_urlmatch(
                &entries,
                &target,
                positional[1],
                null_terminate,
                display,
                value_type,
            )? {
                return Err(GitError::Exit(1));
            }
        }
        ConfigAction::GetAll => {
            let key = key.expect("validated config key");
            // Classic `--get-all <name> <value-pattern>` filters every value of
            // the key by the pattern (git's shared `get_value` with the "all"
            // flag set). Without a pattern, list every value directly.
            if let Some(pattern) = positional.get(1) {
                let filter = ConfigValuePatternFilter::parse(pattern, fixed_value);
                if !config_subcommand_get(
                    &entries,
                    SubcommandGetKey::Exact(key),
                    Some(&filter),
                    display,
                    true,
                    false,
                    false,
                    value_type,
                    None,
                    null_terminate,
                )? {
                    return Err(GitError::Exit(1));
                }
                return Ok(());
            }
            let name = config_key_name(&key);
            let values = entries_get_all(&entries, &key);
            if values.is_empty() {
                return Err(GitError::Exit(1));
            }
            let mut stdout = io::stdout();
            let mut wrote = false;
            for entry in values {
                let Some(formatted) = format_config_entry_value(entry, value_type, &name)? else {
                    continue;
                };
                wrote = true;
                write_config_formatted_entry(
                    &mut stdout,
                    &ConfigValueMeta::of(entry),
                    &name,
                    &formatted,
                    ConfigEntryWriteOptions {
                        display,
                        name_only: false,
                        show_keys: false,
                        value_type,
                        null_terminate,
                        equals_separator: false,
                    },
                )?;
            }
            if !wrote {
                return Err(GitError::Exit(1));
            }
        }
        ConfigAction::GetRegexp => {
            // `--get-regexp <name-regex> [<value-pattern>]`: the optional second
            // positional filters matches by value.
            let value_filter = positional
                .get(1)
                .map(|pattern| ConfigValuePatternFilter::parse(pattern, fixed_value));
            if !config_get_regexp(
                &entries,
                positional[0],
                value_filter.as_ref(),
                display,
                name_only,
                value_type,
                null_terminate,
            )? {
                return Err(GitError::Exit(1));
            }
        }
        _ => unreachable!("write actions handled above"),
    }
    Ok(())
}

struct LoadedEntries {
    entries: Vec<sley_config::ConfigStackEntry>,
    tail_error: Option<GitError>,
}

/// Build the read-side config event stream for a source.
///
/// The default (repository) source walks git's `do_git_config_sequence`:
/// system → global (XDG, then `~/.gitconfig`) → local → worktree (when the
/// `worktreeConfig` extension is enabled), resolving includes per file and
/// attributing every entry to its layer. Explicit sources contribute exactly
/// one layer; includes are then only resolved under `--includes` for file
/// sources (git's `respect_includes = !source.file`), but by default for
/// stdin and blob sources.
fn load_read_entries(
    cli_session: &crate::session::CliSession,
    source: &ConfigSource,
    action: ConfigAction,
    respect_includes_opt: Option<bool>,
) -> Result<LoadedEntries> {
    let context = config_include_context(cli_session);
    let respect_repository_includes = respect_includes_opt.unwrap_or(true);
    let mut stack = sley_config::ConfigStack::new();
    let mut tail_error = None;
    match source {
        ConfigSource::Repository(git_dir) => {
            for (path, scope) in sley_config::default_config_layer_paths() {
                stack
                    .push_file(&path, scope, respect_repository_includes, &context)
                    .map_err(|err| report_config_parse_error(err, Some(&path)))?;
            }
            // An explicit-but-missing git dir (e.g. `--git-dir=nonexistent`)
            // still lists the non-repo layers, like git.
            let common = match common_git_dir_for_git_dir(git_dir) {
                Ok(common) => common,
                Err(_) => git_dir.clone(),
            };
            let local_path = config_display_path(cli_session.cwd(), common.join("config"));
            stack
                .push_file(
                    &local_path,
                    sley_config::ConfigScope::Local,
                    respect_repository_includes,
                    &context,
                )
                .map_err(|err| report_config_parse_error(err, Some(&local_path)))?;
            if worktree_config_extension_enabled(&common) {
                let worktree_path =
                    config_display_path(cli_session.cwd(), git_dir.join("config.worktree"));
                stack
                    .push_file(
                        &worktree_path,
                        sley_config::ConfigScope::Worktree,
                        respect_repository_includes,
                        &context,
                    )
                    .map_err(|err| report_config_parse_error(err, Some(&worktree_path)))?;
            }
        }
        ConfigSource::ScopedFile { path, scope } => {
            tail_error = load_entries_from_file(
                &mut stack,
                path,
                *scope,
                action,
                respect_includes_opt.unwrap_or(false),
                &context,
            )?;
        }
        ConfigSource::File(path) => {
            tail_error = load_entries_from_file(
                &mut stack,
                path,
                sley_config::ConfigScope::Command,
                action,
                respect_includes_opt.unwrap_or(false),
                &context,
            )?;
        }
        ConfigSource::Stdin => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
            let (parsed, tail) = parse_stdin_config_bytes(&bytes, action)?;
            tail_error = tail;
            stack
                .push_parsed(
                    &parsed,
                    sley_config::ConfigOrigin::stdin(),
                    sley_config::ConfigScope::Command,
                    respect_includes_opt.unwrap_or(true),
                    &context,
                )
                .map_err(|err| report_config_parse_error(err, None))?;
        }
        ConfigSource::Blob(spec) => {
            let bytes = read_config_blob(cli_session, spec)?;
            let (parsed, tail) = parse_blob_config_bytes(&bytes, action, spec)?;
            tail_error = tail;
            stack
                .push_parsed(
                    &parsed,
                    sley_config::ConfigOrigin::blob(spec.clone()),
                    sley_config::ConfigScope::Command,
                    respect_includes_opt.unwrap_or(true),
                    &context,
                )
                .map_err(|err| report_config_parse_error(err, None))?;
        }
    }
    Ok(LoadedEntries {
        entries: stack.entries,
        tail_error,
    })
}

/// Read one explicit config file into the stack. A missing file is fatal for
/// `--list` (git: "unable to read config file") and empty otherwise.
fn load_entries_from_file(
    stack: &mut sley_config::ConfigStack,
    path: &Path,
    scope: sley_config::ConfigScope,
    action: ConfigAction,
    respect_includes: bool,
    context: &sley_config::ConfigIncludeContext,
) -> Result<Option<GitError>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound && action != ConfigAction::List => {
            return Ok(None);
        }
        Err(err) => {
            eprintln!(
                "fatal: unable to read config file '{}': {err}",
                path.display()
            );
            return Err(GitError::Exit(128));
        }
    };
    let (parsed, tail_error) = parse_config_bytes(&bytes, action, Some(path))?;
    stack
        .push_parsed(
            &parsed,
            sley_config::ConfigOrigin::file(path.to_string_lossy().into_owned()),
            scope,
            respect_includes,
            context,
        )
        .map_err(|err| report_config_parse_error(err, Some(path)))?;
    Ok(tail_error)
}

/// Parse config bytes; for `--list` the well-formed prefix is kept and the
/// parse error deferred (git prints what it read before dying).
fn parse_config_bytes(
    bytes: &[u8],
    action: ConfigAction,
    path: Option<&Path>,
) -> Result<(GitConfig, Option<GitError>)> {
    if action == ConfigAction::List {
        let (config, tail_error) = GitConfig::parse_collecting(bytes)?;
        Ok((config, tail_error))
    } else {
        GitConfig::parse(bytes)
            .map(|config| (config, None))
            .map_err(|err| report_config_parse_error(err, path))
    }
}

fn parse_stdin_config_bytes(
    bytes: &[u8],
    action: ConfigAction,
) -> Result<(GitConfig, Option<GitError>)> {
    if action == ConfigAction::List {
        let (config, tail_error) = GitConfig::parse_collecting(bytes)?;
        Ok((config, tail_error))
    } else {
        GitConfig::parse(bytes)
            .map(|config| (config, None))
            .map_err(report_config_stdin_parse_error)
    }
}

fn parse_blob_config_bytes(
    bytes: &[u8],
    action: ConfigAction,
    spec: &str,
) -> Result<(GitConfig, Option<GitError>)> {
    if action == ConfigAction::List {
        let (config, tail_error) = GitConfig::parse_collecting(bytes)?;
        Ok((config, tail_error))
    } else {
        GitConfig::parse(bytes)
            .map(|config| (config, None))
            .map_err(|err| report_config_blob_parse_error(err, spec))
    }
}

/// The document writes operate on: the target file parsed alone, includes left
/// in place. Missing files start empty.
/// Resolve the on-disk path a write action targets, or `None` for the
/// unsupported Blob/Stdin sources (the caller already rejects those).
fn config_write_path(source: &ConfigSource) -> Option<PathBuf> {
    match source {
        ConfigSource::Repository(git_dir) => Some(
            common_git_dir_for_git_dir(git_dir)
                .unwrap_or_else(|_| git_dir.clone())
                .join("config"),
        ),
        ConfigSource::ScopedFile { path, .. } | ConfigSource::File(path) => Some(path.clone()),
        ConfigSource::Blob(_) | ConfigSource::Stdin => None,
    }
}

/// Open the selected config file in the user's editor. The edited path comes
/// from the command's file/scope source, while `core.editor` is resolved from
/// the ambient repository config, matching git's `git config --edit -f <file>`.
fn config_edit(
    cli_session: &crate::session::CliSession,
    source: &ConfigSource,
    repo_git_dir: Option<&Path>,
) -> Result<()> {
    let Some(path) = config_write_path(source) else {
        match source {
            ConfigSource::Stdin => eprintln!("fatal: editing stdin is not supported"),
            _ => eprintln!("fatal: editing blobs is not supported"),
        }
        return Err(GitError::Exit(128));
    };
    let editor = config_editor_command(cli_session, repo_git_dir)?;
    if editor == ":" {
        return Ok(());
    }
    match ProcessCommand::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(&path)
        .status()
    {
        Ok(status) => {
            if !status.success() {
                eprintln!("error: there was a problem with the editor '{editor}'");
            }
        }
        Err(err) => {
            eprintln!("error: cannot run {editor}: {err}");
            eprintln!("error: unable to start editor '{editor}'");
        }
    }
    Ok(())
}

fn config_editor_command(
    cli_session: &crate::session::CliSession,
    repo_git_dir: Option<&Path>,
) -> Result<String> {
    if let Ok(editor) = env::var("GIT_EDITOR") {
        return Ok(editor);
    }
    if let Some(editor) = global_config_value("core.editor")? {
        return Ok(editor);
    }
    if let Some(editor) = config_core_editor(cli_session, repo_git_dir)? {
        return Ok(editor);
    }
    if let Ok(editor) = env::var("VISUAL")
        && !editor.is_empty()
        && config_terminal_allows_visual()
    {
        return Ok(editor);
    }
    if let Ok(editor) = env::var("EDITOR") {
        return Ok(editor);
    }
    Ok("vi".to_string())
}

fn config_terminal_allows_visual() -> bool {
    match env::var("TERM") {
        Ok(term) => term != "dumb",
        Err(_) => false,
    }
}

fn config_core_editor(
    cli_session: &crate::session::CliSession,
    repo_git_dir: Option<&Path>,
) -> Result<Option<String>> {
    match repo_git_dir {
        Some(git_dir) => {
            let source = ConfigSource::Repository(git_dir.to_path_buf());
            let loaded = load_read_entries(cli_session, &source, ConfigAction::Get, None)?;
            Ok(config_core_editor_from_entries(&loaded.entries))
        }
        None => config_core_editor_from_default_layers(cli_session),
    }
}

fn config_core_editor_from_default_layers(
    cli_session: &crate::session::CliSession,
) -> Result<Option<String>> {
    let context = config_include_context(cli_session);
    let mut stack = sley_config::ConfigStack::new();
    for (path, scope) in sley_config::default_config_layer_paths() {
        stack
            .push_file(&path, scope, true, &context)
            .map_err(|err| report_config_parse_error(err, Some(&path)))?;
    }
    Ok(config_core_editor_from_entries(&stack.entries))
}

fn config_core_editor_from_entries(entries: &[sley_config::ConfigStackEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.matches("core", None, "editor"))
        .map(|entry| match &entry.value {
            Some(value) => value.clone(),
            None => "true".to_string(),
        })
}

/// Apply a git-faithful surgical edit (`git config set/add/--replace-all/--unset`)
/// to the target file's raw bytes, preserving every untouched byte. Mirrors
/// git's `git_config_set_multivar_in_file_gently`.
///
/// `value == None` is an unset; `multi_replace` is `--replace-all`'s
/// `CONFIG_FLAGS_MULTI_REPLACE`; `value_matches` is the optional value-pattern
/// filter (already folding in `!` negation and `--fixed-value`). Returns `false`
/// when the edit matched nothing to unset, or matched several entries under a
/// single-value set (git's exit code 5).
fn config_raw_edit(
    source: &ConfigSource,
    key: &ConfigKey,
    value: Option<&str>,
    comment: Option<&str>,
    value_matches: Option<&dyn Fn(Option<&str>) -> bool>,
    multi_replace: bool,
) -> Result<bool> {
    let Some(path) = config_write_path(source) else {
        match source {
            ConfigSource::Stdin => eprintln!("fatal: writing to stdin is not supported"),
            _ => eprintln!("fatal: writing config blobs is not supported"),
        }
        return Err(GitError::Exit(128));
    };
    let contents = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            eprintln!(
                "fatal: unable to read config file '{}': {err}",
                path.display()
            );
            return Err(GitError::Exit(128));
        }
    };
    let mut editor = sley_config::raw_edit::RawConfigEditor::new(
        contents,
        &key.section,
        key.subsection.as_deref(),
        &key.key,
    );
    match editor.set_multivar(value, comment, value_matches, multi_replace) {
        sley_config::raw_edit::RawEditOutcome::NothingSet => Ok(false),
        sley_config::raw_edit::RawEditOutcome::Changed => {
            write_raw_config_file(&path, editor.into_bytes())?;
            Ok(true)
        }
    }
}

fn write_raw_config_file(path: &std::path::Path, bytes: Vec<u8>) -> Result<()> {
    sley_config::raw_edit::write_config_file_locked(
        path,
        &bytes,
        sley_config::raw_edit::ConfigFileWriteOptions::default(),
    )
    .map_err(|err| GitError::Io(err.to_string()))
}

/// git's `section_name_is_ok`: a new section name must be non-empty, and the
/// part before the first `.` must be alphanumeric or `-`.
fn section_name_is_ok(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    for ch in std::iter::once(first).chain(chars) {
        if ch == '.' {
            return true;
        }
        if ch != '-' && !ch.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

/// The display form git uses for the section-name passed to rename/remove:
/// `section[.subsection]`, section unchanged (rename is case-sensitive).
fn section_name_string(name: &ConfigSectionName) -> String {
    match &name.subsection {
        Some(subsection) => format!("{}.{}", name.section, subsection),
        None => name.section.clone(),
    }
}

/// `git config --rename-section`/`--remove-section` via git's byte-faithful
/// line-based section copier (`repo_config_copy_or_rename_section_in_file`),
/// preserving every untouched byte.
fn config_rename_or_remove_section(
    source: &ConfigSource,
    old: &str,
    new: Option<&str>,
) -> Result<()> {
    // Validate the names the same way git does (subsection/section syntax).
    let old_name = parse_config_section_name(old)?;
    let new_name = match new {
        Some(new) => {
            let parsed = parse_config_section_name(new)?;
            let rendered = section_name_string(&parsed);
            if !section_name_is_ok(&rendered) {
                eprintln!("error: invalid section name: {rendered}");
                return Err(GitError::Exit(128));
            }
            Some(rendered)
        }
        None => None,
    };
    let old_rendered = section_name_string(&old_name);

    let Some(path) = config_write_path(source) else {
        match source {
            ConfigSource::Stdin => eprintln!("fatal: writing to stdin is not supported"),
            _ => eprintln!("fatal: writing config blobs is not supported"),
        }
        return Err(GitError::Exit(128));
    };
    let display_path = config_source_display_name(source, &path);
    let contents = match fs::read(&path) {
        Ok(bytes) => bytes,
        // git: a missing config file means nothing to rename — no error.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            eprintln!(
                "fatal: unable to read config file '{}': {err}",
                path.display()
            );
            return Err(GitError::Exit(128));
        }
    };
    match sley_config::raw_edit::rename_or_remove_section(
        &contents,
        &old_rendered,
        new_name.as_deref(),
    ) {
        sley_config::raw_edit::SectionEditOutcome::Changed(out) => {
            write_raw_config_file(&path, out)?;
            Ok(())
        }
        sley_config::raw_edit::SectionEditOutcome::NotFound => {
            eprintln!("fatal: no such section: {old_rendered}");
            Err(GitError::Exit(128))
        }
        sley_config::raw_edit::SectionEditOutcome::LineTooLong(line) => {
            eprintln!(
                "error: refusing to work with overly long line in '{display_path}' on line {line}"
            );
            Err(GitError::Exit(128))
        }
    }
}

/// The filename git prints in diagnostics for the given source: the explicit
/// `--file` path verbatim, otherwise the display path.
fn config_source_display_name(source: &ConfigSource, path: &Path) -> String {
    match source {
        ConfigSource::File(p) => p.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Include-condition context shared by every layer: the repository (when one
/// is discoverable) supplies `gitdir:` and `onbranch:`.
fn config_include_context(
    cli_session: &crate::session::CliSession,
) -> sley_config::ConfigIncludeContext {
    match cli_session.git_dir() {
        Ok(git_dir) => sley_config::ConfigIncludeContext::new(
            Some(sley_config::git_dir_for_include_context(&git_dir)),
            repo_current_branch_name(&git_dir),
        ),
        Err(_) => sley_config::ConfigIncludeContext::default(),
    }
}

/// Display a config path the way git reports it: relative to the working
/// directory when it lies underneath (git's repo paths are themselves
/// relative, e.g. `.git/config` at the worktree root).
fn config_display_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return relative.to_path_buf();
    }
    path
}

/// The file `--global` reads and writes: `GIT_CONFIG_GLOBAL` when set,
/// otherwise `~/.gitconfig` — except when that does not exist but the XDG
/// config does, in which case the XDG file is used (git's
/// `git_global_config`).
fn global_config_file_path() -> Result<PathBuf> {
    if let Some(path) = env::var("GIT_CONFIG_GLOBAL")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(path));
    }
    let Some(home) = sley_config::home_dir() else {
        eprintln!("fatal: $HOME not set");
        return Err(GitError::Exit(128));
    };
    let user = PathBuf::from(&home).join(".gitconfig");
    if !user.exists() {
        let xdg = match env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|value| !value.is_empty())
        {
            Some(xdg) => PathBuf::from(xdg).join("git").join("config"),
            None => PathBuf::from(&home)
                .join(".config")
                .join("git")
                .join("config"),
        };
        if xdg.exists() {
            return Ok(xdg);
        }
    }
    Ok(user)
}

/// The file `--system` reads and writes: `GIT_CONFIG_SYSTEM` when set,
/// otherwise `/etc/gitconfig`. (The explicit flag ignores
/// `GIT_CONFIG_NOSYSTEM`, like git.)
fn system_config_file_path() -> PathBuf {
    env::var("GIT_CONFIG_SYSTEM")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/gitconfig"))
}

/// Whether `extensions.worktreeConfig` is enabled in the shared local config.
fn worktree_config_extension_enabled(common_git_dir: &Path) -> bool {
    GitConfig::read(common_git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("extensions", None, "worktreeconfig"))
        .unwrap_or(false)
}

/// Whether any linked worktrees exist (`$GIT_COMMON_DIR/worktrees` non-empty).
fn has_multiple_worktrees(common_git_dir: &Path) -> bool {
    fs::read_dir(common_git_dir.join("worktrees"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Read the config blob for `--blob=<spec>` (a blob id or `<rev>:<path>`).
fn read_config_blob(cli_session: &crate::session::CliSession, spec: &str) -> Result<Vec<u8>> {
    let repo = crate::repository::RepositoryContext::from_session(cli_session)?;
    let oid = if let Some((rev, path)) = sley_rev::split_rev_path_spec(spec) {
        match repo.resolve_path(rev, path) {
            Ok(resolved) => resolved.oid,
            Err(_) => return config_blob_resolve_error(spec),
        }
    } else {
        match repo.resolve_revision(spec) {
            Ok(oid) => oid,
            Err(_) => return config_blob_resolve_error(spec),
        }
    };
    let Ok(object) = repo.objects().read_object(&oid) else {
        return config_blob_resolve_error(spec);
    };
    if object.object_type != ObjectType::Blob {
        eprintln!("fatal: reference '{spec}' does not point to a blob");
        return Err(GitError::Exit(128));
    }
    Ok(object.body.clone())
}

fn config_blob_resolve_error<T>(spec: &str) -> Result<T> {
    eprintln!("fatal: unable to resolve config blob '{spec}'");
    Err(GitError::Exit(128))
}

fn report_config_parse_error(err: GitError, path: Option<&Path>) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if message == "relative config includes must come from files"
                || message.starts_with("exceeded maximum include depth")
                || message
                    == "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
            {
                eprintln!("fatal: {message}");
                return GitError::Exit(128);
            }
            if let Some((line, message_path)) = parse_bad_config_line_with_path(&message) {
                eprintln!("fatal: bad config line {line} in file {message_path}");
                return GitError::Exit(128);
            }
            if let Some(line) = parse_bad_config_line_without_path(&message) {
                if let Some(path) = path {
                    eprintln!("fatal: bad config line {line} in file {}", path.display());
                } else {
                    eprintln!("fatal: bad config line {line}");
                }
                return GitError::Exit(128);
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

fn report_config_stdin_parse_error(err: GitError) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if let Some(line) = parse_bad_config_line_without_path(&message) {
                eprintln!("fatal: bad config line {line} in standard input");
                return GitError::Exit(128);
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

fn report_config_blob_parse_error(err: GitError, spec: &str) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if let Some(line) = parse_bad_config_line_without_path(&message) {
                eprintln!("fatal: bad config line {line} in blob {spec}");
                return GitError::Exit(128);
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

/// git's `option_parse_type` argument decode: the `--type=<arg>` list. `string`
/// is NOT accepted (git only exposes the untyped default implicitly), and an
/// unknown argument dies with git's exact message + exit 128.
fn parse_config_type_arg(value: &str) -> Result<ConfigValueType> {
    match value {
        "bool" => Ok(ConfigValueType::Bool),
        "int" => Ok(ConfigValueType::Int),
        "bool-or-int" => Ok(ConfigValueType::BoolOrInt),
        "expiry-date" => Ok(ConfigValueType::ExpiryDate),
        "color" => Ok(ConfigValueType::Color),
        "path" => Ok(ConfigValueType::Path),
        other => {
            eprintln!("fatal: unrecognized --type argument, {other}");
            Err(GitError::Exit(128))
        }
    }
}

/// git's `option_parse_type` conflict check: applying a *different* explicit
/// type than one already chosen errors "only one type at a time" (exit 129);
/// re-applying the same type is allowed.
fn set_config_type(
    current: &mut ConfigValueType,
    type_set: &mut bool,
    new: ConfigValueType,
) -> Result<()> {
    if *type_set && *current != new {
        eprintln!("error: only one type at a time");
        return Err(GitError::Exit(129));
    }
    *current = new;
    *type_set = true;
    Ok(())
}

fn format_config_value(value: &str, value_type: ConfigValueType) -> Result<String> {
    format_config_value_with(value, value_type, None, None)
}

/// Side-effect-free predicate: can `value` be canonicalised as `value_type`?
/// Used by `--list --type=<T>` to silently drop non-conforming values without
/// printing the formatter's `fatal:` diagnostic. `Raw` always passes; `Path`
/// only passes when `~` expansion is possible.
fn value_canonicalizes_as(value: &str, value_type: ConfigValueType) -> bool {
    match value_type {
        ConfigValueType::Raw => true,
        ConfigValueType::Bool => sley_config::parse_config_bool(value).is_some(),
        ConfigValueType::Int => sley_config::parse_config_int(value).is_some(),
        ConfigValueType::BoolOrInt => sley_config::parse_config_bool_or_int(value).is_some(),
        ConfigValueType::Color => try_format_config_color_value(value).is_ok(),
        ConfigValueType::Path => config_path_value_can_expand(value),
        ConfigValueType::ExpiryDate => {
            // Side-effect-free: accept exactly what git's approxidate accepts
            // (absolute dates, relative dates, the never/now sentinels), without
            // invoking the formatter (which prints a diagnostic on failure).
            super::approxidate::parse_expiry_date(value).is_some()
        }
    }
}

fn value_less_canonicalizes_as(value_type: ConfigValueType) -> bool {
    matches!(
        value_type,
        ConfigValueType::Raw | ConfigValueType::Bool | ConfigValueType::BoolOrInt
    )
}

/// Format a typed value, attributing parse failures to the key and the
/// config source the value came from (git's `die_bad_number` /
/// `git_config_bool`).
fn format_config_value_with(
    value: &str,
    value_type: ConfigValueType,
    name: Option<&str>,
    origin: Option<&sley_config::ConfigOrigin>,
) -> Result<String> {
    match value_type {
        ConfigValueType::Raw => Ok(value.to_string()),
        ConfigValueType::Bool => match sley_config::parse_config_bool(value) {
            Some(true) => Ok("true".into()),
            Some(false) => Ok("false".into()),
            None => config_bad_bool_value(value, name),
        },
        ConfigValueType::Int => sley_config::parse_config_int(value)
            .map(|value| value.to_string())
            .ok_or_else(|| config_bad_numeric_value(value, name, origin)),
        ConfigValueType::BoolOrInt => match sley_config::parse_config_bool_or_int(value) {
            Some(ConfigBoolOrInt::Bool(true)) => Ok("true".into()),
            Some(ConfigBoolOrInt::Bool(false)) => Ok("false".into()),
            Some(ConfigBoolOrInt::Int(value)) => Ok(value.to_string()),
            None => Err(config_bad_numeric_value(value, name, origin)),
        },
        ConfigValueType::ExpiryDate => format_config_expiry_date_value(value),
        ConfigValueType::Color => format_config_color_value(value),
        ConfigValueType::Path => format_config_path_value(value).map(std::borrow::Cow::into_owned),
    }
}

fn config_bad_bool_value<T>(value: &str, name: Option<&str>) -> Result<T> {
    match name {
        Some(name) => eprintln!("fatal: bad boolean config value '{value}' for '{name}'"),
        None => eprintln!("fatal: bad boolean config value '{value}'"),
    }
    Err(GitError::Exit(128))
}

/// git's `die_bad_number`: the message names the key and the source when they
/// are known ("in file .git/config", "in blob <spec>", "in standard input").
fn config_bad_numeric_value(
    value: &str,
    name: Option<&str>,
    origin: Option<&sley_config::ConfigOrigin>,
) -> GitError {
    let location = origin.and_then(|origin| match origin.kind {
        sley_config::ConfigOriginKind::File if !origin.name.is_empty() => {
            Some(format!(" in file {}", origin.name))
        }
        sley_config::ConfigOriginKind::Blob if !origin.name.is_empty() => {
            Some(format!(" in blob {}", origin.name))
        }
        sley_config::ConfigOriginKind::Stdin => Some(" in standard input".to_string()),
        _ => None,
    });
    match (name, location) {
        (Some(name), Some(location)) => eprintln!(
            "fatal: bad numeric config value '{value}' for '{name}'{location}: invalid unit"
        ),
        (Some(name), None) => {
            eprintln!("fatal: bad numeric config value '{value}' for '{name}': invalid unit")
        }
        _ => eprintln!("fatal: bad numeric config value '{value}': invalid unit"),
    }
    GitError::Exit(128)
}

fn format_config_expiry_date_value(value: &str) -> Result<String> {
    // git's `parse_expiry_date` → `approxidate_careful`: canonicalise the value
    // through approxidate and print the resulting timestamp (`%"PRItime"`). The
    // `now`/`all` sentinel renders as the unsigned `TIME_MAX` (u64::MAX).
    match super::approxidate::format_expiry_date(value) {
        Some(formatted) => Ok(formatted),
        None => Err(config_bad_expiry_date_value(value)),
    }
}

fn config_bad_expiry_date_value(value: &str) -> GitError {
    eprintln!("error: '{value}' is not a valid timestamp");
    GitError::Exit(128)
}

fn format_config_default_color_value(value: &str) -> Result<String> {
    format_config_color_value(value).map_err(|_| {
        eprintln!("error: unable to parse default color value");
        GitError::Exit(255)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigColorBoolSetting {
    Never,
    Always,
    Auto,
}

fn config_colorbool_setting(key: &ConfigKey, value: &str) -> Result<ConfigColorBoolSetting> {
    if value.eq_ignore_ascii_case("never") {
        return Ok(ConfigColorBoolSetting::Never);
    }
    if value.eq_ignore_ascii_case("always") {
        return Ok(ConfigColorBoolSetting::Always);
    }
    if value.eq_ignore_ascii_case("auto") {
        return Ok(ConfigColorBoolSetting::Auto);
    }
    match sley_config::parse_config_bool(value) {
        Some(false) => Ok(ConfigColorBoolSetting::Never),
        Some(true) => Ok(ConfigColorBoolSetting::Auto),
        None => {
            eprintln!(
                "fatal: bad boolean config value '{value}' for '{}'",
                config_key_name(key)
            );
            Err(GitError::Exit(128))
        }
    }
}

fn config_colorbool_enabled(setting: ConfigColorBoolSetting, stdout_is_tty: Option<bool>) -> bool {
    match setting {
        ConfigColorBoolSetting::Never => false,
        ConfigColorBoolSetting::Always => true,
        // Mirror git's `check_auto_color`: auto requires stdout to be a tty *and*
        // a usable terminal (TERM is set and not "dumb"). The `--get-colorbool`
        // second argument supplies the tty hint; otherwise probe the real stdout.
        ConfigColorBoolSetting::Auto => {
            let is_tty = stdout_is_tty.unwrap_or_else(|| io::stdout().is_terminal());
            is_tty && config_auto_color_term_ok()
        }
    }
}

/// Match git's `check_auto_color` terminal gate: color is permitted when `TERM`
/// is present in the environment and is not exactly `"dumb"`. An empty `TERM`
/// counts as present (git uses `term && strcmp(term, "dumb")`), while an unset
/// `TERM` disables color.
fn config_auto_color_term_ok() -> bool {
    env::var_os("TERM").is_some_and(|term| term != "dumb")
}

fn format_config_color_value(value: &str) -> Result<String> {
    try_format_config_color_value(value).map_err(|()| config_bad_color_value(value))
}

/// The side-effect-free core of [`format_config_color_value`]: returns `Err(())`
/// (no diagnostic) when the value is not a valid color, so callers like
/// `--list --type=color` can silently skip it.
pub(crate) fn try_format_config_color_value(value: &str) -> std::result::Result<String, ()> {
    try_git_color_spec_to_ansi(value).map_err(|_| ())
}

fn config_bad_color_value(value: &str) -> GitError {
    eprintln!("error: invalid color value: {value}");
    GitError::Exit(128)
}

fn format_config_path_value(value: &str) -> Result<std::borrow::Cow<'_, str>> {
    if let Some(optional) = strip_config_optional_path(value) {
        if optional_path_exists(optional) {
            return Ok(std::borrow::Cow::Borrowed(optional));
        }
        return Err(GitError::Exit(1));
    }
    if value == "~" {
        return expand_config_home_path(value, "");
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return expand_config_home_path(value, rest);
    }
    Ok(std::borrow::Cow::Borrowed(value))
}

fn config_path_value_can_expand(value: &str) -> bool {
    if let Some(optional) = strip_config_optional_path(value) {
        return optional_path_exists(optional);
    }
    !(value == "~" || value.starts_with("~/")) || env::var_os("HOME").is_some()
}

fn strip_config_optional_path(value: &str) -> Option<&str> {
    value.strip_prefix(":(optional)")
}

fn optional_path_exists(value: &str) -> bool {
    !value.is_empty() && Path::new(value).exists()
}

fn expand_config_home_path<'a>(original: &str, rest: &str) -> Result<std::borrow::Cow<'a, str>> {
    let Some(home) = env::var_os("HOME") else {
        eprintln!("error: failed to expand user dir in: '{original}'");
        return Err(GitError::Exit(128));
    };
    Ok(std::borrow::Cow::Owned(
        PathBuf::from(home).join(rest).display().to_string(),
    ))
}

fn config_missing_path_value<T>(name: &str, entry: &sley_config::ConfigStackEntry) -> Result<T> {
    eprintln!("error: missing value for '{name}'");
    match entry.origin.kind {
        sley_config::ConfigOriginKind::File if !entry.origin.name.is_empty() => {
            if let Some(line) = config_entry_physical_line(Path::new(&entry.origin.name), entry) {
                eprintln!(
                    "fatal: bad config line {line} in file {}",
                    entry.origin.name
                );
            } else {
                eprintln!("fatal: bad config line in file {}", entry.origin.name);
            }
        }
        sley_config::ConfigOriginKind::Stdin => {
            eprintln!("fatal: bad config line in standard input");
        }
        _ => {
            eprintln!("fatal: bad config line");
        }
    }
    Err(GitError::Exit(128))
}

fn config_entry_physical_line(
    path: &Path,
    target: &sley_config::ConfigStackEntry,
) -> Option<usize> {
    let text = fs::read_to_string(path).ok()?;
    let mut section = String::new();
    let mut subsection = None::<String>;
    for (line_index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') {
            if let Some((next_section, next_subsection)) =
                parse_config_header_for_line_scan(trimmed)
            {
                section = next_section;
                subsection = next_subsection;
            }
            continue;
        }
        if !section.eq_ignore_ascii_case(&target.section)
            || subsection.as_deref() != target.subsection.as_deref()
        {
            continue;
        }
        let Some(key) = config_line_key_for_line_scan(trimmed) else {
            continue;
        };
        if key.eq_ignore_ascii_case(&target.key) {
            return Some(line_index + 1);
        }
    }
    None
}

fn parse_config_header_for_line_scan(line: &str) -> Option<(String, Option<String>)> {
    let close = line.find(']')?;
    let content = line.get(1..close)?.trim();
    let mut parts = content.splitn(2, char::is_whitespace);
    let section = parts.next()?.trim();
    let rest = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(rest) = rest {
        let subsection = match rest
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            Some(quoted) => unescape_config_subsection_for_line_scan(quoted),
            None => rest.to_string(),
        };
        return Some((section.to_string(), Some(subsection)));
    }
    match content.split_once('.') {
        Some((section, subsection)) => Some((section.to_string(), Some(subsection.to_string()))),
        None => Some((section.to_string(), None)),
    }
}

fn unescape_config_subsection_for_line_scan(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

fn config_line_key_for_line_scan(line: &str) -> Option<&str> {
    let end = match line
        .find(|ch: char| ch == '=' || ch.is_ascii_whitespace() || ch == '#' || ch == ';')
    {
        Some(end) => end,
        None => line.len(),
    };
    let key = line.get(..end)?;
    (!key.is_empty()).then_some(key)
}

#[derive(Debug)]
pub(crate) struct ConfigKey {
    pub(crate) section: String,
    pub(crate) subsection: Option<String>,
    pub(crate) key: String,
}

#[derive(Debug)]
pub(crate) struct ConfigSectionName {
    section: String,
    subsection: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ConfigUrlMatchTarget {
    section: String,
    key: Option<String>,
}

pub(crate) fn parse_config_key(value: &str) -> Result<ConfigKey> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        eprintln!("error: key does not contain a section: {value}");
        return Err(GitError::Exit(1));
    }
    let section = parts[0].to_string();
    let key = parts[parts.len() - 1].to_string();
    if key.is_empty() {
        eprintln!("error: key does not contain variable name: {value}");
        return Err(GitError::Exit(1));
    }
    if validate_config_name(&section).is_err() || validate_config_key_name(&key).is_err() {
        eprintln!("error: invalid key: {value}");
        return Err(GitError::Exit(1));
    }
    let subsection = if parts.len() > 2 {
        let subsection = parts[1..parts.len() - 1].join(".");
        if subsection
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
        {
            eprintln!("error: invalid key: {value}");
            return Err(GitError::Exit(1));
        }
        Some(subsection)
    } else {
        None
    };
    Ok(ConfigKey {
        section,
        subsection,
        key,
    })
}

fn config_key_is_valid(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        return false;
    }
    let section = parts[0];
    let key = parts[parts.len() - 1];
    if key.is_empty()
        || validate_config_name(section).is_err()
        || validate_config_key_name(key).is_err()
    {
        return false;
    }
    if parts.len() > 2 {
        let subsection = parts[1..parts.len() - 1].join(".");
        if subsection
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
        {
            return false;
        }
    }
    true
}

fn config_missing_set_value(value: &str) -> Result<()> {
    let split = value.split_once('=');
    let valid_name = match split {
        Some((name, _)) => config_key_is_valid(name),
        None => config_key_is_valid(value),
    };
    if valid_name {
        eprintln!("error: missing value to set to the variable '{value}'");
    } else {
        eprintln!("error: missing value to set to a variable with an invalid name '{value}'");
    }
    if let Some((name, suggested_value)) = split
        && valid_name
    {
        eprintln!("hint: did you mean \"git config set {name} {suggested_value}\"?");
    }
    Err(GitError::Exit(129))
}

fn parse_config_urlmatch_target(value: &str) -> Result<ConfigUrlMatchTarget> {
    let mut parts = value.splitn(2, '.');
    let section = parts.next().unwrap_or_default().to_string();
    validate_config_name(&section)?;
    let key = parts.next().map(str::to_string);
    if let Some(key) = &key {
        validate_config_name(key)?;
    }
    Ok(ConfigUrlMatchTarget { section, key })
}

fn parse_config_section_name(value: &str) -> Result<ConfigSectionName> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.is_empty() {
        return Err(GitError::InvalidFormat("config section is invalid".into()));
    }
    let section = parts[0].to_string();
    validate_config_name(&section)?;
    let subsection = if parts.len() > 1 {
        let subsection = parts[1..].join(".");
        if subsection.is_empty()
            || subsection
                .bytes()
                .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
        {
            return Err(GitError::InvalidFormat(
                "config subsection is invalid".into(),
            ));
        }
        Some(subsection)
    } else {
        None
    };
    Ok(ConfigSectionName {
        section,
        subsection,
    })
}

fn validate_config_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(GitError::InvalidFormat("config name is invalid".into()));
    }
    Ok(())
}

fn validate_config_key_name(value: &str) -> Result<()> {
    if validate_config_name(value).is_err()
        || !value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
    {
        return Err(GitError::InvalidFormat("config key name is invalid".into()));
    }
    Ok(())
}

fn config_section_matches(section: &ConfigSection, key: &ConfigKey) -> bool {
    section.name.eq_ignore_ascii_case(&key.section)
        && section.subsection.as_deref() == key.subsection.as_deref()
}

fn config_get_urlmatch(
    entries: &[sley_config::ConfigStackEntry],
    target: &ConfigUrlMatchTarget,
    url: &str,
    null_terminate: bool,
    display: ConfigDisplayOptions,
    value_type: ConfigValueType,
) -> Result<bool> {
    let mut values =
        BTreeMap::<String, (ConfigUrlMatchScore, Option<String>, ConfigValueMeta)>::new();
    for entry in entries {
        if !entry.section.eq_ignore_ascii_case(&target.section) {
            continue;
        }
        let score = match entry.subsection.as_deref() {
            None => ConfigUrlMatchScore::default(),
            Some(base) => match config_urlmatch_score(base, url) {
                Some(score) => score,
                None => continue,
            },
        };
        if let Some(key) = &target.key
            && !entry.key.eq_ignore_ascii_case(key)
        {
            continue;
        }
        let name = format!(
            "{}.{}",
            target.section.to_ascii_lowercase(),
            entry.key.to_ascii_lowercase()
        );
        let replace = values
            .get(&name)
            .is_none_or(|(previous_score, _, _)| score >= *previous_score);
        if replace {
            values.insert(
                name,
                (score, entry.value.clone(), ConfigValueMeta::of(entry)),
            );
        }
    }
    if values.is_empty() {
        return Ok(false);
    }
    // git's `get_urlmatch`: a concrete `section.key` prints the value alone,
    // while a bare section dumps `key value` pairs — both via `format_config`,
    // so `--show-scope` prefixes and `--type` formatting apply.
    let show_keys = target.key.is_none();
    let mut stdout = io::stdout();
    for (name, (_, value, meta)) in &values {
        write_config_entry(
            &mut stdout,
            meta,
            name,
            value.as_deref(),
            ConfigEntryWriteOptions {
                display,
                name_only: false,
                show_keys,
                value_type,
                null_terminate,
                equals_separator: false,
            },
        )?;
        if target.key.is_some() {
            break;
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct ConfigUrlMatchScore {
    host_len: usize,
    path_len: usize,
    user_matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigUrlParts {
    scheme: String,
    user: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
}

fn config_urlmatch_score(base: &str, url: &str) -> Option<ConfigUrlMatchScore> {
    let Some(base_url) = parse_config_url(base) else {
        return url.starts_with(base).then_some(ConfigUrlMatchScore {
            host_len: 0,
            path_len: base.len(),
            user_matched: false,
        });
    };
    let url = parse_config_url(url)?;
    if base_url.scheme != url.scheme || base_url.port != url.port {
        return None;
    }
    let user_matched = if let Some(base_user) = base_url.user.as_deref() {
        if Some(base_user) != url.user.as_deref() {
            return None;
        }
        true
    } else {
        false
    };
    if !config_url_host_matches(&url.host, &base_url.host) {
        return None;
    }
    let path_len = config_url_path_match_len(&url.path, &base_url.path)?;
    Some(ConfigUrlMatchScore {
        host_len: base_url.host.len(),
        path_len,
        user_matched,
    })
}

fn config_url_host_matches(url_host: &str, pattern_host: &str) -> bool {
    let mut url_parts = url_host.split('.');
    let mut pattern_parts = pattern_host.split('.');
    loop {
        match (url_parts.next(), pattern_parts.next()) {
            (None, None) => return true,
            (Some(_), None) | (None, Some(_)) => return false,
            (Some(_), Some("*")) => {}
            (Some(url_part), Some(pattern_part)) if url_part == pattern_part => {}
            _ => return false,
        }
    }
}

fn config_url_path_match_len(url_path: &str, base_path: &str) -> Option<usize> {
    let base_path = if base_path.is_empty() { "/" } else { base_path };
    if base_path == "/" {
        return if url_path.is_empty() || url_path.starts_with('/') {
            Some(1)
        } else {
            None
        };
    }
    let base_path = match base_path.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => base_path,
    };
    if !url_path.starts_with(base_path) {
        return None;
    }
    if url_path.len() == base_path.len()
        || url_path
            .as_bytes()
            .get(base_path.len())
            .is_some_and(|byte| *byte == b'/')
    {
        return Some(base_path.len() + 1);
    }
    None
}

fn parse_config_url(value: &str) -> Option<ConfigUrlParts> {
    let (scheme, rest) = value.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }
    let (user, authority) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    let (host, port) = parse_config_url_authority(authority)?;
    let mut path = &rest[authority_end..];
    if let Some(end) = path.find(['?', '#']) {
        path = &path[..end];
    }
    if path.is_empty() {
        path = "/";
    }
    let scheme = scheme.to_ascii_lowercase();
    Some(ConfigUrlParts {
        port: normalize_config_url_port(&scheme, port),
        scheme,
        user: user.map(|user| user.to_ascii_lowercase()),
        host: host.to_ascii_lowercase(),
        path: decode_config_url_path(path),
    })
}

fn parse_config_url_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = if rest.is_empty() {
            None
        } else {
            let port = rest.strip_prefix(':')?;
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            port.parse().ok()
        };
        return Some((host, port));
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some((host, port.parse().ok()));
    }
    Some((authority, None))
}

fn normalize_config_url_port(scheme: &str, port: Option<u16>) -> Option<u16> {
    match (scheme, port) {
        ("http", Some(80)) | ("https", Some(443)) => None,
        _ => port,
    }
}

fn decode_config_url_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                config_url_hex_value(bytes[idx + 1]),
                config_url_hex_value(bytes[idx + 2]),
            )
        {
            let decoded = (high << 4) | low;
            if decoded == b'/' {
                out.push('%');
                out.push((bytes[idx + 1] as char).to_ascii_uppercase());
                out.push((bytes[idx + 2] as char).to_ascii_uppercase());
            } else {
                out.push(decoded as char);
            }
            idx += 3;
            continue;
        }
        out.push(bytes[idx] as char);
        idx += 1;
    }
    out
}

fn config_url_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn config_get_regexp(
    entries: &[sley_config::ConfigStackEntry],
    pattern: &str,
    value_filter: Option<&ConfigValuePatternFilter>,
    display: ConfigDisplayOptions,
    name_only: bool,
    value_type: ConfigValueType,
    null_terminate: bool,
) -> Result<bool> {
    let regex = SimpleConfigRegex::parse(pattern);
    let mut matched = false;
    let mut stdout = io::stdout();
    for entry in entries {
        let name = stack_entry_name(entry);
        if !regex.is_match(&name) {
            continue;
        }
        // `--get-regexp <name-regex> <value-pattern>` additionally filters
        // on the value, matching git's shared `get_value` collector.
        if let Some(filter) = value_filter
            && !filter.matches(entry.value.as_deref())
        {
            continue;
        }
        matched = true;
        write_config_entry(
            &mut stdout,
            &ConfigValueMeta::of(entry),
            &name,
            entry.value.as_deref(),
            ConfigEntryWriteOptions {
                display,
                name_only,
                show_keys: true,
                value_type,
                null_terminate,
                equals_separator: false,
            },
        )?;
    }
    Ok(matched)
}

/// How the modern `git config get` subcommand selects a key: either an exact
/// key (the default) or a regular expression over the full `section.var` name
/// (`--regexp`).
enum SubcommandGetKey {
    Exact(ConfigKey),
    Regexp(SimpleConfigRegex),
}

/// A value-pattern filter shared by the modern `git config get --value=<pat>`
/// and the classic `--get-regexp <name-regex> <value-pattern>`. Git compiles
/// the pattern as an extended regular expression (a leading `!` negates the
/// match) unless `--fixed-value` requests exact string equality.
struct ConfigValuePatternFilter {
    matcher: ConfigValueMatcher,
    negated: bool,
}

impl ConfigValuePatternFilter {
    fn parse(pattern: &str, fixed_value: bool) -> Self {
        // git only honours the `!` negation prefix for the regexp form; under
        // `--fixed-value` the whole pattern (including a leading `!`) is matched
        // literally.
        if !fixed_value && let Some(rest) = pattern.strip_prefix('!') {
            return Self {
                matcher: ConfigValueMatcher::parse(rest, false),
                negated: true,
            };
        }
        Self {
            matcher: ConfigValueMatcher::parse(pattern, fixed_value),
            negated: false,
        }
    }

    fn matches(&self, value: Option<&str>) -> bool {
        // git compares the pattern against the empty string for value-less
        // entries (`value_ ? value_ : ""`).
        let matched = self.matcher.is_match(value.unwrap_or(""));
        matched ^ self.negated
    }
}

/// Handle the modern `git config get` subcommand (git 2.54), which unifies what
/// the classic flags split across `--get`, `--get-all`, and `--get-regexp`.
///
/// * `--regexp` reinterprets the name as a key pattern.
/// * `--all` prints every match (otherwise only the final one is shown).
/// * `--show-names` prefixes each value with its key; `--name-only` prints the
///   key alone. Without `--show-names` only values are printed.
/// * `--value=<pattern>` filters matches by value.
///
/// Returns `false` (mapped by the caller to exit code 1) when nothing matched.
#[allow(clippy::too_many_arguments)]
fn config_subcommand_get(
    entries: &[sley_config::ConfigStackEntry],
    key: SubcommandGetKey,
    value_filter: Option<&ConfigValuePatternFilter>,
    display: ConfigDisplayOptions,
    all: bool,
    show_keys: bool,
    name_only: bool,
    value_type: ConfigValueType,
    default_value: Option<&str>,
    null_terminate: bool,
) -> Result<bool> {
    // Collect matching (name, value) pairs in config (file) order, exactly as
    // git's `collect_config` callback does, so that "last match wins" without
    // `--all` picks the same entry git would.
    let mut matches: Vec<ConfigGetMatch> = Vec::new();
    for entry in entries {
        let name = stack_entry_name(entry);
        let key_matches = match &key {
            SubcommandGetKey::Exact(exact) => {
                entry.matches(&exact.section, exact.subsection.as_deref(), &exact.key)
            }
            SubcommandGetKey::Regexp(regex) => regex.is_match(&name),
        };
        if !key_matches {
            continue;
        }
        if let Some(filter) = value_filter
            && !filter.matches(entry.value.as_deref())
        {
            continue;
        }
        let Some(value) = format_config_entry_value(entry, value_type, &name)? else {
            continue;
        };
        matches.push(ConfigGetMatch {
            name,
            value,
            meta: ConfigValueMeta::of(entry),
        });
    }

    // git falls back to `--default` only when nothing matched. The default is
    // attributed to the requested name (for an exact key) so `--show-names`
    // renders it; under `--regexp` there is no single key, matching git which
    // disallows `--default` together with `--all`/`--url` but still formats the
    // default against the pattern string. Defaults belong to the command line.
    if matches.is_empty()
        && let Some(default) = default_value
    {
        let name = match &key {
            SubcommandGetKey::Exact(exact) => config_key_name(exact),
            SubcommandGetKey::Regexp(_) => String::new(),
        };
        if let Some(value) = format_default_config_value(default, value_type)? {
            matches.push(ConfigGetMatch {
                name,
                value: ConfigFormattedValue::Value(value),
                meta: ConfigValueMeta::command_line(),
            });
        }
    }

    if matches.is_empty() {
        return Ok(false);
    }

    let mut stdout = io::stdout();
    let last = matches.len() - 1;
    for (idx, matched) in matches.iter().enumerate() {
        if !all && idx != last {
            continue;
        }
        write_config_formatted_entry(
            &mut stdout,
            &matched.meta,
            &matched.name,
            &matched.value,
            ConfigEntryWriteOptions {
                display,
                name_only,
                show_keys,
                value_type,
                null_terminate,
                equals_separator: false,
            },
        )?;
    }
    Ok(true)
}

#[derive(Debug)]
pub(crate) struct SimpleConfigRegex {
    anchor_start: bool,
    anchor_end: bool,
    tokens: Vec<SimpleConfigRegexToken>,
}

/// One matchable atom plus an optional repetition quantifier, mirroring the
/// POSIX-ERE subset git's config code compiles with `regcomp(REG_EXTENDED)`:
/// literals, `.`, bracket classes `[...]`/`[^...]` (with `a-z` ranges), and the
/// `*` / `+` / `?` quantifiers applied to the preceding atom.
#[derive(Debug)]
struct SimpleConfigRegexToken {
    atom: SimpleConfigRegexAtom,
    quantifier: SimpleConfigRegexQuantifier,
}

#[derive(Debug)]
enum SimpleConfigRegexAtom {
    Literal(u8),
    Any,
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimpleConfigRegexQuantifier {
    /// Exactly one (no quantifier).
    One,
    /// `*` — zero or more.
    Star,
    /// `+` — one or more.
    Plus,
    /// `?` — zero or one.
    Question,
}

impl SimpleConfigRegexAtom {
    fn matches(&self, byte: u8) -> bool {
        match self {
            SimpleConfigRegexAtom::Literal(expected) => byte == *expected,
            SimpleConfigRegexAtom::Any => true,
            SimpleConfigRegexAtom::Class { negated, ranges } => {
                let inside = ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&byte));
                inside ^ negated
            }
        }
    }
}

#[derive(Debug)]
enum ConfigValueMatcher {
    Regex(SimpleConfigRegex),
    Fixed(String),
}

impl ConfigValueMatcher {
    fn parse(pattern: &str, fixed_value: bool) -> Self {
        if fixed_value {
            Self::Fixed(pattern.to_string())
        } else {
            Self::Regex(SimpleConfigRegex::parse(pattern))
        }
    }

    fn is_match(&self, value: &str) -> bool {
        match self {
            Self::Regex(regex) => regex.is_match(value),
            Self::Fixed(pattern) => value == pattern,
        }
    }
}

impl SimpleConfigRegex {
    pub(crate) fn parse(pattern: &str) -> Self {
        let mut bytes = pattern.as_bytes();
        let anchor_start = bytes.first().copied() == Some(b'^');
        if anchor_start {
            bytes = &bytes[1..];
        }
        let anchor_end = has_unescaped_trailing_dollar(bytes);
        if anchor_end {
            bytes = &bytes[..bytes.len() - 1];
        }
        let mut tokens = Vec::new();
        let mut idx = 0;
        while idx < bytes.len() {
            let atom = match bytes[idx] {
                b'\\' if idx + 1 < bytes.len() => {
                    idx += 1;
                    let literal = SimpleConfigRegexAtom::Literal(bytes[idx]);
                    idx += 1;
                    literal
                }
                b'[' => {
                    if let Some((class, next)) = parse_config_regex_class(bytes, idx) {
                        idx = next;
                        class
                    } else {
                        // An unterminated `[` is a literal bracket.
                        idx += 1;
                        SimpleConfigRegexAtom::Literal(b'[')
                    }
                }
                b'.' => {
                    idx += 1;
                    SimpleConfigRegexAtom::Any
                }
                byte => {
                    idx += 1;
                    SimpleConfigRegexAtom::Literal(byte)
                }
            };
            // A quantifier (if present) binds to the atom just parsed.
            let quantifier = match bytes.get(idx) {
                Some(b'*') => {
                    idx += 1;
                    SimpleConfigRegexQuantifier::Star
                }
                Some(b'+') => {
                    idx += 1;
                    SimpleConfigRegexQuantifier::Plus
                }
                Some(b'?') => {
                    idx += 1;
                    SimpleConfigRegexQuantifier::Question
                }
                _ => SimpleConfigRegexQuantifier::One,
            };
            tokens.push(SimpleConfigRegexToken { atom, quantifier });
        }
        Self {
            anchor_start,
            anchor_end,
            tokens,
        }
    }

    pub(crate) fn is_match(&self, value: &str) -> bool {
        let bytes = value.as_bytes();
        if self.anchor_start {
            return self.match_from(bytes, 0, 0);
        }
        (0..=bytes.len()).any(|start| self.match_from(bytes, 0, start))
    }

    fn match_from(&self, bytes: &[u8], token_idx: usize, byte_idx: usize) -> bool {
        let Some(token) = self.tokens.get(token_idx) else {
            return !self.anchor_end || byte_idx == bytes.len();
        };
        let here_matches =
            |idx: usize| bytes.get(idx).is_some_and(|byte| token.atom.matches(*byte));
        match token.quantifier {
            SimpleConfigRegexQuantifier::One => {
                here_matches(byte_idx) && self.match_from(bytes, token_idx + 1, byte_idx + 1)
            }
            SimpleConfigRegexQuantifier::Question => {
                // Greedy: try consuming one, then fall back to zero.
                (here_matches(byte_idx) && self.match_from(bytes, token_idx + 1, byte_idx + 1))
                    || self.match_from(bytes, token_idx + 1, byte_idx)
            }
            SimpleConfigRegexQuantifier::Star | SimpleConfigRegexQuantifier::Plus => {
                // Greedy: consume as many matching bytes as possible, then
                // backtrack toward the minimum (0 for `*`, 1 for `+`).
                let min = if token.quantifier == SimpleConfigRegexQuantifier::Plus {
                    1
                } else {
                    0
                };
                let mut end = byte_idx;
                while here_matches(end) {
                    end += 1;
                }
                let mut count = end - byte_idx;
                loop {
                    if count >= min && self.match_from(bytes, token_idx + 1, byte_idx + count) {
                        return true;
                    }
                    if count == 0 {
                        return false;
                    }
                    count -= 1;
                }
            }
        }
    }
}

struct ConfigGetMatch {
    name: String,
    value: ConfigFormattedValue,
    meta: ConfigValueMeta,
}

enum ConfigFormattedValue {
    NoValue,
    Value(String),
}

fn format_config_entry_value(
    entry: &sley_config::ConfigStackEntry,
    value_type: ConfigValueType,
    name: &str,
) -> Result<Option<ConfigFormattedValue>> {
    let value = entry.value.as_deref();
    match value {
        None if matches!(
            value_type,
            ConfigValueType::Bool | ConfigValueType::BoolOrInt
        ) =>
        {
            Ok(Some(ConfigFormattedValue::Value("true".to_string())))
        }
        None if value_type == ConfigValueType::Path => config_missing_path_value(name, entry),
        None => Ok(Some(ConfigFormattedValue::NoValue)),
        Some(value) if value_type == ConfigValueType::Path => {
            let Some(formatted) = format_config_path_output_value(value)? else {
                return Ok(None);
            };
            Ok(Some(ConfigFormattedValue::Value(formatted)))
        }
        Some(value) => Ok(Some(ConfigFormattedValue::Value(format_config_value_with(
            value,
            value_type,
            Some(name),
            Some(&entry.origin),
        )?))),
    }
}

fn format_default_config_value(value: &str, value_type: ConfigValueType) -> Result<Option<String>> {
    if value_type == ConfigValueType::Path {
        if let Some(formatted) = format_config_path_output_value(value)? {
            return Ok(Some(formatted));
        }
        return Ok(None);
    }
    if value_type != ConfigValueType::Raw && !value_canonicalizes_as(value, value_type) {
        if matches!(
            value_type,
            ConfigValueType::Bool | ConfigValueType::Int | ConfigValueType::BoolOrInt
        ) {
            return format_config_value_with(value, value_type, None, None).map(Some);
        }
        eprintln!("fatal: failed to format default config value: {value}");
        return Err(GitError::Exit(128));
    }
    format_config_value_with(value, value_type, None, None).map(Some)
}

fn format_config_path_output_value(value: &str) -> Result<Option<String>> {
    if let Some(optional) = strip_config_optional_path(value)
        && !optional_path_exists(optional)
    {
        return Ok(None);
    }
    format_config_path_value(value).map(|value| Some(value.into_owned()))
}

/// Parse a bracket expression `[...]` starting at `start` (which must point at
/// `[`). Returns the atom and the index just past the closing `]`, or `None`
/// when the class is unterminated. Mirrors POSIX-ERE basics: a leading `^`
/// negates, a `]` immediately after the (optional) `^` is a literal, and `a-z`
/// forms a range.
fn parse_config_regex_class(bytes: &[u8], start: usize) -> Option<(SimpleConfigRegexAtom, usize)> {
    let mut idx = start + 1;
    let negated = bytes.get(idx) == Some(&b'^');
    if negated {
        idx += 1;
    }
    let mut ranges: Vec<(u8, u8)> = Vec::new();
    // A `]` as the very first class member is a literal, not the terminator.
    if bytes.get(idx) == Some(&b']') {
        ranges.push((b']', b']'));
        idx += 1;
    }
    while let Some(&byte) = bytes.get(idx) {
        if byte == b']' {
            return Some((SimpleConfigRegexAtom::Class { negated, ranges }, idx + 1));
        }
        // `a-z` range, but only when `-` is not trailing (a trailing `-` before
        // `]` is a literal hyphen).
        if bytes.get(idx + 1) == Some(&b'-') && bytes.get(idx + 2).is_some_and(|&end| end != b']') {
            let end = bytes[idx + 2];
            let (lo, hi) = if byte <= end {
                (byte, end)
            } else {
                (end, byte)
            };
            ranges.push((lo, hi));
            idx += 3;
        } else {
            ranges.push((byte, byte));
            idx += 1;
        }
    }
    None
}

pub(crate) fn has_unescaped_trailing_dollar(bytes: &[u8]) -> bool {
    if bytes.last().copied() != Some(b'$') {
        return false;
    }
    let mut backslashes = 0;
    let mut idx = bytes.len().saturating_sub(1);
    while idx > 0 && bytes[idx - 1] == b'\\' {
        backslashes += 1;
        idx -= 1;
    }
    backslashes % 2 == 0
}

fn config_list(
    entries: &[sley_config::ConfigStackEntry],
    display: ConfigDisplayOptions,
    name_only: bool,
    null_terminate: bool,
    value_type: ConfigValueType,
) -> Result<()> {
    let mut stdout = io::stdout();
    for entry in entries {
        let name = stack_entry_name(entry);
        // git's `--list --type=<T>` shows only values that canonicalise as that
        // type, silently dropping the rest (e.g. `--type=int` skips non-ints).
        // The untyped default (`Raw`) prints everything. The check must be
        // side-effect-free — the formatter prints a `fatal:` diagnostic on
        // failure, which the list path must NOT emit.
        if value_type != ConfigValueType::Raw {
            match entry.value.as_deref() {
                Some(value) if !value_canonicalizes_as(value, value_type) => continue,
                None if !value_less_canonicalizes_as(value_type) => continue,
                _ => {}
            }
        }
        write_config_entry(
            &mut stdout,
            &ConfigValueMeta::of(entry),
            &name,
            entry.value.as_deref(),
            ConfigEntryWriteOptions {
                display,
                name_only,
                show_keys: true,
                value_type,
                null_terminate,
                equals_separator: true,
            },
        )?;
    }
    Ok(())
}

/// The last (highest-precedence) entry matching the key, including value-less
/// boolean-true entries.
fn entries_get<'a>(
    entries: &'a [sley_config::ConfigStackEntry],
    key: &ConfigKey,
) -> Option<&'a sley_config::ConfigStackEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.matches(&key.section, key.subsection.as_deref(), &key.key))
}

/// Every entry matching the key, in precedence order (lowest first).
fn entries_get_all<'a>(
    entries: &'a [sley_config::ConfigStackEntry],
    key: &ConfigKey,
) -> Vec<&'a sley_config::ConfigStackEntry> {
    entries
        .iter()
        .filter(|entry| entry.matches(&key.section, key.subsection.as_deref(), &key.key))
        .collect()
}

fn write_config_formatted_entry(
    stdout: &mut impl Write,
    meta: &ConfigValueMeta,
    name: &str,
    value: &ConfigFormattedValue,
    options: ConfigEntryWriteOptions,
) -> Result<()> {
    write_config_metadata(stdout, meta, options.display, options.null_terminate)?;
    let terminator = if options.null_terminate { '\0' } else { '\n' };
    if options.name_only {
        if options.show_keys {
            write!(stdout, "{name}")?;
        }
        write!(stdout, "{terminator}")?;
        return Ok(());
    }
    let key_delim = if options.null_terminate {
        '\n'
    } else if options.equals_separator {
        '='
    } else {
        ' '
    };
    if options.show_keys {
        write!(stdout, "{name}")?;
        if let ConfigFormattedValue::Value(value) = value {
            write!(stdout, "{key_delim}{value}")?;
        }
    } else if let ConfigFormattedValue::Value(value) = value {
        write!(stdout, "{value}")?;
    }
    write!(stdout, "{terminator}")?;
    Ok(())
}

fn write_config_entry(
    stdout: &mut impl Write,
    meta: &ConfigValueMeta,
    name: &str,
    value: Option<&str>,
    options: ConfigEntryWriteOptions,
) -> Result<()> {
    // Mirror git's `format_config`: metadata, then optionally the key, then the
    // key delimiter and the (typed) value, then the terminator. The key is only
    // emitted when `show_keys` is set; `name_only` (git's `omit_values`) stops
    // after the key and never prints a value or delimiter.
    write_config_metadata(stdout, meta, options.display, options.null_terminate)?;
    let terminator = if options.null_terminate { '\0' } else { '\n' };
    if options.name_only {
        if options.show_keys {
            write!(stdout, "{name}")?;
        }
        write!(stdout, "{terminator}")?;
        return Ok(());
    }
    // git uses `\n` between key and value under `-z`, `=` for `--list`, and a
    // space everywhere else.
    let key_delim = if options.null_terminate {
        '\n'
    } else if options.equals_separator {
        '='
    } else {
        ' '
    };
    let formatted_value = match value {
        None if matches!(
            options.value_type,
            ConfigValueType::Bool | ConfigValueType::BoolOrInt
        ) =>
        {
            Some("true".to_string())
        }
        None if options.value_type == ConfigValueType::Path => {
            eprintln!("error: missing value for '{name}'");
            return Err(GitError::Exit(128));
        }
        // A value-less entry with no requested type prints just the key (git
        // backs out the key delimiter), so there is no value to render.
        None => None,
        Some(value) => Some(format_config_value_with(
            value,
            options.value_type,
            Some(name),
            Some(&meta.origin),
        )?),
    };
    if options.show_keys {
        write!(stdout, "{name}")?;
        if let Some(value) = &formatted_value {
            write!(stdout, "{key_delim}{value}")?;
        }
    } else if let Some(value) = &formatted_value {
        write!(stdout, "{value}")?;
    }
    write!(stdout, "{terminator}")?;
    Ok(())
}

fn write_config_metadata(
    stdout: &mut impl Write,
    meta: &ConfigValueMeta,
    display: ConfigDisplayOptions,
    null_terminate: bool,
) -> Result<()> {
    if display.show_scope {
        write_config_metadata_field(stdout, meta.scope.name(), null_terminate)?;
    }
    if display.show_origin {
        write_config_metadata_field(
            stdout,
            &config_origin_display(&meta.origin, null_terminate),
            null_terminate,
        )?;
    }
    Ok(())
}

fn write_config_metadata_field(
    stdout: &mut impl Write,
    value: &str,
    null_terminate: bool,
) -> Result<()> {
    if null_terminate {
        write!(stdout, "{value}\0")?;
    } else {
        write!(stdout, "{value}\t")?;
    }
    Ok(())
}

/// `--show-origin` rendering: `<kind>:<name>`, with the name C-quoted exactly
/// as git's `quote_c_style` does — except under `-z`, where git emits it raw.
fn config_origin_display(origin: &sley_config::ConfigOrigin, null_terminate: bool) -> String {
    if null_terminate {
        format!("{}:{}", origin.kind.name(), origin.name)
    } else {
        format!(
            "{}:{}",
            origin.kind.name(),
            crate::status_quote_path(origin.name.as_bytes(), false)
        )
    }
}

/// The display name of a stack entry: section and key lower-cased, the
/// subsection byte-for-byte as written — git's canonical `--list` form.
fn stack_entry_name(entry: &sley_config::ConfigStackEntry) -> String {
    let section = entry.section.to_ascii_lowercase();
    let key = entry.key.to_ascii_lowercase();
    match &entry.subsection {
        Some(subsection) => format!("{section}.{subsection}.{key}"),
        None => format!("{section}.{key}"),
    }
}

pub(crate) fn config_entry_name(section: &ConfigSection, key: &str) -> String {
    // git canonicalises the section and variable (key) names to lower case on
    // display (`--list`, `--get-regexp`, `--name-only`), but leaves the
    // subsection byte-for-byte as written. Mirror that exactly.
    let section_name = section.name.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    match &section.subsection {
        Some(subsection) => format!("{section_name}.{subsection}.{key}"),
        None => format!("{section_name}.{key}"),
    }
}

fn config_key_name(key: &ConfigKey) -> String {
    match &key.subsection {
        Some(subsection) => format!("{}.{}.{}", key.section, subsection, key.key),
        None => format!("{}.{}", key.section, key.key),
    }
}

/// git's key spelling in `set`-related diagnostics: the section/variable are
/// lower-cased (subsection keeps its case), matching `git_config_parse_key`.
fn config_key_display(key: &ConfigKey) -> String {
    match &key.subsection {
        Some(subsection) => format!(
            "{}.{}.{}",
            key.section.to_ascii_lowercase(),
            subsection,
            key.key.to_ascii_lowercase()
        ),
        None => format!(
            "{}.{}",
            key.section.to_ascii_lowercase(),
            key.key.to_ascii_lowercase()
        ),
    }
}

fn print_config_multiple_values_error(key: &ConfigKey) {
    let name = config_key_display(key);
    eprintln!("warning: {name} has multiple values");
    eprintln!(
        "error: cannot overwrite multiple values with a single value\n       Use a regexp, --add or --replace-all to change {name}."
    );
}

/// git's `normalize_value` for the set path: `string`/`path`/`expiry-date` are
/// stored verbatim; `int`/`bool`/`bool-or-int` are canonicalised; `color` is
/// validated but stored as written.
fn normalize_set_value(
    key: &ConfigKey,
    value: &str,
    value_type: ConfigValueType,
) -> Result<String> {
    match value_type {
        ConfigValueType::Raw | ConfigValueType::Path | ConfigValueType::ExpiryDate => {
            Ok(value.to_string())
        }
        ConfigValueType::Color => {
            // Validate (git dies on a bad color) but store the original spelling.
            match try_format_config_color_value(value) {
                Ok(_) => Ok(value.to_string()),
                Err(()) => {
                    eprintln!("error: invalid color value: {value}");
                    eprintln!("fatal: cannot parse color '{value}'");
                    Err(GitError::Exit(128))
                }
            }
        }
        ConfigValueType::Int | ConfigValueType::Bool | ConfigValueType::BoolOrInt => {
            format_config_value_with(value, value_type, Some(&config_key_display(key)), None)
        }
    }
}

/// Adapt a [`ConfigValuePatternFilter`] into the value-matching predicate the raw
/// editor expects.
fn filter_predicate(filter: &ConfigValuePatternFilter) -> Box<dyn Fn(Option<&str>) -> bool + '_> {
    Box::new(move |value: Option<&str>| filter.matches(value))
}

/// git's `git_config_prepare_comment_string`: turn the user's `--comment=<text>`
/// into the exact suffix `write_pair` appends after the value.
///
/// * leading blanks followed by `#` → used verbatim (e.g. `\t# c` → `\t# c`);
/// * begins with `#` → a single space is prefixed (`#abc` → ` #abc`);
/// * otherwise → ` # <text>` (`find fish` → ` # find fish`).
///
/// Multi-line comments are rejected (git's `die`).
fn parse_config_comment(value: &str) -> Result<String> {
    if value.contains('\n') {
        eprintln!("fatal: no multi-line comment allowed: '{value}'");
        return Err(GitError::Exit(128));
    }
    let leading_blanks = value.len() - value.trim_start_matches([' ', '\t']).len();
    let prepared = if leading_blanks > 0 && value[leading_blanks..].starts_with('#') {
        value.to_string()
    } else if value.starts_with('#') {
        format!(" {value}")
    } else {
        format!(" # {value}")
    };
    Ok(prepared)
}

pub(crate) fn config_set_value(config: &mut GitConfig, key: &ConfigKey, value: &str, add: bool) {
    config_set_value_with_comment(config, key, value, add, None);
}

pub(crate) fn config_set_value_with_comment(
    config: &mut GitConfig,
    key: &ConfigKey,
    value: &str,
    add: bool,
    comment: Option<&str>,
) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|section| config_section_matches(section, key))
        .unwrap_or_else(|| {
            config.sections.push(ConfigSection::new(
                key.section.clone(),
                key.subsection.clone(),
                Vec::new(),
            ));
            config.sections.len() - 1
        });
    let section = &mut config.sections[section_idx];
    if !add {
        let mut replaced = false;
        section.entries.retain_mut(|entry| {
            if !entry.key.eq_ignore_ascii_case(&key.key) {
                return true;
            }
            if replaced {
                return false;
            }
            entry.key = key.key.clone();
            entry.value = Some(value.to_string());
            entry.comment = comment.map(str::to_string);
            replaced = true;
            true
        });
        if replaced {
            return;
        }
    }
    let mut entry = ConfigEntry::new(key.key.clone(), Some(value.to_string()));
    entry.comment = comment.map(str::to_string);
    section.entries.push(entry);
}
