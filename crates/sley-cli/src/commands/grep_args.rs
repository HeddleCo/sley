use crate::*;

pub(crate) trait GrepArgOptions {
    fn grep_patterns_mut(&mut self) -> &mut Vec<String>;
    fn grep_pattern_kind_mut(&mut self) -> &mut crate::grep_source::PatternKind;
    fn grep_pattern_kind_explicit_mut(&mut self) -> &mut bool;
    fn grep_ignore_case_mut(&mut self) -> &mut bool;
    fn grep_all_match_mut(&mut self) -> &mut bool;
    fn grep_invert_mut(&mut self) -> &mut bool;
}

pub(crate) fn parse_grep_args<'a, I, O>(
    arg: &str,
    iter: &mut I,
    options: &mut O,
) -> Result<bool>
where
    I: Iterator<Item = &'a String>,
    O: GrepArgOptions,
{
    match arg {
        "--grep" => {
            let value = iter
                .next()
                .ok_or_else(|| GitError::Command("--grep requires a value".into()))?;
            options.grep_patterns_mut().push(value.clone());
        }
        value if let Some(pattern) = value.strip_prefix("--grep=") => {
            options.grep_patterns_mut().push(pattern.to_string());
        }
        "--all-match" => *options.grep_all_match_mut() = true,
        "--invert-grep" => *options.grep_invert_mut() = true,
        "-i" | "--regexp-ignore-case" => *options.grep_ignore_case_mut() = true,
        "-F" | "--fixed-strings" => {
            *options.grep_pattern_kind_mut() = crate::grep_source::PatternKind::Fixed;
            *options.grep_pattern_kind_explicit_mut() = true;
        }
        "--basic-regexp" => {
            *options.grep_pattern_kind_mut() = crate::grep_source::PatternKind::Basic;
            *options.grep_pattern_kind_explicit_mut() = true;
        }
        "-E" | "--extended-regexp" => {
            *options.grep_pattern_kind_mut() = crate::grep_source::PatternKind::Extended;
            *options.grep_pattern_kind_explicit_mut() = true;
        }
        "-P" | "--perl-regexp" => {
            *options.grep_pattern_kind_mut() = crate::grep_source::PatternKind::Perl;
            *options.grep_pattern_kind_explicit_mut() = true;
        }
        _ => return Ok(false),
    }
    Ok(true)
}
