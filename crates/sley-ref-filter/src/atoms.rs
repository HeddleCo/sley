//! for-each-ref atom families: identity name/email/date atoms, oid atoms,
//! color escapes, and the typed-atom renderer.

use super::context::{ForEachRefFormatContext, ForEachRefSignatureVerification};
use super::{
    ForEachRefAtom, ForEachRefAtomIdentityPart, ForEachRefAtomIdentityRole, ForEachRefEmailMode,
    ForEachRefNameFormat, ForEachRefNameSource, ForEachRefStripDirection,
    for_each_ref_abbrev_oid, for_each_ref_identity_date, for_each_ref_identity_email,
    parse_for_each_ref_abbrev_width, parse_for_each_ref_hex_color,
    write_for_each_ref_identity, write_for_each_ref_identity_date_mode,
    write_for_each_ref_identity_date_raw, write_for_each_ref_identity_email_mode,
    write_for_each_ref_identity_name,
};
use sley_core::{DateMode, GitError, Result};
use std::io::Write;

pub fn write_for_each_ref_signature(
    stdout: &mut impl Write,
    verification: &dyn ForEachRefSignatureVerification,
    option: &str,
) -> Result<()> {
    match option.strip_prefix(':').unwrap_or("") {
        // The bare atom prints gpg's human-readable verification output.
        "" => stdout.write_all(verification.bare_output())?,
        // grade: 'G'/'U'/'B'/'E'/'N' — git downgrades a good-but-untrusted
        // signature to 'U', which pretty_code already encodes.
        "grade" => stdout.write_all(&[verification.grade_byte()])?,
        "key" => stdout.write_all(verification.key().as_bytes())?,
        "signer" => stdout.write_all(verification.signer().as_bytes())?,
        "fingerprint" => stdout.write_all(verification.fingerprint().as_bytes())?,
        "primarykeyfingerprint" => {
            stdout.write_all(verification.primary_fingerprint().as_bytes())?
        }
        "trustlevel" => stdout.write_all(verification.trust().as_bytes())?,
        _ => {}
    }
    Ok(())
}

pub fn for_each_ref_typed_refname<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    source: ForEachRefNameSource,
) -> &'a str {
    match source {
        ForEachRefNameSource::Ref => context.refname,
        ForEachRefNameSource::Upstream => context
            .upstream
            .as_ref()
            .map(|upstream| upstream.refname.as_str())
            .unwrap_or(""),
        ForEachRefNameSource::Push => context
            .push
            .as_ref()
            .and_then(|push| push.refname.as_deref())
            .unwrap_or(""),
    }
}

pub fn for_each_ref_typed_identity<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
) -> Option<&'a [u8]> {
    if peeled {
        let peeled = context.peeled_object.as_ref();
        return match role {
            ForEachRefAtomIdentityRole::Author => {
                peeled.and_then(|peeled| peeled.author.as_deref())
            }
            ForEachRefAtomIdentityRole::Committer => {
                peeled.and_then(|peeled| peeled.committer.as_deref())
            }
            ForEachRefAtomIdentityRole::Tagger => None,
            ForEachRefAtomIdentityRole::Creator => {
                peeled.and_then(|peeled| peeled.creator.as_deref())
            }
        };
    }

    let contents = context.contents.as_ref();
    match role {
        ForEachRefAtomIdentityRole::Author => {
            contents.and_then(|contents| contents.author.as_deref())
        }
        ForEachRefAtomIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefAtomIdentityRole::Tagger => {
            contents.and_then(|contents| contents.tagger.as_deref())
        }
        ForEachRefAtomIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    }
}

