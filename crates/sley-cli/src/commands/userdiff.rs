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
pub(crate) use sley::plumbing::sley_diff_merge::format::CompiledFuncname;
#[cfg(test)]
use sley_grep::{Regex, RegexMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use sley::plumbing::{sley_config, sley_worktree};

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
        word_regex: Some(b"\\|([^|\\\\]|\\\\.)*\\||([^][)(}{ \t])+|[^[:space:]]|[\xc0-\xff][\x80-\xbf]+"),
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

/// A driver resolved for a concrete path: compiled funcname patterns, the raw
/// word-regex spec (compiled on demand by word-diff), and the binary tristate.
pub(crate) struct ResolvedDriver {
    pub(crate) funcname: Option<CompiledFuncname>,
    pub(crate) word_regex: Option<Vec<u8>>,
    pub(crate) external: Option<ExternalDiffCommand>,
    /// `Some(true)` = `-diff` / binary=true; `Some(false)` = `diff` set /
    /// binary=false; `None` = auto-detect.
    pub(crate) binary: Option<bool>,
    /// `diff.<driver>.textconv`: a command run on the blob to produce a text
    /// representation before diffing / blaming / `cat-file --textconv`.
    pub(crate) textconv: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalDiffCommand {
    pub(crate) command: String,
    pub(crate) trust_exit_code: bool,
}

/// Resolves the userdiff driver for paths in one repository: `.gitattributes`
/// `diff` lookup, then `diff.<driver>.*` config (custom drivers shadow
/// builtins wholesale, as in `userdiff_find_by_namelen`), then the builtin
/// table.
pub(crate) struct UserdiffResolver {
    attributes: Option<sley_worktree::StandardAttributeMatcher>,
    config: Option<GitConfig>,
    drivers: RefCell<HashMap<Vec<u8>, Option<Rc<ResolvedDriver>>>>,
}

impl UserdiffResolver {
    pub(crate) fn with_attributes(
        attributes: Option<sley_worktree::StandardAttributeMatcher>,
        config: Option<GitConfig>,
    ) -> Self {
        Self {
            attributes,
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
        let Some(attributes) = self.attributes.as_ref() else {
            return Ok(None);
        };
        let attrs = attributes.attributes_for_path(path, &[b"diff".to_vec()], false);
        let state = attrs.into_iter().next().and_then(|check| check.state);
        match state {
            None => Ok(None),
            Some(sley_worktree::AttributeState::Set) => {
                // ATTR_TRUE: driver_true — text, no patterns.
                Ok(Some(Rc::new(ResolvedDriver {
                    funcname: None,
                    word_regex: None,
                    external: None,
                    binary: Some(false),
                    textconv: None,
                })))
            }
            Some(sley_worktree::AttributeState::Unset) => {
                // ATTR_FALSE: driver_false — binary.
                Ok(Some(Rc::new(ResolvedDriver {
                    funcname: None,
                    word_regex: None,
                    external: None,
                    binary: Some(true),
                    textconv: None,
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
            external: None,
            binary: None,
            textconv: None,
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
        let mut command: Option<String> = None;
        let mut trust_exit_code = false;
        let mut binary: Option<bool> = None;
        let mut textconv: Option<String> = None;
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
                    "command" => {
                        any = true;
                        command = entry.value.clone();
                    }
                    "trustexitcode" => {
                        any = true;
                        trust_exit_code = entry
                            .value
                            .as_deref()
                            .and_then(sley_config::parse_config_bool)
                            .unwrap_or(true);
                    }
                    "textconv" => {
                        any = true;
                        textconv = entry.value.clone();
                    }
                    "cachetextconv" | "algorithm" => {
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
            external: command.map(|command| ExternalDiffCommand {
                command,
                trust_exit_code,
            }),
            binary,
            textconv,
        }))
    }
}

/// Whether a tree/index entry mode names a regular file. Upstream's
/// `diff_filespec_load_driver` only resolves the path's `diff=<driver>`
/// attribute for regular files; symlinks (and gitlinks) fall back to the
/// builtin "default" driver, which has no textconv — so a textconv program is
/// never run on a symlink's blob (its target path).
pub(crate) fn mode_is_regular_file(mode: u32) -> bool {
    mode & 0o170000 == 0o100000
}

impl UserdiffResolver {
    /// Resolve the `diff.<driver>.textconv` command for `path`, honouring the
    /// regular-file gate (`mode_is_regular_file`). Returns `None` when the path
    /// has no `diff` attribute, the named driver defines no textconv, or the
    /// entry is not a regular file.
    pub(crate) fn textconv_for_path(&self, path: &[u8], mode: u32) -> Result<Option<String>> {
        if !mode_is_regular_file(mode) {
            return Ok(None);
        }
        Ok(self
            .driver_for_path(path)?
            .and_then(|driver| driver.textconv.clone()))
    }
}

/// Run a `diff.<driver>.textconv` command over `content`, returning the
/// converted bytes. Mirrors upstream `run_textconv`: the blob content is
/// written to a temporary file whose name is appended as the command's sole
/// positional argument, the command runs through the shell, and its stdout is
/// captured. A spawn / non-zero-exit / read failure returns `None` (upstream's
/// `run_textconv` yields NULL there, which `fill_textconv` turns into a fatal).
pub(crate) fn run_textconv(command: &str, content: &[u8]) -> Result<Option<Vec<u8>>> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temp_path = std::env::temp_dir();
    temp_path.push(format!(
        "sley-textconv-{}-{}.tmp",
        std::process::id(),
        unique
    ));

    std::fs::write(&temp_path, content)
        .map_err(|err| GitError::Io(format!("textconv tempfile: {err}")))?;

    // Upstream builds the child with `use_shell` and args `[pgm, tempname]`,
    // which `prepare_shell_cmd` turns into `sh -c '<pgm> "$@"' <pgm> <tempname>`
    // — the tempfile becomes "$1" rather than being string-concatenated, so a
    // command ending in `<` (redirect) sees it as its input file.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    let output = if cfg!(windows) {
        std::process::Command::new(shell)
            .arg(flag)
            .arg(format!("{command} {}", temp_path.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output()
    } else {
        std::process::Command::new(shell)
            .arg(flag)
            .arg(format!("{command} \"$@\""))
            .arg(command)
            .arg(&temp_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output()
    };
    let _ = std::fs::remove_file(&temp_path);

    let output = match output {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

fn parse_config_bool_like(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
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
