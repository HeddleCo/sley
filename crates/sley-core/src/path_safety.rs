//! Git's byte-level NTFS/HFS alias predicates, shared by checkout and fsck.
//!
//! These are the path.c and utf8.c rules, independent of repository config
//! and filesystem I/O, so remote object validation needs no worktree dependency.

fn at(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).copied().unwrap_or(0)
}

fn tail(bytes: &[u8], index: usize) -> &[u8] {
    bytes.get(index..).unwrap_or(&[])
}

fn is_xplatform_dir_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

/// git `path.c` `is_ntfs_dotgit`: `name` is the remainder of a path starting
/// at a component. True for `.git` or `git~1` (any case), followed by only
/// dots and spaces up to the end, a `/` or `\`, or a `:` stream suffix.
pub fn is_ntfs_dotgit(name: &[u8]) -> bool {
    let mut i;
    let c = at(name, 0);
    if c == b'.' {
        if !at(name, 1).eq_ignore_ascii_case(&b'g')
            || !at(name, 2).eq_ignore_ascii_case(&b'i')
            || !at(name, 3).eq_ignore_ascii_case(&b't')
        {
            return false;
        }
        i = 4;
    } else if c == b'g' || c == b'G' {
        if !at(name, 1).eq_ignore_ascii_case(&b'i')
            || !at(name, 2).eq_ignore_ascii_case(&b't')
            || at(name, 3) != b'~'
            || at(name, 4) != b'1'
        {
            return false;
        }
        i = 5;
    } else {
        return false;
    }
    loop {
        let c = at(name, i);
        i += 1;
        if c == 0 || is_xplatform_dir_sep(c) || c == b':' {
            return true;
        }
        if c != b'.' && c != b' ' {
            return false;
        }
    }
}

/// git `path.c` `is_ntfs_dotgitmodules`.
pub fn is_ntfs_dotgitmodules(name: &[u8]) -> bool {
    is_ntfs_dot_generic(name, b"gitmodules", b"gi7eba")
}

