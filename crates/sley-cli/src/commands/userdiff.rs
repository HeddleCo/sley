//! Userdiff drivers: per-language funcname (hunk-header) patterns and word
//! regexes, ported from upstream `userdiff.c`, plus the `.gitattributes`
//! `diff=<driver>` / `diff.<driver>.*` config resolution that selects one for
//! a path.
//!
//! The builtin table below is generated mechanically from the upstream
//! `builtin_drivers[]` (git 2.54.0): the `PATTERNS`/`IPATTERN` macro expansion
//! was evaluated by a C compiler and each driver's name, `REG_ICASE` flag,
//! funcname pattern, and word regex (with the macro-appended
//! `|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+` tail) dumped verbatim, so the byte
//! content matches upstream exactly.

use crate::*;
use commands::grep::{Regex, RegexMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// One row of the upstream builtin driver table.
pub(crate) struct BuiltinDriver {
    pub(crate) name: &'static str,
    /// Whether the funcname pattern carries `REG_ICASE` (the `IPATTERN` rows).
    pub(crate) icase: bool,
    pub(crate) funcname: Option<&'static [u8]>,
    pub(crate) word_regex: Option<&'static [u8]>,
}

static BUILTIN_DRIVERS: &[BuiltinDriver] = &[
    BuiltinDriver {
        name: "ada",
        icase: true,
        funcname: Some(b"!^(.*[ \t])?(is[ \t]+new|renames|is[ \t]+separate)([ \t].*)?$\n!^[ \t]*with[ \t].*$\n^[ \t]*((procedure|function)[ \t]+.*)$\n^[ \t]*((package|protected|task)[ \t]+.*)$"),
        word_regex: Some(b"[a-zA-Z][a-zA-Z0-9_]*|[-+]?[0-9][0-9#_.aAbBcCdDeEfF]*([eE][+-]?[0-9_]+)?|=>|\\.\\.|\\*\\*|:=|/=|>=|<=|<<|>>|<>|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "bash",
        icase: false,
        funcname: Some(b"^[ \t]*((([a-zA-Z_][a-zA-Z0-9_]*[ \t]*\\([ \t]*\\))|(function[ \t]+[a-zA-Z_][a-zA-Z0-9_]*(([ \t]*\\([ \t]*\\))|([ \t]+)))).*$)"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|\\$[a-zA-Z0-9_]+|\\$\\{|\\|\\||&&|<<|>>|==|!=|<=|>=|[-+*/%&|^]=|:=|:-|:\\+|:\\?|##|%%|\\^\\^|,,|[-a-zA-Z0-9_]+|\\(|\\)|\\{|\\}|\\[|\\]|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "bibtex",
        icase: false,
        funcname: Some(b"(@[a-zA-Z]{1,}[ \t]*\\{{0,1}[ \t]*[^ \t\"@',\\#}{~%]*).*$"),
        word_regex: Some(b"[={}\"]|[^={}\" \t]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "cpp",
        icase: false,
        funcname: Some(b"!^[ \t]*[A-Za-z_][A-Za-z_0-9]*:[[:space:]]*($|/[/*])\n^((::[[:space:]]*)?[A-Za-z_].*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[0-9][0-9.]*([Ee][-+]?[0-9]+)?[fFlLuU]*|0[xXbB][0-9a-fA-F]+[lLuU]*|\\.[0-9][0-9]*([Ee][-+]?[0-9]+)?[fFlL]?|[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->\\*?|\\.\\*|<=>|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "csharp",
        icase: false,
        funcname: Some(b"!(^|[ \t]+)(do|while|for|foreach|if|else|new|default|return|switch|case|throw|catch|using|lock|fixed)([ \t(]+|$)\n^[ \t]*(([][[:alnum:]@_.](<[][[:alnum:]@_, \t<>]+>)?)+([ \t]+([][[:alnum:]@_.](<[][[:alnum:]@_, \t<>]+>)?)+)+[ \t]*\\([^;]*)$\n^[ \t]*(([][[:alnum:]@_.](<[][[:alnum:]@_, \t<>]+>)?)+([ \t]+([][[:alnum:]@_.](<[][[:alnum:]@_, \t<>]+>)?)+)+[^;=:,()]*)$\n^[ \t]*(((static|public|internal|private|protected|new|unsafe|sealed|abstract|partial)[ \t]+)*(class|enum|interface|struct|record)[ \t]+.*)$\n^[ \t]*(namespace[ \t]+.*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?|[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "css",
        icase: true,
        funcname: Some(b"![:;][[:space:]]*$\n^[:[@.#]?[_a-z0-9].*$"),
        word_regex: Some(b"-?[_a-zA-Z][-_a-zA-Z0-9]*|-?[0-9]+|\\#[0-9a-fA-F]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "dts",
        icase: false,
        funcname: Some(b"!;\n!=\n^[ \t]*((/[ \t]*\\{|&?[a-zA-Z_]).*)"),
        word_regex: Some(b"[a-zA-Z0-9,._+?#-]+|[-+*/%&^|!~]|>>|<<|&&|\\|\\||[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "elixir",
        icase: false,
        funcname: Some(b"^[ \t]*((def(macro|module|impl|protocol|p)?|test)[ \t].*)$"),
        word_regex: Some(b"[@:]?[a-zA-Z0-9@_?!]+|[-+]?0[xob][0-9a-fA-F]+|[-+]?[0-9][0-9_.]*([eE][-+]?[0-9_]+)?|:?(\\+\\+|--|\\.\\.|~~~|<>|\\^\\^\\^|<?\\|>|<<<?|>?>>|<<?~|~>?>|<~>|<=|>=|===?|!==?|=~|&&&?|\\|\\|\\|?|=>|<-|\\\\\\\\|->)|:?%[A-Za-z0-9_.]\\{\\}?|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "fortran",
        icase: true,
        funcname: Some(b"!^([C*]|[ \t]*!)\n!^[ \t]*MODULE[ \t]+PROCEDURE[ \t]\n^[ \t]*((END[ \t]+)?(PROGRAM|MODULE|BLOCK[ \t]+DATA|([^!'\" \t]+[ \t]+)*(SUBROUTINE|FUNCTION))[ \t]+[A-Z].*)$"),
        word_regex: Some(b"[a-zA-Z][a-zA-Z0-9_]*|\\.([Ee][Qq]|[Nn][Ee]|[Gg][TtEe]|[Ll][TtEe]|[Tt][Rr][Uu][Ee]|[Ff][Aa][Ll][Ss][Ee]|[Aa][Nn][Dd]|[Oo][Rr]|[Nn]?[Ee][Qq][Vv]|[Nn][Oo][Tt])\\.|[-+]?[0-9.]+([AaIiDdEeFfLlTtXx][Ss]?[-+]?[0-9.]*)?(_[a-zA-Z0-9][a-zA-Z0-9_]*)?|//|\\*\\*|::|[/<>=]=|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "fountain",
        icase: true,
        funcname: Some(b"^((\\.[^.]|(int|ext|est|int\\.?/ext|i/e)[. ]).*)$"),
        word_regex: Some(b"[^ \t-]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "golang",
        icase: false,
        funcname: Some(b"^[ \t]*(func[ \t]*.*(\\{[ \t]*)?)\n^[ \t]*(type[ \t].*(struct|interface)[ \t]*(\\{[ \t]*)?)"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.eE]+i?|0[xX]?[0-9a-fA-F]+i?|[-+*/<>%&^|=!:]=|--|\\+\\+|<<=?|>>=?|&\\^=?|&&|\\|\\||<-|\\.{3}|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "html",
        icase: false,
        funcname: Some(b"^[ \t]*(<[Hh][1-6]([ \t].*)?>.*)$"),
        word_regex: Some(b"[^<>= \t]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "ini",
        icase: false,
        funcname: Some(b"^[ \t]*\\[[^]]+\\]"),
        word_regex: Some(b"[^ \t]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "java",
        icase: false,
        funcname: Some(b"!^[ \t]*(catch|do|for|if|instanceof|new|return|switch|throw|while)\n^[ \t]*(([a-z-]+[ \t]+)*(class|enum|interface|record)[ \t]+.*)$\n^[ \t]*(([A-Za-z_<>&][][?&<>.,A-Za-z_0-9]*[ \t]+)+[A-Za-z_][A-Za-z_0-9]*[ \t]*\\([^;]*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?|[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>>?=?|&&|\\|\\||[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "kotlin",
        icase: false,
        funcname: Some(b"^[ \t]*(([a-z]+[ \t]+)*(fun|class|interface)[ \t]+.*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|0[xXbB][0-9a-fA-F_]+[lLuU]*|[0-9][0-9_]*([.][0-9_]*)?([Ee][-+]?[0-9]+)?[fFlLuU]*|[.][0-9][0-9_]*([Ee][-+]?[0-9]+)?[fFlLuU]?|[-+*/<>%&^|=!]==?|--|\\+\\+|<<=|>>=|&&|\\|\\||->|\\.\\*|!!|[?:.][.:]|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "markdown",
        icase: false,
        funcname: Some(b"^ {0,3}#{1,6}[ \t].*"),
        word_regex: Some(b"[^<>= \t]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "matlab",
        icase: false,
        funcname: Some(b"^[[:space:]]*((classdef|function)[[:space:]].*)$|^(%%%?|##)[[:space:]].*$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+|[=~<>]=|\\.[*/\\^']|\\|\\||&&|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "objc",
        icase: false,
        funcname: Some(b"!^[ \t]*(do|for|if|else|return|switch|while)\n^[ \t]*([-+][ \t]*\\([ \t]*[A-Za-z_][A-Za-z_0-9* \t]*\\)[ \t]*[A-Za-z_].*)$\n^[ \t]*(([A-Za-z_][A-Za-z_0-9]*[ \t]+)+[A-Za-z_][A-Za-z_0-9]*[ \t]*\\([^;]*)$\n^(@(implementation|interface|protocol)[ \t].*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?|[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "pascal",
        icase: false,
        funcname: Some(b"^(((class[ \t]+)?(procedure|function)|constructor|destructor|interface|implementation|initialization|finalization)[ \t]*.*)$\n^(.*=[ \t]*(class|record).*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+|<>|<=|>=|:=|\\.\\.|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "perl",
        icase: false,
        funcname: Some(b"^package .*\n^sub [[:alnum:]_':]+[ \t]*(\\([^)]*\\)[ \t]*)?(:[^;#]*)?(\\{[ \t]*)?(#.*)?$\n^(BEGIN|END|INIT|CHECK|UNITCHECK|AUTOLOAD|DESTROY)[ \t]*(\\{[ \t]*)?(#.*)?$\n^=head[0-9] .*"),
        word_regex: Some(b"[[:alpha:]_'][[:alnum:]_']*|0[xb]?[0-9a-fA-F_]*|[0-9a-fA-F_]+(\\.[0-9a-fA-F_]+)?([eE][-+]?[0-9_]+)?|=>|-[rwxoRWXOezsfdlpSugkbctTBMAC>]|~~|::|&&=|\\|\\|=|//=|\\*\\*=|&&|\\|\\||//|\\+\\+|--|\\*\\*|\\.\\.\\.?|[-+*/%.^&<>=!|]=|=~|!~|<<|<>|<=>|>>|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "php",
        icase: false,
        funcname: Some(b"^[\t ]*(((public|protected|private|static|abstract|final)[\t ]+)*function.*)$\n^[\t ]*((((final|abstract)[\t ]+)?class|enum|interface|trait).*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+|[-+*/<>%&^|=!.]=|--|\\+\\+|<<=?|>>=?|===|&&|\\|\\||::|->|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "python",
        icase: false,
        funcname: Some(b"^[ \t]*((class|(async[ \t]+)?def)[ \t].*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+[jJlL]?|0[xX]?[0-9a-fA-F]+[lL]?|[-+*/<>%&^|=!]=|//=?|<<=?|>>=?|\\*\\*=?|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "r",
        icase: false,
        funcname: Some(b"^[ \t]*([a-zA-z][a-zA-Z0-9_.]*[ \t]*(<-|=)[ \t]*function.*)$"),
        word_regex: Some(b"[^ \t]+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "ruby",
        icase: false,
        funcname: Some(b"^[ \t]*((class|module|def)[ \t].*)$"),
        word_regex: Some(b"(@|@@|\\$)?[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+|\\?(\\\\C-)?(\\\\M-)?.|//=?|[-+*/<>%&^|=!]=|<<=?|>>=?|===|\\.{1,3}|::|[!=]~|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "rust",
        icase: false,
        funcname: Some(b"^[\t ]*((pub(\\([^\\)]+\\))?[\t ]+)?((async|const|unsafe|extern([\t ]+\"[^\"]+\"))[\t ]+)?(struct|enum|union|mod|trait|fn|impl|macro_rules!)[< \t]+[^;]*)$"),
        word_regex: Some(b"[a-zA-Z_][a-zA-Z0-9_]*|[0-9][0-9_a-fA-Fiosuxz]*(\\.([0-9]*[eE][+-]?)?[0-9_fF]*)?|[-+*\\/<>%&^|=!:]=|<<=?|>>=?|&&|\\|\\||->|=>|\\.{2}=|\\.{3}|::|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "scheme",
        icase: false,
        funcname: Some(b"^[\t ]*(\\(((define|def(struct|syntax|class|method|rules|record|proto|alias)?)[-*/ \t]|(library|module|struct|class)[*+ \t]).*)$"),
        word_regex: Some(b"\\|([^\\\\]*)\\||([^][)(}{[ \t])+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "tex",
        icase: false,
        funcname: Some(b"^(\\\\((sub)*section|chapter|part)\\*{0,1}\\{.*)$"),
        word_regex: Some(b"\\\\[a-zA-Z@]+|\\\\.|([a-zA-Z0-9]|[^\x01-\x7f])+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
    },
    BuiltinDriver {
        name: "default",
        icase: false,
        funcname: None,
        word_regex: None,
    },
];

/// A compiled funcname spec: newline-separated POSIX regexes tried in order;
/// a leading `!` negates (a match rejects the line). Port of
/// `xdiff_set_find_func` + `ff_regexp` from upstream `xdiff-interface.c`.
pub(crate) struct CompiledFuncname {
    patterns: Vec<(bool, Regex)>,
}

/// The upstream hunk-header buffer is `char buf[80]` (`struct func_line`);
/// headings are truncated to it before the trailing-whitespace trim.
const FUNCNAME_BUFFER: usize = 80;

impl CompiledFuncname {
    /// Compile a funcname spec. `extended` selects ERE (`xfuncname` /
    /// builtins) over BRE (`funcname` config). Errors mirror upstream's
    /// `die()` calls byte-for-byte (printed to stderr, exit 128).
    pub(crate) fn compile(spec: &[u8], extended: bool, icase: bool) -> Result<Self> {
        let lines: Vec<&[u8]> = spec.split(|&b| b == b'\n').collect();
        let mut patterns = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let negate = line.first() == Some(&b'!');
            if negate && idx == lines.len() - 1 {
                // die("Last expression must not be negated: %s", value) —
                // `value` is the remaining suffix of the spec at that point.
                let suffix: Vec<u8> = lines[idx..].join(&b'\n');
                eprintln!(
                    "fatal: Last expression must not be negated: {}",
                    String::from_utf8_lossy(&suffix)
                );
                return Err(GitError::Exit(128));
            }
            let expression = if negate { &line[1..] } else { line };
            let mode = if extended {
                RegexMode::Ere
            } else {
                RegexMode::Bre
            };
            let regex = Regex::compile_bytes(expression, mode, icase, false).map_err(|_| {
                eprintln!(
                    "fatal: Invalid regexp to look for hunk header: {}",
                    String::from_utf8_lossy(expression)
                );
                GitError::Exit(128)
            })?;
            patterns.push((negate, regex));
        }
        Ok(Self { patterns })
    }

    /// Port of `ff_regexp`: match `line` (raw record bytes, trailing newline
    /// still attached) against the pattern list; return the heading bytes, or
    /// `None` when no pattern accepts the line.
    pub(crate) fn match_line(&self, line: &[u8]) -> Option<Vec<u8>> {
        // Exclude terminating newline (and cr) from matching.
        let mut len = line.len();
        if len > 0 && line[len - 1] == b'\n' {
            if len > 1 && line[len - 2] == b'\r' {
                len -= 2;
            } else {
                len -= 1;
            }
        }
        let line = &line[..len];
        let mut matched: Option<Vec<Option<(usize, usize)>>> = None;
        for (negate, regex) in &self.patterns {
            if let Some(captures) = regex.find_captures(line) {
                if *negate {
                    return None;
                }
                matched = Some(captures);
                break;
            }
        }
        let captures = matched?;
        let (start, end) = captures
            .get(1)
            .copied()
            .flatten()
            .unwrap_or_else(|| captures[0].expect("whole-match span present"));
        let heading = &line[start..end];
        let mut result = heading.len().min(FUNCNAME_BUFFER);
        while result > 0 && heading[result - 1].is_ascii_whitespace() {
            result -= 1;
        }
        Some(heading[..result].to_vec())
    }
}

/// Port of `def_ff` (the default funcname heuristic): a line whose first byte
/// is a letter, `_`, or `$` is a section heading, truncated to the 80-byte
/// header buffer with trailing whitespace trimmed.
pub(crate) fn default_funcname_heading(line: &[u8]) -> Option<Vec<u8>> {
    let first = line.first()?;
    if !(first.is_ascii_alphabetic() || *first == b'_' || *first == b'$') {
        return None;
    }
    let mut len = line.len().min(FUNCNAME_BUFFER);
    while len > 0 && line[len - 1].is_ascii_whitespace() {
        len -= 1;
    }
    Some(line[..len].to_vec())
}

/// A driver resolved for a concrete path: compiled funcname patterns, the raw
/// word-regex spec (compiled on demand by word-diff), and the binary tristate.
pub(crate) struct ResolvedDriver {
    pub(crate) funcname: Option<CompiledFuncname>,
    pub(crate) word_regex: Option<Vec<u8>>,
    /// `Some(true)` = `-diff` / binary=true; `Some(false)` = `diff` set /
    /// binary=false; `None` = auto-detect.
    pub(crate) binary: Option<bool>,
}

/// Resolves the userdiff driver for paths in one repository: `.gitattributes`
/// `diff` lookup, then `diff.<driver>.*` config (custom drivers shadow
/// builtins wholesale, as in `userdiff_find_by_namelen`), then the builtin
/// table.
pub(crate) struct UserdiffResolver {
    worktree_root: Option<PathBuf>,
    config: Option<GitConfig>,
    drivers: RefCell<HashMap<Vec<u8>, Option<Rc<ResolvedDriver>>>>,
}

impl UserdiffResolver {
    pub(crate) fn new(worktree_root: Option<PathBuf>, config: Option<GitConfig>) -> Self {
        Self {
            worktree_root,
            config,
            drivers: RefCell::new(HashMap::new()),
        }
    }

    /// The `diff.wordRegex` config fallback (used when neither the command
    /// line nor the path's driver supplies a word regex).
    pub(crate) fn config_word_regex(&self) -> Option<Vec<u8>> {
        self.config
            .as_ref()
            .and_then(|config| config.get("diff", None, "wordregex"))
            .map(|value| value.as_bytes().to_vec())
    }

    /// Resolve the driver for `path`, or `None` when the `diff` attribute is
    /// unspecified (default behaviour). Fatal pattern errors propagate.
    pub(crate) fn driver_for_path(&self, path: &[u8]) -> Result<Option<Rc<ResolvedDriver>>> {
        let Some(worktree_root) = self.worktree_root.as_deref() else {
            return Ok(None);
        };
        let attrs = sley_worktree::standard_attributes_for_path(
            worktree_root,
            path,
            &[b"diff".to_vec()],
            false,
        )?;
        let state = attrs.into_iter().next().and_then(|check| check.state);
        match state {
            None => Ok(None),
            Some(sley_worktree::AttributeState::Set) => {
                // ATTR_TRUE: driver_true — text, no patterns.
                Ok(Some(Rc::new(ResolvedDriver {
                    funcname: None,
                    word_regex: None,
                    binary: Some(false),
                })))
            }
            Some(sley_worktree::AttributeState::Unset) => {
                // ATTR_FALSE: driver_false — binary.
                Ok(Some(Rc::new(ResolvedDriver {
                    funcname: None,
                    word_regex: None,
                    binary: Some(true),
                })))
            }
            Some(sley_worktree::AttributeState::Value(name)) => self.driver_by_name(&name),
        }
    }

    /// Resolve a driver by name: config-defined custom drivers shadow the
    /// builtin of the same name entirely (a custom driver starts empty, so a
    /// lone `diff.java.funcname` also discards builtin java's word regex).
    pub(crate) fn driver_by_name(&self, name: &[u8]) -> Result<Option<Rc<ResolvedDriver>>> {
        if let Some(cached) = self.drivers.borrow().get(name) {
            return Ok(cached.clone());
        }
        let resolved = self.resolve_by_name(name)?;
        self.drivers
            .borrow_mut()
            .insert(name.to_vec(), resolved.clone());
        Ok(resolved)
    }

    fn resolve_by_name(&self, name: &[u8]) -> Result<Option<Rc<ResolvedDriver>>> {
        if let Some(custom) = self.custom_driver_config(name)? {
            return Ok(Some(Rc::new(custom)));
        }
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(None);
        };
        let Some(builtin) = BUILTIN_DRIVERS.iter().find(|driver| driver.name == name) else {
            return Ok(None);
        };
        let funcname = builtin
            .funcname
            .map(|spec| CompiledFuncname::compile(spec, true, builtin.icase))
            .transpose()?;
        Ok(Some(Rc::new(ResolvedDriver {
            funcname,
            word_regex: builtin.word_regex.map(<[u8]>::to_vec),
            binary: None,
        })))
    }

    /// Build a custom driver from `diff.<name>.*` config, or `None` when no
    /// such keys exist. Within the config stream the *last* of
    /// `funcname` (BRE) / `xfuncname` (ERE) wins, like repeated
    /// `parse_funcname` calls overwriting `drv->funcname`.
    fn custom_driver_config(&self, name: &[u8]) -> Result<Option<ResolvedDriver>> {
        let Some(config) = self.config.as_ref() else {
            return Ok(None);
        };
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(None);
        };
        let mut any = false;
        let mut funcname_spec: Option<(Vec<u8>, bool)> = None; // (spec, extended)
        let mut word_regex: Option<Vec<u8>> = None;
        let mut binary: Option<bool> = None;
        for section in &config.sections {
            if !section.name.eq_ignore_ascii_case("diff")
                || section.subsection.as_deref() != Some(name)
            {
                continue;
            }
            for entry in &section.entries {
                let key = entry.key.to_ascii_lowercase();
                match key.as_str() {
                    "funcname" => {
                        any = true;
                        if let Some(value) = entry.value.as_deref() {
                            funcname_spec = Some((value.as_bytes().to_vec(), false));
                        }
                    }
                    "xfuncname" => {
                        any = true;
                        if let Some(value) = entry.value.as_deref() {
                            funcname_spec = Some((value.as_bytes().to_vec(), true));
                        }
                    }
                    "wordregex" => {
                        any = true;
                        if let Some(value) = entry.value.as_deref() {
                            word_regex = Some(value.as_bytes().to_vec());
                        }
                    }
                    "binary" => {
                        any = true;
                        binary = match entry.value.as_deref() {
                            Some(value) if value.eq_ignore_ascii_case("auto") => None,
                            Some(value) => parse_config_bool_like(value),
                            None => Some(true),
                        };
                    }
                    "command" | "trustexitcode" | "textconv" | "cachetextconv" | "algorithm" => {
                        any = true;
                    }
                    _ => {}
                }
            }
        }
        if !any {
            return Ok(None);
        }
        let funcname = funcname_spec
            .map(|(spec, extended)| CompiledFuncname::compile(&spec, extended, false))
            .transpose()?;
        Ok(Some(ResolvedDriver {
            funcname,
            word_regex,
            binary,
        }))
    }
}

fn parse_config_bool_like(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every builtin funcname pattern and word regex must compile with the
    /// in-house engine — a compile failure is a fatal at diff time upstream
    /// never produces.
    #[test]
    fn builtin_patterns_compile() {
        for driver in BUILTIN_DRIVERS {
            if let Some(spec) = driver.funcname {
                CompiledFuncname::compile(spec, true, driver.icase)
                    .unwrap_or_else(|_| panic!("funcname for {} failed to compile", driver.name));
            }
            if let Some(word_regex) = driver.word_regex {
                Regex::compile_bytes(word_regex, RegexMode::Ere, false, false)
                    .unwrap_or_else(|_| panic!("word regex for {} failed to compile", driver.name));
            }
        }
    }

    #[test]
    fn builtin_table_is_sorted_with_default_last() {
        let names: Vec<&str> = BUILTIN_DRIVERS.iter().map(|driver| driver.name).collect();
        let (default, rest) = names.split_last().expect("non-empty table");
        assert_eq!(*default, "default");
        let mut sorted = rest.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, rest);
    }

    #[test]
    fn java_heading_matches_method_not_keyword() {
        let driver = BUILTIN_DRIVERS
            .iter()
            .find(|driver| driver.name == "java")
            .unwrap();
        let funcname =
            CompiledFuncname::compile(driver.funcname.unwrap(), true, driver.icase).unwrap();
        assert_eq!(
            funcname.match_line(b"\tpublic static void main(String RIGHT[])\n"),
            Some(b"public static void main(String RIGHT[])".to_vec())
        );
        assert_eq!(funcname.match_line(b"\treturn foo(\n"), None);
        assert_eq!(
            funcname.match_line(b"public class Beer\n"),
            Some(b"public class Beer".to_vec())
        );
    }

    #[test]
    fn negated_last_expression_is_fatal() {
        assert!(CompiledFuncname::compile(b"!static", false, false).is_err());
    }
}
