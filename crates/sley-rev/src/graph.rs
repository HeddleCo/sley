//! `git log --graph` ASCII graph renderer — a faithful port of upstream
//! `graph.c` (the column state machine: commit rows, octopus pre-commit
//! expansion, post-merge fan-out, and collapsing rows).
//!
//! The caller drives it exactly like upstream: [`Graph::update`] with each
//! commit (and its *interesting* parents, pre-filtered to the set that will be
//! shown), then [`Graph::next_line`] / the `show_*` helpers to emit rows.
//! Columns are identified by commit object id.

use sley_core::ObjectId;

/// `\033[m` — the terminating entry of every palette.
pub const COLOR_RESET: &str = "\x1b[m";

/// Upstream `column_colors_ansi` (without the trailing reset, which
/// [`Graph::new`] appends).
pub fn ansi_column_colors() -> Vec<String> {
    [
        "\x1b[31m",
        "\x1b[32m",
        "\x1b[33m",
        "\x1b[34m",
        "\x1b[35m",
        "\x1b[36m",
        "\x1b[1;31m",
        "\x1b[1;32m",
        "\x1b[1;33m",
        "\x1b[1;34m",
        "\x1b[1;35m",
        "\x1b[1;36m",
    ]
    .iter()
    .map(|color| color.to_string())
    .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Padding,
    Skip,
    PreCommit,
    Commit,
    PostMerge,
    Collapsing,
}

#[derive(Clone)]
struct Column {
    commit: ObjectId,
    /// Index into the palette; `colors_max` means "no color".
    color: usize,
}

/// One output row under construction (upstream `struct graph_line`): `width`
/// counts visible characters only, excluding color escapes.
struct Line {
    buf: String,
    width: usize,
}

impl Line {
    fn addch(&mut self, ch: char) {
        self.buf.push(ch);
        self.width += 1;
    }
    fn addchars(&mut self, ch: char, n: usize) {
        for _ in 0..n {
            self.buf.push(ch);
        }
        self.width += n;
    }
    fn addstr(&mut self, s: &str) {
        self.buf.push_str(s);
        self.width += s.len();
    }
}

pub struct Graph {
    /// Palette; the LAST entry is the reset sequence, and `colors_max` is its
    /// index (upstream `column_colors_max`).
    colors: Vec<String>,
    colors_max: usize,
    use_color: bool,

    commit: Option<ObjectId>,
    /// The current commit's interesting parents (pre-filtered by the caller).
    parents: Vec<ObjectId>,
    num_parents: usize,
    width: usize,
    expansion_row: usize,
    state: State,
    prev_state: State,
    commit_index: usize,
    prev_commit_index: usize,
    merge_layout: i64,
    edges_added: i64,
    prev_edges_added: i64,
    columns: Vec<Column>,
    new_columns: Vec<Column>,
    mapping: Vec<i64>,
    old_mapping: Vec<i64>,
    mapping_size: usize,
    default_column_color: usize,
}

impl Graph {
    /// `colors` excludes the reset entry; pass `use_color: false` to disable
    /// coloring entirely (`--color=never`).
    pub fn new(mut colors: Vec<String>, use_color: bool) -> Self {
        if colors.is_empty() {
            colors = ansi_column_colors();
        }
        let colors_max = colors.len();
        colors.push(COLOR_RESET.to_string());
        Graph {
            colors,
            colors_max,
            use_color,
            commit: None,
            parents: Vec::new(),
            num_parents: 0,
            width: 0,
            expansion_row: 0,
            state: State::Padding,
            prev_state: State::Padding,
            commit_index: 0,
            prev_commit_index: 0,
            merge_layout: 0,
            edges_added: 0,
            prev_edges_added: 0,
            columns: Vec::new(),
            new_columns: Vec::new(),
            mapping: Vec::new(),
            old_mapping: Vec::new(),
            mapping_size: 0,
            // Starts at max so the first increment lands on 0.
            default_column_color: colors_max - 1,
        }
    }

