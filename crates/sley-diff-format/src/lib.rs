mod funcname;
mod words;

pub use funcname::CompiledFuncname;
pub use words::{
    DiffColors, WordDiffBuffers, WordDiffConfig, WordDiffMode, parse_color_value, push_colored_line,
};
