//! Shared `for-each-ref` / `refs list` render half plus the CLI-owned adapters
//! (describe engine, trailer formatting). The typed model — sort keys,
//! upstream/push tracking, contents assembly, atom families, colors, worktree
//! and disk-size lookups — lives in `sley-ref-filter` and reaches command
//! modules through the crate-root `sley_ref_filter` re-export.
#![allow(clippy::expect_used)]

use crate::{GitError, Result, sley_rev};
use sley_ref_filter::{
    ForEachRefAtom, ForEachRefFormat, ForEachRefFormatContext, for_each_ref_copy_subject,
    for_each_ref_message_parts, for_each_ref_sanitize_subject, write_for_each_ref_format,
    write_for_each_ref_signature, write_for_each_ref_typed_atom,
};
use std::collections::HashMap;
use std::io::Write;

// Atom families whose option grammar is shared with ref-filter but which bind
// to CLI-only engines here (trailers formatting, describe).
use sley_ref_filter::{
    for_each_ref_ahead_behind_with_diagnostic, for_each_ref_color_escape, for_each_ref_message,
    for_each_ref_oid_atom_arg, for_each_ref_oid_atom_width, for_each_ref_try_date_atom,
    for_each_ref_try_email_atom, for_each_ref_try_name_atom,
    write_for_each_ref_contents_lines,
};

pub(crate) fn print_for_each_ref_format(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    print_for_each_ref_format_with_is_bases(stdout, format_spec, context, &HashMap::new())
}

