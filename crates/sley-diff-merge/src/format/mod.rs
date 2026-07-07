//! Diff rendering format helpers (word-diff, colors, funcname adapters).

mod funcname;
mod hunks;
mod words;

pub use funcname::{CompiledFuncname, default_funcname_heading};
pub use hunks::{WordDiffAdapter, heading_classifier, render_colors};
pub use words::{
    DiffColors, WordDiffBuffers, WordDiffConfig, WordDiffMode, parse_color_value, push_colored_line,
};

#[cfg(test)]
mod tests;