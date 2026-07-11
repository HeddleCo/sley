//! Embeddable column layout used by `git column` and porcelain list renderers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLayout {
    Plain,
    ColumnFirst,
    RowFirst,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnOptions {
    pub layout: ColumnLayout,
    pub dense: bool,
    pub width: usize,
    pub padding: usize,
    pub indent: Vec<u8>,
    pub line_terminator: Vec<u8>,
}

impl Default for ColumnOptions {
    fn default() -> Self {
        Self {
            layout: ColumnLayout::ColumnFirst,
            dense: false,
            width: 80,
            padding: 1,
            indent: Vec::new(),
            line_terminator: vec![b'\n'],
        }
    }
}

pub fn format_columns(items: &[Vec<u8>], options: &ColumnOptions) -> Vec<u8> {
    if items.is_empty() {
        return Vec::new();
    }
    if options.layout == ColumnLayout::Plain {
        let mut out = Vec::new();
        for item in items {
            out.extend_from_slice(&options.indent);
            out.extend_from_slice(item);
            out.extend_from_slice(&options.line_terminator);
        }
        return out;
    }

    let initial_width = items
        .iter()
        .map(|item| display_width(item))
        .max()
        .unwrap_or(0)
        .saturating_add(options.padding)
        .max(1);
    let available = options.width.saturating_sub(options.indent.len());
    let mut columns = (available / initial_width).max(1).min(items.len());
    let mut rows = items.len().div_ceil(columns);
    let widths = if options.dense {
        shrink_dense_layout(items, options, &mut columns, &mut rows)
    } else {
        vec![initial_width.saturating_sub(options.padding); columns]
    };
    let mut out = Vec::new();
    for row in 0..rows {
        out.extend_from_slice(&options.indent);
        for (column, column_width) in widths.iter().copied().enumerate().take(columns) {
            let Some(index) = item_index(options.layout, items.len(), columns, rows, row, column)
            else {
                continue;
            };
            out.extend_from_slice(&items[index]);
            let has_later = ((column + 1)..columns).any(|later| {
                item_index(options.layout, items.len(), columns, rows, row, later).is_some()
            });
            if has_later {
                let spaces = column_width
                    .saturating_sub(display_width(&items[index]))
                    .saturating_add(options.padding);
                out.extend(std::iter::repeat_n(b' ', spaces));
            }
        }
        out.extend_from_slice(&options.line_terminator);
    }
    out
}

/// Port of Git's `shrink_columns()`: start from the uniform-width layout, then
/// remove one row at a time until the resulting per-column widths no longer
/// fit. Searching arbitrary column counts can choose a layout Git never visits
/// and changes both row grouping and output order.
fn shrink_dense_layout(
    items: &[Vec<u8>],
    options: &ColumnOptions,
    columns: &mut usize,
    rows: &mut usize,
) -> Vec<usize> {
    while *rows > 1 {
        let previous_rows = *rows;
        let previous_columns = *columns;
        *rows -= 1;
        *columns = items.len().div_ceil(*rows);
        let widths = column_widths(items, options.layout, *columns, *rows);
        let total_width = options
            .indent
            .len()
            .saturating_add(widths.iter().sum::<usize>())
            .saturating_add(options.padding.saturating_mul(*columns));
        if total_width > options.width {
            *rows = previous_rows;
            *columns = previous_columns;
            break;
        }
    }
    column_widths(items, options.layout, *columns, *rows)
}

fn column_widths(
    items: &[Vec<u8>],
    layout: ColumnLayout,
    columns: usize,
    rows: usize,
) -> Vec<usize> {
    let mut widths = vec![0; columns];
    for row in 0..rows {
        for (column, width) in widths.iter_mut().enumerate() {
            if let Some(index) = item_index(layout, items.len(), columns, rows, row, column) {
                *width = (*width).max(display_width(&items[index]));
            }
        }
    }
    widths
}

