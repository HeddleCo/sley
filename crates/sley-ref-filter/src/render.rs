//! The `for-each-ref` output half: expand a parsed [`ForEachRefFormat`] for one
//! ref, mirroring ref-filter.c's atom dispatch order exactly. The two atoms that
//! bind to engines living above this crate (trailers formatting via
//! sley-pretty, and `%(describe)` via the describe engine) reach the renderer
//! through the injected [`ForEachRefRenderHooks`]; option *parsing* for those
//! atoms stays on the CLI side of the boundary.

use super::{
    ForEachRefAtom, ForEachRefFormat, ForEachRefFormatContext, for_each_ref_abbrev_oid,
    for_each_ref_ahead_behind_with_diagnostic, for_each_ref_color_escape,
    for_each_ref_copy_subject, for_each_ref_lstrip_name, for_each_ref_message,
    for_each_ref_message_parts, for_each_ref_oid_atom_arg, for_each_ref_oid_atom_width,
    for_each_ref_rstrip_name, for_each_ref_sanitize_subject, for_each_ref_track_short,
    for_each_ref_try_date_atom, for_each_ref_try_email_atom, for_each_ref_try_name_atom,
    parse_for_each_ref_abbrev_width, parse_for_each_ref_contents_lines_count,
    parse_for_each_ref_strip_count, write_for_each_ref_contents_lines, write_for_each_ref_format,
    write_for_each_ref_identity, write_for_each_ref_signature, write_for_each_ref_track,
    write_for_each_ref_typed_atom,
};
use sley_core::{GitError, Result};
use std::collections::HashMap;
use std::io::Write;

/// Render through an atom-recognizer hook. The dispatch arms above only reach
/// the recognizers whose placeholder grammar they match, so `None` here is a
/// dispatch invariant violation; report it as an error instead of panicking.
fn write_recognized_atom(placeholder: &str, recognized: Option<Result<()>>) -> Result<()> {
    match recognized {
        Some(rendered) => rendered,
        None => Err(GitError::InvalidFormat(format!(
            "unrecognized for-each-ref atom %({placeholder})"
        ))),
    }
}

