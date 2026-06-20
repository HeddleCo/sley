#!/usr/bin/env bash
#
# check-parity-floors.sh — gate upstream-parity results on a per-file PASS FLOOR.
#
# Reads the per-script summary CSV written by run-upstream-tests.sh
#   columns: script,command,result,ok,notok,total,plan_total
# and verifies that every tracked t-file kept AT LEAST its recorded `ok`
# (passing-assertion) count. A net GAIN passes; only a DROP below floor fails.
#
# Usage:
#   check-parity-floors.sh <summary.csv>
#
# Floors recorded 2026-06-10 against base b069dbe + git 2.54.0 (GIT_TEST_DEFAULT_HASH=sha1).
# Round-2 floors (sequencer/graph/bitmaps) measured 2026-06-11 at the
# integ/round2 tip (epic/sley-sequencer-c + epic/sley-graph-c +
# epic/sley-bitmaps-a merged onto baac87c).
# Round-3 floors (rebase-i/bitmaps-b/diff-formats) measured 2026-06-11 at the
# integ/round3 tip (epic/sley-rebase-i + epic/sley-bitmaps-b +
# epic/sley-diff-formats merged onto 04f10f8).
# Round-4a floors (fetch-pack/shallow + userdiff) measured 2026-06-12 at the
# integ/round4a tip (epic/sley-fetchpack + epic/sley-userdiff merged onto
# b1b606a).
# Round-4b floors (gitlink stage-1: submodule basics/status/diff) measured
# 2026-06-12 at the integ/round4b tip (epic/sley-gitlink-a merged onto
# e08520c). t0008-ignores.sh deadlocks on its last cell ('streaming support
# for --stdin', pre-existing check-ignore buffering bug, sley#44) until the
# harness timeout; the summary row still records ok=284 (counts parse from
# the captured TAP before the hang), so the floor is valid — but the run
# needs SLEY_TEST_TIMEOUT high enough (900s) to reach cell 284 first.
# Round-5c floors (hooks-execution epic + stdin-streaming class) measured
# 2026-06-13 at the integ/round5c tip (epic/sley-hooks +
# fix/stdin-streaming-class merged onto 5a81341, @ 2b5d2d6). Gains over the
# round-5c hooks-layer baseline (ffbdc08): t5407-post-rewrite-hook 3->10,
# t5571-pre-push-hook 6->11, t0008-ignores 284->306 (still deadlocks on its
# last cell, sley#44 — 306 cells record before the 900s timeout kill), plus the
# new t1400-update-ref.sh floored at 175. t1800-hook stays at 55 (already a
# full pass since the hooks-layer landing).
# format-patch wave (2026-06-14, parity/fmtpatch): t4014-format-patch added at
# 96 (was 38 — banked the To/Cc/extra-header, --from/format.from in-body,
# --signature/--signature-file, --signoff trailer-block, and --rfc clusters).
# Same change lifted t4202-log 55->56 (shared --from/identity-header path), so
# its floor is raised to 56. No diff/log floor regressed (t4013/t4015/t4018/
# t4052/t4034/t4045/t4047/t4027 all held; render.rs untouched).
# rm wave (2026-06-14, parity/rm): t3600-rm added at 50 (was 37 — banked the
# check_local_mod message clusters (staged-content/changes-staged/local-mod,
# batched + advice.rmhints-gated hints), the --cached safety check incl.
# intent-to-add, d/f & type-change cases (worktree path now a dir/symlink/
# missing → ENOENT/ENOTDIR skip + abort-before-index-write on non-empty dir),
# trailing-slash-on-file rejection, and the empty-string pathspec error).
# Neighbors held: t7508-status=86, t2020-checkout-detach=16, t2107=10, t3000=15.
# cover-letter wave (2026-06-14, parity/coverletter): t4014-format-patch 96->113
# (+17 — built the cover-letter renderer: --cover-letter/--no-cover-letter,
# format.coverletter incl. =auto, --commit-list-format/format.commitlistformat
# [shortlog/modern/log:<fmt>/bare-%], the 0000-cover-letter file, the synthetic
# From/Date header + *** SUBJECT/BLURB HERE *** template, the author-grouped
# shortlog + cumulative range diffstat in the body, --cover-from-description +
# branch.<name>.description + --description-file, and the cover→patch numbering
# interplay). Same change lifted t4052-stat-output 71->76 (its --cover-letter
# --stat cells now pass), so its floor is raised to 76. The cover-from-description
# / branch-description cells (#161-185) stay blocked by the pre-existing am -3 /
# --ignore-if-in-upstream chain (cells #3/4/6/7) that leaves rebuild-1 empty —
# they are NOT a cover-letter gap. No diff/log floor regressed (t4013/t4015/
# t4018/t4202/t4034/t4045/t4047/t4027 all held; render.rs untouched).
# header-encoding + threading wave (2026-06-14, parity/fmtencode):
# t4014-format-patch 113->142 (+29 — built the email header-encoding + message-
# threading paths in format_patch.rs). Encoding (15 cells, #106/107/109-118/121/
# 141/142): a git-faithful add_rfc2047 (Q-encoded =?UTF-8?q?...?= words folded at
# 76 cols, multibyte never split) for Subject (RFC2047_SUBJECT) and From display
# names (RFC2047_ADDRESS); RFC 822 quoting (needs_rfc822_quoting/add_rfc822_quoted)
# + strbuf_add_wrapped_text-equivalent folding of long ASCII/quoted From names;
# multi-line subject collapse (format_subject); the 8-bit Content-Transfer-Encoding
# block (non-ASCII body / in-body From / --signoff committer-ident). Threading
# (14 cells, #55-67/78): Message-ID generation (<oid>.<ts>.git.<email>), the
# In-Reply-To/References chain, --thread[=shallow|deep]/--no-thread/format.thread,
# --in-reply-to=<msgid> (clean_message_id), replaying git's per-mail
# shallow-vs-deep ref state machine (build_thread_plan). All work is contained in
# crates/sley-cli/src/commands/format_patch.rs. The remaining #161-185 cover-from-
# description / #197-208 --base / #214-220 interdiff / #191-196 outputDirectory /
# #49-53 reroll cells are pre-existing, out-of-scope backlog; #16/20/21/23/24 are
# upstream `# TODO known breakage` (rfc822/rfc2047 To/Cc wrapping). No diff/log
# floor regressed (t4013=133/t4015=55/t4018=287/t4052=76/t4202=56/t4034=64/
# t4045=29/t4047=41/t4027=18 all held; render.rs/diff paths untouched).
# fsck wave (2026-06-14, parity/fsck): t1450-fsck added at 67 (was 22 — built
# the object-content checker in sley-fsck/content.rs, mirroring git's fsck.c
# commit/tree/tag buffer validation: fsck_ident, verify_headers, the tree
# name/mode/dup checks [incl. the verify_ordered d/f-conflict candidate stack],
# tag header structure, and the fsck.<msgid> severity table + config overrides
# [--strict promotion, fsck.largePathname=<sev>:<len>]). Also: the stdout/stderr
# split [content findings→stderr, broken-link/missing/dangling→stdout], git's
# exit-code bitmask [ERROR_OBJECT/REACHABLE/REFS], --tags root scoping,
# explicit-oid roots [no fallback-to-all-heads], NTFS '.git\' detection, and the
# loose-object 'unable to parse type from header <hdr>' message. Neighbors held:
# t1006-cat-file=290, t1007-hash-object=40. Remaining 29 fails are feature-gap
# sprawl (ref/HEAD validation, --connectivity-only/--cache/--name-objects,
# pack-object fsck, gitattributes blob checks, rev-list --verify-objects,
# worktree-index fsck), not content-checker gaps — see the ranked backlog.
# reset wave (2026-06-14, parity/reset): t7102-reset added at 35 (was 13 — banked
# six clusters in cmd_reset/sley-worktree/sley-index: (a) parse-options errors —
# `--no-{soft,mixed,hard,merge,keep}`/`--other`/`-o` now emit `error: unknown
# option ...` + usage, exit 129; (b) ORIG_HEAD written on every whole-tree reset
# (soft/hard/mixed, incl. bare `git reset`), cascade-unblocked cells 15-23; (c)
# `--soft` refused mid-merge ("Cannot do a soft reset in the middle of a merge.",
# exit 128) on MERGE_HEAD or any unmerged index entry; (d) `reset HEAD <path>`
# disambiguation fixed by not swallowing the `HEAD` positional; (e) `-N`/
# --intent-to-add re-records un-staged adds as ITA (write_tree_from_index now skips
# CE_INTENT_TO_ADD, matching cache_tree_update); (f) `--mixed` preserves the
# skip-worktree bit (reset_index_to_commit carries it forward) so skip-worktree
# paths are omitted from the post-reset summary; plus the "HEAD is now at" subject
# is re-encoded commit→logOutputEncoding (cells 7/8). Backlog (out of reset scope):
# cell 14 (checkout -m dep), 23 (pull FF ORIG_HEAD), 28 (--no-refresh needs
# diff-files to honor the stat-cache dirty flag). Neighbors held: t7508-status=86,
# t2020-checkout-detach=16, t7060-wtstatus=7. No workspace test regressed (2171/0).
# reset-refresh wave (2026-06-14, parity/reset-refresh): t7102-reset 36 -> 37 (cell
# 28 "--mixed --[no-]refresh sets refresh behavior") via two coupled pieces:
# (1) `git diff-files` now selects changed paths by the cached *stat* (git's
# ce_match_stat), not by content — it does NOT refresh the index, so a stat-dirty
# entry with unchanged content (a touched file, or a freshly `rm --cached`-then-
# `reset --no-refresh` entry with a zeroed cached stat) is reported `M` in
# raw/name-status/name-only and sets --quiet/--exit-code rc=1, while patch/stat
# (content-based) render an empty hunk. A dedicated diff-files engine layers this
# over the content diff; porcelain `git diff` (which refreshes first) keeps the
# plain content engine. A racily-clean entry is re-hashed (git's ce_compare_data)
# so a touched-then-re-`add`ed file stays clean. (2) `reset --[no-]refresh` threaded
# through cmd_reset: a whole-tree --mixed reset refreshes the stat-cache by default
# (clearing the stat-dirty state for unchanged content), --no-refresh leaves it
# dirty. Supporting fixes: pathspec reset preserves the existing index entry+stat
# for unchanged paths (t7102 cell 26 "resetting an unmodified path is a no-op"); and
# `git add` now re-stats the paths it touches (refresh_index_after_add) so a
# touched-but-content-unchanged tracked file is stamped clean (t2200 cells 14/15),
# while --chmod skips the refresh to preserve its explicit index mode. Remaining
# t7102 fail: cell 14 (checkout -m dep). Neighbors held (measured == floor, none
# regressed vs main): t7508-status=86, t2020-checkout-detach=16, t7506-status-
# submodule=28, t4013-diff-various=133, t4015-diff-whitespace=55, t4018-diff-
# funcname=287, t4034-diff-words=64, t4045-diff-relative=29, t4047-diff-dirstat=41,
# t4052-stat-output=76, t4027-diff-submodule=18, t2107-update-index-basic=10,
# t3000-ls-files-others=15; unfloored held: t2200-add-update=17, t3903-stash=22,
# t4006-diff-mode=7, t4011-diff-symlink=0, t7060-wtstatus=7, t7063-status-untracked-
# cache=14. No workspace test regressed (2171/0).
# status submodule-summary wave (2026-06-14, parity/statussub): t7508-status
# 86->113 (+27 — built the `Submodule changes to be committed:` (HEAD↔index) and
# `Submodules changed but not updated:` (index↔worktree) long-status sections,
# gated on status.submodulesummary, in sley-cli workspace.rs cmd_status. Each
# changed gitlink renders `* <path> <old7>...<new7> (N):` + `  > subj` / `  < subj`
# lines via a date-priority first-parent symmetric-difference walk of the
# submodule's own ODB (git submodule summary --cached/--files --for-status
# format), with the 0000000 add/remove single-tip forms. Honours
# --ignore-submodules[=<when>], submodule.<name>.ignore (.git/config over
# .gitmodules), and diff.ignoreSubmodules at the resolved precedence, applied to
# the worktree-side detail; CLI =all also drops the staged gitlink line + the
# whole summary, per-submodule =all only drops the summary. Side gains in the
# cluster: commit-dry-run divergence-advice suppression (#67) and commit -u<mode>
# threading to the dry-run preview (#108). Residual #70 (commit --amend --dry-run)
# needs the staged section diffed against HEAD^ — a separate commit-amend-preview
# feature, NOT a summary gap; the summary itself is HEAD^-correct. Neighbors held:
# t7506-status-submodule=28, t2020-checkout-detach=16, t7400-submodule-basic=70.
# No workspace test regressed (all crates 0 failed).
# compare-release-timings perf wave (2026-06-14, codex/compare-release-timings,
# PR #98): banks the perf-branch parity GAINs into the floors — t4202-log 56->57,
# t5310-pack-bitmaps 218->221, t5326-multi-pack-bitmaps 336->342,
# t5327-multi-pack-bitmaps-rev 308->314 (the perf commits' rev-list/bitmap fast
# paths produced these and they reproduce at the branch tip). Same wave fixes a
# t0008-ignores regression introduced by the status-ignore perf commits (57e8c5e/
# 20175d0/9bcb5cc): `matches_directory` over-matched — a NEGATED directory-only
# pattern (`!data/**/`) wrongly matched a *file* via an ancestor directory and
# un-ignored it, dropping cell #388 ("directories and ** matches", `data/**` +
# `!data/**/` must keep `data/data1/file1` ignored) and t0008 305<306. Fix gates
# the anchored-glob ancestor-prefix file match on `!self.negated` in
# crates/sley-worktree/src/lib.rs (git: a negated dir-only pattern re-includes
# *directories* but never the files inside them — `!dir/` still needs `!dir/*` to
# reach files), restoring t0008 to 306 with its failing-cell set IDENTICAL to
# main and the 57e8c5e directory-glob gain preserved. t0008 floor stays 306.
# am 3-way wave (2026-06-14, parity/am-3way): t4150-am 41->54 (+13) via two
# coupled pieces. (1) The shared patch-apply engine (sley-diff-merge
# apply_file_patch/find_hunk_pos/preimage_matches_at) now ports git's apply.c
# matching for the default no-whitespace-fuzz path: a hunk anchored at the file
# start (old_start<=1) must match the beginning, a hunk with no trailing context
# must match the END of the file, and the FULL preimage (context+deletes) is
# matched byte-exact at a position found by anchoring at newpos-1 and pinging
# outward over the whole image — replacing the old lenient MAX_HUNK_OFFSET=1000
# context-only search that applied at the first spurious offset. git keeps
# p_context=UINT_MAX by default so there is NO context fuzz and NO begin/end
# relaxation: a hunk whose full preimage does not match at a valid position is
# REJECTED. That rejection is what makes `git am -3` correctly fall back to its
# 3-way merge path (cells 34/35/36/37 + the 42/43/44/45/46/48/50/51 conflict-
# pause / --show-current-patch chain). (2) `am --resolved` now refuses with git's
# "No changes - did you forget to use 'git add'?" (index == HEAD) and "You still
# have unmerged paths" (unmerged index) preconditions (cells 52/53). Cell 70
# (`am -3 works with rerere`) stays blocked on a SEPARATE gap — am does not wire
# the rerere record/replay into its 3-way conflict path — NOT an apply-engine
# gap. Shared-engine neighbors verified measured==floor, NONE regressed (control
# diff vs main d25cd83 IDENTICAL): t3501-revert-cherry-pick=21, t3507-cherry-
# pick-conflict=44, t3510-cherry-pick-sequence=52, t3403-rebase-skip=16,
# t4014-format-patch=142, t4013-diff-various=133; unfloored held: t3400-rebase=17,
# t3401-rebase-and-am-rename=5, t4012-diff-binary=4, t4105-apply-fuzz=5; bonus:
# t4104-apply-boundary 2->3 (the stricter boundary matching is more git-faithful).
# No workspace test regressed (2203/0).
# fsck-residual wave (2026-06-14, parity/fsck): t1450-fsck 67->82 (+15) across
# six tractable sub-clusters. (A) loose-object integrity in sley-odb
# verify_object: trailing bytes after the zlib stream now report `garbage at end
# of loose object '<oid>'` + `unable to unpack contents of <path>` (cells 5/83/84),
# and a body inflated short of its header size reports `corrupt loose object`
# (cell 85); plus FileObjectDatabase::read_object prefers a packed copy over a
# corrupt loose one (no spurious re-inflate, cell 82). (B) ref-target validation in
# cmd_fsck (sley-cli plumbing): a branch ref pointing at a non-commit emits
# `error: <ref>: not a commit` + ERROR_REFS (cell 6); an unreadable ref tip emits
# `error: <ref>: invalid sha1 pointer <oid>` + ERROR_REACHABLE and is not walked;
# the broken-link line now uses git's `%7s` type padding; ERROR_REFS is no longer
# over-attributed to connectivity-walk broken links (cell 72 exit 10->2); a bogus
# explicit head exits 2 not 1 (cell 90). (C) gitattributes blob content check
# (sley-fsck content.rs check_gitattributes_blob + the deferred fsck_blobs pass):
# a .gitattributes-named tree blob is line-length/size checked (cells 93/94). (D)
# an empty tree-entry filename is now `error: empty filename in tree entry` +
# fatal `badTree` (cell 25). (E) index fsck (fsck_index_roots): missing index blobs
# report `missing blob <oid>` (annotated `(<index>:<name>)` under --name-objects,
# cells 95/96), the index trailing checksum is verified under a full/--cache fsck
# (`bad index file sha1 signature`, cell 91), and the cache-tree's tree oids are
# validated (`invalid sha1 pointer in cache-tree`, ERROR_REFS). Cross-command
# unblockers feeding the harness: reflog expire --expire=now routes through git's
# approxidate parser (cell 1, sley-cli lib.rs); rev-parse `<tag>^{blob}` peels a
# tag to a blob (cell 72 setup, sley-rev peel_to_blob); `git rm --cached` of a path
# whose blob was removed succeeds by re-hashing the worktree content vs the cached
# oid instead of reading the (gone) blob, while still honoring git's index-refresh
# semantics for a stat-dirty-but-content-clean path (cell 90 cleanup; sley-worktree
# check_local_mod). Residual 14 fails are larger separate clusters: HEAD/worktree
# ref consistency (`git refs verify`, cells 7-12), rev-list --verify-objects
# (cells 36/37, a rev-list feature), pack-object fsck (cells 80/81), alternate-odb
# loose scan (cell 78), --connectivity-only type-only mode (cell 75),
# --name-objects provenance through the object walk (cell 77), and update-index
# --cacheinfo in a bare repo (cell 4). Neighbors held (measured==floor, none
# regressed): t1006-cat-file=290, t5310-pack-bitmaps=221, t1400-update-ref=175,
# t1461-refs-list=358, t6300-for-each-ref=358, t3600-rm=50, t7508-status=113,
# t7102-reset=37, t1007-hash-object=40, t1500-rev-parse=81, t1462-refs-exists=12,
# t1401-symbolic-ref=25, t7004-tag=159, t3200-branch=117, t3301-notes=144,
# t1300-config=497, t2400-worktree-add=165. No workspace test regressed (2203/0).
# update-ref --stdin lexer wave (2026-06-15, wave/update-ref-stdin):
# t1400-update-ref 175->232 (+57 — replaced the split_whitespace tokenizer with
# a git-faithful stateful RefCommandStream lexer in
# crates/sley-cli/src/commands/ref_command_stream.rs: C-quoted args with
# backslash/octal escapes, the four distinct die-messages (empty command in
# input / whitespace before command / badly quoted argument / unexpected
# character after quoted argument), per-cmd `<cmd> <ref>: extra input:`/`missing
# <ref>`/`missing <new-oid>`/`invalid <new|old-oid>`/`unexpected end of input`
# messages, and faithful empty-value (zero vs unspecified) semantics for the \n
# vs -z paths. The split_whitespace bug also silently created refs from
# malformed lines, which cascade-broke ~40 downstream tests via stale refs — the
# lexer's strict rejection recovers those too). Neighbors held (measured==floor):
# t1461-refs-list=358, t6300-for-each-ref=358, t1401-symbolic-ref=25,
# t1462-refs-exists=12. Residual: 3 cells (-z "too many arguments" #136/139/140)
# need git's batch-commit-at-end transaction model (sley writes eagerly, so a
# prior command's lock failure fires before the trailing record is parsed as an
# unknown command) — out of scope for the tokenizer; not a lexer gap. No
# workspace test regressed (2215/0).
#
# Bisection lift (2026-06-15): lifted the weighted-midpoint bisection engine out
# of the private sley-cli bisect command into a shared sley_rev::bisect primitive
# and wired rev-list --bisect/--bisect-vars/--bisect-all on top of it. NEW floor
# t6002-rev-list-bisect=52 (was 0; the one remaining fail is rev-parse ^rev arg
# support, out of scope). Shared-engine neighbors held (measured==floor, none
# regressed): t6030-bisect-porcelain=95, t6003-rev-list-topo-order=36,
# t6012-rev-list-simplify=9. No workspace test regressed (2206/0).
#
# rev-parse ^rev (2026-06-15): rev-parse now accepts excluded `^rev` positional
# args (resolve the remainder, prefix the rendered output with `^`), closing the
# final t6002 cell. Floor t6002-rev-list-bisect 52->53 (now full pass).
# t1500-rev-parse held at 81 (already full pass, no gain).
# core.safecrlf emitter wave (2026-06-15, wave/safecrlf-emitter): t0027-auto-crlf
# 2281->2354 (+73). sley's clean pipeline converted CRLF<->LF silently; it now
# emits git convert.c's round-trip warnings ("CRLF will be replaced by LF" /
# "LF will be replaced by CRLF") and dies under core.safecrlf=true. New
# ConvFlags::from_config (default WARN, matching git's global_conv_flags_eol),
# ContentFilterPlan::check_safe_crlf_stats (git's crlf_to_git round-trip
# simulation: simulate clean CRLF->LF then checkout LF->CRLF, warn if a line
# ending would not survive), and has_crlf_in_index for the auto-crlf decision,
# threaded through both index-update clean sites (update_index_paths_impl +
# add_update_tracked_path) in crates/sley-worktree/src/lib.rs. PURE additive
# stderr emitter: byte output to the object store is unchanged. t3920-crlf-
# messages newly floored at 9 (unaffected by this change — it exercises ref-
# filter/pretty CRLF message rendering, a different subsystem — floored+enrolled
# per the wave brief). Neighbors held (measured==floor): t0008-ignores=306,
# t0001-init=102. No workspace test regressed (2213/0).
#
# whitespace-engine wave (2026-06-15, wave/whitespace-engine): ported git's
# ws.c into a new crate sley-diff-merge/src/ws.rs (parse_whitespace_rule /
# ws_check / ws_check_emit / ws_fix_copy + the blank-at-EOF helpers) and wired
# it into THREE call sites — `git diff/diff-index/diff-tree --check`, the
# `--ws-error-highlight` (default new-only) paint in the patch render path, and
# `git apply --whitespace=warn|error|error-all|nowarn|fix|strip`. Floors:
# t4015-diff-whitespace 55->78 (+23 — the diff --check cells across
# diff/diff-index/diff-tree plus the conflict-rule die and the ws-highlight
# cells); t4124-apply-ws-rule added at 67 (NEW — the apply --whitespace= matrix:
# warn/error/error-all/nowarn report paths + the fix/strip indent+trailing-ws
# correction, including git's global-whitespace-error-flag semantics that
# re-indents clean-on-their-own lines when a sibling line is dirty); and
# t4019-diff-wserror added at 19 (NEW — `git diff --color` whitespace-error
# highlighting, 90% of the file). Residual t4124 fails are the blank-at-EOF
# fix-mode edge cases + autocrlf + incomplete-line clusters (deeper apply-engine
# work); residual t4019 are the new-trailing-blank-line + context-CR paint
# cases. Shared-engine neighbors held (measured==floor, NONE regressed):
# t4013-diff-various=133, t4014-format-patch=142, t4018-diff-funcname=287. No
# workspace test regressed (2218/0). Perf NEUTRAL: plain `diff` REF 3.24s vs
# NEW 3.20s (interleaved n=8 — the ws resolver only builds when color is on);
# `apply --whitespace=nowarn` REF 0.023s vs NEW 0.022s (the ws pass is skipped
# entirely for nowarn). Raise a floor only after a real, sustained gain lands;
# never lower one.
# Raise a floor only after a real, sustained gain lands; never lower one.
#
# 2026-06-15 (wave/rebase-noff-reflog): `reflog show` now walks every reflog
# entry in order (no reachability filter, no OID dedup) — matching upstream's
# real, monotonic-reflog behavior — fixing the `rebase --no-ff`/`--force-rebase`
# "do-the-work-vs-noop" reflog-growth assertions. t3432-rebase-fast-forward
# 144->219 (full PASS bar the 6 known-breakage fork-point cells); side gain
# t3404-rebase-interactive 36->39. Neighbors held (measured==floor): t3403=16,
# t3418=11, t3420=30, t3406=9, t1400=175, t1500=81, t2020=16, t7102=37. No
# workspace test regressed (2206/0).
#
# stash-on-merge wave (2026-06-15, wave/stash-on-merge): stash apply/pop rebuilt
# on the 3-way merge engine (three_way_merge_trees over the stash base / current
# index / stash working tree, mirroring git's merge_ort_nonrecursive in
# builtin/stash.c) — apply no longer refuses dirty trees, content-conflicts
# render real stage 1/2/3 + markers, and unstage_changes_unless_new / the
# --index has_index path match git. Plus the bare-`n` stash selector and the
# assumed-`push` dispatch guard. t3903-stash NEW floor @52 (was unfloored at 22,
# enrolled in upstream-parity.yml). Side gain via autostash-on-merge:
# t3420-rebase-autostash 30->33. Neighbors held (measured==floor): t7102-reset=37,
# t7508-status=113, t3600-rm=50, t3418-rebase-continue=11. Perf (clean-tree
# apply+pop, both binaries do real work): 0.073s branch vs 0.120s REF (faster —
# the merge engine touches only changed paths vs the old full reset-to-commit).
# No workspace test regressed (2241/0).
# smudge attr-precedence wave (2026-06-15, wave/smudge-precedence): the checkout
# smudge filter resolved .gitattributes from the index ONLY, but git's default
# attr direction (GIT_ATTR_CHECKIN, used by `checkout -- <pathspec>` / `restore`
# and never overridden in builtin/checkout.c) reads each .gitattributes frame
# from the WORKTREE FILE first, falling back to the staged blob only when no
# worktree file exists (sparse). t0027 overwrites the worktree .gitattributes
# without re-staging, so index-only resolution made checkout UNDER-convert line
# endings — naked LFs that should gain CRLF stayed bare across ~150 checkout +
# 74 ls-files cells. smudge_attribute_checks_from_index now walks the path
# ancestry worktree-file-first (index fallback per frame), matching attr.c
# read_attr. Also wired the smudge filter into read-tree -u / --reset -u /
# --prefix materialization (write_blob_to_worktree previously wrote blobs raw),
# with .gitattributes-first write ordering so a freshly-checked-out attributes
# file governs its siblings in the same batch. Floor t0027-auto-crlf 2354->2578
# (+224). NEW floors (sustained gains, NONE regressed): t0020-crlf added at 27
# (REF 25, +2) and t0021-conversion added at 21 (REF 17, +4) — both convert-area
# neighbors lifted by the same fix. Residual t0027 (22) = the NNO clean/commit
# direction (separate engine). Shared-engine neighbors held (measured==floor):
# t0008-ignores=306, t0001-init=102, t4014-format-patch=142, t7508-status=113;
# convert family held: t0022=1, t0023=2, t0024=2, t0025=1, t0028=1, t1007=40,
# t5004=8. t0026-eol-config stays UNFLOORED at 3/6 — its 3 fails now need diff/
# status worktree clean-normalization (diff shows raw CRLF instead of cleaning
# CRLF->LF before comparing), a distinct diff-engine gap exposed (not caused) by
# the now-correct read-tree -u materialization. No workspace test regressed
# (2235/0). Perf NEUTRAL-or-better on the affected materialization path:
# `checkout -- .` on a CRLF-heavy 1000-file tree, interleaved n=8, REF 1.1475s
# vs BRANCH 1.0662s (worktree-first ancestry read is cheaper than the old
# full-index attribute scan); `read-tree --reset -u` now matches checkout's cost
# (~1.1s) because it performs the EOL conversion git also does (REF skipped it).