/// git `path.c` `is_ntfs_dot_generic`: `.<dotgit_name>`, its regular 8.3
/// short name (first six characters, `~1`..`~4`), or the hashed fall-back
/// short name `<shortname_prefix>~N`, then only dots and spaces up to the
/// end or a `:` stream suffix.
pub fn is_ntfs_dot_generic(name: &[u8], dotgit_name: &[u8], shortname_prefix: &[u8]) -> bool {
    let len = dotgit_name.len();
    if at(name, 0) == b'.' && strncasecmp_eq(tail(name, 1), dotgit_name, len) {
        return only_spaces_and_periods(name, len + 1);
    }
    if strncasecmp_eq(name, dotgit_name, 6)
        && at(name, 6) == b'~'
        && (b'1'..=b'4').contains(&at(name, 7))
    {
        return only_spaces_and_periods(name, 8);
    }
    let mut saw_tilde = false;
    let mut i = 0usize;
    while i < 8 {
        let c = at(name, i);
        if c == 0 {
            return false;
        } else if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            i += 1;
            if !(b'1'..=b'9').contains(&at(name, i)) {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || c & 0x80 != 0 || c.to_ascii_lowercase() != at(shortname_prefix, i) {
            return false;
        }
        i += 1;
    }
    only_spaces_and_periods(name, i)
}

fn only_spaces_and_periods(name: &[u8], mut i: usize) -> bool {
    loop {
        let c = at(name, i);
        i += 1;
        if c == 0 || c == b':' {
            return true;
        }
        if c != b' ' && c != b'.' {
            return false;
        }
    }
}

/// C `strncasecmp(a, b, n) == 0` over NUL-terminated views.
fn strncasecmp_eq(a: &[u8], b: &[u8], n: usize) -> bool {
    for index in 0..n {
        let (x, y) = (at(a, index), at(b, index));
        if !x.eq_ignore_ascii_case(&y) {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

/// git `utf8.c` `is_hfs_dotgit`: after dropping the code points HFS+
/// ignores, `.git` (ASCII case-insensitive) followed by the end or `/`.
pub fn is_hfs_dotgit(path: &[u8]) -> bool {
    is_hfs_dot_generic(path, b"git")
}

/// git `utf8.c` `is_hfs_dotgitmodules`.
pub fn is_hfs_dotgitmodules(path: &[u8]) -> bool {
    is_hfs_dot_generic(path, b"gitmodules")
}

/// Git HFS alias predicate for an ASCII dot-name without its leading dot.
pub fn is_hfs_dot_generic(path: &[u8], needle: &[u8]) -> bool {
    let mut cursor = Some(0usize);
    if next_hfs_char(path, &mut cursor) != u32::from(b'.') {
        return false;
    }
    for expected in needle {
        let c = next_hfs_char(path, &mut cursor);
        if c > 127 {
            return false;
        }
        // `c <= 127` was just checked, so the narrowing is lossless.
        if (c as u8).to_ascii_lowercase() != *expected {
            return false;
        }
    }
    let c = next_hfs_char(path, &mut cursor);
    c == 0 || c == u32::from(b'/')
}

/// git `utf8.c` `next_hfs_char`. `cursor` is `None` once malformed UTF-8 has
/// been seen, which reads as the end of the string (as in git).
fn next_hfs_char(path: &[u8], cursor: &mut Option<usize>) -> u32 {
    loop {
        let Some(position) = *cursor else {
            return 0;
        };
        let Some((ch, width)) = pick_one_utf8_char(tail(path, position)) else {
            *cursor = None;
            return 0;
        };
        *cursor = Some(position + width);
        if is_hfs_ignorable(ch) {
            continue;
        }
        return ch;
    }
}

/// The code points HFS+ drops when comparing names (git `next_hfs_char`).
fn is_hfs_ignorable(ch: u32) -> bool {
    matches!(
        ch,
        0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff
    )
}

/// git `utf8.c` `pick_one_utf8_char` on a NUL-terminated string: the code
/// point and its width, or `None` for malformed UTF-8.
fn pick_one_utf8_char(s: &[u8]) -> Option<(u32, usize)> {
    let b = |index: usize| u32::from(at(s, index));
    let s0 = b(0);
    if s0 < 0x80 {
        return Some((s0, 1));
    }
    let cont = |index: usize| b(index) & 0xc0 == 0x80;
    if s0 & 0xe0 == 0xc0 {
        if !cont(1) || s0 & 0xfe == 0xc0 {
            return None;
        }
        return Some((((s0 & 0x1f) << 6) | (b(1) & 0x3f), 2));
    }
    if s0 & 0xf0 == 0xe0 {
        if !cont(1)
            || !cont(2)
            || (s0 == 0xe0 && b(1) & 0xe0 == 0x80)
            || (s0 == 0xed && b(1) & 0xe0 == 0xa0)
            || (s0 == 0xef && b(1) == 0xbf && b(2) & 0xfe == 0xbe)
        {
            return None;
        }
        return Some((
            ((s0 & 0x0f) << 12) | ((b(1) & 0x3f) << 6) | (b(2) & 0x3f),
            3,
        ));
    }
    if s0 & 0xf8 == 0xf0 {
        if !cont(1)
            || !cont(2)
            || !cont(3)
            || (s0 == 0xf0 && b(1) & 0xf0 == 0x80)
            || (s0 == 0xf4 && b(1) > 0x8f)
            || s0 > 0xf4
        {
            return None;
        }
        return Some((
            ((s0 & 0x07) << 18) | ((b(1) & 0x3f) << 12) | ((b(2) & 0x3f) << 6) | (b(3) & 0x3f),
            4,
        ));
    }
    None
}

/// Whether the path component `component` aliases the reserved `name` under
/// any of the rules git applies to `.git`: ASCII case; trailing dots and
/// spaces, a `:` stream or a `\` separator (NTFS); the 8.3 short name of a
/// dot-name (`HEDDLE~1` for `.heddle`); or HFS+ ignorable code points.
pub fn is_reserved_alias(component: &[u8], name: &[u8]) -> bool {
    let ntfs_tail = |rest: &[u8]| {
        let mut index = 0;
        loop {
            match at(rest, index) {
                0 | b'/' | b'\\' | b':' => return true,
                b'.' | b' ' => index += 1,
                _ => return false,
            }
        }
    };
    if strncasecmp_eq(component, name, name.len()) && ntfs_tail(tail(component, name.len())) {
        return true;
    }
    if let Some(stem) = name.strip_prefix(b".")
        && !stem.is_empty()
    {
        let short = &stem[..stem.len().min(6)];
        if strncasecmp_eq(component, short, short.len())
            && at(component, short.len()) == b'~'
            && (b'1'..=b'4').contains(&at(component, short.len() + 1))
            && ntfs_tail(tail(component, short.len() + 2))
        {
            return true;
        }
    }
    // HFS+: compare with ignorable code points dropped.
    let mut cursor = Some(0usize);
    for expected in name {
        let c = next_hfs_char(component, &mut cursor);
        if c > 127 || !(c as u8).eq_ignore_ascii_case(expected) {
            return false;
        }
    }
    let c = next_hfs_char(component, &mut cursor);
    c == 0 || c == u32::from(b'/')
}