pub fn write_for_each_ref_typed_atom(
    stdout: &mut impl Write,
    atom: &ForEachRefAtom,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    match atom {
        ForEachRefAtom::Raw(_) => unreachable!("raw atoms are handled by the compatibility path"),
        ForEachRefAtom::Color(value) => {
            let color = for_each_ref_color_escape(value)?;
            if context.color {
                stdout.write_all(color.as_bytes())?;
            }
        }
        ForEachRefAtom::RefName { source, format } => {
            let refname = for_each_ref_typed_refname(context, *source);
            match format {
                ForEachRefNameFormat::Full => stdout.write_all(refname.as_bytes())?,
                ForEachRefNameFormat::Short => {
                    stdout.write_all(context.shorten_ref(refname).as_bytes())?
                }
                ForEachRefNameFormat::Strip(strip) => {
                    let refname = match strip.direction {
                        ForEachRefStripDirection::Left => {
                            super::for_each_ref_lstrip_name(refname, strip.count)
                        }
                        ForEachRefStripDirection::Right => {
                            super::for_each_ref_rstrip_name(refname, strip.count)
                        }
                    };
                    stdout.write_all(refname.as_bytes())?;
                }
            }
        }
        ForEachRefAtom::ObjectName { peeled, abbrev } => {
            let oid = if *peeled {
                context.peeled_object.as_ref().map(|peeled| &peeled.oid)
            } else {
                Some(context.oid)
            };
            if let Some(oid) = oid {
                match abbrev {
                    None => write!(stdout, "{oid}")?,
                    Some(0) => stdout.write_all(
                        for_each_ref_abbrev_oid(
                            oid,
                            context.objectname_abbrev,
                            context.objectname_candidates,
                        )
                        .as_bytes(),
                    )?,
                    Some(width) => stdout.write_all(
                        for_each_ref_abbrev_oid(oid, Some(*width), context.objectname_candidates)
                            .as_bytes(),
                    )?,
                }
            }
        }
        ForEachRefAtom::Identity { peeled, role, part } => {
            let identity = for_each_ref_typed_identity(context, *peeled, *role);
            match part {
                ForEachRefAtomIdentityPart::Full => write_for_each_ref_identity(stdout, identity)?,
                ForEachRefAtomIdentityPart::Name => {
                    write_for_each_ref_identity_name(stdout, identity)?
                }
                ForEachRefAtomIdentityPart::Email(mode) => {
                    write_for_each_ref_identity_email_mode(stdout, identity, *mode)?
                }
                ForEachRefAtomIdentityPart::Date(mode) => {
                    write_for_each_ref_identity_date_mode(stdout, identity, mode)?
                }
                ForEachRefAtomIdentityPart::DateRaw => {
                    write_for_each_ref_identity_date_raw(stdout, identity)?
                }
            }
        }
        ForEachRefAtom::ContentsLines { peeled, count } => {
            let message = if *peeled {
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_deref())
            } else {
                context
                    .contents
                    .as_ref()
                    .map(|contents| contents.message.as_ref())
            };
            if let Some(message) = message {
                super::write_for_each_ref_contents_lines(stdout, message, *count)?;
            }
        }
    }
    Ok(())
}

/// The set of `%(...email)` options, mirroring git's `email_option` bitset
/// (ref-filter.c `EO_TRIM`/`EO_LOCALPART`/`EO_MAILMAP`).
#[derive(Clone, Copy, Default)]
pub struct ForEachRefEmailOptions {
    trim: bool,
    localpart: bool,
    mailmap: bool,
}

impl ForEachRefEmailOptions {
    pub fn mode(&self) -> ForEachRefEmailMode {
        if self.localpart {
            ForEachRefEmailMode::LocalPart
        } else if self.trim {
            ForEachRefEmailMode::Trim
        } else {
            ForEachRefEmailMode::Bracketed
        }
    }

    pub fn wants_mailmap(&self) -> bool {
        self.mailmap
    }
}

/// Parse the option string after `%(authoremail:...)` exactly as git's
/// `person_email_atom_parser` does. Options are comma-separated and may repeat;
/// each must be an exact `trim`/`localpart`/`mailmap` token between commas.
/// On an unrecognized token, returns `Err(bad_arg)` where `bad_arg` is the
/// unconsumed remainder at the point of failure (git reports this verbatim).
pub fn setup_for_each_ref_email_options(
    arg: &str,
) -> std::result::Result<ForEachRefEmailOptions, String> {
    let mut options = ForEachRefEmailOptions::default();
    let mut rest = arg;
    loop {
        // git's email_atom_option_parser advances past a matched prefix; the
        // `bad_arg` it later reports is the *remaining* string AFTER that
        // consume (so `mailmaptrim` reports `trim`, not `mailmaptrim`).
        let matched = if let Some(tail) = rest.strip_prefix("trim") {
            options.trim = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("localpart") {
            options.localpart = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("mailmap") {
            options.mailmap = true;
            Some(tail)
        } else {
            None
        };
        let Some(tail) = matched else {
            // No prefix consumed: the bad argument is the whole remainder.
            return Err(rest.to_string());
        };
        rest = tail;
        let bad_arg = rest;
        if rest.is_empty() {
            break;
        }
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail;
        } else {
            return Err(bad_arg.to_string());
        }
    }
    Ok(options)
}

/// If `placeholder` is an email atom (`(\*?)(author|committer|tagger)email`
/// with optional `:opts`), render it. Returns `Some(Ok(()))` when handled,
/// `Some(Err(_))` on a bad-option error (already reported to stderr), and
/// `None` when the placeholder is not an email atom.
pub fn for_each_ref_try_email_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authoremail" => (false, ForEachRefAtomIdentityRole::Author),
        "committeremail" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggeremail" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authoremail" => (true, ForEachRefAtomIdentityRole::Author),
        "*committeremail" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggeremail" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let options = match arg {
        Some(arg) => match setup_for_each_ref_email_options(arg) {
            Ok(options) => options,
            Err(bad_arg) => {
                let name = atom.strip_prefix('*').unwrap_or(atom);
                eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
                return Some(Err(GitError::Exit(128)));
            }
        },
        None => ForEachRefEmailOptions::default(),
    };
    Some(for_each_ref_write_email(
        stdout, context, peeled, role, options,
    ))
}