fn item_index(
    layout: ColumnLayout,
    len: usize,
    columns: usize,
    rows: usize,
    row: usize,
    column: usize,
) -> Option<usize> {
    let index = match layout {
        ColumnLayout::Plain => return None,
        ColumnLayout::ColumnFirst => column * rows + row,
        ColumnLayout::RowFirst => row * columns + column,
    };
    (index < len).then_some(index)
}

fn display_width(value: &[u8]) -> usize {
    let mut width = 0usize;
    let mut cursor = 0usize;
    while cursor < value.len() {
        if let Some(length) = sgr_escape_length(&value[cursor..]) {
            cursor += length;
            continue;
        }
        let remainder = &value[cursor..];
        let Ok(text) = std::str::from_utf8(remainder) else {
            // Git's utf8_strnwidth falls back to the byte length for malformed
            // UTF-8, including any bytes already processed.
            return value.len();
        };
        let Some(ch) = text.chars().next() else {
            break;
        };
        width += unicode_column_width(ch);
        cursor += ch.len_utf8();
    }
    width
}

fn sgr_escape_length(value: &[u8]) -> Option<usize> {
    if !value.starts_with(b"\x1b[") {
        return None;
    }
    let mut cursor = 2usize;
    while value
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        cursor += 1;
    }
    (value.get(cursor) == Some(&b'm')).then_some(cursor + 1)
}

fn unicode_column_width(ch: char) -> usize {
    let value = ch as u32;
    if ch == '\0' || ch.is_control() || is_combining(value) {
        0
    } else if is_wide(value) {
        2
    } else {
        1
    }
}

fn is_combining(value: u32) -> bool {
    matches!(
        value,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06ed
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2063
            | 0x206a..=0x206f
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0xe0100..=0xe01ef
    )
}

fn is_wide(value: u32) -> bool {
    matches!(
        value,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1f64f
            | 0x1f900..=0x1f9ff
            | 0x20000..=0x3fffd
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Vec<u8>> {
        "one two three four five six seven eight nine ten eleven"
            .split_whitespace()
            .map(|item| item.as_bytes().to_vec())
            .collect()
    }

    #[test]
    fn column_and_row_layouts_match_git_examples() {
        let mut options = ColumnOptions {
            width: 20,
            ..ColumnOptions::default()
        };
        assert_eq!(
            format_columns(&items(), &options),
            b"one    seven\ntwo    eight\nthree  nine\nfour   ten\nfive   eleven\nsix\n"
        );
        options.dense = true;
        assert_eq!(
            format_columns(&items(), &options),
            b"one   five  nine\ntwo   six   ten\nthree seven eleven\nfour  eight\n"
        );
        options.layout = ColumnLayout::RowFirst;
        assert_eq!(
            format_columns(&items(), &options),
            b"one   two    three\nfour  five   six\nseven eight  nine\nten   eleven\n"
        );
    }

    #[test]
    fn dense_layout_follows_git_row_shrinking() {
        let items = ["aaaaaa", "b", "cccccc", "d", "eeeeee"].map(|item| item.as_bytes().to_vec());
        let options = ColumnOptions {
            width: 18,
            dense: true,
            ..ColumnOptions::default()
        };
        assert_eq!(
            format_columns(&items, &options),
            b"aaaaaa d\nb      eeeeee\ncccccc\n"
        );
    }

    #[test]
    fn ansi_sgr_sequences_do_not_consume_columns() {
        let items = vec![b"\x1b[31mred\x1b[m".to_vec(), b"blue".to_vec()];
        let options = ColumnOptions {
            layout: ColumnLayout::RowFirst,
            width: 20,
            ..ColumnOptions::default()
        };
        assert_eq!(
            format_columns(&items, &options),
            b"\x1b[31mred\x1b[m  blue\n"
        );
    }

    #[test]
    fn blank_items_are_layout_entries() {
        let items = vec![b"one".to_vec(), Vec::new(), b"three".to_vec()];
        let options = ColumnOptions {
            layout: ColumnLayout::Plain,
            indent: b">".to_vec(),
            ..ColumnOptions::default()
        };
        assert_eq!(format_columns(&items, &options), b">one\n>\n>three\n");
    }
}
