use crate::sley_config;
use crate::{common_git_dir_for_git_dir, injected_config_parameters, report_config_setup_error};
use sley::plumbing::sley_config::ConfigIncludeContext;
use sley::{GitError, Result};
use sley_options::{
    CommandFlags, CommandRegistry, CommandSpec, OptFlags, OptValue, OptionSpec,
    completion_helper_options, usage_with_options,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

// Native dispatch and Git's own builtin classification are deliberately
// separate. Some commands implemented in-process by Sley are shell/perl
// commands in Git, while some Git builtins are reserved until their native
// Sley implementation lands.
const NATIVE: CommandFlags = CommandFlags::from_bits(1 << 0);
const MAIN_PORCELAIN: CommandFlags = CommandFlags::from_bits(1 << 1);
const PARSEOPT_HELPER: CommandFlags = CommandFlags::from_bits(1 << 2);
const RESERVED_CORE_HELPER: CommandFlags = CommandFlags::from_bits(1 << 3);
const GIT_BUILTIN: CommandFlags = CommandFlags::from_bits(1 << 4);
const USAGE_HELP: CommandFlags = CommandFlags::from_bits(1 << 5);
const COMMAND_SPECIFIC_HELP: CommandFlags = CommandFlags::from_bits(1 << 6);
const SETUP_BEFORE_HELP: CommandFlags = CommandFlags::from_bits(1 << 7);
const BUILTIN: CommandFlags = NATIVE.union(GIT_BUILTIN);
const BUILTIN_MAIN: CommandFlags = BUILTIN.union(MAIN_PORCELAIN);
const BUILTIN_PARSEOPT: CommandFlags = BUILTIN.union(PARSEOPT_HELPER);
const BUILTIN_MAIN_PARSEOPT: CommandFlags = BUILTIN_MAIN.union(PARSEOPT_HELPER);
const NATIVE_MAIN: CommandFlags = NATIVE.union(MAIN_PORCELAIN);
const GIT_BUILTIN_RESERVED: CommandFlags = GIT_BUILTIN.union(RESERVED_CORE_HELPER);
const HELP_RESERVED: CommandFlags = USAGE_HELP.union(RESERVED_CORE_HELPER);
const MAIN_RESERVED: CommandFlags = MAIN_PORCELAIN.union(RESERVED_CORE_HELPER);

/// Command help that cannot be generated from Documentation's modern
/// `[synopsis]`/`[verse]` blocks. These legacy credential pages describe helper
/// configuration rather than the builtins' parse-options usage, so keep Git's
/// actual command metadata explicitly and render it through the same option
/// table machinery as migrated commands.
struct CommandUsageSpec<'a> {
    command: &'a str,
    usage: &'a [&'a str],
    options: &'a [OptionSpec<'a>],
}

const CREDENTIAL_CACHE_OPTIONS: &[OptionSpec<'static>] = &[
    OptionSpec {
        short: None,
        long: Some("timeout"),
        value: OptValue::Str("n"),
        flags: OptFlags::NONE,
        help: "number of seconds to cache credentials",
    },
    OptionSpec {
        short: None,
        long: Some("socket"),
        value: OptValue::Str("path"),
        flags: OptFlags::NONE,
        help: "path of cache-daemon socket",
    },
];

const CREDENTIAL_STORE_OPTIONS: &[OptionSpec<'static>] = &[OptionSpec {
    short: None,
    long: Some("file"),
    value: OptValue::Str("path"),
    flags: OptFlags::NONE,
    help: "fetch and store credentials in <path>",
}];

const COMMAND_USAGE_OVERRIDES: &[CommandUsageSpec<'static>] = &[
    CommandUsageSpec {
        command: "credential",
        usage: &["git credential (fill|approve|reject)"],
        options: &[],
    },
    CommandUsageSpec {
        command: "credential-cache",
        usage: &["git credential-cache [<options>] <action>"],
        options: CREDENTIAL_CACHE_OPTIONS,
    },
    CommandUsageSpec {
        command: "credential-store",
        usage: &["git credential-store [<options>] <action>"],
        options: CREDENTIAL_STORE_OPTIONS,
    },
];