pub(crate) fn print_for_each_ref_format_with_is_bases(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
    is_base_refs: &HashMap<String, String>,
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
                    sley_ref_filter::for_each_ref_abbrev_oid(
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
                            sley_ref_filter::for_each_ref_abbrev_oid(
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
                        sley_ref_filter::write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "upstream:track,nobracket" | "upstream:nobracket,track" => {
                    if let Some(track) = context.upstream_track {
                        sley_ref_filter::write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "upstream:trackshort" => {
                    if let Some(track) = context.upstream_track {
                        stdout.write_all(sley_ref_filter::for_each_ref_track_short(track).as_bytes())?;
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
                        sley_ref_filter::write_for_each_ref_track(stdout, track, true)?;
                    }
                }
                "push:track,nobracket" | "push:nobracket,track" => {
                    if let Some(track) = context.push_track {
                        sley_ref_filter::write_for_each_ref_track(stdout, track, false)?;
                    }
                }
                "push:trackshort" => {
                    if let Some(track) = context.push_track {
                        stdout.write_all(sley_ref_filter::for_each_ref_track_short(track).as_bytes())?;
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
                "author" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.author.as_deref()),
                )?,
                "*author" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.author.as_deref()),
                )?,
                "authorname" | "*authorname" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "authoremail" | "*authoremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "committer" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.committer.as_deref()),
                )?,
                "*committer" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.committer.as_deref()),
                )?,
                "committername" | "*committername" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "committeremail" | "*committeremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "tagger" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tagger.as_deref()),
                )?,
                "*tagger" => sley_ref_filter::write_for_each_ref_identity(stdout, None)?,
                "taggername" | "*taggername" => {
                    for_each_ref_try_name_atom(stdout, placeholder, context)
                        .expect("name atom recognized")?
                }
                "taggeremail" | "*taggeremail" => {
                    for_each_ref_try_email_atom(stdout, placeholder, context)
                        .expect("email atom recognized")?
                }
                "creator" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.creator.as_deref()),
                )?,
                "*creator" => sley_ref_filter::write_for_each_ref_identity(
                    stdout,
                    context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.creator.as_deref()),
                )?,
                "authordate" | "*authordate" | "committerdate" | "*committerdate"
                | "taggerdate" | "*taggerdate" | "creatordate" | "*creatordate" => {
                    for_each_ref_try_date_atom(stdout, placeholder, context)
                        .expect("date atom recognized")?
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
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_lstrip_name(context.refname, count)
                                .as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("refname:rstrip=") {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_rstrip_name(context.refname, count)
                                .as_bytes(),
                        )?;
                    } else if let Some(value) = other
                        .strip_prefix("upstream:lstrip=")
                        .or_else(|| other.strip_prefix("upstream:strip="))
                    {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_lstrip_name(upstream, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("upstream:rstrip=") {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let upstream = context
                            .upstream
                            .as_ref()
                            .map(|upstream| upstream.refname.as_str())
                            .unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_rstrip_name(upstream, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other
                        .strip_prefix("push:lstrip=")
                        .or_else(|| other.strip_prefix("push:strip="))
                    {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_lstrip_name(push, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("push:rstrip=") {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let push = context
                            .push
                            .as_ref()
                            .and_then(|push| push.refname.as_deref())
                            .unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_rstrip_name(push, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other
                        .strip_prefix("symref:lstrip=")
                        .or_else(|| other.strip_prefix("symref:strip="))
                    {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_lstrip_name(symref, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("symref:rstrip=") {
                        let count = sley_ref_filter::parse_for_each_ref_strip_count(value)?;
                        let symref = context.symref.unwrap_or("");
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_rstrip_name(symref, count).as_bytes(),
                        )?;
                    } else if let Some(width) = other.strip_prefix("objectname:short=") {
                        let width = sley_ref_filter::parse_for_each_ref_abbrev_width(width)?;
                        stdout.write_all(
                            sley_ref_filter::for_each_ref_abbrev_oid(
                                context.oid,
                                Some(width),
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    } else if let Some(width) = other.strip_prefix("*objectname:short=") {
                        let width = sley_ref_filter::parse_for_each_ref_abbrev_width(width)?;
                        if let Some(peeled) = &context.peeled_object {
                            stdout.write_all(
                                sley_ref_filter::for_each_ref_abbrev_oid(
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
                                sley_ref_filter::for_each_ref_abbrev_oid(
                                    tree,
                                    width,
                                    context.objectname_candidates,
                                )
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
                                sley_ref_filter::for_each_ref_abbrev_oid(
                                    tree,
                                    width,
                                    context.objectname_candidates,
                                )
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
                                    sley_ref_filter::for_each_ref_abbrev_oid(
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
                                    sley_ref_filter::for_each_ref_abbrev_oid(
                                        parent,
                                        width,
                                        context.objectname_candidates,
                                    )
                                    .as_bytes(),
                                )?;
                            }
                        }
                    } else if let Some(result) =
                        for_each_ref_try_trailers_atom(stdout, other, context)
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
                        let count =
                            sley_ref_filter::parse_for_each_ref_contents_lines_count(value)?;
                        if let Some(contents) = &context.contents {
                            write_for_each_ref_contents_lines(stdout, &contents.message, count)?;
                        }
                    } else if let Some(value) = other.strip_prefix("*contents:lines=") {
                        let count =
                            sley_ref_filter::parse_for_each_ref_contents_lines_count(value)?;
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
                    } else if let Some((peeled, opts)) = for_each_ref_describe_atom(other) {
                        // %(describe[:opts]) / %(*describe[:opts]) reuse the same
                        // describe engine as log's %(describe); git treats describe
                        // failures as an empty placeholder.
                        let spec = for_each_ref_parse_describe_opts(opts)?;
                        let target = if peeled {
                            context.peeled_object.as_ref().map(|object| object.oid)
                        } else {
                            Some(*context.oid)
                        };
                        if let Some(target) = target
                            && let Some(text) = crate::commands::describe::describe_for_format(
                                context.git_dir,
                                context.format,
                                context.db,
                                &target,
                                spec.tags,
                                spec.abbrev,
                                &spec.matches,
                                &spec.excludes,
                            )?
                        {
                            stdout.write_all(text.as_bytes())?;
                        }
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

/// If `placeholder` is a trailers atom (`%(trailers[:opts])` or
/// `%(contents:trailers[:opts])`, with optional `*` peel), render it. Returns
/// `Some(Err(_))` (after reporting to stderr) for the bad-argument cases.
pub(crate) fn for_each_ref_try_trailers_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (base, peeled) = placeholder
        .strip_prefix('*')
        .map(|rest| (rest, true))
        .unwrap_or((placeholder, false));

    // Accept `trailers`, `trailers:ARG`, `contents:trailers`,
    // `contents:trailers:ARG`. The `contents:` prefix shares git's
    // `%(contents)` bad-argument error for `contents:trailersXXX`.
    let arg: Option<&str> = if base == "trailers" {
        None
    } else if let Some(rest) = base.strip_prefix("trailers:") {
        Some(rest)
    } else {
        let rest = base.strip_prefix("contents:")?;
        if rest == "trailers" {
            None
        } else {
            let rest = rest.strip_prefix("trailers:")?;
            Some(rest)
        }
    };

    let options = match arg {
        None => sley_pretty::ForEachRefTrailerOptions::default(),
        Some(arg) => match sley_pretty::parse_for_each_ref_trailer_options(arg) {
            Ok(options) => options,
            Err(None) => {
                eprintln!("fatal: expected %(trailers:key=<value>)");
                return Some(Err(GitError::Exit(128)));
            }
            Err(Some(invalid)) => {
                eprintln!("fatal: unknown %(trailers) argument: {invalid}");
                return Some(Err(GitError::Exit(128)));
            }
        },
    };

    Some((|| -> Result<()> {
        if let Some(message) = for_each_ref_message(context, peeled) {
            // git formats trailers over the message from the subject start to
            // the signature start (sig stripped).
            let parts = for_each_ref_message_parts(message);
            let sig_len = parts.signature.len();
            let trailer_src = &parts.bare[..parts.bare.len().saturating_sub(sig_len)];
            let rendered = sley_pretty::format_trailers_from_commit(trailer_src, &options);
            stdout.write_all(&rendered)?;
        }
        Ok(())
    })())
}

/// Recognize the `%(describe)` family. Returns `(peeled, opts)` where `peeled`
/// is set for the deref form `%(*describe…)` and `opts` is whatever follows the
/// colon (empty when there is none). Returns `None` for non-describe atoms.
pub(crate) fn for_each_ref_describe_atom(placeholder: &str) -> Option<(bool, &str)> {
    let (peeled, rest) = match placeholder.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, placeholder),
    };
    if rest == "describe" {
        Some((peeled, ""))
    } else {
        rest.strip_prefix("describe:").map(|opts| (peeled, opts))
    }
}

/// Parse `%(describe:opts)` like git's `describe_atom_parser`: walk the
/// comma-separated options, and on the first unrecognized token report
/// `unrecognized %(describe) argument: <bad-token-through-end>` (git keeps the
/// rest of the string, not just the offending token).
pub(crate) fn for_each_ref_parse_describe_opts(opts: &str) -> Result<sley_pretty::DescribeSpec> {
    let mut spec = sley_pretty::DescribeSpec::default();
    let mut rest = opts;
    while !rest.is_empty() {
        let (part, next) = match rest.split_once(',') {
            Some((part, next)) => (part, next),
            None => (rest, ""),
        };
        if part == "tags" {
            spec.tags = true;
        } else if let Some(value) = part.strip_prefix("abbrev=") {
            match value.parse::<usize>() {
                Ok(width) => spec.abbrev = Some(width),
                Err(_) => return Err(for_each_ref_bad_describe_arg(rest)),
            }
        } else if let Some(value) = part.strip_prefix("match=") {
            spec.matches.push(value.to_string());
        } else if let Some(value) = part.strip_prefix("exclude=") {
            spec.excludes.push(value.to_string());
        } else {
            return Err(for_each_ref_bad_describe_arg(rest));
        }
        rest = next;
    }
    Ok(spec)
}

pub(crate) fn for_each_ref_bad_describe_arg(bad: &str) -> GitError {
    eprintln!("fatal: unrecognized %(describe) argument: {bad}");
    GitError::Exit(128)
}
