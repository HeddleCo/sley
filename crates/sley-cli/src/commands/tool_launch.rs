use crate::*;
use std::process::Command as ProcessCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolMode {
    Diff,
    Merge,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCommand {
    pub name: String,
    pub command: String,
    pub trust_exit_code: bool,
    pub is_builtin: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolEnvironment {
    pub local: PathBuf,
    pub remote: PathBuf,
    pub merged: PathBuf,
    pub base: PathBuf,
}

pub(crate) fn config_bool(config: &GitConfig, section: &str, key: &str, default: bool) -> bool {
    config.get_bool(section, None, key).unwrap_or(default)
}

pub(crate) fn resolve_tool_command(
    config: &GitConfig,
    mode: ToolMode,
    name: &str,
    trust_override: Option<bool>,
) -> Result<ToolCommand> {
    let mode_section = match mode {
        ToolMode::Diff => "difftool",
        ToolMode::Merge => "mergetool",
    };
    let fallback_section = match mode {
        ToolMode::Diff => Some("mergetool"),
        ToolMode::Merge => None,
    };

    let command = config
        .get(mode_section, Some(name), "cmd")
        .or_else(|| fallback_section.and_then(|section| config.get(section, Some(name), "cmd")))
        .map(str::to_owned);
    let mut is_builtin = false;
    let command = match command {
        Some(command) => command,
        None => {
            let path = config
                .get(mode_section, Some(name), "path")
                .or_else(|| {
                    fallback_section.and_then(|section| config.get(section, Some(name), "path"))
                })
                .map(str::to_owned)
                .or_else(|| builtin_tool_program(name));
            let Some(path) = path else {
                eprintln!(
                    "error: {}tool.{}.cmd not set for tool '{}'",
                    match mode {
                        ToolMode::Diff => "diff",
                        ToolMode::Merge => "merge",
                    },
                    name,
                    name
                );
                return Err(GitError::Exit(1));
            };
            is_builtin = true;
            match mode {
                ToolMode::Diff => format!("\"{path}\" \"$LOCAL\" \"$REMOTE\""),
                ToolMode::Merge => format!("\"{path}\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\""),
            }
        }
    };

    let trust_exit_code = trust_override.unwrap_or_else(|| {
        config
            .get_bool(mode_section, Some(name), "trustexitcode")
            .or_else(|| {
                fallback_section
                    .and_then(|section| config.get_bool(section, Some(name), "trustexitcode"))
            })
            .or_else(|| config.get_bool(mode_section, None, "trustexitcode"))
            .unwrap_or(false)
    });

    Ok(ToolCommand {
        name: name.to_string(),
        command,
        trust_exit_code,
        is_builtin,
    })
}

pub(crate) fn builtin_tool_program(name: &str) -> Option<String> {
    match name {
        "araxis" | "bc" | "bc3" | "bc4" | "codecompare" | "deltawalker" | "diffmerge"
        | "diffuse" | "ecmerge" | "emerge" | "gvimdiff" | "gvimdiff2" | "gvimdiff3" | "kdiff3"
        | "kompare" | "meld" | "nvimdiff" | "nvimdiff2" | "nvimdiff3" | "opendiff" | "p4merge"
        | "smerge" | "tkdiff" | "tortoisemerge" | "vimdiff" | "vimdiff2" | "vimdiff3"
        | "winmerge" | "xxdiff" => Some(name.to_string()),
        _ => None,
    }
}

pub(crate) fn select_tool_name(
    config: &GitConfig,
    mode: ToolMode,
    cli_tool: Option<&str>,
    gui: bool,
) -> Option<String> {
    if let Some(tool) = cli_tool {
        return Some(tool.to_string());
    }
    match mode {
        ToolMode::Diff => {
            if let Ok(tool) = env::var("GIT_DIFF_TOOL")
                && !tool.is_empty()
            {
                return Some(tool);
            }
            if gui {
                config
                    .get("diff", None, "guitool")
                    .or_else(|| config.get("merge", None, "guitool"))
                    .or_else(|| config.get("diff", None, "tool"))
                    .or_else(|| config.get("merge", None, "tool"))
                    .map(str::to_owned)
            } else {
                config
                    .get("diff", None, "tool")
                    .or_else(|| config.get("merge", None, "tool"))
                    .map(str::to_owned)
            }
        }
        ToolMode::Merge => {
            if gui {
                config
                    .get("merge", None, "guitool")
                    .or_else(|| config.get("merge", None, "tool"))
                    .map(str::to_owned)
            } else {
                config.get("merge", None, "tool").map(str::to_owned)
            }
        }
    }
}

pub(crate) fn gui_default(config: &GitConfig, mode: ToolMode) -> bool {
    let key = match mode {
        ToolMode::Diff => "difftool",
        ToolMode::Merge => "mergetool",
    };
    match config
        .get(key, None, "guidefault")
        .unwrap_or("false")
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()),
        _ => config.get_bool(key, None, "guidefault").unwrap_or(false),
    }
}

pub(crate) fn run_tool_shell(command: &str, envs: &ToolEnvironment) -> Result<i32> {
    run_tool_shell_in_dir(command, envs, Path::new("."))
}

pub(crate) fn run_tool_shell_in_dir(
    command: &str,
    envs: &ToolEnvironment,
    cwd: &Path,
) -> Result<i32> {
    let status = ProcessCommand::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("LOCAL", &envs.local)
        .env("REMOTE", &envs.remote)
        .env("MERGED", &envs.merged)
        .env("BASE", &envs.base)
        .status()
        .map_err(|err| GitError::Command(format!("failed to run tool: {err}")))?;
    Ok(status.code().unwrap_or(128))
}

pub(crate) fn print_tool_help(mode: ToolMode) {
    match mode {
        ToolMode::Diff => {
            println!("'git difftool --tool=<tool>' may be set to one of the following:");
        }
        ToolMode::Merge => {
            println!("'git mergetool --tool=<tool>' may be set to one of the following:");
        }
    }
    for tool in [
        "araxis",
        "bc",
        "bc3",
        "bc4",
        "codecompare",
        "deltawalker",
        "diffmerge",
        "diffuse",
        "ecmerge",
        "emerge",
        "gvimdiff",
        "gvimdiff2",
        "gvimdiff3",
        "kdiff3",
        "kompare",
        "meld",
        "nvimdiff",
        "nvimdiff2",
        "nvimdiff3",
        "opendiff",
        "p4merge",
        "smerge",
        "tkdiff",
        "vimdiff",
        "vimdiff2",
        "vimdiff3",
        "winmerge",
        "xxdiff",
    ] {
        println!("{tool}");
    }
}
