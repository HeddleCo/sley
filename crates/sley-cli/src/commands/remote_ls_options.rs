use super::{LsRemoteOptions, LsRemoteSort};
use crate::commands::cli_options::{last_tri_state_bool, opt_bool, opt_str, option_str};
use crate::*;
use sley_options::{parse_options, OptionName, OptionSpec, ParsedValue};

const LS_REMOTE_USAGE: &[&str] = &[
    "git ls-remote [--branches] [--tags] [--refs] [--upload-pack=<exec>]",
    "                     [-q | --quiet] [--exit-code] [--get-url] [--sort=<key>]",
    "                     [--symref] [<repository> [<patterns>...]]",
];

fn ls_remote_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(Some('q'), Some("quiet"), sley_options::OptFlags::NONE, "do not print remote URL"),
        opt_bool(Some('t'), Some("tags"), sley_options::OptFlags::NONE, "limit to tags"),
        opt_bool(Some('b'), Some("branches"), sley_options::OptFlags::NONE, "limit to branches"),
        opt_bool(Some('h'), None, sley_options::OptFlags::NONE, "limit to branches"),
        opt_bool(None, Some("heads"), sley_options::OptFlags::NONE, "limit to branches"),
        opt_bool(None, Some("refs"), sley_options::OptFlags::NONE, "do not show peeled tags"),
        opt_bool(None, Some("symref"), sley_options::OptFlags::NONE, "show underlying ref"),
        opt_bool(None, Some("exit-code"), sley_options::OptFlags::NONE, "exit with code 2 if no refs"),
        opt_bool(None, Some("get-url"), sley_options::OptFlags::NONE, "take url.<base>.insteadOf into account"),
        opt_str(None, Some("upload-pack"), "exec", sley_options::OptFlags::NONE, "path of git-upload-pack"),
        opt_str(Some('o'), Some("server-option"), "server-specific", sley_options::OptFlags::NONE, "option to transmit"),
        opt_str(None, Some("sort"), "key", sley_options::OptFlags::NONE, "field name to sort on"),
    ];
    SPECS
}

pub(super) fn setup_ls_remote_options(args: &[String]) -> Result<LsRemoteOptions> {
    let parsed = match parse_options(args, ls_remote_option_specs(), LS_REMOTE_USAGE) {
        Ok(parsed) => parsed,
        Err(_) => return ls_remote_usage(),
    };
    let mut options = LsRemoteOptions::default();
    if let Some(value) = last_tri_state_bool(&parsed, "quiet") {
        options.quiet = value;
    }
    if let Some(value) = last_tri_state_bool(&parsed, "tags") {
        options.tags = value;
    }
    let heads = parsed
        .options
        .iter()
        .filter(|option| {
            matches!(
                (option.short, option.long),
                (Some('b'), _)
                    | (Some('h'), _)
                    | (_, Some("branches"))
                    | (_, Some("heads"))
            )
        })
        .filter_map(|option| match option.value {
            ParsedValue::Bool(value) => Some(value),
            _ => None,
        })
        .last();
    if let Some(value) = heads {
        options.heads = value;
    }
    if let Some(value) = last_tri_state_bool(&parsed, "refs") {
        options.refs_only = value;
    }
    if let Some(value) = last_tri_state_bool(&parsed, "symref") {
        options.symref = value;
    }
    if let Some(value) = last_tri_state_bool(&parsed, "exit-code") {
        options.exit_code = value;
    }
    if let Some(value) = last_tri_state_bool(&parsed, "get-url") {
        options.get_url = value;
    }
    for option in &parsed.options {
        if matches!(option.name, OptionName::NegatedLong("sort")) {
            options.sort = None;
            continue;
        }
        match (option.short, option.long) {
            (_, Some("upload-pack")) if !matches!(option.name, OptionName::NegatedLong("upload-pack")) => {
                if let Some(value) = option_str(option) {
                    options.upload_pack_command = Some(value.to_string());
                }
            }
            (Some('o'), Some("server-option")) | (_, Some("server-option"))
                if !matches!(option.name, OptionName::NegatedLong("server-option")) =>
            {
                if let Some(value) = option_str(option) {
                    options.server_options.push(value.to_string());
                }
            }
            (_, Some("sort")) => {
                if let Some(value) = option_str(option) {
                    options.sort = Some(parse_ls_remote_sort(value)?);
                }
            }
            _ => {}
        }
    }
    if let Some(repository) = parsed.positionals.first() {
        options.repository = Some((*repository).to_string());
        options.patterns = parsed.positionals[1..]
            .iter()
            .map(|value| (*value).to_string())
            .collect();
    }
    Ok(options)
}

fn parse_ls_remote_sort(value: &str) -> Result<LsRemoteSort> {
    match value {
        "refname" => Ok(LsRemoteSort::Refname),
        "-refname" => Ok(LsRemoteSort::RefnameDescending),
        "version:refname" | "v:refname" => Ok(LsRemoteSort::VersionRefname),
        "-version:refname" | "-v:refname" => Ok(LsRemoteSort::VersionRefnameDescending),
        "objectname" => Ok(LsRemoteSort::ObjectName),
        "-objectname" => Ok(LsRemoteSort::ObjectNameDescending),
        "objecttype" => Ok(LsRemoteSort::ObjectType),
        "-objecttype" => Ok(LsRemoteSort::ObjectTypeDescending),
        "objectsize" => Ok(LsRemoteSort::ObjectSize),
        "-objectsize" => Ok(LsRemoteSort::ObjectSizeDescending),
        "objectsize:disk" => Ok(LsRemoteSort::ObjectSizeDisk),
        "-objectsize:disk" => Ok(LsRemoteSort::ObjectSizeDiskDescending),
        "authordate" => Ok(LsRemoteSort::AuthorDate),
        "-authordate" => Ok(LsRemoteSort::AuthorDateDescending),
        "committerdate" => Ok(LsRemoteSort::CommitterDate),
        "-committerdate" => Ok(LsRemoteSort::CommitterDateDescending),
        "taggerdate" => Ok(LsRemoteSort::TaggerDate),
        "-taggerdate" => Ok(LsRemoteSort::TaggerDateDescending),
        "creatordate" => Ok(LsRemoteSort::CreatorDate),
        "-creatordate" => Ok(LsRemoteSort::CreatorDateDescending),
        other => {
            eprintln!("fatal: unknown field name: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn ls_remote_usage<T>() -> Result<T> {
    eprintln!("usage: git ls-remote [--branches] [--tags] [--refs] [--upload-pack=<exec>]");
    eprintln!("                     [-q | --quiet] [--exit-code] [--get-url] [--sort=<key>]");
    eprintln!("                     [--symref] [<repository> [<patterns>...]]");
    eprintln!();
    eprintln!("    -q, --[no-]quiet      do not print remote URL");
    eprintln!("    --[no-]upload-pack <exec>");
    eprintln!("                          path of git-upload-pack on the remote host");
    eprintln!("    -t, --[no-]tags       limit to tags");
    eprintln!("    -b, --[no-]branches   limit to branches");
    eprintln!("    --[no-]refs           do not show peeled tags");
    eprintln!("    --[no-]get-url        take url.<base>.insteadOf into account");
    eprintln!("    --[no-]sort <key>     field name to sort on");
    eprintln!("    --[no-]exit-code      exit with exit code 2 if no matching refs are found");
    eprintln!(
        "    --[no-]symref         show underlying ref in addition to the object pointed by it"
    );
    eprintln!("    -o, --[no-]server-option <server-specific>");
    eprintln!("                          option to transmit");
    eprintln!();
    Err(GitError::Exit(129))
}