/// Trailers formatting for `%(trailers...)` / `%(contents:trailers...)`.
/// Returns `Some(Err(_))` after reporting a bad-argument diagnostic, `None`
/// when the placeholder is not a trailers atom (dispatch falls through).
pub type ForEachRefTrailersFormatter =
    dyn Fn(&mut Vec<u8>, &str, &ForEachRefFormatContext<'_>) -> Option<Result<()>> + Send + Sync;

/// `%(describe[:opts])` rendering. Returns `Ok(false)` when the placeholder is
/// not a describe atom (dispatch falls through); `Ok(true)` after rendering
/// (possibly an empty expansion, matching git's failure-is-empty semantics).
pub type ForEachRefDescribeRenderer =
    dyn Fn(&mut Vec<u8>, &str, &ForEachRefFormatContext<'_>) -> Result<bool> + Send + Sync;

/// The engine hooks injected into the per-atom dispatch. Both bounds are
/// `Send + Sync` so the CLI can hold one shared static instance.
#[derive(Clone, Copy)]
pub struct ForEachRefRenderHooks<'a> {
    pub trailers_formatter: &'a ForEachRefTrailersFormatter,
    pub describe_renderer: &'a ForEachRefDescribeRenderer,
}

pub fn print_for_each_ref_format(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
    hooks: ForEachRefRenderHooks<'_>,
) -> Result<()> {
    print_for_each_ref_format_with_is_bases(stdout, format_spec, context, &HashMap::new(), hooks)
}

pub fn print_for_each_ref_format_with_is_bases(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
    is_base_refs: &HashMap<String, String>,
    hooks: ForEachRefRenderHooks<'_>,
) -> Result<()> {
    let reset_color_at_eol = context.color && format_spec.ends_with_unreset_color();
    write_for_each_ref_format(
        stdout,
        format_spec,
        context.quote,
        reset_color_at_eol,
        |stdout, atom| {
            let placeholder = match atom {
                ForEachRefAtom::Raw(placeholder) => placeholder.as_str(),
                atom => {
                    write_for_each_ref_typed_atom(stdout, atom, context)?;
                    return Ok(());
                }
            };
            match placeholder {
                "HEAD" => stdout.write_all(if context.is_head { b"*" } else { b" " })?,
                "refname" => stdout.write_all(context.refname.as_bytes())?,
                "refname:short" => {
                    stdout.write_all(context.shorten_ref(context.refname).as_bytes())?
                }
                "objectname" => write!(stdout, "{}", context.oid)?,
                "objectname:short" => stdout.write_all(
                    for_each_ref_abbrev_oid(
                        context.oid,
                        context.objectname_abbrev,
                        context.objectname_candidates,
                    )
                    .as_bytes(),
                )?,
                "*objectname" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.oid)?;
                    }
                }
                "*objectname:short" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(
                            for_each_ref_abbrev_oid(
                                &peeled.oid,
                                context.objectname_abbrev,
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    }
                }
                "deltabase" => write!(stdout, "{}", context.deltabase)?,
                "*deltabase" => {
                    if context.peeled_object.is_some() {
                        write!(stdout, "{}", context.deltabase)?;
                    }
                }
                "raw" => stdout.write_all(context.object_body)?,
                "raw:size" => write!(stdout, "{}", context.object_body.len())?,
                "*raw" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(&peeled.object_body)?;
                    }
                }
                "*raw:size" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.object_body.len())?;
                    }
                }
                "objectsize" => write!(stdout, "{}", context.object_size)?,
                "*objectsize" => {
                    if let Some(peeled) = &context.peeled_object {
                        write!(stdout, "{}", peeled.object_size)?;
                    }
                }
                "objectsize:disk" => {
                    if let Some(size) = context.object_disk_size {
                        write!(stdout, "{size}")?;
                    }
                }
                "*objectsize:disk" => {
                    if let Some(size) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.object_disk_size)
                    {
                        write!(stdout, "{size}")?;
                    }
                }
                "objecttype" => stdout.write_all(context.object_type.as_str().as_bytes())?,
                "*objecttype" => {
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(peeled.object_type.as_str().as_bytes())?;
                    }
                }
                "worktreepath" => {
                    stdout.write_all(context.worktree_path.unwrap_or("").as_bytes())?
                }
                "symref" => stdout.write_all(context.symref.unwrap_or("").as_bytes())?,
                "symref:short" => stdout.write_all(
                    context
                        .symref
                        .map(|symref| context.shorten_ref(symref))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "upstream" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.refname.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:short" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| context.shorten_ref(&upstream.refname))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "upstream:remotename" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.remote.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:remoteref" => stdout.write_all(
                    context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.merge.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "upstream:track" => {
                    if let Some(track) = context.upstream_track {
                        write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "upstream:track,nobracket" | "upstream:nobracket,track" => {
                    if let Some(track) = context.upstream_track {
                        write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "upstream:trackshort" => {
                    if let Some(track) = context.upstream_track {
                        stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                    }
                }
                "push" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:short" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .map(|refname| context.shorten_ref(refname))
                        .unwrap_or_default()
                        .as_bytes(),
                )?,
                "push:remotename" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .map(|push| push.remote.as_str())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:remoteref" => stdout.write_all(
                    context
                        .push
                        .as_ref()
                        .and_then(|push| push.remote_ref.as_deref())
                        .unwrap_or("")
                        .as_bytes(),
                )?,
                "push:track" => {
                    if let Some(track) = context.push_track {
                        write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "push:track,nobracket" | "push:nobracket,track" => {
                    if let Some(track) = context.push_track {
                        write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "push:trackshort" => {
                    if let Some(track) = context.push_track {
                        stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                    }
                }
                "signature"
                | "signature:grade"
                | "signature:key"
                | "signature:signer"
                | "signature:fingerprint"
                | "signature:primarykeyfingerprint"
                | "signature:trustlevel" => {
                    if let Some(signature) = context.signature {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["signature".len()..],
                        )?;
                    }
                }
                "*signature"
                | "*signature:grade"
                | "*signature:key"
                | "*signature:signer"
                | "*signature:fingerprint"
                | "*signature:primarykeyfingerprint"
                | "*signature:trustlevel" => {
                    if let Some(signature) = context.peeled_signature {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["*signature".len()..],
                        )?;
                    }
                }
                "subject" | "contents:subject" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        let parts = for_each_ref_message_parts(message);
                        stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                    }
                }
                "*subject" | "*contents:subject" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        let parts = for_each_ref_message_parts(message);
                        stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                    }
                }
                "subject:sanitize" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        let parts = for_each_ref_message_parts(message);
                        let subject = for_each_ref_copy_subject(parts.subject);
                        stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                    }
                }
                "*subject:sanitize" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        let parts = for_each_ref_message_parts(message);
                        let subject = for_each_ref_copy_subject(parts.subject);
                        stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                    }
                }
                "contents:body" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                    }
                }
                "*contents:body" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                    }
                }
                "contents:signature" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).signature)?;
                    }
                }
                "*contents:signature" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).signature)?;
                    }
                }
                "body" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                    }
                }
                "*body" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                    }
                }
                "contents" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        stdout.write_all(for_each_ref_message_parts(message).bare)?;
                    }
                }
                "*contents" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        stdout.write_all(for_each_ref_message_parts(message).bare)?;
                    }
                }
                "contents:size" => {
                    if let Some(message) = for_each_ref_message(context, false) {
                        write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                    }
                }
                "*contents:size" => {
                    if let Some(message) = for_each_ref_message(context, true) {
                        write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                    }
                }
                "author" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.author.as_deref()),
                )?,
                "*author" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.author.as_deref()),
                )?,
                "authorname" | "*authorname" => {
                    let recognized = for_each_ref_try_name_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "authoremail" | "*authoremail" => {
                    let recognized = for_each_ref_try_email_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "committer" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.committer.as_deref()),
                )?,
                "*committer" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.committer.as_deref()),
                )?,
                "committername" | "*committername" => {
                    let recognized = for_each_ref_try_name_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "committeremail" | "*committeremail" => {
                    let recognized = for_each_ref_try_email_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "tagger" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tagger.as_deref()),
                )?,
                "*tagger" => write_for_each_ref_identity(stdout, None)?,
                "taggername" | "*taggername" => {
                    let recognized = for_each_ref_try_name_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "taggeremail" | "*taggeremail" => {
                    let recognized = for_each_ref_try_email_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "creator" => write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.creator.as_deref()),
                )?,
                "*creator" => write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.creator.as_deref()),
                )?,
                "authordate" | "*authordate" | "committerdate" | "*committerdate"
                | "taggerdate" | "*taggerdate" | "creatordate" | "*creatordate" => {
                    let recognized = for_each_ref_try_date_atom(stdout, placeholder, context);
                    write_recognized_atom(placeholder, recognized)?
                }
                "tree" => {
                    if let Some(tree) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tree.as_ref())
                    {
                        write!(stdout, "{tree}")?;
                    }
                }
                "parent" => {
                    if let Some(contents) = &context.contents {
                        for (idx, parent) in contents.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            write!(stdout, "{parent}")?;
                        }
                    }
                }
                "numparent" => {
                    if let Some(contents) = &context.contents
                        && contents.tree.is_some()
                    {
                        write!(stdout, "{}", contents.parents.len())?;
                    }
                }
                "*tree" => {
                    if let Some(tree) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.tree.as_ref())
                    {
                        write!(stdout, "{tree}")?;
                    }
                }
                "*parent" => {
                    if let Some(peeled) = &context.peeled_object {
                        for (idx, parent) in peeled.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            write!(stdout, "{parent}")?;
                        }
                    }
                }
                "*numparent" => {
                    if let Some(peeled) = &context.peeled_object
                        && peeled.tree.is_some()
                    {
                        write!(stdout, "{}", peeled.parents.len())?;
                    }
                }
                "tag" => {
                    if let Some(tag) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag.as_ref())
                    {
                        stdout.write_all(tag)?;
                    }
                }
                "type" => {
                    if let Some(object_type) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag_object_type)
                    {
                        stdout.write_all(object_type.as_str().as_bytes())?;
                    }
                }
                "object" => {
                    if let Some(object) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tag_object.as_ref())
                    {
                        write!(stdout, "{object}")?;
                    }
                }
                other => {
                    if let Some(value) = other.strip_prefix("color:") {
                        let color = for_each_ref_color_escape(value)?;
                        if context.color {
                            stdout.write_all(color.as_bytes())?;
                        }
                    } else if let Some(value) = other
                        .strip_prefix("refname:lstrip=")
                        .or_else(|| other.strip_prefix("refname:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            for_each_ref_lstrip_name(context.refname, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("refname:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            for_each_ref_rstrip_name(context.refname, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other
                        .strip_prefix("upstream:lstrip=")
                        .or_else(|| other.strip_prefix("upstream:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(upstream, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("upstream:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(upstream, count).as_bytes())?;
                    } else if let Some(value) = other
                        .strip_prefix("push:lstrip=")
                        .or_else(|| other.strip_prefix("push:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(push, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("push:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(push, count).as_bytes())?;
                    } else if let Some(value) = other
                        .strip_prefix("symref:lstrip=")
                        .or_else(|| other.strip_prefix("symref:strip="))
                    {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(for_each_ref_lstrip_name(symref, count).as_bytes())?;
                    } else if let Some(value) = other.strip_prefix("symref:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(for_each_ref_rstrip_name(symref, count).as_bytes())?;
                    } else if let Some(width) = other.strip_prefix("objectname:short=") {
                        let width = parse_for_each_ref_abbrev_width(width)?;
                        stdout.write_all(
                            for_each_ref_abbrev_oid(
                                context.oid,
                                Some(width),
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    } else if let Some(width) = other.strip_prefix("*objectname:short=") {
                        let width = parse_for_each_ref_abbrev_width(width)?;
                        if let Some(peeled) = &context.peeled_object {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(
                                    &peeled.oid,
                                    Some(width),
                                    context.objectname_candidates,
                                )
                                .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "tree") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(tree) = context
                            .contents
                            .as_ref()
                            .and_then(|contents| contents.tree.as_ref())
                        {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                    .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*tree") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(tree) = context
                            .peeled_object
                            .as_ref()
                            .and_then(|peeled| peeled.tree.as_ref())
                        {
                            stdout.write_all(
                                for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                    .as_bytes(),
                            )?;
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "parent") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(contents) = &context.contents {
                            for (idx, parent) in contents.parents.iter().enumerate() {
                                if idx > 0 {
                                    stdout.write_all(b" ")?;
                                }
                                stdout.write_all(
                                    for_each_ref_abbrev_oid(
                                        parent,
                                        width,
                                        context.objectname_candidates,
                                    )
                                    .as_bytes(),
                                )?;
                            }
                        }
                    } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*parent") {
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
                        if let Some(peeled) = &context.peeled_object {
                            for (idx, parent) in peeled.parents.iter().enumerate() {
                                if idx > 0 {
                                    stdout.write_all(b" ")?;
                                }
                                stdout.write_all(
                                    for_each_ref_abbrev_oid(
                                        parent,
                                        width,
                                        context.objectname_candidates,
                                    )
                                    .as_bytes(),
                                )?;
                            }
                        }
                    } else if let Some(result) = (hooks.trailers_formatter)(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_email_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_name_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_date_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(target) = other.strip_prefix("is-base:") {
                        if is_base_refs
                            .get(target)
                            .is_some_and(|refname| refname == context.refname)
                        {
                            write!(stdout, "({target})")?;
                        }
                    } else if let Some(rev) = other.strip_prefix("ahead-behind:") {
                        let target = sley_rev::RevisionResolver::new(
                            context.git_dir,
                            context.format,
                            context.db,
                        )
                        .resolve(rev)?;
                        if let Some(track) = for_each_ref_ahead_behind_with_diagnostic(
                            context.git_dir,
                            context.db,
                            context.format,
                            context.oid,
                            &target,
                        )? {
                            write!(stdout, "{} {}", track.ahead, track.behind)?;
                        }
                    } else if let Some(value) = other.strip_prefix("contents:lines=") {
                        let count = parse_for_each_ref_contents_lines_count(value)?;
                        if let Some(contents) = &context.contents {
                            write_for_each_ref_contents_lines(stdout, &contents.message, count)?;
                        }
                    } else if let Some(value) = other.strip_prefix("*contents:lines=") {
                        let count = parse_for_each_ref_contents_lines_count(value)?;
                        if let Some(message) = context
                            .peeled_object
                            .as_ref()
                            .and_then(|peeled| peeled.message.as_ref())
                        {
                            write_for_each_ref_contents_lines(stdout, message, count)?;
                        }
                    } else if let Some(arg) = other
                        .strip_prefix("contents:")
                        .or_else(|| other.strip_prefix("*contents:"))
                    {
                        // A `%(contents:XXX)` that none of the contents sub-atoms
                        // above recognized — git reports the bare contents arg.
                        eprintln!("fatal: unrecognized %(contents) argument: {arg}");
                        return Err(GitError::Exit(128));
                    } else if (hooks.describe_renderer)(stdout, other, context)? {
                        // %(describe[:opts]) / %(*describe[:opts]) are rendered by
                        // the CLI-injected describe engine (the engine itself lives
                        // above this crate to keep sley-ref-filter acyclic); git
                        // treats describe failures as an empty placeholder.
                    } else if other.starts_with("HEAD:") {
                        // git's head_atom_parser: %(HEAD) takes no arguments.
                        eprintln!("fatal: %(HEAD) does not take arguments");
                        return Err(GitError::Exit(128));
                    } else if let Some(arg) = other
                        .strip_prefix("subject:")
                        .or_else(|| other.strip_prefix("*subject:"))
                    {
                        // The only valid %(subject) arg is `sanitize` (matched
                        // above); anything else is rejected like git's
                        // subject_atom_parser.
                        eprintln!("fatal: unrecognized %(subject) argument: {arg}");
                        return Err(GitError::Exit(128));
                    } else {
                        return Err(GitError::Command(format!(
                            "unsupported for-each-ref format placeholder %({other})"
                        )));
                    }
                }
            }
            Ok(())
        },
    )
}