pub fn for_each_ref_write_email(
    stdout: &mut impl Write,
    context: &ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
    options: ForEachRefEmailOptions,
) -> Result<()> {
    let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
        return Ok(());
    };
    let mode = options.mode();
    if options.wants_mailmap() {
        let (_, email) = context.mailmap.rewrite_identity(identity);
        // Reassemble a synthetic identity so the shared email extractor applies
        // trim/localpart over the rewritten address.
        let mut synthetic = Vec::with_capacity(email.len() + 2);
        synthetic.push(b'<');
        synthetic.extend_from_slice(&email);
        synthetic.push(b'>');
        if let Some(value) = for_each_ref_identity_email(&synthetic, mode) {
            stdout.write_all(value)?;
        }
    } else if let Some(value) = for_each_ref_identity_email(identity, mode) {
        stdout.write_all(value)?;
    }
    Ok(())
}

/// The raw message bytes for the ref's own object (`peeled == false`) or the
/// peeled tag target (`peeled == true`), if available.
pub fn for_each_ref_message<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
) -> Option<&'a [u8]> {
    if peeled {
        context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.message.as_deref())
    } else {
        context.contents.as_ref().map(|contents| &*contents.message)
    }
}

/// If `placeholder` is a date atom (`(\*?)(author|committer|tagger|creator)date`
/// with an optional `:spec`), render it through the full date grammar. Returns
/// `Some(Err(_))` (after reporting to stderr) on an invalid specifier.
pub fn for_each_ref_try_date_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authordate" => (false, ForEachRefAtomIdentityRole::Author),
        "committerdate" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggerdate" => (false, ForEachRefAtomIdentityRole::Tagger),
        "creatordate" => (false, ForEachRefAtomIdentityRole::Creator),
        "*authordate" => (true, ForEachRefAtomIdentityRole::Author),
        "*committerdate" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggerdate" => (true, ForEachRefAtomIdentityRole::Tagger),
        "*creatordate" => (true, ForEachRefAtomIdentityRole::Creator),
        _ => return None,
    };
    let Some(mode) = DateMode::parse_atom_modifier(arg) else {
        let name = atom.strip_prefix('*').unwrap_or(atom);
        eprintln!(
            "fatal: unrecognized %({name}) argument: {}",
            arg.unwrap_or("")
        );
        return Some(Err(GitError::Exit(128)));
    };
    Some((|| -> Result<()> {
        if let Some(identity) = for_each_ref_typed_identity(context, peeled, role)
            && let Some(value) = for_each_ref_identity_date(identity, &mode)
        {
            stdout.write_all(value.as_bytes())?;
        }
        Ok(())
    })())
}

/// For an oid atom like `tree:short` / `parent:short=7`, return the option
/// argument (`short` or `short=7`) when `placeholder` is exactly `atom:<arg>`.
pub fn for_each_ref_oid_atom_arg<'a>(placeholder: &'a str, atom: &str) -> Option<&'a str> {
    let rest = placeholder.strip_prefix(atom)?;
    rest.strip_prefix(':')
}

