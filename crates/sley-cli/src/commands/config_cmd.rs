//! `git config`: read and write repository configuration.

use crate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigAction {
    Get,
    GetColor,
    GetColorBool,
    GetUrlMatch,
    GetAll,
    GetRegexp,
    List,
    Set,
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
    Repository(PathBuf),
    File(PathBuf),
    /// `--global`: the single global config file git chose (the user file, or
    /// the XDG file when only it exists). Reads and writes both target this
    /// one file, exactly like git's `given_config_source.file`.
    Global(PathBuf),
    /// `--system`: `$GIT_CONFIG_SYSTEM` or `/etc/gitconfig`.
    System(PathBuf),
    Stdin,
}

/// Which config file family an explicit scope option selected. Git rejects
/// combining two of these (or a scope with `--file`) with "only one config
/// file at a time".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ConfigScope {
    #[default]
    Default,
    Global,
    System,
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
            ConfigAction::Set => Self::Set,
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

pub(crate) fn cmd_config(args: &[String]) -> Result<()> {
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
    let mut positional = Vec::new();
    // Subcommand-mode `git config get` filter options (git 2.54). These only
    // exist on the `get` subcommand; the classic flag form rejects them.
    let mut subcommand_get_regexp = false;
    let mut subcommand_show_names = false;
    let mut subcommand_value_pattern: Option<String> = None;
    let mut scope = ConfigScope::Default;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--local" => {}
            "--global" => scope = ConfigScope::Global,
            "--system" => scope = ConfigScope::System,
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
            "--all" if subcommand == Some(ConfigSubcommand::Get) => {
                action = Some(ConfigAction::GetAll);
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
            "--value" if subcommand == Some(ConfigSubcommand::Get) => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--value requires a value".into()))?;
                subcommand_value_pattern = Some(value.to_string());
            }
            value
                if subcommand == Some(ConfigSubcommand::Get)
                    && value.starts_with("--value=") =>
            {
                subcommand_value_pattern = Some(value["--value=".len()..].to_string());
            }
            "--name-only" => name_only = true,
            "--show-origin" => display.show_origin = true,
            "--show-scope" => display.show_scope = true,
            "--fixed-value" => fixed_value = true,
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
            "--bool" => value_type = ConfigValueType::Bool,
            "--int" => value_type = ConfigValueType::Int,
            "--bool-or-int" => value_type = ConfigValueType::BoolOrInt,
            "--expiry-date" => value_type = ConfigValueType::ExpiryDate,
            "--path" => value_type = ConfigValueType::Path,
            "--type" => {
                let kind = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--type requires a value".into()))?;
                value_type = parse_config_value_type(kind)?;
            }
            value if value.starts_with("--type=") => {
                let kind = value
                    .strip_prefix("--type=")
                    .ok_or_else(|| GitError::Command("--type requires a value".into()))?;
                value_type = parse_config_value_type(kind)?;
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
            value => positional.push(value),
        }
    }
    let action = if let Some(action) = action {
        action
    } else {
        match positional.len() {
            1 => ConfigAction::Get,
            2 => ConfigAction::Set,
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
        ConfigAction::Set | ConfigAction::Add if positional.len() != 2 => {
            return Err(GitError::Command(
                "config write action requires <key> <value>".into(),
            ));
        }
        ConfigAction::ReplaceAll if !(2..=3).contains(&positional.len()) => {
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
    if is_subcommand_get {
        // git's `cmd_config_get` validation order. `--fixed-value` is only
        // meaningful with a `--value=<pattern>`; `--default` cannot combine with
        // `--all`. Both abort via `die()` (exit 128).
        if fixed_value && subcommand_value_pattern.is_none() {
            eprintln!("fatal: --fixed-value only applies with 'value-pattern'");
            return Err(GitError::Exit(128));
        }
        if default_value.is_some() && action == ConfigAction::GetAll {
            eprintln!("fatal: --default= cannot be used with --all or --url=");
            return Err(GitError::Exit(128));
        }
    } else if default_value.is_some() && action != ConfigAction::Get {
        eprintln!("error: --default is only applicable to --get");
        return Err(GitError::Exit(129));
    }
    if matches!(
        action,
        ConfigAction::GetColor | ConfigAction::GetColorBool | ConfigAction::GetUrlMatch
    ) && (display.show_origin || display.show_scope)
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
            ConfigAction::Set | ConfigAction::ReplaceAll | ConfigAction::Add
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
            | ConfigAction::UnsetAll => positional.len() > 1,
            ConfigAction::Set | ConfigAction::ReplaceAll => positional.len() > 2,
            _ => false,
        };
        if !allowed {
            eprintln!("error: --fixed-value only applies with 'value-pattern'");
            return Err(GitError::Exit(129));
        }
    }
    let value_matcher = match action {
        ConfigAction::ReplaceAll if positional.len() == 3 => {
            Some(ConfigValueMatcher::parse(positional[2], fixed_value))
        }
        ConfigAction::Unset | ConfigAction::UnsetAll if positional.len() == 2 => {
            Some(ConfigValueMatcher::parse(positional[1], fixed_value))
        }
        _ => None,
    };
    let value_matcher = value_matcher.as_ref();

    let key = if matches!(
        action,
        ConfigAction::List
            | ConfigAction::GetColor
            | ConfigAction::GetColorBool
            | ConfigAction::GetUrlMatch
            | ConfigAction::GetRegexp
            | ConfigAction::RenameSection
            | ConfigAction::RemoveSection
    ) || (is_subcommand_get && subcommand_get_regexp)
    {
        // Under `git config get --regexp` the positional is a key pattern, not a
        // concrete key, so it must not be validated/parsed as one.
        None
    } else {
        Some(parse_config_key(positional[0])?)
    };
    // Source precedence for `git config`: an explicit `--file`/`-f` (or `-` for
    // stdin) wins; otherwise the legacy `GIT_CONFIG` env var names a single file
    // to read and write (like `--file`, and like `--file` it suppresses the `-c`
    // / env config-injection overlay); otherwise the repository config is used.
    let git_config_env = env::var_os("GIT_CONFIG").filter(|value| !value.is_empty());
    let source = match config_file {
        Some(value) if value == "-" => ConfigSource::Stdin,
        Some(value) => ConfigSource::File(PathBuf::from(value)),
        None => match scope {
            ConfigScope::Global => ConfigSource::Global(global_config_file_path()?),
            ConfigScope::System => ConfigSource::System(system_config_file_path()),
            ConfigScope::Default => match git_config_env {
                Some(path) => ConfigSource::File(PathBuf::from(path)),
                None => ConfigSource::Repository(discover_git_dir(env::current_dir()?)?),
            },
        },
    };
    let loaded = read_config_source(&source, action)?;
    let mut config = loaded.config;
    // Command-line / environment config injection (`-c`, `--config-env`,
    // `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`) applies only to the default
    // (repository) config read, never to an explicit `-f <file>` or stdin source
    // — git layers these on top of the file stack at highest precedence, so the
    // document model's last-one-wins lookup yields the right value. Reads also
    // validate the injection stream here, surfacing a bogus `-c`/env entry the
    // same way git does.
    if matches!(source, ConfigSource::Repository(_)) {
        let parameters = crate::injected_config_parameters()?;
        config
            .sections
            .extend(sley_config::injected_config_sections(&parameters));
    }
    if is_subcommand_get {
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
            &config,
            &source,
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
            config_list(&config, &source, display, name_only, null_terminate)?;
            if let Some(err) = loaded.tail_error {
                let path = match &source {
                    ConfigSource::Repository(git_dir) => Some(git_dir.join("config")),
                    ConfigSource::File(path)
                    | ConfigSource::Global(path)
                    | ConfigSource::System(path) => Some(path.clone()),
                    ConfigSource::Stdin => None,
                };
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
                    &config,
                    &source,
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
            let formatted = match value_type {
                ConfigValueType::Bool => {
                    let Some(value) = config
                        .get_bool(&key.section, key.subsection.as_deref(), &key.key)
                        .or_else(|| {
                            default_value
                                .as_deref()
                                .and_then(sley_config::parse_config_bool)
                        })
                    else {
                        return Err(GitError::Exit(1));
                    };
                    value.to_string()
                }
                _ => match config.get_entry(&key.section, key.subsection.as_deref(), &key.key) {
                    Some(Some(value)) => format_config_value(value, value_type)?,
                    Some(None) => String::new(),
                    None => {
                        let Some(default) = default_value.as_deref() else {
                            return Err(GitError::Exit(1));
                        };
                        format_config_value(default, value_type)?
                    }
                },
            };
            write_config_value(
                &mut io::stdout(),
                &source,
                display,
                &formatted,
                null_terminate,
            )?;
        }
        ConfigAction::GetColor => {
            let key = parse_config_key(positional[0])?;
            if let Some(value) = config.get(&key.section, key.subsection.as_deref(), &key.key) {
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
            let value = config
                .get(&key.section, key.subsection.as_deref(), &key.key)
                .or_else(|| config.get("color", None, "ui"));
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
            if !config_get_urlmatch(&config, &target, positional[1], null_terminate)? {
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
                    &config,
                    &source,
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
            let values = config.get_all(&key.section, key.subsection.as_deref(), &key.key);
            if values.is_empty() {
                return Err(GitError::Exit(1));
            }
            let mut stdout = io::stdout();
            for value in values {
                let formatted = match value {
                    None if value_type == ConfigValueType::Bool => "true".to_string(),
                    None => String::new(),
                    Some(value) => format_config_value(value, value_type)?,
                };
                write_config_value(&mut stdout, &source, display, &formatted, null_terminate)?;
            }
        }
        ConfigAction::GetRegexp => {
            // `--get-regexp <name-regex> [<value-pattern>]`: the optional second
            // positional filters matches by value.
            let value_filter = positional
                .get(1)
                .map(|pattern| ConfigValuePatternFilter::parse(pattern, fixed_value));
            if !config_get_regexp(
                &config,
                &source,
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
        ConfigAction::Set => {
            let key = key.expect("validated config key");
            if config_value_count(&config, &key) > 1 {
                return Err(GitError::Exit(5));
            }
            config_set_value_with_comment(
                &mut config,
                &key,
                positional[1],
                false,
                comment.as_deref(),
            );
            write_config_source(&source, &config)?;
        }
        ConfigAction::ReplaceAll => {
            let key = key.expect("validated config key");
            config_replace_all_value(
                &mut config,
                &key,
                positional[1],
                value_matcher,
                comment.as_deref(),
            );
            write_config_source(&source, &config)?;
        }
        ConfigAction::Add => {
            let key = key.expect("validated config key");
            config_set_value_with_comment(
                &mut config,
                &key,
                positional[1],
                true,
                comment.as_deref(),
            );
            write_config_source(&source, &config)?;
        }
        ConfigAction::Unset => {
            let key = key.expect("validated config key");
            if value_matcher.is_none() && config_value_count(&config, &key) > 1 {
                return Err(GitError::Exit(5));
            }
            if !config_unset_value(&mut config, &key, false, value_matcher) {
                return Err(GitError::Exit(5));
            }
            write_config_source(&source, &config)?;
        }
        ConfigAction::UnsetAll => {
            let key = key.expect("validated config key");
            if !config_unset_value(&mut config, &key, true, value_matcher) {
                return Err(GitError::Exit(5));
            }
            write_config_source(&source, &config)?;
        }
        ConfigAction::RenameSection => {
            let old = parse_config_section_name(positional[0])?;
            let new = parse_config_section_name(positional[1])?;
            if !config_rename_section(&mut config, &old, &new) {
                return Err(GitError::Exit(128));
            }
            write_config_source(&source, &config)?;
        }
        ConfigAction::RemoveSection => {
            let section = parse_config_section_name(positional[0])?;
            if !config_remove_section(&mut config, &section) {
                return Err(GitError::Exit(128));
            }
            write_config_source(&source, &config)?;
        }
    }
    Ok(())
}

/// The single file `git config --global` reads and writes: `$GIT_CONFIG_GLOBAL`
/// when set; otherwise `~/.gitconfig`, unless it is missing and the XDG file
/// (`$XDG_CONFIG_HOME/git/config`, default `~/.config/git/config`) exists, in
/// which case the XDG file is chosen (builtin/config.c).
fn global_config_file_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("GIT_CONFIG_GLOBAL") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let Ok(home) = env::var("HOME") else {
        eprintln!("fatal: $HOME not set");
        return Err(GitError::Exit(128));
    };
    let user = PathBuf::from(&home).join(".gitconfig");
    let xdg = match env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("git").join("config"),
        _ => PathBuf::from(&home).join(".config").join("git").join("config"),
    };
    if !user.exists() && xdg.exists() {
        Ok(xdg)
    } else {
        Ok(user)
    }
}

/// The file `git config --system` reads and writes: `$GIT_CONFIG_SYSTEM` when
/// set, otherwise `/etc/gitconfig`. (Unlike implicit reads, the explicit
/// `--system` option ignores `GIT_CONFIG_NOSYSTEM`.)
fn system_config_file_path() -> PathBuf {
    match env::var("GIT_CONFIG_SYSTEM") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from("/etc/gitconfig"),
    }
}

struct LoadedConfig {
    config: GitConfig,
    tail_error: Option<GitError>,
}

fn read_config_source(source: &ConfigSource, action: ConfigAction) -> Result<LoadedConfig> {
    match source {
        ConfigSource::Repository(git_dir) => {
            let path = git_dir.join("config");
            match read_repo_config(git_dir) {
                Ok(config) => Ok(LoadedConfig {
                    config,
                    tail_error: None,
                }),
                Err(err) => Err(report_config_parse_error(err, Some(&path))),
            }
        }
        ConfigSource::File(path) | ConfigSource::Global(path) | ConfigSource::System(path) => match fs::read(path) {
            Ok(bytes) => load_config_bytes(&bytes, action, Some(path.as_path())),
            Err(err) if err.kind() == io::ErrorKind::NotFound && action != ConfigAction::List => {
                Ok(LoadedConfig {
                    config: GitConfig::default(),
                    tail_error: None,
                })
            }
            Err(err) => {
                eprintln!(
                    "fatal: unable to read config file '{}': {err}",
                    path.display()
                );
                Err(GitError::Exit(128))
            }
        },
        ConfigSource::Stdin => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
            load_config_bytes(&bytes, action, None)
        }
    }
}

