use sley_diff_merge::render::{HunkWordDiff, RenderColors};

use crate::{CompiledFuncname, DiffColors, WordDiffBuffers, WordDiffConfig};

/// Map a diff color palette into the engine's render-color borrow.
pub fn render_colors(colors: &DiffColors) -> RenderColors<'_> {
    RenderColors {
        frag: &colors.frag,
        func: &colors.func,
        old: &colors.old,
        new: &colors.new,
        context: &colors.context,
        reset: &colors.reset,
        whitespace: &colors.whitespace,
        old_moved: &colors.old_moved,
        old_moved_alt: &colors.old_moved_alt,
        old_moved_dim: &colors.old_moved_dim,
        old_moved_alt_dim: &colors.old_moved_alt_dim,
        new_moved: &colors.new_moved,
        new_moved_alt: &colors.new_moved_alt,
        new_moved_dim: &colors.new_moved_dim,
        new_moved_alt_dim: &colors.new_moved_alt_dim,
    }
}

/// Bridge a word-diff config + its line buffers into the engine's
/// [`HunkWordDiff`] hook. The engine owns hunk shaping; this adapter owns the
/// word-level rendering.
pub struct WordDiffAdapter<'a> {
    config: &'a WordDiffConfig<'a>,
    buffers: WordDiffBuffers,
}

impl<'a> WordDiffAdapter<'a> {
    pub fn new(config: &'a WordDiffConfig<'a>) -> Self {
        Self {
            config,
            buffers: WordDiffBuffers::new(),
        }
    }
}

impl HunkWordDiff for WordDiffAdapter<'_> {
    fn push_minus(&mut self, content: &[u8]) {
        self.buffers.push_minus(content);
    }

    fn push_plus(&mut self, content: &[u8]) {
        self.buffers.push_plus(content);
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        self.buffers.flush(out, self.config);
    }

    fn emit_context_line(&mut self, out: &mut Vec<u8>, content: &[u8]) {
        WordDiffBuffers::emit_context_line(out, self.config, content);
    }
}

/// A per-line section-heading classifier matching git's funcname resolution:
/// a userdiff `xfuncname` pattern when a driver is present, else the default
/// `def_ff` heuristic.
pub fn heading_classifier<'a>(
    funcname: Option<&'a CompiledFuncname>,
) -> impl FnMut(&[u8]) -> Option<Vec<u8>> + 'a {
    move |line: &[u8]| match funcname {
        Some(funcname) => funcname.match_line(line),
        None => crate::default_funcname_heading(line),
    }
}