set -euo pipefail

summary=${1:?usage: check-parity-floors.sh <summary.csv>}

if [ ! -f "$summary" ]; then
    echo "FAIL: summary CSV not found: $summary" >&2
    echo "      (did run-upstream-tests.sh run? it writes this file)" >&2
    exit 1
fi

# script -> floor (minimum acceptable ok-assertion count).
declare -A FLOOR=(
    [t0001-init.sh]=102
    [t1006-cat-file.sh]=290
    [t1007-hash-object.sh]=40
    [t1300-config.sh]=497
    # 271->275 RESTORED 2026-06-20 (wave-23 fixes): the wave-4 (eb1f4dfd) floor of 275
    # was silently regressed to 271 by a later reftable-migration commit; the badrefname
    # fix-slice restores the 4 cells. CI-reproducible (local plumbing, no transport).
    [t1400-update-ref.sh]=275
    [t1401-symbolic-ref.sh]=25
    # t1430 + t7450 NEW 2026-06-20 (wave-23): bad-ref-name validation + bad-git-dotfiles
    # security hardening. Enrolled in SLEY_TESTS below.
    [t1430-bad-ref-name.sh]=40
    [t1450-fsck.sh]=96
    [t7450-bad-git-dotfiles.sh]=45
    # codex-wave-10 (refs optimize / pack-refs): native pack-refs (selected loose
    # packing w/ include/exclude + default tag behavior), prune of packed loose
    # refs, packed-refs parse diagnostics, --auto heuristic, lock retry, packed
    # no-op update, verified tag peeling. t1463 11->43. Ref-store consumers held:
    # t1400=275 t6300=410 t7004=228 t3200=134 t1450=96 t5505=93 t1410=20 t5601=72.
    [t1463-refs-optimize.sh]=45
    # codex-wave-10 (reflog expire/delete): native expire (--expire/--expire-unreachable/
    # --all/--updateref/--rewrite/--stale-fix), delete w/ entry-rewrite, exists, gc/prune
    # reachability, worktree-local reflogs, branch/tag reflog cleanup. t1410 20->41 FULL;
    # side gain t1463 43->45. Auto-merge w/ wave-10 (branch.rs/log.rs/pack.rs) held:
    # t6040=44 t4205=110 t7700=29 t3404=80.
    [t1500-rev-parse.sh]=81
    [t2400-worktree-add.sh]=214
    # codex-wave-11 (worktree repair): re-link .git file + worktrees/<id>/gitdir
    # back-pointer after a move, broken-link detection + repair messages, repair
    # both-moved + specific-path. t2406 7->24 FULL; side gain t2403 25->27.
    [t2406-worktree-repair.sh]=24
    # codex-wave-11 (checkout DWIM + checkout-index --temp): remote-tracking DWIM
    # auto-create local branch + track, --no-guess; checkout-index --temp/--all/-z
    # tempname mapping + --prefix/--stage. t2024 ->21, t2004 ->23 FULL. (t2501
    # cwd-removed unchanged @3.) worktree/branch floors held t2400=214 t3200=134
    # t6040=44 t7508=114.
    [t2024-checkout-dwim.sh]=21
    [t2004-checkout-cache-temp.sh]=23
    [t3070-wildmatch.sh]=1861
    [t6300-for-each-ref.sh]=410
    # codex-wave-10 (branch tracking-info): branch -vv ahead/behind+gone column,
    # status -sb branch header, upstream:track/trackshort atoms, @{u} resolution,
    # left-right ahead/behind count. t6040 9->44 FULL. Auto-merged w/ remote verbs:
    # t5505=126 t3200=134 held.
    [t6040-tracking-info.sh]=44
    # codex-wave-4 (2026-06-17): for-each-ref atoms (sley-ref-filter) t6302 enroll@17.
    [t6302-for-each-ref-filter.sh]=55
    # codex-wave-4: merge-tree --write-tree t4301 enroll@18; blame siblings t8001 99->110/t8002 117->128/t8012 98->109; sparse t1091 40->45.
    [t4301-merge-tree-write-tree.sh]=18
    # codex-wave-2 (2026-06-17): tag annotated-edit/TAG_EDITMSG/reflog/column 176->189 (stable 3x).
    [t7004-tag.sh]=228
    [t3200-branch.sh]=145
    [t0027-auto-crlf.sh]=2578
    # t0020-crlf: was FLAKY 27/28. FIXED 2026-06-17 (codex-wave-3, 4 rounds): sorted worktree
    # readdir + checkout-- stat-refresh SCOPED to the call site (R4: reset's tree-sourced entries
    # stay zero-stat so `reset --mixed --no-refresh` is unaffected). Now DETERMINISTIC at 29
    # (stable 8x+). The flake was diff-files comparing raw vs clean-filtered bytes + stale cached stat.
    [t0020-crlf.sh]=29
    [t0021-conversion.sh]=21
    [t3920-crlf-messages.sh]=9
    # codex-wave-5 (2026-06-17): lib new commands — repo-info t1900 full@38 / replay t3650@43.
    [t1900-repo-info.sh]=38
    [t2107-update-index-basic.sh]=10
    [t7810-grep.sh]=249
    # codex-wave-4-recovery: notes merge t3309 enroll@31 / t3311 enroll@24 (full pass).
    [t3309-notes-merge-auto-resolve.sh]=31
    [t3311-notes-merge-fanout.sh]=24
    [t3301-notes.sh]=144
    [t1461-refs-list.sh]=410
    [t1462-refs-exists.sh]=12
    [t1510-repo-setup.sh]=109
    # codex-wave-3: merge --no-edit rename-cleanup also fixed rename-dir merges 23->27 (stable 3x).
    # codex-wave-9 (dir-rename engine): dirs_removed parity (recreated old dirs block
    # false dir-renames), rename/rename(1to2) split higher-stages, transitive dest
    # remapping for rename/delete. t6423 34->41 (raw 41/82, 2 known breakages).
    # Side gains banked: t6402 34->35, t6422 NEW@6.
    [t6423-merge-rename-directories.sh]=55
    [t6422-merge-rename-corner-cases.sh]=6
    [t3501-revert-cherry-pick.sh]=21
    [t3502-cherry-pick-merge.sh]=12
    [t3505-cherry-pick-empty.sh]=17
    [t3507-cherry-pick-conflict.sh]=44
    [t3510-cherry-pick-sequence.sh]=52
    [t4214-log-graph-octopus.sh]=17
    [t4215-log-skewed-merges.sh]=9
    [t6002-rev-list-bisect.sh]=53
    [t6030-bisect-porcelain.sh]=95
    [t5310-pack-bitmaps.sh]=221
    [t5326-multi-pack-bitmaps.sh]=344
    # codex-wave-4-recovery: rev_list filters t6006 enroll@56 / t6112 enroll@48.
    [t6006-rev-list-format.sh]=56
    [t6112-rev-list-filters-objects.sh]=48
    [t6113-rev-list-bitmap-filters.sh]=13
    [t1800-hook.sh]=55
    [t2020-checkout-detach.sh]=16
    [t6003-rev-list-topo-order.sh]=36
    [t6012-rev-list-simplify.sh]=36
    [t4205-log-pretty-formats.sh]=108
    [t4216-log-bloom.sh]=161
    [t5318-commit-graph.sh]=95
    [t3432-rebase-fast-forward.sh]=219
    [t3600-rm.sh]=69
    # codex-wave-2 (2026-06-17): log --graph/--source/--end-of-options/follow-pathspec 80->96 (stable 3x).
    [t4202-log.sh]=124
    # codex-wave-3 (2026-06-17): shortlog --group/trailer/-w/-cnse 6->21 (stable 3x); read-tree
    # confusing-path rejection (.git/HFS/NTFS/backslash/NUL) 4->28 FULL PASS (safe trees still load).
    [t4201-shortlog.sh]=21
    [t1014-read-tree-confusing.sh]=28
    [t3000-ls-files-others.sh]=15
    [t3103-ls-tree-misc.sh]=10
    # codex-wave-9 (ls-tree output): gitlink mode 160000 classified as commit
    # object type; -d shows gitlinks (skip only blobs); subdir ../ pathspec norm
    # + above-root --full-tree rejection. t3105-ls-tree-output 13->60 FULL PASS.
    # Gitlink blast-radius held: t1006=290 t4027/t4060/t4041 submodule-diff,
    # t7400=88 t7508=114 unchanged.
    [t3105-ls-tree-output.sh]=60
    # codex-wave-6: rebase porcelain t3400@18 / incompatible-options t3422@52.
    [t3400-rebase.sh]=19
    [t3422-rebase-incompatible-options.sh]=52
    [t3403-rebase-skip.sh]=16
    # codex-wave-8 (rebase-i r2): squash/fixup conflict-resume cleanup, partial
    # pathspec staging before pre-commit, post-commit on replay/start, rebase-vs-
    # cherry-pick error precedence. t3404 63->80. Neighbors held: t3400=19,
    # t3403=16, t3406=32, t3420=40; sequencer t3501/t3510/t3502 held.
    [t3404-rebase-interactive.sh]=94
    [t3406-rebase-message.sh]=32
    [t3418-rebase-continue.sh]=12
    [t3420-rebase-autostash.sh]=40
    [t5327-multi-pack-bitmaps-rev.sh]=314
    [t5332-multi-pack-reuse.sh]=9
    [t4013-diff-various.sh]=205
    # codex-wave-3 (2026-06-17): format-patch --notes/format.notes, --output/format.outputDirectory, --numstat 154->164.
    [t4014-format-patch.sh]=202
    # codex-wave-3 (2026-06-17): am --empty=stop/drop/keep + --allow-empty resume + -3 -q quiet 54->56.
    [t4150-am.sh]=84
    # codex-wave-6 (2026-06-17): diff function-context t4051@32 / submodule-format t4060@7; t4015 101->102.
    # wave-2 submodule (2026-06-18, integ/submodule): t4060 7->15 (diff porcelain options).
    [t4051-diff-function-context.sh]=32
    # codex-wave-10 (diff --submodule formats): short/log/diff + diff.submodule
    # default + dirty-suffix + (rewind)/(not present) annotations. FULL PASS both:
    # t4060 15->51, t4041 14->47. log/show/stash blast-radius held (t4205=110
    # t4202=101 t3903=134 t4013=191 t4014=202 t7508=114).
    [t4060-diff-submodule-option-diff-format.sh]=51
    [t4041-diff-submodule-option.sh]=47
    [t4052-stat-output.sh]=80
    [t4045-diff-relative.sh]=30
    [t4047-diff-dirstat.sh]=41
    [t4015-diff-whitespace.sh]=114
    [t4018-diff-funcname.sh]=287
    [t4124-apply-ws-rule.sh]=67
    [t4019-diff-wserror.sh]=19
    [t4034-diff-words.sh]=64
    [t5407-post-rewrite-hook.sh]=17
    [t5500-fetch-pack.sh]=359
    [t5571-pre-push-hook.sh]=11
    [t5537-fetch-shallow.sh]=12
    [t0008-ignores.sh]=398
    # wave-2 submodule (2026-06-18, integ/submodule): t7400 87->88.
    # codex-wave-11 (submodule verbs): add/init/status/sync/deinit/update/foreach/
    # set-url/set-branch, relative-URL resolution, .gitmodules+config writes. t7400
    # 88->113; side gain t7406 54->57. submodule-diff/ls-tree floors held t4060=51
    # t4041=47 t4027=18 t3105=60 t7508=114.
    [t7400-submodule-basic.sh]=113
    [t7506-status-submodule.sh]=34
    [t7508-status.sh]=119
    [t4027-diff-submodule.sh]=18
    [t7102-reset.sh]=37
    # blame scoreboard wave (blame.c pass_blame/blame_chunk port + annotate-compat
    # output + -L /regex/ ranges + -b/--first-parent/^rev/abbrev parity): NEW floors.
    # t8002 54->117, t8001 44->99, t8012 44->98. Residual: :funcname ranges,
    # --contents working-tree overlay, --progress, --color-lines/--color-by-age.
    [t8002-blame.sh]=128
    [t8001-annotate.sh]=110
    [t8012-blame-colors.sh]=109
    # t3903-stash FLAKY: cell #46 "stash symlink to file (stage rm)" oscillates 82/83
    # (symlink<->file type-change race, independent of any wave — flips on a pristine
    # origin/main binary). Floor lowered 83->82 (safe lower bound) — banking 83 from a
    # gitlink-rm-wave flaky read silently reddened the gate. Same class as t0020 27/28.
    [t3903-stash.sh]=133
    [t4209-log-pickaxe.sh]=45
    # codex-wave-3 (2026-06-17): merge --no-edit/--edit accepted + merge cleans up renamed-away source;
    # unmasks the line-log merge+rename cells #61-64 (no crash on -G/-S/--find-object). 69->70.
    [t4211-line-log.sh]=70
    [t5300-pack-object.sh]=46
    [t5302-pack-index.sh]=31
    [t5303-pack-corruption-resilience.sh]=21
    [t5304-prune.sh]=13
    [t5319-multi-pack-index.sh]=77
    [t5324-split-commit-graph.sh]=25
    [t5329-pack-objects-cruft.sh]=19
    [t5504-fetch-receive-strict.sh]=7
    # codex-wave-10 (remote verbs): add (config forms/mirror/tags/fetch-on-add),
    # rename (config rewrite + tracking-ref moves + nested refs), remove/prune,
    # set-url/get-url/set-branches/set-head, show -n report formatting, update
    # group. t5505 93->126. branch.rs blast-radius held: t3200=134 t6040=9.
    [t5505-remote.sh]=126
    # codex-wave-11 (protocol allowlist): ONE scheme-gating chokepoint (file/local/
    # git/ssh/ext/<helper>:: classifier + protocol.<name>.allow + GIT_ALLOW_PROTOCOL
    # + GIT_PROTOCOL_FROM_USER demotion + fatal-not-allowed errors) closes all FOUR
    # proto suites FULL: t5810 28->54, t5811 10->26, t5813 33->81, t5814 11->27.
    # Wide transport blast-radius held (15 files touched): t5601=72 t5516=72 t5520=38
    # t5505=126 t5528=31 t5500=359 t5510=7 t6040=44.
    [t5810-proto-disable-local.sh]=54
    [t5811-proto-disable-git.sh]=26
    [t5813-proto-disable-ssh.sh]=81
    [t5814-proto-disable-ext.sh]=27
    [t5511-refspec.sh]=47
    [t5515-fetch-merge-logic.sh]=65
    # codex-wave-11 (push.default resolution): nothing/current/upstream/simple/
    # matching modes, triangular pushRemote/pushDefault, @{push}, no-upstream +
    # name-mismatch errors + --set-upstream hint, autoSetupRemote. t5528 NEW@31;
    # side gain t5516-fetch-push 63->72. remote/tracking floors held t5505=126
    # t5520=38 t6040=44 t3200=134.
    [t5528-push-default.sh]=31
    [t5516-fetch-push.sh]=92
    [t5520-pull.sh]=75
    [t5601-clone.sh]=86
    # codex-wave-11 (partial clone): --filter=blob:none/blob:limit/tree/sparse:oid,
    # remote.origin.promisor + partialclonefilter config, promisor-pack + lazy
    # object fetch-on-read, filter+depth. t5616 14->36. MERGE-RESOLUTION: clone's
    # FetchOptions{record_promisor_refs,refetch} fields back-filled into protoallow's
    # remote-add fetch constructor (E0063, both=false). proto suites stayed FULL
    # (t5810=54 t5813=81 t5814=27); object-read held t6000=11 t8002=128 t7600=83.
    [t5616-partial-clone.sh]=36
    # wave-10 transport (clone/remote config-write fix): t5611 full-pass enrolled; t5505 81->90;
    # t5601 60->62 measured but HELD at 60 (clone server-handshake is parallel-flake-prone, +2 too
    # small to risk a fresh flaky floor — the +2 cells still land on main, just not floor-locked).
    [t5611-clone-config.sh]=13
    [t5603-clone-dirname.sh]=39
    [t7502-commit-porcelain.sh]=77
    # codex-wave-1 (2026-06-17): config stop-at-non-option (+3) + commit SQUASH_MSG (+2),
    # disjoint files, combined t7600 44->49 (stable 49x3). describe enrolled at 84 (74->84).
    # codex-wave-3 (2026-06-17): merge --no-edit acceptance + rename cleanup 49->50.
    [t7600-merge.sh]=83
    [t6120-describe.sh]=84
    # wave-1 integration (2026-06-18, integ/wave1): codex/parity-maintenance lifted
    # t7900-maintenance 12->37 (cmd_maintenance gain in pack.rs). Stable 37x3 on the
    # integrated binary.
    # codex-wave-11 (maintenance task runner): prefetch, commit-graph auto, rerere-gc
    # auto, worktree-prune expiry/threshold, register/unregister, scheduler lock,
    # strategy/schedule ordering, post-commit auto-maintenance run path. t7900 37->64.
    # RE-DISPATCHED off post-worktree-repair main (the first auto-merge regressed
    # t2406 24->14); rebuilt clean — t2406 HELD at 24. Shared-file floors held:
    # t7700=29 t5300=46 t5324=11 t1450=96 t2400=214 t5505=126 t5516=72.
    [t7900-maintenance.sh]=64
    # wave-1 integration (2026-06-18, integ/wave1): NEW floors for the difftool epic
    # (codex/parity-difftool: difftool.rs/mergetool.rs/tool_launch.rs) + status-cache
    # (codex/parity-status-cache: workspace.rs/index.rs). All measured on the integrated
    # binary; small bumps t7610 and t7063 re-measured 3x (stable LOW value banked).
    # t7800-difftool 12->69 (+57), t7610-mergetool 1->5 (+4, stable 5x3), t7063-status-
    # untracked-cache 14->15 (+1, stable 15x3). codex/parity-grep raised t7810 230->235.
    # Neighbor watch-set measured base-2730844 vs integrated, ALL identical (zero
    # interaction regression): t4015=102, t4013=172, t7600=50, t5304=13, t6500-gc=14,
    # t7700-repack=17, t5319=77, t5324=11, t7508=114, t2107=10, t7008=5.
    [t7800-difftool.sh]=91
    [t7610-mergetool.sh]=5
    # codex-wave-10 (untracked-cache UNTR extension): native read/write, update-index
    # toggles, status create/remove/keep, -uall/-unormal bypass, exclude-OID hashing,
    # mutation invalidation, trace2 perf, ident-mismatch, UNTR-preserve across rewrites.
    # t7063 15->44. index/worktree blast-radius held: t7508=114 t2107=10 t2400=214
    # t1092=29 t7102=37.
    [t7063-status-untracked-cache.sh]=44
    [t1410-reflog.sh]=41
    [t1060-object-corruption.sh]=13
    [t2203-add-intent.sh]=11
    [t3650-replay-basics.sh]=43
    [t3701-add-interactive.sh]=105
    [t4011-diff-symlink.sh]=1
    # codex-wave-3: merge --no-edit rename-cleanup lifted merge-rename 30->34 (stable 3x).
    # codex-wave-9 dir-rename engine side-gain: 34->35.
    [t6402-merge-rename.sh]=35
    [t5400-send-pack.sh]=17
    [t5404-tracking-branches.sh]=6
    [t5543-atomic-push.sh]=11
    [t5548-push-porcelain.sh]=11
    [t6430-merge-recursive.sh]=23
    [t5702-protocol-v2.sh]=21
    [t7103-reset-bare.sh]=12
    [t7110-reset-merge.sh]=21
    [t7201-co.sh]=37
    # wave-8 engine-completion (2026-06-17): rebase-i sequencer (autosquash + fixup
    # -C/-c message machinery), update-ref --stdin ref-transaction hook + git-faithful
    # error surface, sparse-checkout builtin + the sparse-index collapse/expand format.
    # NEW floors locking the gains (t1400 also bumped 232->238 above):
    [t3415-rebase-autosquash.sh]=22
    [t3437-rebase-fixup-options.sh]=7
    [t1404-update-ref-errors.sh]=38
    [t1416-ref-transaction-hooks.sh]=7
    # codex-wave-9 (sparse-checkout engine): cone/non-cone + escaped-cone patterns,
    # sparse-index expansion for diff/status, skip-worktree missing-file suppression,
    # sparse-dir write-tree, read-tree sparse reapply hook, native reset -p path.
    # t1092 22->29; side gains t1091 45->50 (measured 51/53, slack closed), t1011 7->9.
    # Auto-merge of sley-diff-merge/lib.rs vs wave-9 dir-rename verified SAFE: merge
    # floors held (t6423=41 t6402=35 t6422=6 t7600=83 t6430=23).
    [t1091-sparse-checkout-builtin.sh]=50
    [t1092-sparse-checkout-compatibility.sh]=39
    # wave-9 engine-completion (2026-06-17): merge porcelain (octopus + --squash/--abort/
    # --continue/--quit state machine), submodule engine (relative_url primitive + summary/
    # foreach/update), mailmap canonicalization engine. Bumps applied above: t7600 38->44,
    # t7400 70->85, t6423 14->17, t4202 72->80, t2400 213->214, t5318 92->93, t7502 74->75,
    # t0020 27->28. NEW floors locking the gains:
    [t7602-merge-octopus-many.sh]=5
    [t7604-merge-custom-message.sh]=8
    [t7607-merge-state.sh]=1
    [t7611-merge-abort.sh]=19
    # wave-2 submodule (2026-06-18, integ/submodule): t7401 22->25 (submodule-summary porcelain).
    [t7401-submodule-summary.sh]=25
    [t7407-submodule-foreach.sh]=21
    [t7406-submodule-update.sh]=57
    [t4203-mailmap.sh]=69
    # wave-12 (2026-06-17): repack/gc engine (geometric + cruft repack + gc orchestration),
    # diff indent-heuristic, reftable log-block engine. Incidental pack-floor gains bumped above
    # (t5304 10->13, t5319 74->77, t5326 342->344, t5329 16->19). NEW floors locking the gains:
    # codex-wave-10 (repack engine): kept-pack retention, .keep/--keep-pack,
    # cruft retention around kept packs, server-info, bitmap/stale-bitmap, orphan
    # idx, cruft numeric config validation + error shape. t7700 17->29. Pack
    # floors held t5300=46 t5303=21 t5319=77 t1450=96. Plus wave-10 merge/pull
    # config (t7601 NEW@65 FULL: merge.ff/pull.ff/pull.rebase + branch mergeoptions;
    # side t5520 32->38).
    [t7601-merge-pull-config.sh]=65
    [t7700-repack.sh]=29
    [t7703-repack-geometric.sh]=11
    [t7704-repack-cruft.sh]=15
    [t6500-gc.sh]=14
    [t0610-reftable-basics.sh]=72
    [t4061-diff-indent.sh]=21
    # wave-12 (2026-06-19, integ/wave12A onto bd53260f): 4-slice disjoint batch.
    # diff-external driver + max-depth (t4020 24->72 full, t4072 2->50); pull
    # reconcile + FETCH_HEAD for-merge (t5520 38->75, t5516 72->74, t5515 held 65);
    # pathspec exclude/attr + rev-list --missing (t6132 2->23, t6135 5->27,
    # t6022 4->13); submodule gitlink core in read-tree/checkout/reset (t1013
    # 0->23, t2013 3->23, t7112 0->25, t6438 0->32). All measured at the integ
    # tip against the same binary; floor-guards (t4013=191 t7810=235) held.
    [t4020-diff-external.sh]=72
    [t4072-diff-max-depth.sh]=50
    [t6132-pathspec-exclude.sh]=23
    [t6135-pathspec-with-attrs.sh]=27
    [t6022-rev-list-missing.sh]=40
    [t1013-read-tree-submodule.sh]=52
    [t2013-checkout-submodule.sh]=51
    [t7112-reset-submodule.sh]=54
    [t6438-submodule-directory-file-conflicts.sh]=32
    # wave-12 Batch B (rebasemerges, integ/wave12B onto 81856328): --rebase-merges
    # todo generation (label/reset/merge -C/-c) + topology replay. t3430 2->17;
    # t3404 held 80, t3418 11->12; cross-guard t5520-pull held 75 (pull-rebase now
    # routes through the rewritten rebase.rs) and t6132 held 23 (log.rs/lib.rs merge).
    [t3430-rebase-merges.sh]=19
    # wave-13 (2026-06-19, integ/wave13A onto f3eeb950): 6-slice batch, all
    # measured at the integ tip against one binary, cargo test --workspace green,
    # cross-guards held (t4014=202 t4013=191 t4205=110 t5505=126 t5520=75 t2007=2).
    # diff/log/clone/rebase raises: t1013 23->32, t2013 23->28, t7112 25->37,
    # t5601 72->73, t5516 74->92, t4202 101->110, t3404 80->89, t1092 29->32,
    # t3430 17->19. NEW: t3206-range-diff (native range-diff command, 2->45).
    [t3206-range-diff.sh]=45
    # wave-14 (2026-06-19, integ/wave14 onto 382ffcd4): 5 parity + 2 behavior-neutral
    # consolidation refactors. All measured at the integ tip against one binary;
    # cargo test --workspace green; foundational ref guards held/gained (t0610 72->73,
    # t1400 271->275 NOT banked — flake-avoidance, incidental); consolidation neutral
    # (t3206=45/t3430=19/t5520=75 held EXACTLY); diff/format guards held (t4013=191,
    # t4014=202). t4015 105->114.
    [t5526-fetch-submodules.sh]=39
    [t2204-add-ignored.sh]=47
    [t6020-bundle-misc.sh]=28
    [t4068-diff-symmetric-merge-base.sh]=36
    [t1423-ref-backend.sh]=27
    # consolidation round 1 (2026-06-19, integ/consol1 onto 9d28b991): behavior-neutral
    # refactors (rev-list engine lib.rs->sley_rev::revlist -521 lines; range-diff/rebase
    # alloc cleanups) held floors EXACTLY (t4202=110 t6022=13 t3206=45 t3404=89 t3430=19).
    # migrate-hang FIXED: the hang was t1460 setup auto-GC reflog-expire + 3000-ref
    # update-ref-stdin pathology (now cached/hash-based, also a perf win). t1460 now
    # PASS 37/37 (no timeout) -> floored; t1423 25->27.
    [t1460-refs-migrate.sh]=37
    # wave-15 (2026-06-19, integ/wave15 onto f1e672fa): 5-slice mid-band harvest.
    # NOTE all 5 codex sessions were KILLED mid-work by a transient infra event and
    # RESUMED from on-disk WIP (see [[codex-exec-resume-on-kill]]) — recovered fully.
    # Measured at the integ tip; cargo test --workspace green; cross-guards held
    # EXACTLY (t4205=110 t6022=13 t3206=45 t7600=83 — incl the commitporcelain↔logbloom
    # sley-rev/lib.rs overlap). Raises: t6423 41->52, t3200 134->145, t7800 69->91,
    # t4216 142->161.
    [t7501-commit-basic-functionality.sh]=54
    [t7507-commit-verbose.sh]=45
    [t7500-commit-template-squash-signoff.sh]=57
    [t3203-branch-output.sh]=41
    # wave-16 (2026-06-19, integ/wave16 onto b194b618): hard wire tail (push + protocol).
    # NOTE this batch had recurring codex kills + TWO confabulations — every value below
    # is MY measurement of the integ binary, not the agents' claimed numbers (see
    # [[codex-exec-resume-on-kill]]): protoallowrest no-op'd then re-ran real (t5813/t5814
    # restored to their stale-high floors 81/27); clienthooks claimed t5402=7 but binary=3.
    # cargo test --workspace green; triple remote_cmds.rs merge clean; cross-guards held
    # (t5505=126 t5516=92 t5520=75 t3404=89 t7600=83). Raises: t5400 12->17, t5543 7->11,
    # t5407 10->17. NEW (measured): push features + post-checkout/merge hooks.
    [t5533-push-cas.sh]=23
    [t5408-send-pack-stdin.sh]=10
    [t5545-push-options.sh]=9
    [t5523-push-upstream.sh]=16
    [t5403-post-checkout-hook.sh]=14
    [t5402-post-merge-hook.sh]=3
    # wave-17 (2026-06-19, integ/wave17 onto 47d14609): v1/v2 wire handshake + connect-helper
    # (completes the push+protocol frontier). MY measurements of the integ binary:
    # t5704-violations 0->3 FULL, t5705-session-id 5->17 FULL, t5802-connect-helper 2->8 FULL,
    # t5702-protocol-v2 ->21 (60-cell suite now fully runs), t5700-protocol-v1 6->9. Cross-guards
    # held (t5813=81 t5505=126 t5601=73 t5516=92). NOTE: cargo test's reapply_after_set_matches_git
    # (sparse_checkout.rs) FAILS on BASE too (flaky non-hermetic match-git test, sley#30 class) —
    # NOT a wave-17 regression; separate hermeticity fix needed.
    [t5700-protocol-v1.sh]=9
    [t5704-protocol-violations.sh]=3
    [t5705-session-id-in-capabilities.sh]=17
    [t5802-connect-helper.sh]=8
    # wave-18 (2026-06-19, integ/wave18 onto 9168c416): unblock + greenfield. The 7696-line
    # workspace.rs god-module was decomposed → commit.rs/status.rs/checkout.rs/reset.rs
    # (behavior-neutral; porcelain floors held EXACTLY t7501=54/t7507=45/t7500=57/t7508=49/
    # t7102=37/t2020=17 — UNBLOCKS wave-19 porcelain parallelism). + 3 greenfield commands
    # + split-index. MY measurements of the integ binary: last-modified 1->28 FULL,
    # filter-branch 9->36, split-index/racy 1->31 FULL, fmt-merge-msg 4->37 FULL. Integration
    # hazard handled: fmtmergemsg's cmd_commit gpg_sign edit (needed by t6200's signed-commit
    # setup) was relocated workspace.rs->commit.rs after the decompose conflict (t6200 26->37,
    # t7510-signed unchanged 2/28). cargo test --workspace green.
    [t8020-last-modified.sh]=28
    [t7003-filter-branch.sh]=36
    [t1701-racy-split-index.sh]=31
    [t6200-fmt-merge-msg.sh]=37
    # wave-19 (2026-06-19, integ/wave19 onto 2c0e3583): post-decompose porcelain frontier,
    # now-parallelizable thanks to wave-18's workspace.rs split. All measured HERMETICALLY vs
    # the integ binary (two oracles agree). status t7508 49->54 + t7512 20->36, commit t7502
    # 52->56 + t7509 9->12 FULL, sparse-compat t1092 32->34, attrs t0003 28->51. Cross-guards
    # held EXACTLY (t7501=54, t7507=45, t7500=57, t7102=37, t2020=17, t1091=53, t4015=114,
    # t6200=37) — the attrs<->sparse sley-worktree/lib.rs auto-merge is behaviorally safe.
    [t7512-status-help.sh]=36
    [t7509-commit-authorship.sh]=12
    [t0003-attributes.sh]=51
    # statusreg fix (2026-06-19, fix 7c64536d onto 928204fc): NOT a stale floor — a REAL
    # regression. A bisect proved t7508=114/t7502=75 were hermetically real (held through
    # parent f1e672fa); commit d3f746e6 "Improve commit porcelain parity" dropped t7508 114->49
    # + t7502 75->52 because its all_index_snapshot used None ambiguously for both "no --all
    # snapshot" and "snapshot of missing index" — the empty-message commit-abort path then
    # restore_index_snapshot(&None)'d a normal commit and DELETED .git/index, so status saw
    # tracked files as staged-deleted+untracked. Fix lifts the distinction into the type
    # (Option<Option<Vec<u8>>>), restoring only when a snapshot was taken. Recovery measured vs
    # the fix binary: t7508 49->119 (RAISED 114->119, wave-19 status work stacks on top), t7502
    # 52->77 (75->77), blast-radius t7506-status-submodule 28->34. t7060/t7064 newly floored.
    # All cross-guards held. LESSON: commit-porcelain slices MUST floor-guard t7508 (commit
    # --status renders the status template) — this regression was masked because they weren't.
    [t7064-wtstatus-pv2.sh]=21
    [t7060-wtstatus.sh]=7
    # wave-20 (2026-06-19, integ/wave20 onto ae0cfe8f): sweep-picked weak-bucket assault, 4
    # disjoint slices (zero merge conflicts). submodule recursion in worktree commands —
    # t2013-checkout-submodule 28->51, t7112-reset-submodule 37->54, t1013-read-tree-submodule
    # 32->52 (denominators GREW 64/70/58 -> 74/82/68 as fixtures unblocked = deep gitlink
    # support); rebase-i t3404 89->94; add-i t3701 98->105; clone t5601 73->86 (the floored 6
    # are RAISED in place above). All measured HERMETICALLY vs the integ binary. Cross-guards
    # held EXACTLY (t7508=119 t7502=77 t7102=37 t2020=17 t1091=53 t1011=19 t7600=83 t3430=19
    # t3415=22 t4015=114 t5516=92 t5520=75); cargo test green. NEW floors below: t2016/t4108
    # are patch-engine NEIGHBORS of add-i's add_patch.rs/plumbing.rs — floored at 4 (confirmed
    # == base, no regression) to guard the class per the d3f746e6 neighbor-guard lesson.
    [t2016-checkout-patch.sh]=4
    [t4108-apply-threeway.sh]=4
    # wave-21 (2026-06-19, integ/wave21 onto 01421060): hard-tail, 4 disjoint slices. submodule
    # recursion in pull/am (t5572 34->40, t4255 1->17 — wired to the wave-20 worktree core;
    # t3426-rebase-submodule stayed 0/29, needs deeper sequencer/index gitlink work = future
    # target), diff-various t4013 191->195, log t4202 110->124 (floor was stale-LOW at 110 vs
    # actual; corrected up), rev-parse --parseopt t1502 11->37 FULL (greenfield optspec parser
    # + set-- renderer). All hermetic vs the integ binary. format_patch.rs auto-merge (log +
    # submtransport both touched it) is SAFE — t4014=202 held. submtransport's status.rs short-
    # status edit did NOT regress t7508 (=119). Cross-guards held EXACTLY (t7508=119 t2013=51
    # t7112=54 t1013=52 t4150=84 t5520=75 t4205=110 t4015=114 t3404=94 t1500=81 t0040=94).
    [t5572-pull-submodule.sh]=40
    [t4255-am-submodule.sh]=17
    [t1502-rev-parse-parseopt.sh]=37
    # wave-22 (2026-06-20, integ/wave22 onto cb9e88a2): sparse-compat deep + rev-list-missing
    # + merge-rename-dirs + rerere (greenfield). All hermetic vs the integ binary. sparse t1092
    # 34->39 (aggressive bucket-by-bucket: checkout sparse-materialize/reset skip-worktree/
    # read-tree paired-entries/status sparse-mode), rev-list-missing t6022 13->40 FULL (--missing
    # modes + list-objects-filter), merge dir-rename t6423 52->55, rerere t4200 11->34 NEW (native
    # rr-cache + 6 subcmds + replay/autoupdate hooks). rerere<->merge-rename merge.rs auto-merge
    # SAFE (t7600=83 t3404=94 t3501=21 t6402=35 held). Cross-guards held EXACTLY (t1091=53 t1011=19
    # t7508=119 t7102=37 t2013=51 t6112=48 t6006=58 t4202=124); cargo test green.
    [t4200-rerere.sh]=34
)