/// Parse the `short`/`short=N` argument of an oid atom into an abbreviation
/// width, mirroring git's `oid_atom_parser` validation. A bare `short` resolves
/// to the repository's `DEFAULT_ABBREV` (git's `O_SHORT` case), supplied by the
/// caller via `default_abbrev`; `short=N` overrides it.
pub fn for_each_ref_oid_atom_width(
    arg: &str,
    atom: &str,
    default_abbrev: Option<usize>,
) -> Result<Option<usize>> {
    if arg == "short" {
        Ok(default_abbrev)
    } else if let Some(value) = arg.strip_prefix("short=") {
        Ok(Some(parse_for_each_ref_abbrev_width(value).map_err(
            |_| {
                eprintln!("fatal: positive value expected '{value}' in %({atom})");
                GitError::Exit(128)
            },
        )?))
    } else {
        eprintln!("fatal: unrecognized %({atom}) argument: {arg}");
        Err(GitError::Exit(128))
    }
}

/// If `placeholder` is a name atom (`(\*?)(author|committer|tagger)name` with an
/// optional `:mailmap`/`:` argument), render it. Mirrors git's
/// `person_name_atom_parser`: the only accepted argument is `mailmap`.
pub fn for_each_ref_try_name_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authorname" => (false, ForEachRefAtomIdentityRole::Author),
        "committername" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggername" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authorname" => (true, ForEachRefAtomIdentityRole::Author),
        "*committername" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggername" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let mailmap = match arg {
        None => false,
        Some("mailmap") => true,
        Some(bad_arg) => {
            let name = atom.strip_prefix('*').unwrap_or(atom);
            eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
            return Some(Err(GitError::Exit(128)));
        }
    };
    Some((|| -> Result<()> {
        let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
            return Ok(());
        };
        if mailmap {
            let (name, _) = context.mailmap.rewrite_identity(identity);
            stdout.write_all(&name)?;
        } else {
            write_for_each_ref_identity_name(stdout, Some(identity))?;
        }
        Ok(())
    })())
}

pub fn for_each_ref_color_escape(value: &str) -> Result<String> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(GitError::Command("empty for-each-ref color".into()));
    }
    if tokens.len() == 1
        && let Some((red, green, blue)) = parse_for_each_ref_hex_color(tokens[0])
    {
        return Ok(format!("\x1b[38;2;{red};{green};{blue}m"));
    }
    let mut attributes = Vec::new();
    let mut foreground = None;
    let mut background = None;
    for token in tokens.iter().copied() {
        match token {
            "reset" => return Ok("\x1b[m".to_string()),
            "normal" if tokens.len() == 1 || (foreground.is_some() && background.is_none()) => {}
            "bold" => attributes.push("1".to_string()),
            "dim" => attributes.push("2".to_string()),
            "italic" => attributes.push("3".to_string()),
            "ul" => attributes.push("4".to_string()),
            "blink" => attributes.push("5".to_string()),
            "reverse" => attributes.push("7".to_string()),
            "strike" => attributes.push("9".to_string()),
            "nobold" | "nodim" => attributes.push("22".to_string()),
            "noitalic" => attributes.push("23".to_string()),
            "noul" => attributes.push("24".to_string()),
            "noblink" => attributes.push("25".to_string()),
            "noreverse" => attributes.push("27".to_string()),
            "nostrike" => attributes.push("29".to_string()),
            "black" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 30)?,
            "red" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 31)?,
            "green" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 32)?,
            "yellow" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 33)?,
            "blue" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 34)?,
            "magenta" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 35)?
            }
            "cyan" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 36)?,
            "white" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 37)?,
            "brightblack" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 90)?
            }
            "brightred" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 91)?
            }
            "brightgreen" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 92)?
            }
            "brightyellow" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 93)?
            }
            "brightblue" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 94)?
            }
            "brightmagenta" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 95)?
            }
            "brightcyan" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 96)?
            }
            "brightwhite" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 97)?
            }
            _ => {
                return Err(GitError::Command(format!(
                    "unsupported for-each-ref color {value}"
                )));
            }
        }
    }
    let mut codes = attributes;
    if let Some(foreground) = foreground {
        codes.push(foreground.to_string());
    }
    if let Some(background) = background {
        codes.push(background.to_string());
    }
    if codes.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("\x1b[{}m", codes.join(";")))
}

pub fn for_each_ref_push_color_code(
    value: &str,
    foreground: &mut Option<u16>,
    background: &mut Option<u16>,
    code: u16,
) -> Result<()> {
    if foreground.is_none() {
        *foreground = Some(code);
    } else if background.is_none() {
        *background = Some(code + 10);
    } else {
        return Err(GitError::Command(format!(
            "unsupported for-each-ref color {value}"
        )));
    }
    Ok(())
}