fn load_config_bytes(
    bytes: &[u8],
    action: ConfigAction,
    path: Option<&Path>,
) -> Result<LoadedConfig> {
    if action == ConfigAction::List {
        let (config, tail_error) = GitConfig::parse_collecting(bytes)?;
        Ok(LoadedConfig { config, tail_error })
    } else {
        GitConfig::parse(bytes)
            .map(|config| LoadedConfig {
                config,
                tail_error: None,
            })
            .map_err(|err| report_config_parse_error(err, path))
    }
}

fn report_config_parse_error(err: GitError, path: Option<&Path>) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if let Some(line) = message.strip_prefix("config line ") {
                if let Some((line, _)) = line.split_once(':') {
                    if let Some(path) = path {
                        eprintln!("fatal: bad config line {line} in file {}", path.display());
                    } else {
                        eprintln!("fatal: bad config line {line}");
                    }
                    return GitError::Exit(128);
                }
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

fn write_config_source(source: &ConfigSource, config: &GitConfig) -> Result<()> {
    match source {
        ConfigSource::Repository(git_dir) => write_repo_config(git_dir, config),
        ConfigSource::File(path) | ConfigSource::Global(path) | ConfigSource::System(path) => {
            fs::write(path, config.to_canonical_bytes())?;
            Ok(())
        }
        ConfigSource::Stdin => {
            eprintln!("fatal: writing to stdin is not supported");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_config_value_type(value: &str) -> Result<ConfigValueType> {
    match value {
        "bool" => Ok(ConfigValueType::Bool),
        "int" => Ok(ConfigValueType::Int),
        "bool-or-int" => Ok(ConfigValueType::BoolOrInt),
        "expiry-date" => Ok(ConfigValueType::ExpiryDate),
        "color" => Ok(ConfigValueType::Color),
        "path" => Ok(ConfigValueType::Path),
        "string" => Ok(ConfigValueType::Raw),
        other => Err(GitError::Unsupported(format!(
            "config value type {other} is not supported"
        ))),
    }
}

fn format_config_value(value: &str, value_type: ConfigValueType) -> Result<String> {
    match value_type {
        ConfigValueType::Raw => Ok(value.to_string()),
        ConfigValueType::Bool => match sley_config::parse_config_bool(value) {
            Some(true) => Ok("true".into()),
            Some(false) => Ok("false".into()),
            None => config_bad_bool_value(value),
        },
        ConfigValueType::Int => sley_config::parse_config_int(value)
            .map(|value| value.to_string())
            .ok_or_else(|| config_bad_numeric_value(value)),
        ConfigValueType::BoolOrInt => match sley_config::parse_config_bool_or_int(value) {
            Some(ConfigBoolOrInt::Bool(true)) => Ok("true".into()),
            Some(ConfigBoolOrInt::Bool(false)) => Ok("false".into()),
            Some(ConfigBoolOrInt::Int(value)) => Ok(value.to_string()),
            None => Err(config_bad_numeric_value(value)),
        },
        ConfigValueType::ExpiryDate => format_config_expiry_date_value(value),
        ConfigValueType::Color => format_config_color_value(value),
        ConfigValueType::Path => Ok(format_config_path_value(value)),
    }
}

fn config_bad_bool_value<T>(value: &str) -> Result<T> {
    eprintln!("fatal: bad boolean config value '{value}'");
    Err(GitError::Exit(128))
}

fn config_bad_numeric_value(value: &str) -> GitError {
    eprintln!("fatal: bad numeric config value '{value}': invalid unit");
    GitError::Exit(128)
}

fn format_config_expiry_date_value(value: &str) -> Result<String> {
    match value {
        "now" => Ok(u64::MAX.to_string()),
        "never" => Ok("0".into()),
        value if value.bytes().all(|byte| byte.is_ascii_digit()) => value
            .parse::<u64>()
            .map(|value| value.to_string())
            .map_err(|_| config_bad_expiry_date_value(value)),
        _ => Err(config_bad_expiry_date_value(value)),
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
    let mut codes = Vec::new();
    let mut color_slot = 0usize;
    for token in value.split_whitespace() {
        if token.eq_ignore_ascii_case("normal") {
            continue;
        }
        if let Some(code) = config_color_attribute_code(token) {
            codes.push(code.to_string());
            continue;
        }
        if color_slot >= 2 {
            return Err(config_bad_color_value(value));
        }
        let foreground = color_slot == 0;
        codes.extend(
            config_color_code(token, foreground).ok_or_else(|| config_bad_color_value(value))?,
        );
        color_slot += 1;
    }
    if codes.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("\x1b[{}m", codes.join(";")))
}

fn config_color_attribute_code(token: &str) -> Option<u8> {
    match token.to_ascii_lowercase().as_str() {
        "bold" => Some(1),
        "dim" => Some(2),
        "italic" => Some(3),
        "ul" | "underline" => Some(4),
        "blink" => Some(5),
        "reverse" => Some(7),
        "strike" => Some(9),
        _ => None,
    }
}

fn config_color_code(token: &str, foreground: bool) -> Option<Vec<String>> {
    if let Some((red, green, blue)) = parse_config_hex_color(token) {
        let prefix = if foreground { "38" } else { "48" };
        return Some(vec![
            prefix.into(),
            "2".into(),
            red.to_string(),
            green.to_string(),
            blue.to_string(),
        ]);
    }
    if let Ok(value) = token.parse::<u8>() {
        if value < 16 {
            let base = match (foreground, value < 8) {
                (true, true) => 30,
                (false, true) => 40,
                (true, false) => 90 - 8,
                (false, false) => 100 - 8,
            };
            return Some(vec![(base + value).to_string()]);
        }
        let prefix = if foreground { "38" } else { "48" };
        return Some(vec![prefix.into(), "5".into(), value.to_string()]);
    }
    let color = match token.to_ascii_lowercase().as_str() {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "brightblack" | "bright-black" | "gray" | "grey" => 8,
        "brightred" | "bright-red" => 9,
        "brightgreen" | "bright-green" => 10,
        "brightyellow" | "bright-yellow" => 11,
        "brightblue" | "bright-blue" => 12,
        "brightmagenta" | "bright-magenta" => 13,
        "brightcyan" | "bright-cyan" => 14,
        "brightwhite" | "bright-white" => 15,
        _ => return None,
    };
    let base = match (foreground, color < 8) {
        (true, true) => 30,
        (false, true) => 40,
        (true, false) => 90 - 8,
        (false, false) => 100 - 8,
    };
    Some(vec![(base + color).to_string()])
}

fn parse_config_hex_color(token: &str) -> Option<(u8, u8, u8)> {
    let hex = token.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((red, green, blue))
}

fn config_bad_color_value(value: &str) -> GitError {
    eprintln!("error: invalid color value: {value}");
    GitError::Exit(128)
}

fn format_config_path_value(value: &str) -> String {
    if value == "~" {
        return env::var("HOME").unwrap_or_else(|_| value.to_string());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    value.to_string()
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
        if subsection.is_empty()
            || subsection
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

fn config_section_name_matches(section: &ConfigSection, name: &ConfigSectionName) -> bool {
    section.name.eq_ignore_ascii_case(&name.section)
        && section.subsection.as_deref() == name.subsection.as_deref()
}

fn config_get_urlmatch(
    config: &GitConfig,
    target: &ConfigUrlMatchTarget,
    url: &str,
    null_terminate: bool,
) -> Result<bool> {
    let mut values = BTreeMap::<String, (usize, String)>::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case(&target.section) {
            continue;
        }
        let match_len = match section.subsection.as_deref() {
            None => 0,
            Some(base) => match config_urlmatch_score(base, url) {
                Some(score) => score,
                None => continue,
            },
        };
        for entry in &section.entries {
            if let Some(key) = &target.key
                && !entry.key.eq_ignore_ascii_case(key)
            {
                continue;
            }
            let Some(value) = entry.value.as_deref() else {
                continue;
            };
            let name = format!(
                "{}.{}",
                target.section.to_ascii_lowercase(),
                entry.key.to_ascii_lowercase()
            );
            let replace = values
                .get(&name)
                .is_none_or(|(previous_len, _)| match_len >= *previous_len);
            if replace {
                values.insert(name, (match_len, value.to_string()));
            }
        }
    }
    if values.is_empty() {
        return Ok(false);
    }
    let mut stdout = io::stdout();
    if target.key.is_some() {
        if let Some((_, value)) = values.values().next() {
            if null_terminate {
                write!(stdout, "{value}\0")?;
            } else {
                writeln!(stdout, "{value}")?;
            }
        }
        return Ok(true);
    }
    for (name, (_, value)) in values {
        if null_terminate {
            write!(stdout, "{name}\n{value}\0")?;
        } else {
            writeln!(stdout, "{name} {value}")?;
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigUrlParts {
    scheme: String,
    user: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
}

fn config_urlmatch_score(base: &str, url: &str) -> Option<usize> {
    let Some(base_url) = parse_config_url(base) else {
        return url.starts_with(base).then_some(base.len());
    };
    let url = parse_config_url(url)?;
    if base_url.scheme != url.scheme
        || base_url.host != url.host
        || base_url.port != url.port
        || (base_url.user.is_some() && base_url.user != url.user)
    {
        return None;
    }
    let base_path = normalize_config_url_path_for_match(&base_url.path);
    let url_path = normalize_config_url_path_for_match(&url.path);
    if base_path != "/" && url_path != base_path && !url_path.starts_with(&format!("{base_path}/"))
    {
        return None;
    }
    Some(
        base_url.scheme.len()
            + base_url.host.len()
            + base_url.user.as_ref().map_or(0, |user| user.len() + 1)
            + base_path.len(),
    )
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

fn normalize_config_url_path_for_match(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    if path == "/" {
        return "/".into();
    }
    path.trim_end_matches('/').to_string()
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

fn config_value_count(config: &GitConfig, key: &ConfigKey) -> usize {
    config
        .sections
        .iter()
        .filter(|section| config_section_matches(section, key))
        .map(|section| {
            section
                .entries
                .iter()
                .filter(|entry| entry.key.eq_ignore_ascii_case(&key.key))
                .count()
        })
        .sum()
}

#[allow(clippy::too_many_arguments)]
fn config_get_regexp(
    config: &GitConfig,
    source: &ConfigSource,
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
    for section in &config.sections {
        for entry in &section.entries {
            let name = config_entry_name(section, &entry.key);
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
                source,
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
    config: &GitConfig,
    source: &ConfigSource,
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
    let mut matches: Vec<(String, Option<String>)> = Vec::new();
    for section in &config.sections {
        for entry in &section.entries {
            let name = config_entry_name(section, &entry.key);
            let key_matches = match &key {
                SubcommandGetKey::Exact(exact) => {
                    config_section_matches(section, exact)
                        && entry.key.eq_ignore_ascii_case(&exact.key)
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
            matches.push((name, entry.value.clone()));
        }
    }

    // git falls back to `--default` only when nothing matched. The default is
    // attributed to the requested name (for an exact key) so `--show-names`
    // renders it; under `--regexp` there is no single key, matching git which
    // disallows `--default` together with `--all`/`--url` but still formats the
    // default against the pattern string.
    if matches.is_empty()
        && let Some(default) = default_value
    {
        let name = match &key {
            SubcommandGetKey::Exact(exact) => config_key_name(exact),
            SubcommandGetKey::Regexp(_) => String::new(),
        };
        matches.push((name, Some(default.to_string())));
    }

    if matches.is_empty() {
        return Ok(false);
    }

    let mut stdout = io::stdout();
    let last = matches.len() - 1;
    for (idx, (name, value)) in matches.iter().enumerate() {
        if !all && idx != last {
            continue;
        }
        write_config_entry(
            &mut stdout,
            source,
            name,
            value.as_deref(),
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
    Class { negated: bool, ranges: Vec<(u8, u8)> },
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
        if bytes.get(idx + 1) == Some(&b'-')
            && bytes.get(idx + 2).is_some_and(|&end| end != b']')
        {
            let end = bytes[idx + 2];
            let (lo, hi) = if byte <= end { (byte, end) } else { (end, byte) };
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
    config: &GitConfig,
    source: &ConfigSource,
    display: ConfigDisplayOptions,
    name_only: bool,
    null_terminate: bool,
) -> Result<()> {
    let mut stdout = io::stdout();
    for section in &config.sections {
        for entry in &section.entries {
            let name = config_entry_name(section, &entry.key);
            write_config_entry(
                &mut stdout,
                source,
                &name,
                entry.value.as_deref(),
                ConfigEntryWriteOptions {
                    display,
                    name_only,
                    show_keys: true,
                    value_type: ConfigValueType::Raw,
                    null_terminate,
                    equals_separator: true,
                },
            )?;
        }
    }
    Ok(())
}

fn write_config_value(
    stdout: &mut impl Write,
    source: &ConfigSource,
    display: ConfigDisplayOptions,
    value: &str,
    null_terminate: bool,
) -> Result<()> {
    write_config_metadata(stdout, source, display, null_terminate)?;
    if null_terminate {
        write!(stdout, "{value}\0")?;
    } else {
        writeln!(stdout, "{value}")?;
    }
    Ok(())
}

fn write_config_entry(
    stdout: &mut impl Write,
    source: &ConfigSource,
    name: &str,
    value: Option<&str>,
    options: ConfigEntryWriteOptions,
) -> Result<()> {
    // Mirror git's `format_config`: metadata, then optionally the key, then the
    // key delimiter and the (typed) value, then the terminator. The key is only
    // emitted when `show_keys` is set; `name_only` (git's `omit_values`) stops
    // after the key and never prints a value or delimiter.
    write_config_metadata(stdout, source, options.display, options.null_terminate)?;
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
        None if options.value_type == ConfigValueType::Bool => Some("true".to_string()),
        // A value-less entry with no requested type prints just the key (git
        // backs out the key delimiter), so there is no value to render.
        None => None,
        Some(value) => Some(format_config_value(value, options.value_type)?),
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
    source: &ConfigSource,
    display: ConfigDisplayOptions,
    null_terminate: bool,
) -> Result<()> {
    if display.show_scope {
        write_config_metadata_field(stdout, config_source_scope(source), null_terminate)?;
    }
    if display.show_origin {
        write_config_metadata_field(stdout, &config_source_origin(source), null_terminate)?;
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

fn config_source_scope(source: &ConfigSource) -> &'static str {
    match source {
        ConfigSource::Repository(_) => "local",
        ConfigSource::Global(_) => "global",
        ConfigSource::System(_) => "system",
        ConfigSource::File(_) | ConfigSource::Stdin => "command",
    }
}

fn config_source_origin(source: &ConfigSource) -> String {
    match source {
        ConfigSource::Repository(_) => "file:.git/config".to_string(),
        ConfigSource::File(path) | ConfigSource::Global(path) | ConfigSource::System(path) => {
            format!("file:{}", path.display())
        }
        ConfigSource::Stdin => "standard input:".to_string(),
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

fn parse_config_comment(value: &str) -> Result<String> {
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r')) {
        eprintln!("fatal: no multi-line comment allowed: '{value}'");
        return Err(GitError::Exit(128));
    }
    let value = value
        .strip_prefix('#')
        .map(|rest| rest.trim_start_matches([' ', '\t']))
        .unwrap_or(value);
    Ok(value.to_string())
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

fn config_replace_all_value(
    config: &mut GitConfig,
    key: &ConfigKey,
    value: &str,
    value_pattern: Option<&ConfigValueMatcher>,
    comment: Option<&str>,
) {
    let Some(value_pattern) = value_pattern else {
        config_set_value_with_comment(config, key, value, false, comment);
        return;
    };

    let mut replaced = false;
    let mut matched = false;
    for section in &mut config.sections {
        if !config_section_matches(section, key) {
            continue;
        }
        section.entries.retain_mut(|entry| {
            if !entry.key.eq_ignore_ascii_case(&key.key)
                || !entry
                    .value
                    .as_deref()
                    .is_some_and(|value| value_pattern.is_match(value))
            {
                return true;
            }
            matched = true;
            if replaced {
                return false;
            }
            entry.key = key.key.clone();
            entry.value = Some(value.to_string());
            entry.comment = comment.map(str::to_string);
            replaced = true;
            true
        });
    }
    if !matched {
        config_set_value_with_comment(config, key, value, true, comment);
    }
}

fn config_unset_value(
    config: &mut GitConfig,
    key: &ConfigKey,
    all: bool,
    value_pattern: Option<&ConfigValueMatcher>,
) -> bool {
    if let Some(value_pattern) = value_pattern {
        return config_unset_value_matching(config, key, all, value_pattern);
    }
    let mut removed = false;
    for section in &mut config.sections {
        if !config_section_matches(section, key) {
            continue;
        }
        if all {
            let before = section.entries.len();
            section
                .entries
                .retain(|entry| !entry.key.eq_ignore_ascii_case(&key.key));
            removed |= section.entries.len() != before;
        } else if let Some(position) = section
            .entries
            .iter()
            .rposition(|entry| entry.key.eq_ignore_ascii_case(&key.key))
        {
            section.entries.remove(position);
            return true;
        }
    }
    if all {
        config
            .sections
            .retain(|section| !config_section_matches(section, key) || !section.entries.is_empty());
    }
    removed
}

fn config_unset_value_matching(
    config: &mut GitConfig,
    key: &ConfigKey,
    all: bool,
    value_pattern: &ConfigValueMatcher,
) -> bool {
    let matches = config
        .sections
        .iter()
        .filter(|section| config_section_matches(section, key))
        .flat_map(|section| section.entries.iter())
        .filter(|entry| entry.key.eq_ignore_ascii_case(&key.key))
        .filter(|entry| {
            entry
                .value
                .as_deref()
                .is_some_and(|value| value_pattern.is_match(value))
        })
        .count();
    if matches == 0 || (!all && matches != 1) {
        return false;
    }
    for section in &mut config.sections {
        if !config_section_matches(section, key) {
            continue;
        }
        section.entries.retain(|entry| {
            !entry.key.eq_ignore_ascii_case(&key.key)
                || !entry
                    .value
                    .as_deref()
                    .is_some_and(|value| value_pattern.is_match(value))
        });
    }
    config
        .sections
        .retain(|section| !config_section_matches(section, key) || !section.entries.is_empty());
    true
}

fn config_rename_section(
    config: &mut GitConfig,
    old: &ConfigSectionName,
    new: &ConfigSectionName,
) -> bool {
    let mut renamed = false;
    for section in &mut config.sections {
        if config_section_name_matches(section, old) {
            section.name = new.section.clone();
            section.subsection = new.subsection.clone();
            renamed = true;
        }
    }
    renamed
}

fn config_remove_section(config: &mut GitConfig, name: &ConfigSectionName) -> bool {
    let before = config.sections.len();
    config
        .sections
        .retain(|section| !config_section_name_matches(section, name));
    config.sections.len() != before
}