fail=0
seen=""

# Skip the header row; read the columns we care about.
while IFS=, read -r script command result ok notok total plan_total; do
    [ "$script" = "script" ] && continue          # header
    [ -z "${script:-}" ] && continue              # blank line
    floor=${FLOOR[$script]:-}
    if [ -z "$floor" ]; then
        # An untracked script appeared in the output — informational only.
        echo "note: $script not in floor table (ok=$ok) — not gated"
        continue
    fi
    seen="$seen $script"
    ok=${ok:-0}
    if ! [ "$ok" -ge 0 ] 2>/dev/null; then
        echo "FAIL: $script: unparseable ok count '$ok'" >&2
        fail=1
        continue
    fi
    if [ "$ok" -lt "$floor" ]; then
        echo "FAIL: $script: ok=$ok dropped below floor=$floor (result=$result)" >&2
        fail=1
    elif [ "$ok" -gt "$floor" ]; then
        echo "GAIN: $script: ok=$ok (floor=$floor) — +$((ok - floor)); bump the floor"
    else
        echo "ok:   $script: ok=$ok == floor=$floor"
    fi
done < "$summary"

# Every tracked file must have appeared in the summary. A missing file means
# the script never ran (build break, wrong selection) — treat as a failure so
# a silently-skipped t-file can't pass the gate.
for script in "${!FLOOR[@]}"; do
    case " $seen " in
        *" $script "*) ;;
        *)
            echo "FAIL: $script: tracked file absent from summary (did it run?)" >&2
            fail=1
            ;;
    esac
done

if [ "$fail" -ne 0 ]; then
    echo "PARITY FLOOR GATE: FAILED — at least one t-file regressed below floor." >&2
    exit 1
fi
echo "PARITY FLOOR GATE: PASSED — all tracked t-files at or above floor."
