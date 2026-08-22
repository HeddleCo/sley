//! glibc strverscmp-derived version ordering, as in git's versioncmp.c.

// states: S_N normal, S_I integral part, S_F fractional parts, S_Z idem but
// leading zeroes only (from glibc strverscmp, as in git's versioncmp.c).
const VS_S_N: usize = 0x0;
const VS_S_I: usize = 0x3;
const VS_S_F: usize = 0x6;
const VS_S_Z: usize = 0x9;
// result_type sentinels: CMP return diff, LEN compare via len_diff/diff.
const VS_CMP: i8 = 2;
const VS_LEN: i8 = 3;

#[rustfmt::skip]
const VS_NEXT_STATE: [usize; 12] = [
    /* state    x    d    0  */
    /* S_N */  VS_S_N, VS_S_I, VS_S_Z,
    /* S_I */  VS_S_N, VS_S_I, VS_S_I,
    /* S_F */  VS_S_N, VS_S_F, VS_S_F,
    /* S_Z */  VS_S_N, VS_S_F, VS_S_Z,
];

#[rustfmt::skip]
const VS_RESULT_TYPE: [i8; 36] = [
    /* state   x/x  x/d  x/0  d/x  d/d  d/0  0/x  0/d  0/0  */
    /* S_N */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_LEN, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_I */  VS_CMP, -1,     -1,     1,      VS_LEN, VS_LEN, 1,      VS_LEN, VS_LEN,
    /* S_F */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_Z */  VS_CMP, 1,      1,      -1,     VS_CMP, VS_CMP, -1,     VS_CMP, VS_CMP,
];

#[inline]
pub fn vs_digit_class(c: u8) -> usize {
    // 0 if not a digit, 1 if digit 1-9, 2 if '0' (matches git's
    // (c=='0') + (isdigit(c) != 0)).
    (c == b'0') as usize + c.is_ascii_digit() as usize
}

pub struct VsSuffixMatch {
    conf_pos: i64,
    start: usize,
    len: i64,
}

pub fn vs_find_better_matching_suffix(
    tagname: &[u8],
    suffix: &[u8],
    start: usize,
    conf_pos: usize,
    m: &mut VsSuffixMatch,
) {
    // A better match either starts earlier, or at the same offset but longer.
    let end = if m.len < suffix.len() as i64 {
        m.start
    } else {
        m.start.saturating_sub(1)
    };
    for i in start..=end {
        if tagname.len() >= i && tagname[i..].starts_with(suffix) {
            m.conf_pos = conf_pos as i64;
            m.start = i;
            m.len = suffix.len() as i64;
            break;
        }
    }
}

/// Port of git's swap_prereleases(). `off` is the offset of the first
/// differing character. Returns Some(diff) if a prerelease suffix forces an
/// order.
pub fn vs_swap_prereleases(
    s1: &[u8],
    s2: &[u8],
    off: usize,
    prereleases: &[String],
) -> Option<std::cmp::Ordering> {
    let mut m1 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    let mut m2 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    for (i, suffix) in prereleases.iter().enumerate() {
        let suffix = suffix.as_bytes();
        let suffix_len = suffix.len();
        let start = off.saturating_sub(suffix_len);
        vs_find_better_matching_suffix(s1, suffix, start, i, &mut m1);
        vs_find_better_matching_suffix(s2, suffix, start, i, &mut m2);
    }
    if m1.conf_pos == -1 && m2.conf_pos == -1 {
        return None;
    }
    if m1.conf_pos == m2.conf_pos {
        // Same suffix in both: caller decides by the rest.
        return None;
    }
    let ord = if m1.conf_pos >= 0 && m2.conf_pos >= 0 {
        m1.conf_pos.cmp(&m2.conf_pos)
    } else if m1.conf_pos >= 0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    };
    Some(ord)
}

/// Faithful port of git's versioncmp() (glibc strverscmp + prerelease swap).
pub fn version_sort_cmp(s1: &str, s2: &str, prereleases: &[String]) -> std::cmp::Ordering {
    let b1 = s1.as_bytes();
    let b2 = s2.as_bytes();
    // Iterate with a sentinel NUL so we faithfully follow git's pointer walk.
    let get1 = |i: usize| -> u8 { if i < b1.len() { b1[i] } else { 0 } };
    let get2 = |i: usize| -> u8 { if i < b2.len() { b2[i] } else { 0 } };

    if std::ptr::eq(b1.as_ptr(), b2.as_ptr()) && b1.len() == b2.len() {
        return std::cmp::Ordering::Equal;
    }

    let mut p1 = 0usize;
    let mut p2 = 0usize;
    let mut c1 = get1(p1);
    let mut c2 = get2(p2);
    p1 += 1;
    p2 += 1;
    let mut state = VS_S_N + vs_digit_class(c1);

    let diff = loop {
        let d = c1 as i32 - c2 as i32;
        if d != 0 {
            break d;
        }
        if c1 == 0 {
            return std::cmp::Ordering::Equal;
        }
        state = VS_NEXT_STATE[state];
        c1 = get1(p1);
        c2 = get2(p2);
        p1 += 1;
        p2 += 1;
        state += vs_digit_class(c1);
    };

    // off is the index of the first differing character: pointer is one past it.
    if !prereleases.is_empty()
        && let Some(ord) = vs_swap_prereleases(b1, b2, p1 - 1, prereleases)
    {
        return ord;
    }

    let result = VS_RESULT_TYPE[state * 3 + vs_digit_class(c2)];
    match result {
        VS_CMP => diff.cmp(&0),
        VS_LEN => {
            // while (isdigit(*p1++)) if (!isdigit(*p2++)) return 1;
            loop {
                let d1 = get1(p1).is_ascii_digit();
                p1 += 1;
                if !d1 {
                    break;
                }
                let d2 = get2(p2).is_ascii_digit();
                p2 += 1;
                if !d2 {
                    return std::cmp::Ordering::Greater;
                }
            }
            if get2(p2).is_ascii_digit() {
                std::cmp::Ordering::Less
            } else {
                diff.cmp(&0)
            }
        }
        other => (other as i32).cmp(&0),
    }
}
