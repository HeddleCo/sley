# Sley 0.12 migration

0.12 is a breaking release. Ref-name validation now matches Git exactly (#245).

## `FullName` is stricter

`FullName` now follows `git check-ref-format`. Names that 0.11 accepted are
rejected if they contain:

- `..`
- a component ending in `.lock`
- any of `~ ^ : ? * [ \`
- `@{`
- a lone `@`
- a trailing `.`

Validate untrusted input with `FullName` before you create or update refs, and
handle the new `Err` cases.

## Deletion takes `DeleteRefName`

Ref deletion now takes `sley::DeleteRefName`. It is validated by
`sley_refs::refname_is_safe`, Git's delete-time rule, rather than by
`check-ref-format`. As with `git update-ref -d`, this lets you remove a
malformed ref that Git would refuse to create (for example
`refs/heads/broken...ref`). Build one with `DeleteRefName::new(name)?`.

## `find_reference` rejects malformed names

`Repository::find_reference` now returns `Err` for a malformed name instead of
`Ok(None)`. `Ok(None)` still means a well-formed ref that does not exist.