    fn update_state(&mut self, state: State) {
        self.prev_state = self.state;
        self.state = state;
    }

    fn write_column(&self, line: &mut Line, col: &Column, ch: char) {
        if col.color < self.colors_max {
            line.buf.push_str(&self.colors[col.color]);
        }
        line.addch(ch);
        if col.color < self.colors_max {
            line.buf.push_str(&self.colors[self.colors_max]);
        }
    }

    fn current_column_color(&self) -> usize {
        if self.use_color {
            self.default_column_color
        } else {
            self.colors_max
        }
    }

    fn increment_column_color(&mut self) {
        self.default_column_color = (self.default_column_color + 1) % self.colors_max;
    }

    fn find_commit_color(&self, commit: &ObjectId) -> usize {
        for col in &self.columns {
            if col.commit == *commit {
                return col.color;
            }
        }
        self.current_column_color()
    }

    fn find_new_column_by_commit(&self, commit: &ObjectId) -> Option<usize> {
        self.new_columns
            .iter()
            .position(|col| col.commit == *commit)
    }

    fn insert_into_new_columns(&mut self, commit: ObjectId, idx: Option<usize>) {
        let i = match self.find_new_column_by_commit(&commit) {
            Some(i) => i,
            None => {
                let color = self.find_commit_color(&commit);
                self.new_columns.push(Column { commit, color });
                self.new_columns.len() - 1
            }
        };

        let mapping_idx: usize;
        if self.num_parents > 1
            && self.merge_layout == -1
            && let Some(idx) = idx
        {
            // First parent of a merge: pick the layout based on whether the
            // parent's column sits left of the merge.
            let idx = idx as i64;
            let dist = idx - i as i64;
            let shift = if dist > 1 { 2 * dist - 3 } else { 1 };
            self.merge_layout = if dist > 0 { 0 } else { 1 };
            self.edges_added = self.num_parents as i64 + self.merge_layout - 2;
            mapping_idx = (self.width as i64 + (self.merge_layout - 1) * shift) as usize;
            self.width = (self.width as i64 + 2 * self.merge_layout) as usize;
        } else if self.edges_added > 0
            && self.width >= 2
            && self.mapping[self.width - 2] == i as i64
        {
            // The added edge joins the last existing column immediately.
            mapping_idx = self.width - 2;
            self.edges_added = -1;
        } else {
            mapping_idx = self.width;
            self.width += 2;
        }

        if self.mapping.len() <= mapping_idx {
            self.mapping.resize(mapping_idx + 1, -1);
        }
        self.mapping[mapping_idx] = i as i64;
    }