static COMMAND_REGISTRY: CommandRegistry<'static> = CommandRegistry::new(&[
    CommandSpec::new("add", BUILTIN_MAIN),
    CommandSpec::new("am", BUILTIN_MAIN),
    CommandSpec::new("annotate", BUILTIN),
    CommandSpec::new("apply", BUILTIN),
    CommandSpec::new("archimport", RESERVED_CORE_HELPER),
    CommandSpec::new("archive", BUILTIN),
    CommandSpec::new("backfill", BUILTIN),
    CommandSpec::new("bisect", BUILTIN_MAIN),
    CommandSpec::new("blame", BUILTIN),
    CommandSpec::new("branch", BUILTIN_MAIN),
    CommandSpec::new("bugreport", BUILTIN),
    CommandSpec::new("bundle", BUILTIN),
    CommandSpec::new("cat-file", BUILTIN),
    CommandSpec::new("check-attr", BUILTIN),
    CommandSpec::new("check-ignore", BUILTIN),
    CommandSpec::new("check-mailmap", BUILTIN),
    CommandSpec::new("check-ref-format", BUILTIN),
    CommandSpec::new("checkout", BUILTIN_MAIN_PARSEOPT),
    CommandSpec::new("checkout--worker", GIT_BUILTIN_RESERVED),
    CommandSpec::new("checkout-index", BUILTIN),
    CommandSpec::new("cherry", GIT_BUILTIN_RESERVED),
    CommandSpec::new("cherry-pick", BUILTIN_MAIN),
    CommandSpec::new("clean", BUILTIN_MAIN),
    CommandSpec::new("clone", BUILTIN_MAIN_PARSEOPT),
    CommandSpec::new("column", BUILTIN),
    CommandSpec::new("commit", BUILTIN_MAIN),
    CommandSpec::new("commit-graph", BUILTIN),
    CommandSpec::new("commit-tree", BUILTIN),
    CommandSpec::new("config", BUILTIN_PARSEOPT),
    CommandSpec::new("count-objects", BUILTIN),
    CommandSpec::new("credential", BUILTIN),
    CommandSpec::new("credential-cache", BUILTIN),
    CommandSpec::new("credential-cache--daemon", BUILTIN),
    CommandSpec::new("credential-store", BUILTIN),
    CommandSpec::new("cvsexportcommit", RESERVED_CORE_HELPER),
    CommandSpec::new("cvsimport", RESERVED_CORE_HELPER),
    CommandSpec::new("cvsserver", RESERVED_CORE_HELPER),
    CommandSpec::new("daemon", NATIVE),
    CommandSpec::new("describe", BUILTIN_MAIN),
    CommandSpec::new("diagnose", BUILTIN),
    CommandSpec::new("diff", BUILTIN_MAIN),
    CommandSpec::new(
        "diff-files",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new(
        "diff-index",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("diff-pairs", GIT_BUILTIN_RESERVED),
    CommandSpec::new("diff-tree", BUILTIN),
    CommandSpec::new("difftool", BUILTIN),
    CommandSpec::new("difftool--helper", RESERVED_CORE_HELPER),
    CommandSpec::new("fast-export", BUILTIN),
    CommandSpec::new("fast-import", BUILTIN),
    CommandSpec::new("fetch", BUILTIN_MAIN),
    CommandSpec::new("fetch-pack", BUILTIN),
    CommandSpec::new("filter-branch", NATIVE),
    CommandSpec::new("fmt-merge-msg", BUILTIN),
    CommandSpec::new("for-each-ref", BUILTIN),
    CommandSpec::new("for-each-repo", BUILTIN),
    CommandSpec::new("format-patch", BUILTIN_MAIN),
    CommandSpec::new("format-rev", BUILTIN),
    CommandSpec::new("fsck", BUILTIN),
    CommandSpec::new("fsck-objects", GIT_BUILTIN_RESERVED),
    CommandSpec::new("fsmonitor--daemon", GIT_BUILTIN_RESERVED.union(NATIVE)),
    CommandSpec::new("gc", BUILTIN),
    CommandSpec::new("get-tar-commit-id", BUILTIN),
    CommandSpec::new("gitk", MAIN_RESERVED),
    CommandSpec::new("grep", BUILTIN_MAIN),
    CommandSpec::new("hash-object", BUILTIN),
    CommandSpec::new("help", BUILTIN_PARSEOPT),
    CommandSpec::new("history", BUILTIN),
    CommandSpec::new("hook", BUILTIN),
    CommandSpec::new("http-backend", NATIVE),
    CommandSpec::new("http-fetch", RESERVED_CORE_HELPER),
    CommandSpec::new("http-push", RESERVED_CORE_HELPER),
    CommandSpec::new("imap-send", NATIVE.union(USAGE_HELP)),
    CommandSpec::new("index-pack", BUILTIN),
    CommandSpec::new("init", BUILTIN),
    CommandSpec::new("init-db", GIT_BUILTIN_RESERVED),
    CommandSpec::new("instaweb", HELP_RESERVED),
    CommandSpec::new(
        "interpret-trailers",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("last-modified", BUILTIN),
    CommandSpec::new("log", BUILTIN_MAIN),
    CommandSpec::new("ls-files", BUILTIN),
    CommandSpec::new("ls-remote", BUILTIN_PARSEOPT),
    CommandSpec::new("ls-tree", BUILTIN),
    CommandSpec::new("mailinfo", GIT_BUILTIN_RESERVED),
    CommandSpec::new("mailsplit", GIT_BUILTIN_RESERVED),
    CommandSpec::new(
        "maintenance",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("merge", BUILTIN_MAIN),
    CommandSpec::new("merge-base", BUILTIN),
    CommandSpec::new("merge-file", BUILTIN),
    CommandSpec::new("merge-index", BUILTIN),
    CommandSpec::new("merge-octopus", RESERVED_CORE_HELPER),
    CommandSpec::new("merge-one-file", RESERVED_CORE_HELPER),
    CommandSpec::new("merge-ours", GIT_BUILTIN_RESERVED),
    CommandSpec::new("merge-recursive", BUILTIN),
    CommandSpec::new("merge-recursive-ours", GIT_BUILTIN_RESERVED),
    CommandSpec::new("merge-recursive-theirs", GIT_BUILTIN_RESERVED),
    CommandSpec::new("merge-resolve", RESERVED_CORE_HELPER),
    CommandSpec::new("merge-subtree", GIT_BUILTIN_RESERVED),
    CommandSpec::new("merge-tree", BUILTIN),
    CommandSpec::new("mergetool", NATIVE_MAIN),
    CommandSpec::new("mktag", BUILTIN.union(COMMAND_SPECIFIC_HELP)),
    CommandSpec::new("mktree", BUILTIN),
    CommandSpec::new("multi-pack-index", BUILTIN),
    CommandSpec::new("mv", BUILTIN_MAIN),
    CommandSpec::new("name-rev", BUILTIN),
    CommandSpec::new("notes", BUILTIN_PARSEOPT),
    CommandSpec::new("p4", RESERVED_CORE_HELPER),
    CommandSpec::new("pack-objects", BUILTIN),
    CommandSpec::new("pack-redundant", BUILTIN),
    CommandSpec::new("pack-refs", BUILTIN),
    CommandSpec::new(
        "patch-id",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("pickaxe", GIT_BUILTIN_RESERVED),
    CommandSpec::new("prune", BUILTIN),
    CommandSpec::new("prune-packed", BUILTIN),
    CommandSpec::new("pull", BUILTIN_MAIN),
    CommandSpec::new("push", BUILTIN_MAIN),
    CommandSpec::new("quiltimport", HELP_RESERVED),
    CommandSpec::new("range-diff", BUILTIN),
    CommandSpec::new("read-tree", BUILTIN),
    CommandSpec::new("rebase", BUILTIN_MAIN),
    CommandSpec::new("receive-pack", BUILTIN),
    CommandSpec::new("reflog", BUILTIN),
    CommandSpec::new("refs", BUILTIN),
    CommandSpec::new("remote", BUILTIN_PARSEOPT),
    CommandSpec::new("remote-ext", GIT_BUILTIN_RESERVED),
    CommandSpec::new("remote-fd", GIT_BUILTIN_RESERVED),
    CommandSpec::new("remote-ftp", RESERVED_CORE_HELPER),
    CommandSpec::new("remote-ftps", RESERVED_CORE_HELPER),
    CommandSpec::new("remote-http", NATIVE),
    CommandSpec::new("remote-https", RESERVED_CORE_HELPER),
    CommandSpec::new("repack", BUILTIN),
    CommandSpec::new("replace", BUILTIN),
    CommandSpec::new("replay", BUILTIN.union(COMMAND_SPECIFIC_HELP)),
    CommandSpec::new("repo", BUILTIN),
    CommandSpec::new("request-pull", HELP_RESERVED),
    CommandSpec::new("rerere", BUILTIN),
    CommandSpec::new("reset", BUILTIN_MAIN),
    CommandSpec::new("restore", BUILTIN_MAIN),
    CommandSpec::new("rev-list", BUILTIN),
    CommandSpec::new("rev-parse", BUILTIN),
    CommandSpec::new("revert", BUILTIN_MAIN),
    CommandSpec::new("rm", BUILTIN_MAIN),
    CommandSpec::new("send-email", RESERVED_CORE_HELPER),
    CommandSpec::new("send-pack", BUILTIN),
    #[cfg(feature = "git-compat-i18n")]
    CommandSpec::new("sh-i18n--envsubst", NATIVE),
    #[cfg(not(feature = "git-compat-i18n"))]
    CommandSpec::new("sh-i18n--envsubst", RESERVED_CORE_HELPER),
    CommandSpec::new("shell", RESERVED_CORE_HELPER),
    CommandSpec::new(
        "shortlog",
        BUILTIN_MAIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("show", BUILTIN_MAIN),
    CommandSpec::new(
        "show-branch",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("show-index", BUILTIN),
    CommandSpec::new("show-ref", BUILTIN),
    CommandSpec::new("sparse-checkout", BUILTIN_MAIN),
    // git.c: `{ "stage", cmd_add, ... }` — synonym for `add`, not a reserved helper.
    CommandSpec::new("stage", BUILTIN),
    CommandSpec::new("stash", BUILTIN_MAIN),
    CommandSpec::new("status", BUILTIN_MAIN),
    CommandSpec::new("stripspace", BUILTIN),
    CommandSpec::new("submodule", NATIVE_MAIN.union(COMMAND_SPECIFIC_HELP)),
    CommandSpec::new("submodule--helper", GIT_BUILTIN_RESERVED),
    CommandSpec::new("svn", RESERVED_CORE_HELPER),
    CommandSpec::new("switch", BUILTIN_MAIN),
    CommandSpec::new("symbolic-ref", BUILTIN_PARSEOPT),
    CommandSpec::new("tag", BUILTIN_MAIN),
    CommandSpec::new("testkit", NATIVE),
    CommandSpec::new("unpack-file", BUILTIN),
    CommandSpec::new("unpack-objects", BUILTIN),
    CommandSpec::new("update-index", BUILTIN),
    CommandSpec::new("update-ref", BUILTIN),
    CommandSpec::new("update-server-info", BUILTIN),
    CommandSpec::new("upload-archive", GIT_BUILTIN_RESERVED),
    CommandSpec::new("upload-archive--writer", GIT_BUILTIN_RESERVED),
    CommandSpec::new("upload-pack", BUILTIN),
    CommandSpec::new("url-parse", GIT_BUILTIN_RESERVED),
    CommandSpec::new("var", BUILTIN),
    CommandSpec::new(
        "verify-commit",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("verify-pack", BUILTIN),
    CommandSpec::new(
        "verify-tag",
        BUILTIN
            .union(COMMAND_SPECIFIC_HELP)
            .union(SETUP_BEFORE_HELP),
    ),
    CommandSpec::new("version", BUILTIN_PARSEOPT),
    CommandSpec::new("web--browse", RESERVED_CORE_HELPER),
    CommandSpec::new("whatchanged", BUILTIN),
    CommandSpec::new("worktree", BUILTIN_MAIN),
    CommandSpec::new("write-tree", BUILTIN),
]);

const GUIDE_PAGES: &[(&str, &str)] = &[
    ("core-tutorial", "A Git core tutorial for developers"),
    ("credentials", "Providing usernames and passwords to Git"),
    ("cvs-migration", "Git for CVS users"),
    ("diffcore", "Tweaking diff output"),
    (
        "everyday",
        "A useful minimum set of commands for Everyday Git",
    ),
    ("faq", "Frequently asked questions about using Git"),
    ("glossary", "A Git Glossary"),
    ("namespaces", "Git namespaces"),
    (
        "remote-helpers",
        "Helper programs to interact with remote repositories",
    ),
    ("submodules", "Mounting one repository inside another"),
    ("tutorial", "A tutorial introduction to Git"),
    ("tutorial-2", "A tutorial introduction to Git: part two"),
    ("workflows", "An overview of recommended workflows with Git"),
];

const USER_INTERFACES: &[(&str, &str)] = &[
    ("attributes", "Defining attributes per path"),
    ("cli", "Git command-line interface and conventions"),
    ("hooks", "Hooks used by Git"),
    (
        "ignore",
        "Specifies intentionally untracked files to ignore",
    ),
    (
        "mailmap",
        "Map author/committer names and/or E-Mail addresses",
    ),
    ("modules", "Defining submodule properties"),
    ("repository-layout", "Git Repository Layout"),
    ("revisions", "Specifying revisions and ranges for Git"),
];

const DEVELOPER_INTERFACES: &[(&str, &str)] = &[
    ("format-bundle", "The bundle file format"),
    ("format-chunk", "Chunk-based file formats"),
    ("format-commit-graph", "Git commit-graph format"),
    ("format-index", "Git index format"),
    ("format-pack", "Git pack format"),
    ("format-signature", "Git cryptographic signature formats"),
    ("protocol-capabilities", "Protocol v0 and v1 capabilities"),
    ("protocol-common", "Things common to various protocols"),
    ("protocol-http", "Git HTTP-based protocols"),
    ("protocol-pack", "How packs are transferred over-the-wire"),
    ("protocol-v2", "Git Wire Protocol, Version 2"),
];

const CONFIG_VARIABLES: &[&str] = &[
    "add.ignoreErrors",
    "advice.defaultBranchName",
    "advice.statusHints",
    "am.keepcr",
    "apply.whitespace",
    "branch.",
    "branch.autoSetupMerge",
    "branch.autoSetupRebase",
    "branch.sort",
    "browser.",
    "checkout.defaultRemote",
    "clean.requireForce",
    "clone.defaultRemoteName",
    "color.pager",
    "color.ui",
    "column.ui",
    "commit.gpgSign",
    "completion.commands",
    "core.abbrev",
    "core.autocrlf",
    "core.bare",
    "core.editor",
    "core.fileMode",
    "core.repositoryFormatVersion",
    "credential.helper",
    "diff.algorithm",
    "diff.context",
    "fetch.prune",
    "format.pretty",
    "gc.auto",
    "grep.patternType",
    "help.autocorrect",
    "help.browser",
    "help.format",
    "help.htmlpath",
    "include.path",
    "includeIf.",
    "init.defaultBranch",
    "log.date",
    "log.decorate",
    "log.diffMerges",
    "merge.conflictStyle",
    "pull.rebase",
    "push.default",
    "rebase.autoSquash",
    "remote.",
    "remote.pushDefault",
    "rerere.enabled",
    "status.showUntrackedFiles",
    "submodule.",
    "submodule.active",
    "submodule.alternateErrorStrategy",
    "submodule.alternateLocation",
    "submodule.fetchJobs",
    "submodule.propagateBranches",
    "submodule.recurse",
    "tag.gpgSign",
    "user.email",
    "user.name",
];

const CONFIG_ALL_VARIABLES: &[&str] = &[
    "branch.<name>.description",
    "branch.<name>.merge",
    "branch.<name>.mergeOptions",
    "branch.<name>.pushRemote",
    "branch.<name>.rebase",
    "branch.<name>.remote",
    "browser.<tool>.cmd",
    "browser.<tool>.path",
    "includeIf.<condition>.path",
    "remote.<name>.fetch",
    "remote.<name>.followRemoteHEAD",
    "remote.<name>.mirror",
    "remote.<name>.negotiationInclude",
    "remote.<name>.negotiationRestrict",
    "remote.<name>.partialclonefilter",
    "remote.<name>.promisor",
    "remote.<name>.proxy",
    "remote.<name>.proxyAuthMethod",
    "remote.<name>.prune",
    "remote.<name>.pruneTags",
    "remote.<name>.push",
    "remote.<name>.pushurl",
    "remote.<name>.receivepack",
    "remote.<name>.serverOption",
    "remote.<name>.skipDefaultUpdate",
    "remote.<name>.skipFetchAll",
    "remote.<name>.tagOpt",
    "remote.<name>.uploadpack",
    "remote.<name>.url",
    "remote.<name>.vcs",
    "submodule.<name>.active",
    "submodule.<name>.branch",
    "submodule.<name>.fetchRecurseSubmodules",
    "submodule.<name>.gitdir",
    "submodule.<name>.ignore",
    "submodule.<name>.update",
    "submodule.<name>.url",
];

pub(crate) fn is_builtin_command(command: &str) -> bool {
    matches!(command, "-v" | "--version") || COMMAND_REGISTRY.contains_with(command, NATIVE)
}

pub(crate) fn is_main_command(command: &str) -> bool {
    !matches!(command, "gitk" | "testkit") && COMMAND_REGISTRY.find(command).is_some()
}

pub(crate) fn supports_usage_help(command: &str) -> bool {
    COMMAND_REGISTRY.find(command).is_some_and(|command| {
        command
            .flags
            .intersects(NATIVE.union(GIT_BUILTIN).union(USAGE_HELP))
    })
}

pub(crate) fn is_reserved_git_core_helper(command: &str) -> bool {
    COMMAND_REGISTRY.contains_with(command, RESERVED_CORE_HELPER)
}

pub(crate) fn has_command_specific_help(command: &str) -> bool {
    COMMAND_REGISTRY.contains_with(command, COMMAND_SPECIFIC_HELP)
}

pub(crate) fn runs_setup_before_command_help(command: &str) -> bool {
    COMMAND_REGISTRY.contains_with(command, SETUP_BEFORE_HELP)
}

pub(crate) fn cmd_help(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut mode = HelpMode::Default;
    let mut format = HelpFormat::Default;
    let mut command = None;
    let mut exclude_guides = false;
    let mut list_modifiers = 0usize;
    let mut doc_format_flags = 0usize;

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help_usage();
                return Err(GitError::Exit(129));
            }
            "-a" | "--all" => {
                mode = set_mode(mode, HelpMode::All)?;
            }
            "-g" | "--guides" => {
                mode = set_mode(mode, HelpMode::Guides)?;
            }
            "-c" | "--config" => {
                mode = set_mode(mode, HelpMode::Config)?;
            }
            "--config-for-completion" => {
                mode = set_mode(mode, HelpMode::ConfigForCompletion)?;
            }
            "--config-sections-for-completion" => {
                mode = set_mode(mode, HelpMode::ConfigSectionsForCompletion)?;
            }
            "--user-interfaces" => {
                mode = set_mode(mode, HelpMode::UserInterfaces)?;
            }
            "--developer-interfaces" => {
                mode = set_mode(mode, HelpMode::DeveloperInterfaces)?;
            }
            "--exclude-guides" => exclude_guides = true,
            "--no-verbose"
            | "--verbose"
            | "-v"
            | "--external-commands"
            | "--aliases"
            | "--no-external-commands"
            | "--no-aliases" => list_modifiers += 1,
            "-m" | "--man" => {
                format = HelpFormat::Man;
                doc_format_flags += 1;
            }
            "-w" | "--web" => {
                format = HelpFormat::Web;
                doc_format_flags += 1;
            }
            "-i" | "--info" => {
                format = HelpFormat::Info;
                doc_format_flags += 1;
            }
            value if value.starts_with('-') => return help_usage_error(),
            value => {
                if command.replace(value.to_string()).is_some() {
                    return help_usage_error();
                }
            }
        }
    }

    if command.is_some() {
        match mode {
            HelpMode::Default => {}
            HelpMode::All
            | HelpMode::Guides
            | HelpMode::Config
            | HelpMode::ConfigForCompletion
            | HelpMode::ConfigSectionsForCompletion
            | HelpMode::UserInterfaces
            | HelpMode::DeveloperInterfaces => return help_usage_error(),
        }
    }
    if doc_format_flags > 0 && mode != HelpMode::Default {
        return help_usage_error();
    }
    if list_modifiers > 0 && !matches!(mode, HelpMode::All | HelpMode::Default) {
        return help_usage_error();
    }
    if exclude_guides
        && command
            .as_deref()
            .is_none_or(|name| !is_builtin_command(name))
    {
        return unknown_command(command.as_deref().unwrap_or(""), 1);
    }

    match (mode, command.as_deref()) {
        (HelpMode::Default, Some(name)) => show_doc(cli_session, name, format),
        (HelpMode::Default, None) => {
            print_common_help();
            Ok(())
        }
        (HelpMode::All, None) => {
            print_all_commands(cli_session);
            Ok(())
        }
        (HelpMode::Guides, None) => {
            print_guides();
            Ok(())
        }
        (HelpMode::Config, None) => {
            print_config_human();
            Ok(())
        }
        (HelpMode::ConfigForCompletion, None) => {
            print_config_for_completion();
            Ok(())
        }
        (HelpMode::ConfigSectionsForCompletion, None) => {
            print_config_sections_for_completion();
            Ok(())
        }
        (HelpMode::UserInterfaces, None) => {
            print_interface_list(
                "User-facing repository, command and file interfaces",
                USER_INTERFACES,
            );
            Ok(())
        }
        (HelpMode::DeveloperInterfaces, None) => {
            print_interface_list(
                "Developer-facing file formats, protocols and other interfaces",
                DEVELOPER_INTERFACES,
            );
            Ok(())
        }
        _ => help_usage_error(),
    }
}

pub(crate) fn print_common_help() {
    println!("usage: git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]");
    println!("           [--exec-path[=<path>]] [--html-path] [--man-path] [--info-path]");
    println!(
        "           [-p | --paginate | -P | --no-pager] [--no-replace-objects] [--no-lazy-fetch]"
    );
    println!("           [--no-optional-locks] [--no-advice] [--bare] [--git-dir=<path>]");
    println!("           [--work-tree=<path>] [--namespace=<name>] [--config-env=<name>=<envvar>]");
    println!("           <command> [<args>]");
    println!();
    println!("These are common Git commands used in various situations:");
    println!();
    println!("start a working area (see also: git help tutorial)");
    println!("   clone      Clone a repository into a new directory");
    println!("   init       Create an empty Git repository or reinitialize an existing one");
    println!();
    println!("work on the current change (see also: git help everyday)");
    println!("   add        Add file contents to the index");
    println!("   mv         Move or rename a file, a directory, or a symlink");
    println!("   restore    Restore working tree files");
    println!("   rm         Remove files from the working tree and from the index");
    println!();
    println!("examine the history and state (see also: git help revisions)");
    println!("   bisect     Use binary search to find the commit that introduced a bug");
    println!("   diff       Show changes between commits, commit and working tree, etc");
    println!("   grep       Print lines matching a pattern");
    println!("   log        Show commit logs");
    println!("   show       Show various types of objects");
    println!("   status     Show the working tree status");
    println!();
    println!("grow, mark and tweak your common history");
    println!("   branch     List, create, or delete branches");
    println!("   commit     Record changes to the repository");
    println!("   merge      Join two or more development histories together");
    println!("   rebase     Reapply commits on top of another base tip");
    println!("   reset      Set `HEAD` or the index to a known state");
    println!("   switch     Switch branches");
    println!("   tag        Create, list, delete or verify tags");
    println!();
    println!("collaborate (see also: git help workflows)");
    println!("   fetch      Download objects and refs from another repository");
    println!("   pull       Fetch from and integrate with another repository or a local branch");
    println!("   push       Update remote refs along with associated objects");
    println!();
    println!("'git help -a' and 'git help -g' list available subcommands and some");
    println!("concept guides. See 'git help <command>' or 'git help <concept>'");
    println!("to read about a specific subcommand or concept.");
    println!("See 'git help git' for an overview of the system.");
}

pub(crate) fn print_list_cmds(
    cli_session: &crate::session::CliSession,
    groups: &str,
) -> Result<()> {
    if groups == "parseopt" {
        println!(
            "{}",
            COMMAND_REGISTRY
                .names_with(PARSEOPT_HELPER)
                .collect::<Vec<_>>()
                .join(" ")
        );
        return Ok(());
    }

    let mut commands = BTreeSet::new();
    for group in groups.split(',') {
        match group {
            "builtins" => {
                commands.extend(COMMAND_REGISTRY.names_with(GIT_BUILTIN).map(str::to_string));
            }
            "main" => {
                commands.extend(
                    COMMAND_REGISTRY
                        .entries()
                        .iter()
                        .filter(|command| is_main_command(command.name))
                        .map(|command| command.name.to_string()),
                );
            }
            "mainporcelain" | "list-mainporcelain" => {
                commands.extend(
                    COMMAND_REGISTRY
                        .names_with(MAIN_PORCELAIN)
                        .map(str::to_string),
                );
            }
            "list-guide" => {
                commands.extend(GUIDE_PAGES.iter().map(|(name, _)| (*name).to_string()));
            }
            "alias" => {
                for (name, _) in crate::commands::alias::list_aliases(cli_session)? {
                    commands.insert(name);
                }
            }
            "others" | "nohelpers" | "list-complete" | "config" => {}
            "" => {}
            _ => {}
        }
    }

    apply_completion_command_config(cli_session, &mut commands)?;
    for command in commands {
        println!("{command}");
    }
    Ok(())
}

pub(crate) fn print_completion_helper(args: &[String]) -> bool {
    let Some((helper_index, helper)) = args.iter().enumerate().find(|(_, arg)| {
        matches!(
            arg.as_str(),
            "--git-completion-helper" | "--git-completion-helper-all"
        )
    }) else {
        return false;
    };
    let show_all = helper == "--git-completion-helper-all";
    let key = args[..helper_index]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let output = match key.as_str() {
        "checkout" | "switch" => Some(CHECKOUT_COMPLETION_HELPER),
        "clone" if show_all => Some(CLONE_COMPLETION_HELPER_ALL),
        "clone" => Some(CLONE_COMPLETION_HELPER),
        "config" => Some(CONFIG_COMPLETION_HELPER),
        "config get" => Some(CONFIG_GET_COMPLETION_HELPER),
        "config set" => Some(CONFIG_SET_COMPLETION_HELPER),
        "help" => Some(HELP_COMPLETION_HELPER),
        "ls-remote" => Some(LS_REMOTE_COMPLETION_HELPER),
        "merge-tree" => Some(MERGE_TREE_COMPLETION_HELPER),
        "notes edit" => Some(NOTES_EDIT_COMPLETION_HELPER),
        "reflog" => Some(REFLOG_COMPLETION_HELPER),
        "remote" => Some(REMOTE_COMPLETION_HELPER),
        "send-email" => Some(SEND_EMAIL_COMPLETION_HELPER),
        "symbolic-ref" => Some(SYMBOLIC_REF_COMPLETION_HELPER),
        "version" => {
            println!("{}", version_completion_helper());
            return true;
        }
        _ => Some(""),
    };
    if let Some(output) = output {
        println!("{output}");
        true
    } else {
        false
    }
}

fn apply_completion_command_config(
    cli_session: &crate::session::CliSession,
    commands: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(value) = config_value(cli_session, "completion.commands")? else {
        return Ok(());
    };
    for token in value.split_whitespace() {
        if let Some(remove) = token.strip_prefix('-') {
            commands.remove(remove);
        } else if !token.is_empty() {
            commands.insert(token.to_string());
        }
    }
    Ok(())
}

fn version_completion_helper() -> String {
    static SPECS: &[OptionSpec<'static>] = &[OptionSpec {
        short: None,
        long: Some("build-options"),
        value: OptValue::Bool,
        flags: OptFlags::NONE,
        help: "",
    }];
    completion_helper_options(SPECS, false)
}

const CHECKOUT_COMPLETION_HELPER: &str = "--guess --overlay --auto-advance --quiet --recurse-submodules --progress --merge --conflict= --detach --track --orphan= --ignore-other-worktrees --ours --theirs --patch --unified= --inter-hunk-context= --ignore-skip-worktree-bits --pathspec-from-file= --pathspec-file-nul --no-guess -- --no-overlay --no-auto-advance --no-quiet --no-recurse-submodules --no-progress --no-merge --no-conflict --no-detach --no-track --no-orphan --no-ignore-other-worktrees --no-patch --no-ignore-skip-worktree-bits --no-pathspec-from-file --no-pathspec-file-nul";

const MERGE_TREE_COMPLETION_HELPER: &str = "--write-tree --trivial-merge --messages --quiet --name-only --allow-unrelated-histories --stdin --merge-base= --strategy-option= --no-messages -- --no-merge-base --no-strategy-option";

const CLONE_COMPLETION_HELPER: &str = "--verbose --quiet --progress --reject-shallow --no-checkout --bare --mirror --local --no-hardlinks --shared --recurse-submodules --jobs= --template= --reference= --reference-if-able= --dissociate --origin= --branch= --revision= --upload-pack= --depth= --shallow-since= --shallow-exclude= --single-branch --tags --shallow-submodules --separate-git-dir= --ref-format= --config= --server-option= --ipv4 --ipv6 --filter= --also-filter-submodules --remote-submodules --sparse --bundle-uri= --checkout --hardlinks -- --no-verbose --no-quiet --no-progress --no-reject-shallow --no-bare --no-mirror --no-local --no-shared --no-recurse-submodules --no-recursive --no-jobs --no-template --no-reference --no-reference-if-able --no-dissociate --no-origin --no-branch --no-revision --no-upload-pack --no-depth --no-shallow-since --no-shallow-exclude --no-single-branch --no-tags --no-shallow-submodules --no-separate-git-dir --no-ref-format --no-config --no-server-option --no-filter --no-also-filter-submodules --no-remote-submodules --no-sparse --no-bundle-uri";

const CLONE_COMPLETION_HELPER_ALL: &str = "--verbose --quiet --progress --reject-shallow --no-checkout --bare --naked --mirror --local --no-hardlinks --shared --recurse-submodules --recursive --jobs= --template= --reference= --reference-if-able= --dissociate --origin= --branch= --revision= --upload-pack= --depth= --shallow-since= --shallow-exclude= --single-branch --tags --shallow-submodules --separate-git-dir= --ref-format= --config= --server-option= --ipv4 --ipv6 --filter= --also-filter-submodules --remote-submodules --sparse --bundle-uri= --checkout --hardlinks -- --no-verbose --no-quiet --no-progress --no-reject-shallow --no-bare --no-naked --no-mirror --no-local --no-shared --no-recurse-submodules --no-recursive --no-jobs --no-template --no-reference --no-reference-if-able --no-dissociate --no-origin --no-branch --no-revision --no-upload-pack --no-depth --no-shallow-since --no-shallow-exclude --no-single-branch --no-tags --no-shallow-submodules --no-separate-git-dir --no-ref-format --no-config --no-server-option --no-filter --no-also-filter-submodules --no-remote-submodules --no-sparse --no-bundle-uri";

const CONFIG_COMPLETION_HELPER: &str = "list get set unset rename-section remove-section edit";
const CONFIG_GET_COMPLETION_HELPER: &str = "--global --system --local --worktree --file= --blob= --all --regexp --value= --fixed-value --url= --null --name-only --show-origin --show-scope --show-names --type= --bool --int --bool-or-int --bool-or-str --path --expiry-date --includes --default= --no-global -- --no-system --no-local --no-worktree --no-file --no-blob --no-all --no-regexp --no-value --no-fixed-value --no-url --no-null --no-name-only --no-show-origin --no-show-scope --no-show-names --no-type --no-includes --no-default";
const CONFIG_SET_COMPLETION_HELPER: &str = "--global --system --local --worktree --file= --blob= --type= --bool --int --bool-or-int --bool-or-str --path --expiry-date --all --value= --fixed-value --comment= --append --no-global -- --no-system --no-local --no-worktree --no-file --no-blob --no-type --no-all --no-value --no-fixed-value --no-comment --no-append";
const HELP_COMPLETION_HELPER: &str = "--all --external-commands --aliases --man --web --info --verbose --guides --user-interfaces --developer-interfaces --config --no-external-commands -- --no-aliases --no-man --no-web --no-info --no-verbose";
const LS_REMOTE_COMPLETION_HELPER: &str = "--quiet --upload-pack= --tags --branches --refs --get-url --sort= --symref --server-option= --no-quiet -- --no-upload-pack --no-tags --no-branches --no-refs --no-get-url --no-sort --no-symref --no-server-option";
const NOTES_EDIT_COMPLETION_HELPER: &str = "--message= --file= --reedit-message= --reuse-message= --edit --allow-empty --separator --stripspace --no-edit -- --no-allow-empty --no-separator --no-stripspace";
const REFLOG_COMPLETION_HELPER: &str = "show list exists write delete drop expire";
const REMOTE_COMPLETION_HELPER: &str = "--verbose add rename remove set-head set-branches get-url set-url show prune update --no-verbose";
const SEND_EMAIL_COMPLETION_HELPER: &str = "--cover-from-description= --cover-letter --validate --full-index --not --all --no-prefix --src-prefix= --dst-prefix= --notes";
const SYMBOLIC_REF_COMPLETION_HELPER: &str =
    "--quiet --delete --short --recurse --no-quiet -- --no-delete --no-short --no-recurse";

pub(crate) fn unknown_command(command: &str, code: i32) -> Result<()> {
    eprintln!("git: '{command}' is not a git command. See 'git --help'.");
    Err(GitError::Exit(code))
}

pub(crate) fn print_command_usage(command: &str) {
    if let Some(usage) = command_usage_override(command) {
        print!("{usage}");
        return;
    }
    crate::command_synopsis::print_command_synopsis(command);
}

fn command_usage_override(command: &str) -> Option<String> {
    let spec = COMMAND_USAGE_OVERRIDES
        .iter()
        .find(|spec| spec.command == command)?;
    if !spec.options.is_empty() {
        return Some(usage_with_options(spec.options, spec.usage));
    }
    let mut out = String::new();
    for (index, line) in spec.usage.iter().enumerate() {
        if index == 0 {
            out.push_str("usage: ");
        } else {
            out.push_str("   or: ");
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn set_mode(current: HelpMode, next: HelpMode) -> Result<HelpMode> {
    if current != HelpMode::Default {
        return help_usage_error();
    }
    Ok(next)
}

fn show_doc(
    cli_session: &crate::session::CliSession,
    name: &str,
    format: HelpFormat,
) -> Result<()> {
    if !is_builtin_command(name) && !is_guide(name) && !is_interface(name) && name != "git" {
        return unknown_command(name, 1);
    }
    let format = match format {
        HelpFormat::Default => config_value(cli_session, "help.format")?
            .as_deref()
            .map(HelpFormat::from_config)
            .unwrap_or(HelpFormat::Man),
        other => other,
    };
    match format {
        HelpFormat::Web => open_html_doc(cli_session, name),
        HelpFormat::Man | HelpFormat::Info | HelpFormat::Default => Ok(()),
    }
}

fn open_html_doc(cli_session: &crate::session::CliSession, name: &str) -> Result<()> {
    let html_path = config_value(cli_session, "help.htmlpath")?.unwrap_or_else(|| ".".to_string());
    let page = html_page_for(name);
    let target = if html_path.contains("://") {
        format!("{}/{}", html_path.trim_end_matches('/'), page)
    } else {
        let path = Path::new(&html_path).join(page);
        if !path.exists() {
            return Err(GitError::Exit(1));
        }
        path.to_string_lossy().into_owned()
    };
    let browser = config_value(cli_session, "help.browser")?.unwrap_or_default();
    let browser_cmd = if browser.is_empty() {
        config_value(cli_session, "browser.test.cmd")?.unwrap_or_else(|| "true".to_string())
    } else {
        config_value(cli_session, &format!("browser.{browser}.cmd"))?.unwrap_or(browser)
    };
    let status = Command::new(&browser_cmd)
        .arg(&target)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn html_page_for(name: &str) -> String {
    match name {
        "revisions" => "gitrevisions.html".to_string(),
        "cli" => "gitcli.html".to_string(),
        "core-tutorial" => "gitcore-tutorial.html".to_string(),
        "cvs-migration" => "gitcvs-migration.html".to_string(),
        "diffcore" => "gitdiffcore.html".to_string(),
        "everyday" => "giteveryday.html".to_string(),
        "faq" => "gitfaq.html".to_string(),
        "glossary" => "gitglossary.html".to_string(),
        "namespaces" => "gitnamespaces.html".to_string(),
        "repository-layout" => "gitrepository-layout.html".to_string(),
        "tutorial" => "gittutorial.html".to_string(),
        "tutorial-2" => "gittutorial-2.html".to_string(),
        "workflows" => "gitworkflows.html".to_string(),
        other => format!("git-{other}.html"),
    }
}

fn config_value(cli_session: &crate::session::CliSession, key: &str) -> Result<Option<String>> {
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir().ok();
    let common_git_dir = git_dir
        .as_ref()
        .and_then(|dir| common_git_dir_for_git_dir(dir).ok());
    let context = ConfigIncludeContext::new(common_git_dir.clone(), None);
    let mut config = sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)
        .map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        cwd,
    )
    .map_err(report_config_setup_error)?;
    let Some((section, rest)) = key.split_once('.') else {
        return Ok(None);
    };
    let (subsection, entry_key) = match rest.rsplit_once('.') {
        Some((subsection, entry_key)) if section == "browser" => (Some(subsection), entry_key),
        _ => (None, rest),
    };
    Ok(config
        .get(section, subsection, entry_key)
        .map(str::to_string))
}

fn is_guide(name: &str) -> bool {
    GUIDE_PAGES.iter().any(|(guide, _)| *guide == name)
}

fn is_interface(name: &str) -> bool {
    USER_INTERFACES
        .iter()
        .chain(DEVELOPER_INTERFACES.iter())
        .any(|(page, _)| *page == name)
}

fn print_all_commands(cli_session: &crate::session::CliSession) {
    println!("See 'git help <command>' to read about a specific subcommand");
    print_command_section(
        "Main Porcelain Commands",
        &[
            ("add", "Add file contents to the index"),
            ("am", "Apply a series of patches from a mailbox"),
            ("archive", "Create an archive of files from a named tree"),
            (
                "bisect",
                "Use binary search to find the commit that introduced a bug",
            ),
            ("branch", "List, create, or delete branches"),
            ("bundle", "Move objects and refs by archive"),
            ("checkout", "Switch branches or restore working tree files"),
            (
                "cherry-pick",
                "Apply the changes introduced by some existing commits",
            ),
            ("clean", "Remove untracked files from the working tree"),
            ("clone", "Clone a repository into a new directory"),
            ("commit", "Record changes to the repository"),
            (
                "describe",
                "Give an object a human readable name based on an available ref",
            ),
            (
                "diff",
                "Show changes between commits, commit and working tree, etc",
            ),
            ("fetch", "Download objects and refs from another repository"),
            ("format-patch", "Prepare patches for e-mail submission"),
            (
                "gc",
                "Cleanup unnecessary files and optimize the local repository",
            ),
            ("grep", "Print lines matching a pattern"),
            (
                "init",
                "Create an empty Git repository or reinitialize an existing one",
            ),
            ("log", "Show commit logs"),
            ("merge", "Join two or more development histories together"),
            ("mv", "Move or rename a file, a directory, or a symlink"),
            ("notes", "Add or inspect object notes"),
            (
                "pull",
                "Fetch from and integrate with another repository or a local branch",
            ),
            ("push", "Update remote refs along with associated objects"),
            (
                "range-diff",
                "Compare two commit ranges (e.g. two versions of a branch)",
            ),
            ("rebase", "Reapply commits on top of another base tip"),
            ("reset", "Set `HEAD` or the index to a known state"),
            ("restore", "Restore working tree files"),
            ("revert", "Revert some existing commits"),
            (
                "rm",
                "Remove files from the working tree and from the index",
            ),
            ("shortlog", "Summarize 'git log' output"),
            ("show", "Show various types of objects"),
            (
                "sparse-checkout",
                "Reduce your working tree to a subset of tracked files",
            ),
            (
                "stash",
                "Stash the changes in a dirty working directory away",
            ),
            ("status", "Show the working tree status"),
            ("submodule", "Initialize, update or inspect submodules"),
            ("switch", "Switch branches"),
            ("tag", "Create, list, delete or verify tags"),
            ("worktree", "Manage multiple working trees"),
        ],
    );
    print_command_section(
        "Ancillary Commands / Manipulators",
        &[
            ("config", "Get and set repository or global options"),
            ("fast-import", "Backend for fast Git data importers"),
            ("filter-branch", "Rewrite branches"),
            (
                "mergetool",
                "Run merge conflict resolution tools to resolve merge conflicts",
            ),
            (
                "pack-refs",
                "Pack heads and tags for efficient repository access",
            ),
            (
                "prune",
                "Prune all unreachable objects from the object database",
            ),
            ("reflog", "Manage reflog information"),
            ("refs", "Low-level access to refs"),
            ("remote", "Manage set of tracked repositories"),
            ("repack", "Pack unpacked objects in a repository"),
            ("replace", "Create, list, delete refs to replace objects"),
        ],
    );
    print_command_section(
        "Ancillary Commands / Interrogators",
        &[
            ("annotate", "Annotate file lines with commit information"),
            (
                "blame",
                "Show what revision and author last modified each line of a file",
            ),
            (
                "bugreport",
                "Collect information for user to file a bug report",
            ),
            (
                "count-objects",
                "Count unpacked number of objects and their disk consumption",
            ),
            ("difftool", "Show changes using common diff tools"),
            (
                "fsck",
                "Verifies the connectivity and validity of the objects in the database",
            ),
            ("help", "Display help information about Git"),
            (
                "merge-tree",
                "Perform merge without touching index or working tree",
            ),
            ("rerere", "Reuse recorded resolution of conflicted merges"),
            ("show-branch", "Show branches and their commits"),
            ("verify-commit", "Check the GPG signature of commits"),
            ("verify-tag", "Check the GPG signature of tags"),
            ("version", "Display version information about Git"),
            (
                "whatchanged",
                "Show logs with differences each commit introduces",
            ),
        ],
    );
    print_heading("Interacting with Others");
    print_command_section(
        "Low-level Commands / Manipulators",
        &[
            ("apply", "Apply a patch to files and/or to the index"),
            (
                "checkout-index",
                "Copy files from the index to the working tree",
            ),
            ("commit-graph", "Write and verify Git commit-graph files"),
            ("commit-tree", "Create a new commit object"),
            (
                "hash-object",
                "Compute object ID and optionally create an object from a file",
            ),
            (
                "index-pack",
                "Build pack index file for an existing packed archive",
            ),
            ("merge-file", "Run a three-way file merge"),
            ("mktag", "Creates a tag object with extra validation"),
            ("mktree", "Build a tree-object from ls-tree formatted text"),
            ("multi-pack-index", "Write and verify multi-pack-indexes"),
            ("pack-objects", "Create a packed archive of objects"),
            (
                "prune-packed",
                "Remove extra objects that are already in pack files",
            ),
            ("read-tree", "Reads tree information into the index"),
            (
                "replay",
                "EXPERIMENTAL: Replay commits on a new base, works with bare repos too",
            ),
            ("symbolic-ref", "Read, modify and delete symbolic refs"),
            ("unpack-objects", "Unpack objects from a packed archive"),
            (
                "update-index",
                "Register file contents in the working tree to the index",
            ),
            (
                "update-ref",
                "Update the object name stored in a ref safely",
            ),
            ("write-tree", "Create a tree object from the current index"),
        ],
    );
    print_command_section(
        "Low-level Commands / Interrogators",
        &[
            (
                "cat-file",
                "Provide contents or details of repository objects",
            ),
            (
                "diff-files",
                "Compares files in the working tree and the index",
            ),
            ("diff-index", "Compare a tree to the working tree or index"),
            (
                "diff-tree",
                "Compares the content and mode of blobs found via two tree objects",
            ),
            ("for-each-ref", "Output information on each ref"),
            (
                "get-tar-commit-id",
                "Extract commit ID from an archive created using git-archive",
            ),
            (
                "last-modified",
                "EXPERIMENTAL: Show when files were last modified",
            ),
            (
                "ls-files",
                "Show information about files in the index and the working tree",
            ),
            ("ls-remote", "List references in a remote repository"),
            ("ls-tree", "List the contents of a tree object"),
            (
                "merge-base",
                "Find as good common ancestors as possible for a merge",
            ),
            ("name-rev", "Find symbolic names for given revs"),
            ("repo", "Retrieve information about the repository"),
            (
                "rev-list",
                "Lists commit objects in reverse chronological order",
            ),
            ("rev-parse", "Pick out and massage parameters"),
            ("show-index", "Show packed archive index"),
            ("show-ref", "List references in a local repository"),
            (
                "unpack-file",
                "Creates a temporary file with a blob's contents",
            ),
            ("var", "Show a Git logical variable"),
            ("verify-pack", "Validate packed Git archive files"),
        ],
    );
    print_command_section(
        "Low-level Commands / Syncing Repositories",
        &[
            ("daemon", "A really simple server for Git repositories"),
            (
                "fetch-pack",
                "Receive missing objects from another repository",
            ),
            (
                "send-pack",
                "Push objects over Git protocol to another repository",
            ),
            (
                "update-server-info",
                "Update auxiliary info file to help dumb servers",
            ),
        ],
    );
    print_command_section(
        "Low-level Commands / Internal Helpers",
        &[
            ("check-attr", "Display gitattributes information"),
            ("check-ignore", "Debug gitignore / exclude files"),
            (
                "check-mailmap",
                "Show canonical names and email addresses of contacts",
            ),
            (
                "check-ref-format",
                "Ensures that a reference name is well formed",
            ),
            ("fmt-merge-msg", "Produce a merge commit message"),
            ("hook", "Run git hooks"),
            (
                "interpret-trailers",
                "Add or parse structured information in commit messages",
            ),
            ("patch-id", "Compute unique IDs for patches"),
            ("stripspace", "Remove unnecessary whitespace"),
        ],
    );
    print_interface_section(
        "User-facing repository, command and file interfaces",
        USER_INTERFACES,
    );
    print_interface_section(
        "Developer-facing file formats, protocols and other interfaces",
        DEVELOPER_INTERFACES,
    );
    print_alias_section(cli_session);
}

/// List configured `alias.*` entries under git's "Command aliases" heading
/// (omitted entirely when no aliases are defined).
fn print_alias_section(cli_session: &crate::session::CliSession) {
    let Ok(aliases) = crate::commands::alias::list_aliases(cli_session) else {
        return;
    };
    if aliases.is_empty() {
        return;
    }
    let width = aliases
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        .max(1);
    print_heading("Command aliases");
    for (name, value) in aliases {
        let pad = width.saturating_sub(name.chars().count());
        println!("   {name}{}   {value}", " ".repeat(pad));
    }
}

fn print_command_section(title: &str, rows: &[(&str, &str)]) {
    print_heading(title);
    for (name, description) in rows {
        println!("   {name:<23} {description}");
    }
}

fn print_heading(title: &str) {
    println!("\n{title}");
}

fn print_guides() {
    println!("The Git concept guides are:");
    for (name, description) in GUIDE_PAGES {
        println!("   {name:<16} {description}");
    }
    println!(
        "\n'git help -a' and 'git help -g' list available subcommands and some\n\
concept guides. See 'git help <command>' or 'git help <concept>'\n\
to read about a specific subcommand or concept.\n\
See 'git help git' for an overview of the system."
    );
}

fn print_interface_list(title: &str, rows: &[(&str, &str)]) {
    println!("{title}");
    for (name, description) in rows {
        println!("   {name:<23} {description}");
    }
}

fn print_interface_section(title: &str, rows: &[(&str, &str)]) {
    println!("\n{title}");
    for (name, description) in rows {
        println!("   {name:<23} {description}");
    }
}

fn print_config_human() {
    let mut names = CONFIG_VARIABLES
        .iter()
        .chain(CONFIG_ALL_VARIABLES.iter())
        .copied()
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    for name in names {
        if name.ends_with('.') {
            continue;
        }
        println!("{name}");
    }
    println!("\n'git help config' for more information");
}

fn print_config_for_completion() {
    let mut names = CONFIG_VARIABLES.to_vec();
    names.sort_unstable();
    names.dedup();
    for name in names {
        println!("{name}");
    }
}

fn print_config_sections_for_completion() {
    let mut sections = BTreeSet::new();
    for name in CONFIG_VARIABLES {
        if let Some((section, _)) = name.split_once('.') {
            sections.insert(section);
        }
    }
    for section in sections {
        println!("{section}");
    }
}

fn help_usage_error<T>() -> Result<T> {
    print_help_usage();
    Err(GitError::Exit(129))
}

fn print_help_usage() {
    println!(
        "usage: git help [-a|--all] [--[no-]verbose] [--[no-]external-commands] [--[no-]aliases]\n\
   or: git help [[-i|--info] [-m|--man] [-w|--web]] [<command>|<doc>]\n\
   or: git help [-g|--guides]\n\
   or: git help [-c|--config]\n\
   or: git help [--user-interfaces]\n\
   or: git help [--developer-interfaces]\n\n\
    -a, --all             print all available commands\n\
    --[no-]external-commands\n\
                          show external commands in --all\n\
    --[no-]aliases        show aliases in --all\n\
    -m, --[no-]man        show man page\n\
    -w, --[no-]web        show manual in web browser\n\
    -i, --[no-]info       show info page\n\
    -v, --[no-]verbose    print command description\n\
    -g, --guides          print list of useful guides\n\
    --user-interfaces     print list of user-facing repository, command and file interfaces\n\
    --developer-interfaces\n\
                          print list of file formats, protocols and other developer interfaces\n\
    -c, --config          print all configuration variable names"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpMode {
    Default,
    All,
    Guides,
    Config,
    ConfigForCompletion,
    ConfigSectionsForCompletion,
    UserInterfaces,
    DeveloperInterfaces,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpFormat {
    Default,
    Man,
    Web,
    Info,
}

impl HelpFormat {
    fn from_config(value: &str) -> Self {
        match value {
            "html" | "web" => HelpFormat::Web,
            "info" => HelpFormat::Info,
            _ => HelpFormat::Man,
        }
    }
}

#[cfg(test)]
mod command_registry_tests {
    use super::*;

    #[test]
    fn registry_is_sorted_and_categories_have_expected_shape() {
        assert!(COMMAND_REGISTRY.is_sorted_unique());
        assert_eq!(
            COMMAND_REGISTRY.names_with(NATIVE).count(),
            if cfg!(feature = "git-compat-i18n") {
                140
            } else {
                139
            }
        );
        assert_eq!(COMMAND_REGISTRY.names_with(GIT_BUILTIN).count(), 149);
        assert_eq!(
            COMMAND_REGISTRY
                .entries()
                .iter()
                .filter(|command| is_main_command(command.name))
                .count(),
            178
        );
        assert_eq!(COMMAND_REGISTRY.names_with(MAIN_PORCELAIN).count(), 35);
        assert!(
            COMMAND_REGISTRY
                .entries_with(PARSEOPT_HELPER)
                .all(|command| command.flags.contains(NATIVE))
        );
        assert!(
            COMMAND_REGISTRY
                .entries()
                .iter()
                .all(|command| { command.flags.intersects(NATIVE.union(RESERVED_CORE_HELPER)) })
        );
        assert!(COMMAND_REGISTRY.contains_with("daemon", NATIVE));
        assert!(!COMMAND_REGISTRY.contains_with("daemon", GIT_BUILTIN));
        assert!(COMMAND_REGISTRY.contains_with("backfill", BUILTIN));
    }

    #[test]
    fn parseopt_helper_order_matches_git_protocol_output() {
        assert_eq!(
            COMMAND_REGISTRY
                .names_with(PARSEOPT_HELPER)
                .collect::<Vec<_>>()
                .join(" "),
            "checkout clone config help ls-remote notes remote symbolic-ref version"
        );
    }

    #[test]
    fn credential_usage_overrides_match_git_parse_options_help() {
        assert_eq!(
            command_usage_override("credential").as_deref(),
            Some("usage: git credential (fill|approve|reject)\n")
        );
        assert_eq!(
            command_usage_override("credential-cache").as_deref(),
            Some(
                "usage: git credential-cache [<options>] <action>\n\n\
                 \x20   --[no-]timeout <n>    number of seconds to cache credentials\n\
                 \x20   --[no-]socket <path>  path of cache-daemon socket\n\n"
            )
        );
        assert_eq!(
            command_usage_override("credential-store").as_deref(),
            Some(
                "usage: git credential-store [<options>] <action>\n\n\
                 \x20   --[no-]file <path>    fetch and store credentials in <path>\n\n"
            )
        );
    }

    #[test]
    fn command_specific_help_setup_is_declarative() {
        assert_eq!(
            COMMAND_REGISTRY
                .names_with(COMMAND_SPECIFIC_HELP)
                .collect::<Vec<_>>(),
            [
                "diff-files",
                "diff-index",
                "interpret-trailers",
                "maintenance",
                "mktag",
                "patch-id",
                "replay",
                "shortlog",
                "show-branch",
                "submodule",
                "verify-commit",
                "verify-tag",
            ]
        );
        assert_eq!(
            COMMAND_REGISTRY
                .names_with(SETUP_BEFORE_HELP)
                .collect::<Vec<_>>(),
            [
                "diff-files",
                "diff-index",
                "interpret-trailers",
                "maintenance",
                "patch-id",
                "shortlog",
                "show-branch",
                "verify-commit",
                "verify-tag",
            ]
        );
    }

    #[test]
    fn main_only_commands_do_not_become_builtin() {
        assert!(!is_builtin_command("cherry"));
        assert!(!is_builtin_command("gitk"));
        assert!(is_reserved_git_core_helper("cherry"));
        assert!(is_reserved_git_core_helper("gitk"));
        assert!(is_builtin_command("checkout"));
        assert!(is_builtin_command("version"));
        assert!(is_builtin_command("-v"));
        assert!(is_builtin_command("--version"));
        assert!(is_reserved_git_core_helper("remote-https"));
        assert!(is_reserved_git_core_helper("request-pull"));
        assert!(!is_reserved_git_core_helper("lfs"));
    }
}