    fn update_columns(&mut self) {
        std::mem::swap(&mut self.columns, &mut self.new_columns);
        self.new_columns.clear();

        let max_new_columns = self.columns.len() + self.num_parents;
        self.mapping_size = 2 * max_new_columns;
        self.mapping.clear();
        self.mapping.resize(self.mapping_size, -1);

        self.width = 0;
        self.prev_edges_added = self.edges_added;
        self.edges_added = 0;

        let Some(commit) = self.commit else {
            return;
        };
        let num_columns = self.columns.len();
        let mut seen_this = false;
        let mut is_commit_in_columns = true;
        for i in 0..=num_columns {
            let col_commit: ObjectId = if i == num_columns {
                if seen_this {
                    break;
                }
                is_commit_in_columns = false;
                commit
            } else {
                self.columns[i].commit
            };

            if col_commit == commit {
                seen_this = true;
                self.commit_index = i;
                self.merge_layout = -1;
                let parents = self.parents.clone();
                for parent in &parents {
                    if self.num_parents > 1 || !is_commit_in_columns {
                        self.increment_column_color();
                    }
                    self.insert_into_new_columns(*parent, Some(i));
                }
                // The commit takes at least 2 spaces even with no parents.
                if self.num_parents == 0 {
                    self.width += 2;
                }
            } else {
                self.insert_into_new_columns(col_commit, None);
            }
        }

        // Shrink mapping_size to the minimum necessary.
        while self.mapping_size > 1 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }
    }

    fn num_dashed_parents(&self) -> i64 {
        self.num_parents as i64 + self.merge_layout - 3
    }

    fn num_expansion_rows(&self) -> i64 {
        self.num_dashed_parents() * 2
    }

    fn needs_pre_commit_line(&self) -> bool {
        self.num_parents >= 3
            && !self.columns.is_empty()
            && self.commit_index < self.columns.len() - 1
            && (self.expansion_row as i64) < self.num_expansion_rows()
    }

    /// Set the next commit to render (upstream `graph_update`). `parents` must
    /// already be filtered to the interesting set (and truncated to the first
    /// parent in first-parent mode).
    pub fn update(&mut self, commit: ObjectId, parents: &[ObjectId]) {
        self.commit = Some(commit);
        self.parents = parents.to_vec();
        self.num_parents = parents.len();

        self.prev_commit_index = self.commit_index;
        self.update_columns();
        self.expansion_row = 0;

        if self.state != State::Padding {
            self.state = State::Skip;
        } else if self.needs_pre_commit_line() {
            self.state = State::PreCommit;
        } else {
            self.state = State::Commit;
        }
    }

    fn is_mapping_correct(&self) -> bool {
        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            if target < 0 {
                continue;
            }
            if target as usize == i / 2 {
                continue;
            }
            return false;
        }
        true
    }

    fn pad_horizontally(&self, line: &mut Line) {
        if line.width < self.width {
            let pad = self.width - line.width;
            line.addchars(' ', pad);
        }
    }

    fn output_padding_line(&self, line: &mut Line) {
        for col in &self.new_columns {
            self.write_column(line, col, '|');
            line.addch(' ');
        }
    }

    pub fn graph_width(&self) -> usize {
        self.width
    }

    fn output_skip_line(&mut self, line: &mut Line) {
        line.addstr("...");
        if self.needs_pre_commit_line() {
            self.update_state(State::PreCommit);
        } else {
            self.update_state(State::Commit);
        }
    }

    fn output_pre_commit_line(&mut self, line: &mut Line) {
        debug_assert!(self.num_parents >= 3);
        let Some(commit) = self.commit else {
            return;
        };

        let mut seen_this = false;
        for i in 0..self.columns.len() {
            let col = self.columns[i].clone();
            if col.commit == commit {
                seen_this = true;
                self.write_column(line, &col, '|');
                line.addchars(' ', self.expansion_row);
            } else if seen_this && self.expansion_row == 0 {
                if self.prev_state == State::PostMerge && self.prev_commit_index < i {
                    self.write_column(line, &col, '\\');
                } else {
                    self.write_column(line, &col, '|');
                }
            } else if seen_this && self.expansion_row > 0 {
                self.write_column(line, &col, '\\');
            } else {
                self.write_column(line, &col, '|');
            }
            line.addch(' ');
        }

        self.expansion_row += 1;
        if !self.needs_pre_commit_line() {
            self.update_state(State::Commit);
        }
    }

    fn output_commit_char(&self, line: &mut Line) {
        line.addch('*');
    }

    /// Draw the horizontal dashes of an octopus merge.
    fn draw_octopus_merge(&self, line: &mut Line) {
        let dashed_parents = self.num_dashed_parents();
        for i in 0..dashed_parents {
            let mapping_idx = (self.commit_index as i64 + i + 2) * 2;
            let j = self.mapping[mapping_idx as usize];
            let col = self.new_columns[j as usize].clone();
            self.write_column(line, &col, '-');
            self.write_column(line, &col, if i == dashed_parents - 1 { '.' } else { '-' });
        }
    }

    fn output_commit_line(&mut self, line: &mut Line) {
        let Some(commit) = self.commit else {
            return;
        };
        let num_columns = self.columns.len();
        let mut seen_this = false;
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                commit
            } else {
                self.columns[i].commit
            };

            if col_commit == commit {
                seen_this = true;
                self.output_commit_char(line);
                if self.num_parents > 2 {
                    self.draw_octopus_merge(line);
                }
            } else {
                let col = self.columns[i].clone();
                if seen_this && self.edges_added > 1 {
                    self.write_column(line, &col, '\\');
                } else if seen_this && self.edges_added == 1 {
                    if self.prev_state == State::PostMerge
                        && self.prev_edges_added > 0
                        && self.prev_commit_index < i
                    {
                        self.write_column(line, &col, '\\');
                    } else {
                        self.write_column(line, &col, '|');
                    }
                } else if self.prev_state == State::Collapsing
                    && self.old_mapping.get(2 * i + 1).copied() == Some(i as i64)
                    && self.mapping.get(2 * i).copied().unwrap_or(-1) < i as i64
                {
                    self.write_column(line, &col, '/');
                } else {
                    self.write_column(line, &col, '|');
                }
            }
            line.addch(' ');
        }

        if self.num_parents > 1 {
            self.update_state(State::PostMerge);
        } else if self.is_mapping_correct() {
            self.update_state(State::Padding);
        } else {
            self.update_state(State::Collapsing);
        }
    }

    fn output_post_merge_line(&mut self, line: &mut Line) {
        let Some(commit) = self.commit else {
            return;
        };
        let first_parent = self.parents.first().copied();
        let mut parent_col: Option<Column> = None;
        let num_columns = self.columns.len();
        let mut seen_this = false;

        const MERGE_CHARS: [char; 3] = ['/', '|', '\\'];

        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                commit
            } else {
                self.columns[i].commit
            };

            if col_commit == commit {
                seen_this = true;
                let mut idx = self.merge_layout;
                for (j, parent) in self.parents.clone().iter().enumerate() {
                    let Some(par_column) = self.find_new_column_by_commit(parent) else {
                        continue;
                    };
                    let par_col = self.new_columns[par_column].clone();
                    let ch = MERGE_CHARS[idx as usize];
                    self.write_column(line, &par_col, ch);
                    if idx == 2 {
                        if self.edges_added > 0 || j < self.num_parents - 1 {
                            line.addch(' ');
                        }
                    } else {
                        idx += 1;
                    }
                }
                if self.edges_added == 0 {
                    line.addch(' ');
                }
            } else if seen_this {
                let col = self.columns[i].clone();
                if self.edges_added > 0 {
                    self.write_column(line, &col, '\\');
                } else {
                    self.write_column(line, &col, '|');
                }
                line.addch(' ');
            } else {
                let col = self.columns[i].clone();
                self.write_column(line, &col, '|');
                if self.merge_layout != 0 || i as i64 != self.commit_index as i64 - 1 {
                    match &parent_col {
                        Some(parent_col) => {
                            let parent_col = parent_col.clone();
                            self.write_column(line, &parent_col, '_');
                        }
                        None => line.addch(' '),
                    }
                }
            }

            if Some(col_commit) == first_parent && i < num_columns {
                parent_col = Some(self.columns[i].clone());
            }
        }

        if self.is_mapping_correct() {
            self.update_state(State::Padding);
        } else {
            self.update_state(State::Collapsing);
        }
    }

    fn output_collapsing_line(&mut self, line: &mut Line) {
        let mut used_horizontal = false;
        let mut horizontal_edge: i64 = -1;
        let mut horizontal_edge_target: i64 = -1;

        std::mem::swap(&mut self.mapping, &mut self.old_mapping);
        if self.mapping.len() < self.mapping_size {
            self.mapping.resize(self.mapping_size, -1);
        }
        for entry in self.mapping.iter_mut() {
            *entry = -1;
        }

        for i in 0..self.mapping_size {
            let target = self.old_mapping.get(i).copied().unwrap_or(-1);
            if target < 0 {
                continue;
            }
            debug_assert!((target as usize) * 2 <= i);

            if target as usize * 2 == i {
                // Already in the correct place.
                self.mapping[i] = target;
            } else if self.mapping[i - 1] < 0 {
                // Nothing on the left: move left by one.
                self.mapping[i - 1] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as i64;
                    horizontal_edge_target = target;
                    let mut j = (target as usize) * 2 + 3;
                    while j + 2 < i {
                        // j < i - 2, guarded against underflow
                        self.mapping[j] = target;
                        j += 2;
                    }
                }
            } else if self.mapping[i - 1] == target {
                // Combined with the branch line on the left (same parent).
            } else {
                // Cross over the branch line on the left.
                debug_assert!(self.mapping[i - 1] > target);
                self.mapping[i - 2] = target;
                if horizontal_edge == -1 {
                    horizontal_edge_target = target;
                    horizontal_edge = i as i64 - 1;
                    let mut j = (target as usize) * 2 + 3;
                    while j + 2 < i {
                        self.mapping[j] = target;
                        j += 2;
                    }
                }
            }
        }

        // Copy the new mapping into old_mapping.
        if self.old_mapping.len() < self.mapping_size {
            self.old_mapping.resize(self.mapping_size, -1);
        }
        self.old_mapping[..self.mapping_size].copy_from_slice(&self.mapping[..self.mapping_size]);

        // The new mapping may be 1 smaller than the old one.
        if self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }

        // Output a line based on the new mapping info.
        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            if target < 0 {
                line.addch(' ');
            } else if target as usize * 2 == i {
                let col = self.new_columns[target as usize].clone();
                self.write_column(line, &col, '|');
            } else if target == horizontal_edge_target && i as i64 != horizontal_edge - 1 {
                // Horizontal traversal: only the first segment survives into
                // the next line.
                if i != (target as usize) * 2 + 3 {
                    self.mapping[i] = -1;
                }
                used_horizontal = true;
                let col = self.new_columns[target as usize].clone();
                self.write_column(line, &col, '_');
            } else {
                if used_horizontal && (i as i64) < horizontal_edge {
                    self.mapping[i] = -1;
                }
                let col = self.new_columns[target as usize].clone();
                self.write_column(line, &col, '/');
            }
        }

        if self.is_mapping_correct() {
            self.update_state(State::Padding);
        }
    }

    /// Emit the next graph row into `out`; returns true when this row was the
    /// commit row (upstream `graph_next_line`).
    pub fn next_line(&mut self, out: &mut String) -> bool {
        if self.commit.is_none() {
            return false;
        }
        let mut line = Line {
            buf: String::new(),
            width: 0,
        };
        let mut shown_commit_line = false;
        match self.state {
            State::Padding => self.output_padding_line(&mut line),
            State::Skip => self.output_skip_line(&mut line),
            State::PreCommit => self.output_pre_commit_line(&mut line),
            State::Commit => {
                self.output_commit_line(&mut line);
                shown_commit_line = true;
            }
            State::PostMerge => self.output_post_merge_line(&mut line),
            State::Collapsing => self.output_collapsing_line(&mut line),
        }
        self.pad_horizontally(&mut line);
        out.push_str(&line.buf);
        shown_commit_line
    }

    /// A padding row that never shows the commit line (upstream
    /// `graph_padding_line`); used for inter-commit separators and diff-line
    /// prefixes.
    pub fn padding_line(&mut self, out: &mut String) {
        if self.state != State::Commit {
            self.next_line(out);
            return;
        }
        let Some(commit) = self.commit else {
            return;
        };
        let mut line = Line {
            buf: String::new(),
            width: 0,
        };
        for i in 0..self.columns.len() {
            let col = self.columns[i].clone();
            self.write_column(&mut line, &col, '|');
            if col.commit == commit && self.num_parents > 2 {
                line.addchars(' ', (self.num_parents - 2) * 2);
            } else {
                line.addch(' ');
            }
        }
        self.pad_horizontally(&mut line);
        out.push_str(&line.buf);
        self.prev_state = State::Padding;
    }

    pub fn is_commit_finished(&self) -> bool {
        self.state == State::Padding
    }
}
