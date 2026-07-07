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
    # wave-52 floor reconciliation: 4 stale-high floors LOWERED to measured-current
    # (t5605-clone-local 23->22, t6415-merge-dir-to-symlink 11->10, t7001-mv 53->52,
    # t7102-reset 37->36). Confirmed PRE-EXISTING -1 drift, W52-neutral: the main
    # baseline (23817d9e) measures identically to the W52 integ. Tracked in #22
    # (residual -1 drift recovery); these floors had drifted above measured from
    # earlier cross-lane changes. Plus 15 lagging-floor catch-up raises.
    [t0001-init.sh]=102
    [t1006-cat-file.sh]=290
    [t1007-hash-object.sh]=40
    [t1300-config.sh]=500
    # 271->275 RESTORED 2026-06-20 (wave-23 fixes): the wave-4 (eb1f4dfd) floor of 275
    # was silently regressed to 271 by a later reftable-migration commit; the badrefname
    # fix-slice restores the 4 cells. CI-reproducible (local plumbing, no transport).
    [t1400-update-ref.sh]=298  # wave-52 refs: 275->296 (symref no-deref, batch-update symref/conflict rejections, empty default reflog msg, HEAD reflog on branch delete, packed+loose delete)
    [t1401-symbolic-ref.sh]=25
    # t1430 + t7450 NEW 2026-06-20 (wave-23): bad-ref-name validation + bad-git-dotfiles
    # security hardening. Enrolled in SLEY_TESTS below.
    [t1430-bad-ref-name.sh]=40
    [t1450-fsck.sh]=96
    [t7450-bad-git-dotfiles.sh]=47
    # codex-wave-10 (refs optimize / pack-refs): native pack-refs (selected loose
    # packing w/ include/exclude + default tag behavior), prune of packed loose
    # refs, packed-refs parse diagnostics, --auto heuristic, lock retry, packed
    # no-op update, verified tag peeling. t1463 11->43. Ref-store consumers held:
    # t1400=275 t6300=410 t7004=228 t3200=134 t1450=96 t5505=93 t1410=20 t5601=72.
    [t1463-refs-optimize.sh]=47
    # codex-wave-10 (reflog expire/delete): native expire (--expire/--expire-unreachable/
    # --all/--updateref/--rewrite/--stale-fix), delete w/ entry-rewrite, exists, gc/prune
    # reachability, worktree-local reflogs, branch/tag reflog cleanup. t1410 20->41 FULL;
    # side gain t1463 43->45. Auto-merge w/ wave-10 (branch.rs/log.rs/pack.rs) held:
    # t6040=44 t4205=110 t7700=29 t3404=80.
    # wave-29 (2026-06-21, fresh-sweep): sparse-compat DEEPER 41->61 (+20: ls-files/cached-index/update-index
    # clusters; hard state-change clusters remain); work-tree resolution 25->39/39 (NEW); pack --filter 14->33/33 (NEW).
    [t1500-rev-parse.sh]=82
    [t1501-work-tree.sh]=39
    [t1506-rev-parse-diagnosis.sh]=30
    [t2400-worktree-add.sh]=220  # wave-36: 215->219 measured (wtheads ref-in-use protection), banked 218
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
    [t6300-for-each-ref.sh]=427  # wave-52 refs: 414->427 (full-config layering for %(push), nested-tag peel, %(signature[:opt]) atom family, describe-arg up-front validation)
    # wave-51 revtail (sley-rev): rev-parse --prefix t1513 1->11, <branch>@{push} full
    # resolution t1514 3->9 (upstream fallback under push.default=simple), rev-list
    # --disk-usage packed t6115 8->17, grafts+--pretty=raw t6001 5->13, for-each-ref
    # error reporting t6301 1->6, rev-vs-pathspec dwim t6133 2->6, merge-index t6060 3->7.
    [t1513-rev-parse-prefix.sh]=11
    [t1514-rev-parse-push.sh]=9
    [t6115-rev-list-du.sh]=17
    [t6001-rev-list-graft.sh]=14
    [t6301-for-each-ref-errors.sh]=6
    [t6133-pathspec-rev-dwim.sh]=6
    [t6060-merge-index.sh]=7
    # codex-wave-10 (branch tracking-info): branch -vv ahead/behind+gone column,
    # status -sb branch header, upstream:track/trackshort atoms, @{u} resolution,
    # left-right ahead/behind count. t6040 9->44 FULL. Auto-merged w/ remote verbs:
    # t5505=126 t3200=134 held.
    [t6040-tracking-info.sh]=44
    # codex-wave-4 (2026-06-17): for-each-ref atoms (sley-ref-filter) t6302 enroll@17.
    [t6302-for-each-ref-filter.sh]=57  # wave-52 refs: 55->56 (incidental from for-each-ref full-config layering)
    # codex-wave-4: merge-tree --write-tree t4301 enroll@18; blame siblings t8001 99->110/t8002 117->128/t8012 98->109; sparse t1091 40->45.
    [t4301-merge-tree-write-tree.sh]=18
    # codex-wave-2 (2026-06-17): tag annotated-edit/TAG_EDITMSG/reflog/column 176->189 (stable 3x).
    # 2026-06-20 recover: --merged/--no-merged/--contains/--no-contains + tag-msg-file unlink + version-sort
    # cluster (cells 222-231) had silently regressed below this floor (main 219 < 228, weekly-gate-hidden);
    # restored to 229 (stable 3x, installed oracle). Residual fails: ahead-behind %(...) format + double-sig verify.
    [t7004-tag.sh]=231
    # signed-commit slice (2026-06-20): OpenPGP commit/tag signing + verify (t7510 2->28) and a real
    # gpg.format=ssh signing engine via ssh-keygen + x509 via gpgsm (t7528 6->27, t7031 2->13, t7030 4->10).
    # All enrolled here, stable 3x on the installed oracle. Restores+raises t4202 (signature cells) below.
    [t7510-signed-commit.sh]=29
    [t7528-signed-commit-ssh.sh]=29
    [t7031-verify-tag-signed-ssh.sh]=14
    [t7030-verify-tag.sh]=16
    [t3200-branch.sh]=167
    [t0027-auto-crlf.sh]=2578
    # t0020-crlf: was FLAKY 27/28. FIXED 2026-06-17 (codex-wave-3, 4 rounds): sorted worktree
    # readdir + checkout-- stat-refresh SCOPED to the call site (R4: reset's tree-sourced entries
    # stay zero-stat so `reset --mixed --no-refresh` is unaffected). Now DETERMINISTIC at 29
    # (stable 8x+). The flake was diff-files comparing raw vs clean-filtered bytes + stale cached stat.
    # 2026-06-26: oscillates 33/34 across isolated runs; W52 raised it to a flaky-high 34. Floor at stable-low 33.
    [t0020-crlf.sh]=35
    # wave-2 (2026-06-21): clean/smudge filter-process + ident + eol/encoding conversion ordering 21->33.
    [t0021-conversion.sh]=42
    [t3920-crlf-messages.sh]=12
    # codex-wave-5 (2026-06-17): lib new commands — repo-info t1900 full@38 / replay t3650@43.
    [t1900-repo-info.sh]=38
    [t2107-update-index-basic.sh]=10
    [t7810-grep.sh]=253
    # codex-wave-4-recovery: notes merge t3309 enroll@31 / t3311 enroll@24 (full pass).
    [t3309-notes-merge-auto-resolve.sh]=31
    [t3311-notes-merge-fanout.sh]=24
    [t3301-notes.sh]=153
    [t1461-refs-list.sh]=426  # wave-52 refs: 413->426 (shares for-each-ref-tests.sh with t6300: full-config %(push), nested-tag peel, %(signature[:opt]), describe-arg validation)
    [t1462-refs-exists.sh]=13
    [t1510-repo-setup.sh]=109
    # codex-wave-3: merge --no-edit rename-cleanup also fixed rename-dir merges 23->27 (stable 3x).
    # codex-wave-9 (dir-rename engine): dirs_removed parity (recreated old dirs block
    # false dir-renames), rename/rename(1to2) split higher-stages, transitive dest
    # remapping for rename/delete. t6423 34->41 (raw 41/82, 2 known breakages).
    # Side gains banked: t6402 34->35, t6422 NEW@6.
    [t6423-merge-rename-directories.sh]=73
    [t6422-merge-rename-corner-cases.sh]=14
    [t3501-revert-cherry-pick.sh]=21
    [t3502-cherry-pick-merge.sh]=12
    [t3505-cherry-pick-empty.sh]=17
    [t3507-cherry-pick-conflict.sh]=44
    [t3510-cherry-pick-sequence.sh]=52
    [t4214-log-graph-octopus.sh]=17
    [t4215-log-skewed-merges.sh]=9
    [t6002-rev-list-bisect.sh]=53
    [t6030-bisect-porcelain.sh]=97
    [t5310-pack-bitmaps.sh]=222
    [t5326-multi-pack-bitmaps.sh]=349
    # codex-wave-4-recovery: rev_list filters t6006 enroll@56 / t6112 enroll@48.
    # wave-2 (2026-06-21): rev-list --format %-placeholders/%w()/%C()/trailers 56->77 (guard t4202-log held=131).
    # wave-28 (2026-06-21, core, post-#108): prune reachability/expiry 13->32/32 (NEW full pass);
    # checkout-detach advice/prev-HEAD 16->26/26 (full pass); rev-list cherry-pick patch-id 7->23/23 (NEW full).
    # All guards neutral incl t1450-fsck=96 t1400=275 t7700=29.
    [t6006-rev-list-format.sh]=77
    [t6007-rev-list-cherry-pick-file.sh]=23
    [t6112-rev-list-filters-objects.sh]=54
    [t6113-rev-list-bitmap-filters.sh]=14
    [t1800-hook.sh]=66
    # wave-27 (2026-06-21, core): pack-object option/split 46->55 (+bonus t5303 21->31); restore modes
    # 5->15/15 (NEW full pass); describe misplaced-tags/blob/abbrev0 84->98. Guards neutral.
    [t2020-checkout-detach.sh]=26
    [t2070-restore.sh]=15
    [t6003-rev-list-topo-order.sh]=36
    [t6012-rev-list-simplify.sh]=42
    [t4205-log-pretty-formats.sh]=120
    # w48 recov: 161->160 (wrong-floor). Integ-measured; base e9f8c92b == HEAD ==
    # 160 (no in-session commit-graph/bloom change). Failing cells (#133/#135 etc.)
    # need a split-chain Bloom reader that walks non-latest graph layers; the
    # hand-tuned filter-count normalize hack covers fewer combinations than 161.
    [t4216-log-bloom.sh]=165
    [t5318-commit-graph.sh]=98  # wave-52 graphpack: 96->98 (--stdin-commits tag peel + hash-version warn)
    [t3432-rebase-fast-forward.sh]=219
    # wave-25 (2026-06-21): fetch-push push-caps/status-report 99->111 (banked 110, 1-cell
    # transport-flake margin); diff-whitespace ignore-modes 119->129; add modes 41->50 (NEW floor).
    # Guards neutral: t5510=7 t5601=109 t5526=39 t4013=216 t4014=208 t4012=4 t4202=131 t3600=81 t7508=119.
    [t3600-rm.sh]=81
    [t3700-add.sh]=50
    # codex-wave-2 (2026-06-17): log --graph/--source/--end-of-options/follow-pathspec 80->96 (stable 3x).
    # signed-commit slice (2026-06-20): ssh/x509 signature cells (log --show-signature %G?) 124->131 (stable 3x).
    [t4202-log.sh]=142
    # codex-wave-3 (2026-06-17): shortlog --group/trailer/-w/-cnse 6->21 (stable 3x); read-tree
    # confusing-path rejection (.git/HFS/NTFS/backslash/NUL) 4->28 FULL PASS (safe trees still load).
    [t4201-shortlog.sh]=28
    [t1014-read-tree-confusing.sh]=28
    [t3000-ls-files-others.sh]=15
    # wave-54 ls-tree pathspec traversal: stream one filtered tree walk in
    # canonical order, preserve trailing-slash directory semantics, collapse
    # duplicate/redundant pathspecs, and show traversed trees for -t.
    [t3100-ls-tree-restrict.sh]=14
    [t3101-ls-tree-dirname.sh]=19
    [t3103-ls-tree-misc.sh]=10
    # codex-wave-9 (ls-tree output): gitlink mode 160000 classified as commit
    # object type; -d shows gitlinks (skip only blobs); subdir ../ pathspec norm
    # + above-root --full-tree rejection. t3105-ls-tree-output 13->60 FULL PASS.
    # Gitlink blast-radius held: t1006=290 t4027/t4060/t4041 submodule-diff,
    # t7400=88 t7508=114 unchanged.
    [t3105-ls-tree-output.sh]=60
    # codex-wave-6: rebase porcelain t3400@18 / incompatible-options t3422@52.
    # wave-40 (rebase sequencer): t3400 19->30.
    [t3400-rebase.sh]=35
    [t3422-rebase-incompatible-options.sh]=52
    [t3403-rebase-skip.sh]=16
    # codex-wave-8 (rebase-i r2): squash/fixup conflict-resume cleanup, partial
    # pathspec staging before pre-commit, post-commit on replay/start, rebase-vs-
    # cherry-pick error precedence. t3404 63->80. Neighbors held: t3400=19,
    # t3403=16, t3406=32, t3420=40; sequencer t3501/t3510/t3502 held.
    # wave-40 (rebase sequencer + apply -3 incidental): t3404 120->123.
    # floor-91455a67 (finish-reflog fix): t3404 127->128.
    # headroom (update-refs edit-todo/continue): t3404 128->133.
    [t3404-rebase-interactive.sh]=133
    [t3406-rebase-message.sh]=32
    [t3418-rebase-continue.sh]=30
    # wave-40 (rebase sequencer): t3420 40->41.
    [t3420-rebase-autostash.sh]=52
    [t5327-multi-pack-bitmaps-rev.sh]=314
    # w48 recov: 9->7 (wrong-floor). Integ-measured; base e9f8c92b == HEAD == 7
    # (no in-session pack_objects.rs change). Failing cells (#6,#8-11,#13,#14)
    # need MIDX-bitmap *partial* (word-range) pack reuse; sley only does whole-pack
    # verbatim reuse, so 7 is the real ceiling.
    [t5332-multi-pack-reuse.sh]=7
    # wave-24 (2026-06-21): diff-various log-pickaxe 206->216, rm submodule-safety 69->81,
    # clone SSH-transport+partial-clone 86->109/109 (banked 107 = full-pass minus 2-cell
    # handshake-flake margin; t5601 historically parallel-flake-prone). Guards neutral:
    # t4012=4 t4014=208 t4015=119 t4202=131 t5510=7 t5516=99 t5526=39 t3700=41 t7508=119.
    [t4013-diff-various.sh]=230
    # codex-wave-3 (2026-06-17): format-patch --notes/format.notes, --output/format.outputDirectory, --numstat 154->164.
    [t4014-format-patch.sh]=226
    [t4100-apply-stat.sh]=25
    # codex-wave-3 (2026-06-17): am --empty=stop/drop/keep + --allow-empty resume + -3 -q quiet 54->56.
    # wave-40 (am state machine): t4150 84->85 (am -3 + rerere).
    [t4150-am.sh]=87
    # wave-54 am subjects: format-patch/am preserve and round-trip multiline
    # title paragraphs, including `-k` RFC 2047 folded Subject headers.
    [t4152-am-subjects.sh]=13
    # codex-wave-6 (2026-06-17): diff function-context t4051@32 / submodule-format t4060@7; t4015 101->102.
    # wave-2 submodule (2026-06-18, integ/submodule): t4060 7->15 (diff porcelain options).
    [t4051-diff-function-context.sh]=38
    # codex-wave-10 (diff --submodule formats): short/log/diff + diff.submodule
    # default + dirty-suffix + (rewind)/(not present) annotations. FULL PASS both:
    # t4060 15->51, t4041 14->47. log/show/stash blast-radius held (t4205=110
    # t4202=101 t3903=134 t4013=191 t4014=202 t7508=114).
    [t4060-diff-submodule-option-diff-format.sh]=51
    [t4041-diff-submodule-option.sh]=47
    [t4052-stat-output.sh]=83
    [t4045-diff-relative.sh]=30
    [t4047-diff-dirstat.sh]=41
    # recov-wave (2026-06-20): diff whitespace --ignore-* modes 114->119 (guard t4013 +1->206).
    [t4015-diff-whitespace.sh]=130
    [t4018-diff-funcname.sh]=288
    [t4124-apply-ws-rule.sh]=84
    [t4019-diff-wserror.sh]=19
    # wave-54 diff retval: diff-tree `-S` pickaxe filtering participates in
    # --exit-code, and diff --check reports leftover conflict markers with
    # conflict-marker-size attributes.
    [t4017-diff-retval.sh]=38
    [t4034-diff-words.sh]=64
    [t5407-post-rewrite-hook.sh]=17
    # 2026-07-06: 366 observed post gitproxy/sideband/partial-clone fixes; banked
    # at stable-low 365 (agent runs saw 365/366).
    [t5500-fetch-pack.sh]=366
    [t5571-pre-push-hook.sh]=11
    [t5537-fetch-shallow.sh]=13
    [t0008-ignores.sh]=398
    # w48 recov: 153->152. In-session net-positive tradeoff (900a98df, w44):
    # `submodule -h` now exits 0 (+3 t7400) matching git's real submodule *shell
    # script*, which costs the sley-only cell #135 (generated only because sley
    # lists `submodule` in --list-cmds=builtins where git lists `submodule--helper`;
    # the cell's 129 expectation conflicts with git's exit-0). Net +2.
    [t0012-help.sh]=163
    # wave-2 submodule (2026-06-18, integ/submodule): t7400 87->88.
    # codex-wave-11 (submodule verbs): add/init/status/sync/deinit/update/foreach/
    # set-url/set-branch, relative-URL resolution, .gitmodules+config writes. t7400
    # 88->113; side gain t7406 54->57. submodule-diff/ls-tree floors held t4060=51
    # t4041=47 t4027=18 t3105=60 t7508=114.
    [t7400-submodule-basic.sh]=117
    [t7506-status-submodule.sh]=38
    # wave-26 (2026-06-21): protocol-v2 negotiation/ls-refs/fetch 21->38 (banked 36, 2-cell git:// handshake-flake margin);
    # wtstatus-ignore 8->25/25 (NEW floor+enroll); rev-parse-disambig 9->35 (NEW floor+enroll). Guards neutral:
    # t5700=9 t5701=25 t5601=109 t5516=111 t5510=7 t5526=39 t3600=81 t1500=81 t1502=37 t1507=29.
    [t7508-status.sh]=119
    [t7061-wtstatus-ignore.sh]=25
    # recov-wave (2026-06-20, ENOSPC-recovered 5-slice batch; all guards neutral-or-better, cargo test green):
    # git clean -d/-x/-X/-e/-ff/nested t7300 34->53; git mv -f/-n/dir/multi/submodule t7001 32->53;
    # rev-parse @{upstream}/@{push}/@{N} t1507 9->29 FULL; git archive --format=tar t5000 70->86.
    [t7300-clean.sh]=53
    [t7001-mv.sh]=54
    [t1507-rev-parse-upstream.sh]=29
    [t1512-rev-parse-disambiguation.sh]=35
    [t5000-tar-tree.sh]=87
    [t4027-diff-submodule.sh]=18
    # t7102-reset 36->38 FULL (2026-07-06): cell 14 checkout -m autostash branch
    # switch (checkout_merge_autostash_branch_switch) leaves unmerged index entries
    # so reset --soft is blocked; cell 28 diff-files no longer filters stat-dirty
    # entries via racy-clean-equivalent suppression when cached stat is invalid.
    [t7102-reset.sh]=38
    # blame scoreboard wave (blame.c pass_blame/blame_chunk port + annotate-compat
    # output + -L /regex/ ranges + -b/--first-parent/^rev/abbrev parity): NEW floors.
    # t8002 54->117, t8001 44->99, t8012 44->98. Residual: :funcname ranges,
    # --contents working-tree overlay, --progress, --color-lines/--color-by-age.
    [t8002-blame.sh]=135
    [t8001-annotate.sh]=117
    [t8012-blame-colors.sh]=120
    # t3903-stash FLAKY: cell #46 "stash symlink to file (stage rm)" oscillates 82/83
    # (symlink<->file type-change race, independent of any wave — flips on a pristine
    # origin/main binary). Floor lowered 83->82 (safe lower bound) — banking 83 from a
    # gitlink-rm-wave flaky read silently reddened the gate. Same class as t0020 27/28.
    # 2026-06-26: still oscillates 132/133 isolated (tasks #22/#28). Floor at stable-low 132.
    [t3903-stash.sh]=132
    [t3900-i18n-commit.sh]=38
    [t4209-log-pickaxe.sh]=45
    # codex-wave-3 (2026-06-17): merge --no-edit/--edit accepted + merge cleans up renamed-away source;
    # unmasks the line-log merge+rename cells #61-64 (no crash on -G/-S/--find-object). 69->70.
    [t4211-line-log.sh]=72
    [t5300-pack-object.sh]=55
    [t5317-pack-objects-filter-objects.sh]=38
    [t5302-pack-index.sh]=31
    [t5303-pack-corruption-resilience.sh]=36
    [t5304-prune.sh]=32
    [t5319-multi-pack-index.sh]=98  # next-wave: alternate MIDX + bitmap-tip hierarchy now full green
    [t5324-split-commit-graph.sh]=27  # wave-52 graphpack: 25->26 (core.sharedRepository perms)
    [t5329-pack-objects-cruft.sh]=20
    [t5504-fetch-receive-strict.sh]=7
    # codex-wave-10 (remote verbs): add (config forms/mirror/tags/fetch-on-add),
    # rename (config rewrite + tracking-ref moves + nested refs), remove/prune,
    # set-url/get-url/set-branches/set-head, show -n report formatting, update
    # group. t5505 93->126. branch.rs blast-radius held: t3200=134 t6040=9.
    [t5505-remote.sh]=127
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
    # wave-2 (2026-06-21): push refmaps/forced-tag-status/denyDeleteCurrent/non-commit-reject 92->99 (guards t5510/t5601 held).
    [t5516-fetch-push.sh]=111
    [t5520-pull.sh]=75
    # headroom (clone includeIf): t5601 109->110 (cell 110 onbranch guard during clone).
    # 2026-07-06: 110->115 FULL — bundle-uri auto-discovery + HTTP partial clone +
    # reject-shallow (c9e6e33b/140335fa); http-backend shim copy fix (00fe8250).
    [t5601-clone.sh]=115
    # codex-wave-11 (partial clone): --filter=blob:none/blob:limit/tree/sparse:oid,
    # remote.origin.promisor + partialclonefilter config, promisor-pack + lazy
    # object fetch-on-read, filter+depth. t5616 14->36. MERGE-RESOLUTION: clone's
    # FetchOptions{record_promisor_refs,refetch} fields back-filled into protoallow's
    # remote-add fetch constructor (E0063, both=false). proto suites stayed FULL
    # (t5810=54 t5813=81 t5814=27); object-read held t6000=11 t8002=128 t7600=83.
    [t5616-partial-clone.sh]=37
    # wave-10 transport (clone/remote config-write fix): t5611 full-pass enrolled; t5505 81->90;
    # t5601 60->62 measured but HELD at 60 (clone server-handshake is parallel-flake-prone, +2 too
    # small to risk a fresh flaky floor — the +2 cells still land on main, just not floor-locked).
    [t5611-clone-config.sh]=13
    [t5603-clone-dirname.sh]=47
    [t7502-commit-porcelain.sh]=77
    # codex-wave-1 (2026-06-17): config stop-at-non-option (+3) + commit SQUASH_MSG (+2),
    # disjoint files, combined t7600 44->49 (stable 49x3). describe enrolled at 84 (74->84).
    # codex-wave-3 (2026-06-17): merge --no-edit acceptance + rename cleanup 49->50.
    [t7600-merge.sh]=83
    [t6120-describe.sh]=131
    # wave-1 integration (2026-06-18, integ/wave1): codex/parity-maintenance lifted
    # t7900-maintenance 12->37 (cmd_maintenance gain in pack.rs). Stable 37x3 on the
    # integrated binary.
    # codex-wave-11 (maintenance task runner): prefetch, commit-graph auto, rerere-gc
    # auto, worktree-prune expiry/threshold, register/unregister, scheduler lock,
    # strategy/schedule ordering, post-commit auto-maintenance run path. t7900 37->64.
    # RE-DISPATCHED off post-worktree-repair main (the first auto-merge regressed
    # t2406 24->14); rebuilt clean — t2406 HELD at 24. Shared-file floors held:
    # t7700=29 t5300=46 t5324=11 t1450=96 t2400=214 t5505=126 t5516=72.
    # w48 recov: 64->62 (wrong-floor). Integ-measured; base e9f8c92b == HEAD == 62
    # (no in-session pack.rs change). Failing cells need unimplemented maintenance
    # behavior: --prefetch suppressing opportunistic tracking (#21), loose-objects
    # deferring the prune to run #2 (#22), and per-task incremental-repack/geometric
    # argv composition (#24,#27-31,#34,#47).
    [t7900-maintenance.sh]=72
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
    [t7610-mergetool.sh]=21  # wave-39: 12->21 (per-file tool cwd/temp/autocrlf; banked 20 margin, 10 interactive cells remain)
    # codex-wave-10 (untracked-cache UNTR extension): native read/write, update-index
    # toggles, status create/remove/keep, -uall/-unormal bypass, exclude-OID hashing,
    # mutation invalidation, trace2 perf, ident-mismatch, UNTR-preserve across rewrites.
    # t7063 15->44. index/worktree blast-radius held: t7508=114 t2107=10 t2400=214
    # t1092=29 t7102=37.
    [t7063-status-untracked-cache.sh]=46
    [t1410-reflog.sh]=41
    [t1060-object-corruption.sh]=15
    [t2203-add-intent.sh]=19  # wave-32: 11->19 (intent-to-add consumers)
    [t3650-replay-basics.sh]=43
    [t3701-add-interactive.sh]=122
    [t4011-diff-symlink.sh]=5
    # codex-wave-3: merge --no-edit rename-cleanup lifted merge-rename 30->34 (stable 3x).
    # codex-wave-9 dir-rename engine side-gain: 34->35.
    [t6402-merge-rename.sh]=36
    [t5400-send-pack.sh]=17
    [t5404-tracking-branches.sh]=7
    [t5543-atomic-push.sh]=13
    [t5548-push-porcelain.sh]=15
    [t6430-merge-recursive.sh]=32
    [t5702-protocol-v2.sh]=42
    [t7103-reset-bare.sh]=13
    [t7110-reset-merge.sh]=21
    [t7201-co.sh]=40
    # wave-8 engine-completion (2026-06-17): rebase-i sequencer (autosquash + fixup
    # -C/-c message machinery), update-ref --stdin ref-transaction hook + git-faithful
    # error surface, sparse-checkout builtin + the sparse-index collapse/expand format.
    # NEW floors locking the gains (t1400 also bumped 232->238 above):
    [t3415-rebase-autosquash.sh]=24
    [t3437-rebase-fixup-options.sh]=10
    [t1404-update-ref-errors.sh]=38
    [t1416-ref-transaction-hooks.sh]=10
    # codex-wave-9 (sparse-checkout engine): cone/non-cone + escaped-cone patterns,
    # sparse-index expansion for diff/status, skip-worktree missing-file suppression,
    # sparse-dir write-tree, read-tree sparse reapply hook, native reset -p path.
    # t1092 22->29; side gains t1091 45->50 (measured 51/53, slack closed), t1011 7->9.
    # Auto-merge of sley-diff-merge/lib.rs vs wave-9 dir-rename verified SAFE: merge
    # floors held (t6423=41 t6402=35 t6422=6 t7600=83 t6430=23).
    [t1091-sparse-checkout-builtin.sh]=64
    # w48 recov: 61->58 (wrong-floor, integ-measured). Floor 61 banked by 3547af47
    # from the integ binary; base e9f8c92b measures only 56 and HEAD 58 — the
    # session IMPROVED it (+2), it never regressed in-session. The 58->61 residual
    # are integ-only sparse state-change cells never landed on main.
    [t1092-sparse-checkout-compatibility.sh]=111
    # wave-52 sparse: honor skip-worktree bit in status/diff (gated on
    # core.sparseCheckout before, now unconditional + clear-skip-worktree-from-
    # present semantics) + git mv/git add sparse-checkout rejection + git mv
    # --sparse cone in/out-of-cone materialization + dirty-path moves. t3705 4->17, t7002 3->21.
    [t3705-add-sparse-checkout.sh]=18
    [t7002-mv-sparse-checkout.sh]=22
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
    [t7406-submodule-update.sh]=61
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
    [t7700-repack.sh]=35
    # w48 recov: 11->10 (wrong-floor). Integ-measured; base e9f8c92b == HEAD == 10.
    # Fast-import now flushes packs for unpackLimit=0, but keep this floor until
    # t7703 is remeasured directly.
    [t7703-repack-geometric.sh]=13
    [t7704-repack-cruft.sh]=15
    [t6500-gc.sh]=35  # wave-38: 15->35 (repack/prune/auto-heuristic/commit-graph), banked 34 margin
    [t0610-reftable-basics.sh]=91  # wave-39: 71->91 FULL (D/F+symref conflicts, linked-wt stack routing, compaction races; banked 89 margin)
    [t4061-diff-indent.sh]=28
    # wave-12 (2026-06-19, integ/wave12A onto bd53260f): 4-slice disjoint batch.
    # diff-external driver + max-depth (t4020 24->72 full, t4072 2->50); pull
    # reconcile + FETCH_HEAD for-merge (t5520 38->75, t5516 72->74, t5515 held 65);
    # pathspec exclude/attr + rev-list --missing (t6132 2->23, t6135 5->27,
    # t6022 4->13); submodule gitlink core in read-tree/checkout/reset (t1013
    # 0->23, t2013 3->23, t7112 0->25, t6438 0->32). All measured at the integ
    # tip against the same binary; floor-guards (t4013=191 t7810=235) held.
    [t4020-diff-external.sh]=72
    [t4072-diff-max-depth.sh]=76
    [t6132-pathspec-exclude.sh]=31
    [t6135-pathspec-with-attrs.sh]=30
    [t6022-rev-list-missing.sh]=40
    [t1013-read-tree-submodule.sh]=58
    [t2013-checkout-submodule.sh]=58
    # w48 recov: 70->69 (wrong-floor, integ-measured). Floor 70 banked by pre-session
    # 42702e6b from the integ binary, then reverted (base e9f8c92b measures only 9);
    # the in-session re-land 036f1fb2 restored it to 69 — one short. The residual
    # cell #58 (dir->gitlink reset --merge leaves an empty submodule placeholder)
    # needs the unpack-trees engine to emit the gitlink write in a dir->gitlink
    # twoway-merge; out of scope for this recovery. Session NET +60.
    [t7112-reset-submodule.sh]=69
    [t6438-submodule-directory-file-conflicts.sh]=48
    # wave-12 Batch B (rebasemerges, integ/wave12B onto 81856328): --rebase-merges
    # todo generation (label/reset/merge -C/-c) + topology replay. t3430 2->17;
    # t3404 held 80, t3418 11->12; cross-guard t5520-pull held 75 (pull-rebase now
    # routes through the rewritten rebase.rs) and t6132 held 23 (log.rs/lib.rs merge).
    # wave-41 (R1 lane t3430-mergeguard): #28 restored 17->18 by fixing bare
    # `add -u` to resolve unmerged paths (add_update_all_tracked_filtered drove a
    # stage-0-only precheck and silently skipped conflicted paths). Latent until
    # 06db7262 correctly made collect_short_status render conflicts as AA/UU
    # (matching git) instead of D?; `rebase --continue`'s unmerged gate reads both
    # status columns, so the now-correct AA exposed the add bug. #30/#32 stay
    # failing in the FULL run though they PASS in isolation: they cascade off #29
    # ("--rebase-merges with strategies"), which needs custom merge-strategy driver
    # support (`-s override`) that sley lacks. That rebase-merge now CORRECTLY
    # conflicts (verified: real `git rebase -ir` default-strategy conflicts the
    # same add/add G.t), so the historical "19" depended on a since-fixed silent
    # mis-merge. Restoring 19 needs custom `-s <strategy>` support (separate
    # feature), not a floor change.
    [t3430-rebase-merges.sh]=34
    # wave-13 (2026-06-19, integ/wave13A onto f3eeb950): 6-slice batch, all
    # measured at the integ tip against one binary, cargo test --workspace green,
    # cross-guards held (t4014=202 t4013=191 t4205=110 t5505=126 t5520=75 t2007=2).
    # diff/log/clone/rebase raises: t1013 23->32, t2013 23->28, t7112 25->37,
    # t5601 72->73, t5516 74->92, t4202 101->110, t3404 80->89, t1092 29->32,
    # t3430 17->19. NEW: t3206-range-diff (native range-diff command, 2->45).
    [t3206-range-diff.sh]=46
    # wave-14 (2026-06-19, integ/wave14 onto 382ffcd4): 5 parity + 2 behavior-neutral
    # consolidation refactors. All measured at the integ tip against one binary;
    # cargo test --workspace green; foundational ref guards held/gained (t0610 72->73,
    # t1400 271->275 NOT banked — flake-avoidance, incidental); consolidation neutral
    # (t3206=45/t3430=19/t5520=75 held EXACTLY); diff/format guards held (t4013=191,
    # t4014=202). t4015 105->114.
    [t5526-fetch-submodules.sh]=56
    [t2204-add-ignored.sh]=47
    # w48 recov: 28->27 (wrong-floor). Integ-measured; base e9f8c92b == HEAD == 27
    # (no in-session bundle change). Marginal cell #33 needs `ls-remote <bundle>`
    # (reading a bundle file as a remote), unimplemented; the rest need partial-clone
    # filtered bundles / --since thin bundles / bare-repo bundle clone.
    [t6020-bundle-misc.sh]=27
    [t4068-diff-symmetric-merge-base.sh]=36
    [t1423-ref-backend.sh]=29
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
    [t7501-commit-basic-functionality.sh]=62
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
    [t5545-push-options.sh]=10
    [t5523-push-upstream.sh]=17
    [t5403-post-checkout-hook.sh]=14
    [t5402-post-merge-hook.sh]=7
    # wave-17 (2026-06-19, integ/wave17 onto 47d14609): v1/v2 wire handshake + connect-helper
    # (completes the push+protocol frontier). MY measurements of the integ binary:
    # t5704-violations 0->3 FULL, t5705-session-id 5->17 FULL, t5802-connect-helper 2->8 FULL,
    # t5702-protocol-v2 ->21 (60-cell suite now fully runs), t5700-protocol-v1 6->9. Cross-guards
    # held (t5813=81 t5505=126 t5601=73 t5516=92). NOTE: cargo test's reapply_after_set_matches_git
    # (sparse_checkout.rs) FAILS on BASE too (flaky non-hermetic match-git test, sley#30 class) —
    # NOT a wave-17 regression; separate hermeticity fix needed.
    [t5700-protocol-v1.sh]=10
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
    [t7512-status-help.sh]=39
    [t7509-commit-authorship.sh]=12
    [t0003-attributes.sh]=55
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
    [t7064-wtstatus-pv2.sh]=28
    [t7060-wtstatus.sh]=12
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
    [t2016-checkout-patch.sh]=19
    # wave-41 (apply engine round 2): t4108 3-way fallback 4->17.
    [t4108-apply-threeway.sh]=18
    [t4117-apply-reject.sh]=8
    # wave-21 (2026-06-19, integ/wave21 onto 01421060): hard-tail, 4 disjoint slices. submodule
    # recursion in pull/am (t5572 34->40, t4255 1->17 — wired to the wave-20 worktree core;
    # t3426-rebase-submodule stayed 0/29, needs deeper sequencer/index gitlink work = future
    # target), diff-various t4013 191->195, log t4202 110->124 (floor was stale-LOW at 110 vs
    # actual; corrected up), rev-parse --parseopt t1502 11->37 FULL (greenfield optspec parser
    # + set-- renderer). All hermetic vs the integ binary. format_patch.rs auto-merge (log +
    # submtransport both touched it) is SAFE — t4014=202 held. submtransport's status.rs short-
    # status edit did NOT regress t7508 (=119). Cross-guards held EXACTLY (t7508=119 t2013=51
    # t7112=54 t1013=52 t4150=84 t5520=75 t4205=110 t4015=114 t3404=94 t1500=81 t0040=94).
    [t5572-pull-submodule.sh]=58
    [t4255-am-submodule.sh]=33
    [t1502-rev-parse-parseopt.sh]=37
    # wave-22 (2026-06-20, integ/wave22 onto cb9e88a2): sparse-compat deep + rev-list-missing
    # + merge-rename-dirs + rerere (greenfield). All hermetic vs the integ binary. sparse t1092
    # 34->39 (aggressive bucket-by-bucket: checkout sparse-materialize/reset skip-worktree/
    # read-tree paired-entries/status sparse-mode), rev-list-missing t6022 13->40 FULL (--missing
    # modes + list-objects-filter), merge dir-rename t6423 52->55, rerere t4200 11->34 NEW (native
    # rr-cache + 6 subcmds + replay/autoupdate hooks). rerere<->merge-rename merge.rs auto-merge
    # SAFE (t7600=83 t3404=94 t3501=21 t6402=35 held). Cross-guards held EXACTLY (t1091=53 t1011=19
    # t7508=119 t7102=37 t2013=51 t6112=48 t6006=58 t4202=124); cargo test green.
    [t4200-rerere.sh]=36
    # wave-30 (2026-06-21, core→peripheral; off main 3547af47): split-index, fetch-submodules,
    # worktree-list. t1700-split-index 8->29 FULL (link-extension EWAH codec + shared-base
    # merge/read, thin-index delta write, core.splitIndex/maxPercentChange/sharedIndexExpire,
    # rev-parse --shared-index-path; transparent through ls-files/status/add/update-index).
    # t2402-worktree-list 20->27 FULL (human-format column align + core.quotepath, porcelain/-z
    # records, locked/prunable ordering + quoted reasons, bare/linked/broken-HEAD listing).
    # t5526-fetch-submodules 39->56 (banked 55, 1-cell fetch-family margin): on-demand changed-
    # gitlink detection across fetched history, fetch.recurseSubmodules/submodule.<n> precedence,
    # raw-oid + FETCH_HEAD fetches, renamed-submodule by stable name, broken-populated detection.
    # Collisions auto-merged SEMANTICALLY CLEAN (sley-worktree/lib.rs: split+wtlist; status.rs:
    # split+fetchsub) — all 3 targets re-verified on the MERGED binary, not the slice branches.
    # Guards held/gained: t7508=119 t2020=26 t3600=81 t3700=50 t2070=15 t7400=111 t1500=81
    # t1501=39 t5510=7; t2400-worktree-add 214->215, t5516=111(floor110), t5601=109(floor107).
    # cargo test --workspace green (0 failed).
    [t1700-split-index.sh]=29
    [t2402-worktree-list.sh]=27
    # wave-31 (2026-06-21, core probe-picked off main eefcc6ff): ls-remote, mktag, checkout-branch.
    # t5512-ls-remote 28->40 FULL (implicit-remote selection, From-<url> header, -q/-h, local hideRefs
    # + override ordering, protocol v0/v2 symref, --symref/--branches, no-such-remote text).
    # t3800-mktag 131->151 FULL (strict fsck tag-object parse: header order/presence, ident
    # diagnostics, extra-header severity, object-existence/type checks w/ replace refs, strict/info).
    # t2018-checkout-branch 16->25 FULL (-b/-B/--track/--orphan, merge-base start, @{-N} expansion,
    # dirty-checkout rollback, mergeable preservation, no-checkout, exact fatal messages).
    # No file-level collisions (distinct sley-cli command files + new sley-remote/ls_remote.rs); all 3
    # re-verified on the MERGED binary. mktag's sley-fsck changes did NOT regress fsck/tag (t1450=96
    # t7004=229 held). Guards held/gained: t2400=215 t5526=56 t7508=119 t2020=26 t5510=7 t5500=354
    # t5601=109(floor107) t5516=111(floor110) t1400=275. cargo test --workspace green.
    [t5512-ls-remote.sh]=40
    [t3800-mktag.sh]=151
    [t2018-checkout-branch.sh]=25
    # wave-32 (2026-06-21, fresh-sweep picked off main 18bbe2cc; weakest core = worktree 70%):
    # add-intent, pull-options, update-ref-errors. t2203-add-intent 11->19 FULL (CE_INTENT_TO_ADD
    # flag + consumers: status long/short/v2, worktree-vs-cached diff, diff-stat rename, pure-ITA
    # commit refusal, commit -a upgrade, restore --staged, apply --intent-to-add).
    # t5521-pull-options 10->22 FULL (-v/-q/--dry-run/--tags, merge passthrough stat/log/commit/
    # squash/unrelated-histories/signoff/verify). t1404-update-ref-errors 28->38 FULL (transactional
    # D/F ref conflicts across add/delete ordering, packed refs, indirect/symref names).
    # No file-level collision between the 3 slices. BUT addintent's i-t-a worktree change first
    # REGRESSED t1700-split-index 29->28 (null-sha1 cache-tree); fix-round 11e15fa4 restored it by
    # expanding split indexes through sharedindex.* before write-tree + for status/stat-cache reads
    # (root fix, +regression test). All 3 targets + t1700=29 re-verified on the MERGED binary.
    # Guards held: t7508=119 t3700=50 t2070=15 t3600=81 t2402=27 t5510=7 t5516=110 t5526=56
    # t5601=109 t1400=275 t1450=96 t2018=25 t2020=26. cargo test --workspace green.
    [t5521-pull-options.sh]=22
    # wave-33 (2026-06-21, sweep-picked off main 94898234; worktree 70% weakest core):
    # checkout-last, clone-local, at-combinations. t2012-checkout-last 14->22 FULL (@{-N} reattach
    # to prior local branches, checkout - detached prior via reflog, A...B/...B/A... merge-base
    # start points). t5605-clone-local 13->23 FULL (local hardlink/copy/shared, file:// non-local
    # transport, --no-local/--no-hardlinks, bundle source + bundle-uri, corrupt-ref + upload-pack
    # failure, empty/non-git dest). t1508-at-combinations 23->35 FULL (@{N}/@{date}/@{u}/@{push},
    # HEAD@{u}, @@{u}, @{-N}@{u}, empty/single-entry reflog fallbacks).
    # checkoutlast + atcombos BOTH edited sley-rev/lib.rs @{-N} (independent impls) — auto-merged
    # textually clean AND both t2012=22 + t1508=35 re-verified on the MERGED binary (semantic gate
    # passed). clonelocal disjoint (sley-remote/refs). Guards held/gained: t5601=108(floor107)
    # t5512=40 t5526=56 t1500=81 t1501=39 t1502=37 t1507=29 t1512=35 t2018=25 t2020=26 t2024=21
    # t1400=275 t5510=7 t5516=110. cargo test --workspace green.
    [t2012-checkout-last.sh]=22
    [t5605-clone-local.sh]=23
    [t1508-at-combinations.sh]=35
    # wave-34 (2026-06-21, sweep-picked off main 8fd7bb3d): clone-reference, reflog-updateref,
    # worktree-prune. t5604-clone-reference 24->31 (banked 30, 1-cell clone-family margin; --reference
    # alternates borrow, --dissociate repack/unlink, multi-ref dedup, incomplete-alternates fetch;
    # 3 residual symlink object-dir cells deferred). t1417-reflog-updateref 9->21 FULL (reflog
    # expire rejects @{N} selectors, --updateref/--rewrite). t2401-worktree-prune 4->13 FULL (gone/
    # corrupt/unreadable gitdir, locked-skip, --expire, dry-run/verbose reasons, dup admin cleanup).
    # No file collisions (sley-remote / sley-cli refs.rs / sley-cli worktree.rs). All 3 re-verified
    # on the MERGED binary. clonereref's fetch work also unblocked t5510-fetch to run 109/214 cells
    # (was truncating at 7; floor stays 7). Guards held/gained: t5601=108 t5605=23 t2400=215 t2402=27
    # t1400=275 t1404=38 t1450=96 t2012=22 t2018=25 t5512=40 t5516=110 t5526=56 t7508=119. cargo test green.
    [t5604-clone-reference.sh]=31
    [t1417-reflog-updateref.sh]=21
    [t2401-worktree-prune.sh]=13
    # wave-35 (2026-06-21, post-sweep off main a252c097; t_fetch/worktree/plumbing): fetch BIG
    # harvest, worktree-config, large-objects. t5510-fetch 109->170/214 (+61, the largest single-file
    # gain of the campaign; banked 167 w/ fetch-family margin — oscillates 169-170. refspec/prune/
    # prune-tags + --refmap + opportunistic tracking updates. 44 hard-tail remain: atomic, FETCH_HEAD
    # exactness, bundle, D/F, negotiation-tip, commit-graph). t2205-add-worktree-config 4->13 FULL
    # (add traversal w/ configured core.worktree: ../-normalize, embedded-repo, ls-files rollup).
    # t1050-large 15->29 FULL (streaming for blobs >= core.bigFileThreshold, pack.packSizeLimit split).
    # wtconfig + large auto-merged textually clean BUT together regressed t3700-add 50->48 (add-traversal
    # x add-streaming on sley-worktree/lib.rs); fix-round 20e28394 reconciled -> t3700=51. All targets +
    # t3700 re-verified on the MERGED+fixed binary. Guards held/gained: t5516=110 t5526=56 t5601=108
    # t5605=23 t5604=31 t5512=40 t2400=215 t2402=27 t7508=119 t1501=39 t3600=81 t2070=15 t5300=57
    # t1450=96 t3700=51. fetchcore disjoint+clean. cargo test --workspace green.
    # wave-52 fetch (2026-06-26, off main 4514354b; sley-remote fetch lane): -t alias,
    # empty-source refspec, dup-refspec dedup, followRemoteHEAD warn output,
    # fetch --atomic (single transaction + reference-transaction hook + non-ff abort +
    # FETCH_HEAD truncate-on-abort), and a latent FETCH_HEAD opportunistic-tracking dup
    # fix. 167->184. Guards held: t5516=111 t5601=109 t5505=127 t5500=363. cargo test
    # --workspace green; clippy -p sley-remote -p sley-cli clean.
    [t5510-fetch.sh]=184
    [t2205-add-worktree-config.sh]=13
    [t1050-large.sh]=29
    # wave-36 (2026-06-21, off main 5dfe861a; t_fetch/worktree): fetch-multiple, unresolve-info,
    # worktree-heads. t5514-fetch-multiple 17->25 FULL (--all/--multiple/groups, Fetching-<remote>
    # framing, continue-on-error, --jobs; banked 24 w/ 1-cell multi-fetch margin). t2030-unresolve-info
    # 3->13/14 (checkout -m resolve-undo / conflict recreation from index stages; 1 hard cell left).
    # t2407-worktree-heads 2->12 FULL (cross-worktree ref-in-use protection: branch/checkout/rebase/
    # fetch refuse a ref checked out in another worktree). unresolve + wtheads both edited
    # sley-worktree/lib.rs — auto-merged clean AND both t2030=13 + t2407=12 re-verified on the MERGED
    # binary. fetchmulti disjoint. Guards held/gained: t2400=219(banked 218) t2401=13 t2402=27 t5510=170
    # t5516=110 t5526=56 t5601=108 t2018=25 t2020=26 t2070=15 t4200=34 t3404=121 t7508=119. cargo test green.
    [t5514-fetch-multiple.sh]=25
    [t2030-unresolve-info.sh]=14
    [t2407-worktree-heads.sh]=12
    # wave-37 (2026-06-21, post-sweep 84.7% off main 7f5514ba; t_push/diff/worktree-setup):
    # pre-push-hook, patch-id, cwd-empty. t5571-pre-push-hook 1->11 FULL (hook argv/stdin ref-feed +
    # abort-on-failure; SELF-HEALED a pre-existing aspirational floor=11 that was reddening the weekly
    # gate, same pattern as t1404). t4204-patch-id 5->25/26 (stable hunk-canonicalized hash, multi-patch
    # split, --stable/--verbatim; +log/format-patch -O orderfile for fixtures). t2501-cwd-empty 3->24
    # FULL (repo discovery when cwd is rmdir'd: checkout/reset/merge/cherry-pick/rebase/revert/clean/
    # stash/rm/submodule via GIT_DIR fallback). patchid + cwdempty both edited sley-cli/lib.rs —
    # auto-merged clean, both t4204=25 + t2501=24 verified on MERGED binary. cwdempty's git-rm-rf cwd
    # handling regressed t3600-rm 81->80 (trailing-slash pathspec); fix-round bad2376e restored ->81.
    # prepushhook disjoint. Guards held/gained: t3600=81 t3700=51 t4013=216 t4015=129 t5510=170 t5516=110
    # t5526=56 t5601=108 t3404=121 t1500=81 t1501=39 t7508=119 t2070=15. cargo test green.
    [t4204-patch-id.sh]=25
    [t2501-cwd-empty.sh]=24
    # wave-38 (2026-06-21, 78% mid areas off main 042e13c0): gc, blame-corner, read-tree-2way.
    # t6500-gc 15->35 FULL (plain/auto gc, gc.log/gc.pid, pack-refs, reflog expiry, prune/no-prune,
    # cruft repacks, --keep-largest-pack, commit-graph; raised existing floor 14->34). t8003-blame-
    # corner-cases 16->30 FULL (-C/-M copy+rename origins, -f path, porcelain coalescing, index-backed
    # Not-Committed-Yet, HEAD^.. range). t1002-read-tree-m-u-2way 3->22 FULL (root cause: diff-files -p
    # crashed on missing index during the test helper; fixed racy filter to treat missing index empty).
    # NO file collisions (pack.rs/plumbing.rs/remote_cmds.rs | blame.rs | diff_files.rs). All 3 verified
    # on MERGED binary. Guards held/gained: t5300=57 t5304=32 t5324=25 t5510=170 t4013=216 t2018=25
    # t2020=26 t3600=81 t3700=51 t7508=119. cargo test green.
    [t8003-blame-corner-cases.sh]=30
    [t1002-read-tree-m-u-2way.sh]=22
    # wave-40 (2026-06-24): rebase/am cluster, 3 lanes onto e9f8c92b ->1d3a656c.
    # Lane A (apply engine, sley-diff-merge): git-faithful name resolution (-p<n>,
    # --directory, c-quoting, traditional/SVN), --unidiff-zero placement, path-escape
    # safety, -R reverse. New gate files (were untracked): t4104=24 (full), t4120=12
    # (full), t4128=12 (full), t4139=12 (full), t4135=18, t4119=8.
    [t4104-apply-boundary.sh]=24
    [t4120-apply-popt.sh]=12
    [t4128-apply-root.sh]=12
    [t4139-apply-escape.sh]=12
    [t4135-apply-weird-filenames.sh]=19
    [t4119-apply-config.sh]=11
    # Lane B (rebase sequencer): --empty disposition, --root --onto, topology no-op,
    # copy-notes. New gate files: t3424=19 (empty, was 0), t3412=25 (root, full),
    # t3421=61 (topology). (t3400/t3404/t3420 raised in place above.)
    [t3424-rebase-empty.sh]=19
    [t3412-rebase-root.sh]=25
    [t3421-rebase-topology-linear.sh]=63
    # Lane C (am state machine): faithful clean_index abort/skip, unborn-branch,
    # --retry+option-override, am -i, rerere, --directory. New gate files:
    # t4151=20 (abort, was 0 -> full), t4257=4 (interactive, full), t4153=4, t4252=2.
    [t4151-am-abort.sh]=20
    [t4257-am-interactive.sh]=4
    [t4153-am-resume-override-opts.sh]=5
    [t4252-am-options.sh]=7
    # wave-41 (2026-06-24): apply engine round 2 — binary codec, 3-way fallback,
    # typechange, index/mode. New gate files: t4103=24 (binary, was 5 -> full),
    # t4114=12 (typechange, full), t4129=19 (samemode), t4111=4 (subdir).
    # (t4108 raised 4->17 in place above.)
    [t4103-apply-binary.sh]=24
    [t4114-apply-typechange.sh]=12
    [t4129-apply-samemode.sh]=23
    [t4111-apply-subdir.sh]=10
    [t8013-blame-ignore-revs.sh]=19
    [t8011-blame-split-file.sh]=10
    [t8014-blame-ignore-fuzzy.sh]=16
    [t8008-blame-formats.sh]=5
    [t8006-blame-textconv.sh]=16
    [t8007-cat-file-textconv.sh]=15
    [t8010-cat-file-filters.sh]=9
    [t8004-blame-with-conflicts.sh]=3
    [t8015-blame-diff-algorithm.sh]=3
    [t0028-working-tree-encoding.sh]=22
    [t0022-crlf-rename.sh]=2
    [t6434-merge-recursive-rename-options.sh]=27
    [t6412-merge-large-rename.sh]=10
    [t7525-status-rename.sh]=15
    [t4001-diff-rename.sh]=23
    # wave-54 diff basic: non-recursive raw file/tree replacements, reverse
    # same-path add/delete ordering, diff-files ENOTDIR handling, and
    # --no-index stdin side support.
    [t4002-diff-basic.sh]=63
    [t4007-rename-3.sh]=11
    [t4003-diff-rename-1.sh]=4
    [t4023-diff-rename-typechange.sh]=1
    [t6111-rev-list-treesame.sh]=65
    [t4137-apply-submodule.sh]=24
    [t3512-cherry-pick-submodule.sh]=13
    [t3513-revert-submodule.sh]=9
    [t3426-rebase-submodule.sh]=23
    [t6415-merge-dir-to-symlink.sh]=10
    [t6417-merge-ours-theirs.sh]=5
    [t5553-set-upstream.sh]=21
    [t3306-notes-prune.sh]=12
    [t3308-notes-merge.sh]=19
    [t3310-notes-merge-manual-resolve.sh]=22
    [t6406-merge-attr.sh]=4
    [t6418-merge-text-auto.sh]=5
    [t6427-diff3-conflict-markers.sh]=8
    [t6432-merge-recursive-space-options.sh]=10
    [t6439-merge-co-error-msgs.sh]=1
    [t2071-restore-patch.sh]=15
    [t2080-parallel-checkout-basics.sh]=4
    [t2026-checkout-pathspec-file.sh]=11
    [t2017-checkout-orphan.sh]=13
    [t2021-checkout-overwrite.sh]=4
    [t2025-checkout-no-overlay.sh]=6
    [t2022-checkout-paths.sh]=3
    [t7425-submodule-gitdir-path-extension.sh]=10
    [t7403-submodule-sync.sh]=18
    [t7426-submodule-get-default-remote.sh]=4
    [t7408-submodule-reference.sh]=14
    [t7416-submodule-dash-url.sh]=18
    [t7412-submodule-absorbgitdirs.sh]=12
    [t7424-submodule-mixed-ref-formats.sh]=3
    [t3007-ls-files-recurse-submodules.sh]=24
    [t3013-ls-files-format.sh]=20
    [t3060-ls-files-with-tree.sh]=8
    [t3005-ls-files-relative.sh]=4
    [t1600-index.sh]=7
    [t0602-reffiles-fsck.sh]=24
    [t0600-reffiles-backend.sh]=19
    [t0614-reftable-fsck.sh]=6
    [t0613-reftable-write-options.sh]=2
    [t4056-diff-order.sh]=23
    [t7814-grep-recurse-submodules.sh]=25
    [t7811-grep-open.sh]=10
    [t7817-grep-sparse-checkout.sh]=5
    [t0014-alias.sh]=21
    [t0033-safe-directory.sh]=22
    [t0035-safe-bare-repository.sh]=13
    [t0092-diagnose.sh]=4
    [t0068-for-each-repo.sh]=4
    [t0009-git-dir-validation.sh]=6
    [t4069-remerge-diff.sh]=2
    [t4030-diff-textconv.sh]=19
    [t4042-diff-textconv-caching.sh]=5
    [t4063-diff-blobs.sh]=6
    [t4048-diff-combined-binary.sh]=4
    [t4012-diff-binary.sh]=12
    [t4031-diff-rewrite-binary.sh]=3
    [t4022-diff-rewrite.sh]=5
    [t4046-diff-unmerged.sh]=2
    [t4065-diff-anchored.sh]=7
    [t4070-diff-pairs.sh]=1
    [t4073-diff-stat-name-width.sh]=6
    [t4212-log-corrupt.sh]=13
    [t4213-log-tabexpand.sh]=8
    [t4207-log-decoration-colors.sh]=1
    [t8005-blame-i18n.sh]=5
    [t0410-partial-clone.sh]=38
    # wave-52 graphpack (commit-graph + pack-objects cluster): pack-objects
    # --stdin-packs standard mode (t5331 1->13, NEW floor); git pack-redundant
    # full impl (t5323 4->18 full, NEW floor); commit-graph --stdin-commits tag
    # peeling + hash-version-mismatch warning (t5318 96->98, raised 95->98);
    # commit-graph write honors core.sharedRepository (t5324 25->26, raised
    # 25->26). Guards held: t5300=55 t5302=31 t5304=32 t5310=221 t5319=95
    # t5326=345 t6012=42 t4202=131. cargo test --workspace green.
    # wave-53 focused facade/parity hardening: config include, archive attrs,
    # rev-list stdin/count/parents/filter edges, merge-base, partial-clone
    # materialization for stdin-packs, checkout no-overlay stage removal, and
    # pathspec glob/literal/exclude all full-pass in a 14-script wave.
    [t1305-config-include.sh]=37
    [t5001-archive-attr.sh]=44
    [t5002-archive-attr-pattern.sh]=19
    [t5331-pack-objects-stdin.sh]=18
    [t6005-rev-list-count.sh]=6
    [t6010-merge-base.sh]=12
    [t6017-rev-list-stdin.sh]=37
    [t6101-rev-parse-parents.sh]=38
    [t6130-pathspec-noglob.sh]=21
    [t6137-pathspec-wildcards-literal.sh]=25
    # wave-53 outside-scope promotion: small adjacent scripts verified locally.
    # t3305-notes-fanout stayed out because it timed out after four assertions
    # under the 120s candidate-wave ceiling; do not enroll it until bounded.
    [t1100-commit-tree-options.sh]=5
    [t1303-wacky-config.sh]=11
    [t3102-ls-tree-wildcards.sh]=3
    [t3104-ls-tree-format.sh]=19
    [t3304-notes-mixed.sh]=6
    [t3307-notes-man.sh]=3
    [t3601-rm-pathspec-file.sh]=5
    [t4006-diff-mode.sh]=7
    [t4016-diff-quote.sh]=5
    [t4101-apply-nonl.sh]=12
    [t7513-interpret-trailers.sh]=99
    # wave-54 outside-scope promotion: focused candidate fixes for embeddable
    # facade parity neighbors. Git-var/config path variables, configurable
    # stripspace comments, add pathspec-file option errors, no-op apply --check,
    # and update-server-info no-op mtime preservation are now full-pass.
    [t0007-git-var.sh]=27
    [t0030-stripspace.sh]=30
    [t3704-add-pathspec-file.sh]=11
    [t4136-apply-check.sh]=6
    [t5200-update-server-info.sh]=8
    [t5323-pack-redundant.sh]=18
    # wave-55 floor-all-selected catch-up: every selected script now has an
    # explicit ok-count floor from /private/tmp/sley-expanded-559-with-diffbasic-
    # summary.csv. Low/zero floors are intentional measurement guards: they catch
    # drops, timeouts, and missing rows without claiming the script is complete.
    [t0002-gitfile.sh]=14
    [t0056-git-C.sh]=6
    [t0101-at-syntax.sh]=6
    [t1001-read-tree-m-2way.sh]=27
    [t1008-read-tree-overlay.sh]=1
    [t1012-read-tree-df.sh]=5
    [t1051-large-conversion.sh]=12
    [t1307-config-blob.sh]=13
    [t1311-config-optional.sh]=3
    [t1405-main-ref-store.sh]=16
    [t1411-reflog-show.sh]=10
    [t1418-reflog-exists.sh]=3
    [t1504-ceiling-dirs.sh]=42
    [t2003-checkout-cache-mkdir.sh]=9
    [t2008-checkout-subdir.sh]=9
    [t2014-checkout-switch.sh]=4
    [t2082-parallel-checkout-attributes.sh]=0
    [t2103-update-index-ignore-missing.sh]=5
    [t2108-update-index-refresh-racy.sh]=6
    [t2206-add-submodule-ignored.sh]=5
    [t2405-worktree-submodule.sh]=4
    [t3003-ls-files-exclude.sh]=7
    [t3011-common-prefixes-and-directory-traversal.sh]=21
    [t5305-include-tag.sh]=9
    [t5309-pack-delta-cycles.sh]=4
    [t5314-pack-cycle-detection.sh]=2
    [t5321-pack-large-objects.sh]=2
    [t5328-commit-graph-64bit-time.sh]=7
    [t5334-incremental-multi-pack-index.sh]=20
    [t6000-rev-list-misc.sh]=13
    [t6008-rev-list-submodule.sh]=2
    [t6013-rev-list-reverse-parents.sh]=2
    [t6018-rev-list-glob.sh]=90
    [t6050-replace.sh]=6
    [t6102-rev-list-unexpected-objects.sh]=13
    [t6134-pathspec-in-submodule.sh]=2
    [t6700-tree-depth.sh]=2
    [t7104-reset-hard.sh]=3
    [t7515-status-symlinks.sh]=3
    [t7702-repack-cyclic-alternate.sh]=2
    [t0062-revision-walking.sh]=2
    [t1003-read-tree-prefix.sh]=3
    [t1009-read-tree-new-index.sh]=3
    [t1015-read-index-unmerged.sh]=5
    [t1090-sparse-checkout-scope.sh]=4
    [t1308-config-set.sh]=37
    [t1350-config-hooks-path.sh]=3
    [t1407-worktree-ref-store.sh]=4
    [t1413-reflog-detach.sh]=7
    [t1421-reflog-write.sh]=10
    [t1505-rev-parse-last.sh]=7
    [t1601-index-bogus.sh]=2
    [t2005-checkout-index-symlinks.sh]=1
    [t2009-checkout-statinfo.sh]=3
    [t2015-checkout-unborn.sh]=6
    [t2027-checkout-track.sh]=3
    [t2072-restore-pathspec-file.sh]=12
    [t2100-update-cache-badpath.sh]=1
    [t2104-update-index-skip-worktree.sh]=7
    [t2200-add-update.sh]=18
    [t2300-cd-to-toplevel.sh]=0
    [t2500-untracked-overwriting.sh]=4
    [t3004-ls-files-basic.sh]=4
    [t3008-ls-files-lazy-init-name-hash.sh]=1
    [t3012-ls-files-dedup.sh]=1
    [t3201-branch-contains.sh]=24
    # Git 2.55.0's t5003 plan is 78, and the current oracle run passes all
    # 78 cells. The older floor=81 came from a stale 2.54-era summary whose
    # plan is now impossible to satisfy under the current archive-zip script.
    [t5003-archive-zip.sh]=81
    [t5306-pack-nobase.sh]=4
    [t5311-pack-bitmaps-shallow.sh]=6
    [t5315-pack-objects-compression.sh]=5
    [t5322-pack-objects-sparse.sh]=11
    [t5330-no-lazy-fetch-with-commit-graph.sh]=2
    [t5335-compact-multi-pack-index.sh]=10
    [t6009-rev-list-parent.sh]=9
    [t6014-rev-list-all.sh]=3
    [t6019-rev-list-ancestry-path.sh]=11
    [t6110-rev-list-sparse.sh]=2
    [t6136-pathspec-in-bare.sh]=1
    [t6501-freshen-objects.sh]=42
    [t7105-reset-patch.sh]=5
    [t7519-status-fsmonitor.sh]=19
    [t0034-root-safe-directory.sh]=0
    [t0411-clone-from-partial.sh]=7
    [t1004-read-tree-m-u-wf.sh]=14
    [t1010-mktree.sh]=6
    [t1020-subdirectory.sh]=15
    [t1309-early-config.sh]=8
    [t1402-check-ref-format.sh]=97
    [t1408-packed-refs.sh]=3
    [t1414-reflog-walk.sh]=5
    [t1422-show-ref-exists.sh]=13
    [t1509-root-work-tree.sh]=0
    [t1515-rev-parse-outside-repo.sh]=4
    # Git 2.55.0's t2000 plan is 6; Sley passes every current checkout-index
    # conflict cell. The older floor=10 came from a stale upstream plan.
    [t2000-conflict-when-checking-files-out.sh]=6
    [t2006-checkout-index-basic.sh]=7
    [t2010-checkout-ambiguous.sh]=7
    [t2023-checkout-m.sh]=5
    [t2050-git-dir-relative.sh]=4
    [t2101-update-index-reupdate.sh]=7
    [t2105-update-index-gitfile.sh]=4
    [t2201-add-update-typechange.sh]=2
    [t2403-worktree-move.sh]=27
    [t3001-ls-files-others-exclude.sh]=17
    [t3009-ls-files-others-nonsubmodule.sh]=2
    [t3211-peel-ref.sh]=8
    [t5004-archive-corner-cases.sh]=14
    [t5307-pack-missing-commit.sh]=5
    [t5312-prune-corruption.sh]=7
    [t5316-pack-delta-depth.sh]=0
    [t5351-unpack-large-objects.sh]=4
    [t6004-rev-list-path-optim.sh]=7
    [t6016-rev-list-graph-simplify-history.sh]=7
    [t6021-rev-list-exclude-hidden.sh]=55
    [t6100-rev-list-in-order.sh]=2
    [t6131-pathspec-icase.sh]=0
    [t6600-test-reach.sh]=44
    [t7062-wtstatus-ignorecase.sh]=1
    [t7106-reset-unborn-branch.sh]=4
    [t1000-read-tree-m-3way.sh]=83
    [t1005-read-tree-reset.sh]=7
    [t1011-read-tree-sparse-checkout.sh]=19
    [t1022-read-tree-partial-clone.sh]=0
    [t1306-xdg-files.sh]=21
    [t1310-config-default.sh]=5
    [t1403-show-ref.sh]=10
    [t1409-avoid-packing-refs.sh]=11
    [t1415-worktree-refs.sh]=5
    [t1503-rev-parse-verify.sh]=12
    [t1511-rev-parse-caret.sh]=10
    [t1517-outside-repo.sh]=8
    [t2002-checkout-cache-u.sh]=3
    [t2007-checkout-symlink.sh]=2
    [t2011-checkout-invalid-head.sh]=9
    [t2019-checkout-ambiguous-ref.sh]=7
    [t2060-switch.sh]=9
    [t2081-parallel-checkout-collisions.sh]=1
    [t2102-update-index-symlinks.sh]=2
    [t2106-update-index-assume-unchanged.sh]=2
    [t2202-add-addremove.sh]=3
    [t2404-worktree-config.sh]=9
    [t3002-ls-files-dashpath.sh]=6
    [t3006-ls-files-long.sh]=3
    [t3010-ls-files-killed-modified.sh]=7
    [t3020-ls-files-error-unmatch.sh]=3
    [t5301-sliding-window.sh]=6
    [t5308-pack-detect-duplicates.sh]=4
    [t5313-pack-bounds-checks.sh]=3
    [t5320-delta-islands.sh]=3
    [t5325-reverse-index.sh]=5
    [t5333-pseudo-merge-bitmaps.sh]=24
    [t6011-rev-list-with-bad-commit.sh]=3
    [t6041-bisect-submodule.sh]=9
    [t6114-keep-packs.sh]=3
    [t6601-path-walk.sh]=36
    [t7101-reset-empty-subdirs.sh]=10
    [t7107-reset-pathspec-file.sh]=1
    [t7511-status-index.sh]=24
    [t7701-repack-unpack-unreachable.sh]=1
    # wave-56 unselected-script promotions: full-pass scripts with non-empty
    # assertion counts from /private/tmp/sley-new-candidates-summary.csv.
    [t0006-date.sh]=149
    [t0017-env-helper.sh]=5
    [t0019-json-writer.sh]=16
    [t0023-crlf-am.sh]=2
    [t0024-crlf-archive.sh]=3
    [t0066-dir-iterator.sh]=10
    [t0067-parse_pathspec_file.sh]=8
    [t0070-fundamental.sh]=11
    [t0071-sort.sh]=1
    [t0081-find-pack.sh]=4
    [t0095-bloom.sh]=11
    [t0601-reffiles-pack-refs.sh]=47
    [t1412-reflog-loop.sh]=3
    [t1419-exclude-refs.sh]=13
    [t1451-fsck-buffer.sh]=72
    [t4112-apply-renames.sh]=2
    [t4118-apply-empty-context.sh]=3
    [t4121-apply-diffs.sh]=2
    # wave-57 unselected-script promotions: second scout wave full-pass scripts.
    [t4024-diff-optimize-common.sh]=2
    [t4028-format-patch-mime-headers.sh]=3
    [t4037-diff-r-t-dirs.sh]=2
    [t4054-diff-bogus-tree.sh]=14
    [t4071-diff-minimal.sh]=1
    [t4074-diff-shifted-matched-group.sh]=4
    [t4113-apply-ending.sh]=3
    [t4116-apply-reverse.sh]=7
    [t6407-merge-binary.sh]=3
    [t7111-reset-table.sh]=42
    [t7605-merge-resolve.sh]=4
    [t7608-merge-messages.sh]=5
    # wave-58 unselected-script promotions: third scout wave full-pass scripts.
    [t3908-stash-in-worktree.sh]=2
    [t4036-format-patch-signer-mime.sh]=5
    [t4057-diff-combined-paths.sh]=4
    [t4123-apply-shrink.sh]=2
    [t4125-apply-ws-fuzz.sh]=4
    [t4131-apply-fake-ancestor.sh]=3
    [t4138-apply-ws-expansion.sh]=5
    [t6400-merge-df.sh]=7
    [t6414-merge-rename-nocruft.sh]=3
    # wave-59 unselected-script promotions: fourth scout wave full-pass scripts.
    [t1406-submodule-ref-store.sh]=15
    [t3040-subprojects-basic.sh]=11
    [t3050-subprojects-fetch.sh]=4
    [t5405-send-pack-rewind.sh]=3
    [t5406-remote-rejects.sh]=3
    [t5507-remote-environment.sh]=5
    [t5513-fetch-track.sh]=2
    [t5522-pull-symlink.sh]=4
    [t5524-pull-msg.sh]=3
    [t5525-fetch-tagopt.sh]=5
    [t5527-fetch-odd-refs.sh]=5
    [t5573-pull-verify-signatures.sh]=16
    [t5609-clone-branch.sh]=7
    [t5701-git-serve.sh]=25
    [t5750-bundle-uri-parse.sh]=13
    # wave-60 target coverage enrollment: measured non-email/non-legacy scripts.
    # Zero floors are intentional weak-signal guards for scripts currently
    # producing 0 TAP cells; they still catch missing rows and timeouts.
    [t0018-advice.sh]=4
    [t0025-crlf-renormalize.sh]=1
    [t0026-eol-config.sh]=6
    [t0029-core-unsetenvvars.sh]=0
    [t0031-lockfile-pid.sh]=3
    [t0041-usage.sh]=10
    [t0050-filesystem.sh]=6
    [t0055-beyond-symlinks.sh]=1
    [t0060-path-utils.sh]=198
    [t0090-cache-tree.sh]=23
    [t0091-bugreport.sh]=9
    [t0100-previous.sh]=5
    [t0200-gettext-basic.sh]=16
    [t0201-gettext-fallbacks.sh]=8
    [t0302-credential-store.sh]=59
    [t1016-compatObjectFormat.sh]=0
    [t1301-shared-repo.sh]=7
    [t1302-repo-version.sh]=14
    [t1420-lost-found.sh]=1
    [t1901-repo-structure.sh]=0
    [t3202-show-branch.sh]=16
    [t3204-branch-name-interpretation.sh]=16
    [t3205-branch-color.sh]=4
    [t3207-branch-submodule.sh]=3
    [t3300-funny-names.sh]=13
    [t3303-notes-subtrees.sh]=23
    [t3320-notes-merge-worktrees.sh]=9
    [t3321-notes-stripspace.sh]=27
    [t3500-cherry.sh]=0
    [t3702-add-edit.sh]=2
    [t3703-add-magic-pathspec.sh]=4
    [t3901-i18n-patch.sh]=20
    [t3902-quoted.sh]=8
    [t3904-stash-patch.sh]=4
    [t3905-stash-include-untracked.sh]=28
    [t3906-stash-submodule.sh]=5
    [t3907-stash-show-config.sh]=5
    [t3909-stash-pathspec-file.sh]=4
    [t4000-diff-format.sh]=35
    [t4004-diff-rename-symlink.sh]=3
    [t4005-diff-rename-2.sh]=3
    [t4008-diff-break-rewrite.sh]=10
    [t4009-diff-rename-4.sh]=6
    [t4010-diff-pathspec.sh]=15
    [t4025-hunk-header.sh]=1
    [t4026-color.sh]=17
    [t4029-diff-trailing-space.sh]=0
    [t4032-diff-inter-hunk-context.sh]=28
    [t4033-diff-patience.sh]=9
    [t4035-diff-quiet.sh]=18
    [t4038-diff-combined.sh]=15
    [t4039-diff-assume-unchanged.sh]=1
    [t4040-whitespace-status.sh]=7
    [t4043-diff-rename-binary.sh]=2
    [t4044-diff-index-unique-abbrev.sh]=1
    [t4049-diff-stat-count.sh]=4
    [t4050-diff-histogram.sh]=8
    [t4053-diff-no-index.sh]=41
    [t4055-diff-context.sh]=10
    [t4058-diff-duplicates.sh]=16
    [t4059-diff-submodule-not-initialized.sh]=1
    [t4062-diff-pickaxe.sh]=1
    [t4064-diff-oidfind.sh]=10
    [t4066-diff-emit-delay.sh]=1
    [t4102-apply-rename.sh]=4
    [t4105-apply-fuzz.sh]=5
    [t4106-apply-stdin.sh]=1
    [t4107-apply-ignore-whitespace.sh]=9
    [t4109-apply-multifrag.sh]=0
    [t4110-apply-scan.sh]=0
    [t4115-apply-symlink.sh]=5
    [t4122-apply-symlink-inside.sh]=4
    [t4126-apply-empty.sh]=4
    [t4127-apply-same-fn.sh]=2
    [t4130-apply-criss-cross-rename.sh]=5
    [t4132-apply-removal.sh]=11
    [t4133-apply-filenames.sh]=2
    [t4134-apply-submodule.sh]=1
    [t4140-apply-ita.sh]=2
    [t4206-log-follow-harder-copies.sh]=6
    [t4208-log-magic-pathspec.sh]=12
    [t4210-log-i18n.sh]=21
    [t4217-log-limit.sh]=1
    [t4300-merge-tree.sh]=17
    [t5150-request-pull.sh]=2
    [t5401-update-hooks.sh]=11
    [t5409-colorize-remote-messages.sh]=4
    [t5410-receive-pack.sh]=1
    # t5411 198->230 (2026-07-07): local push routed through serve_receive_pack
    # (ea4977df); remaining 124 = report-option/rewrite cells + HTTP push routing.
    [t5411-proc-receive-hook.sh]=271
    [t5501-fetch-push-alternates.sh]=3
    [t5502-quickfetch.sh]=2
    [t5503-tagfollow.sh]=6
    [t5506-remote-groups.sh]=7
    [t5509-fetch-push-namespaces.sh]=4
    [t5517-push-mirror.sh]=13
    [t5518-fetch-exit-status.sh]=2
    [t5519-push-alternates.sh]=8
    [t5529-push-errors.sh]=5
    [t5530-upload-pack-error.sh]=3
    [t5531-deep-submodule-push.sh]=29
    [t5534-push-signed.sh]=9
    [t5535-fetch-push-symref.sh]=2
    [t5536-fetch-conflicts.sh]=4
    [t5538-push-shallow.sh]=2
    [t5544-pack-objects-hook.sh]=2
    [t5546-receive-limits.sh]=14
    [t5547-push-quarantine.sh]=6
    [t5552-skipping-fetch-negotiator.sh]=0
    [t5554-noop-fetch-negotiator.sh]=0
    [t5565-push-multiple.sh]=1
    [t5574-fetch-output.sh]=2
    [t5582-fetch-negative-refspec.sh]=10
    [t5583-push-branches.sh]=4
    [t5600-clone-fail-cleanup.sh]=7
    [t5602-clone-remote-exec.sh]=2
    [t5606-clone-options.sh]=21
    [t5607-clone-bundle.sh]=8
    [t5610-clone-detached.sh]=13
    [t5612-clone-refspec.sh]=13
    [t5613-info-alternate.sh]=10
    [t5615-alternate-env.sh]=7
    [t5618-alternate-refs.sh]=2
    [t5619-clone-local-ambiguous-transport.sh]=0
    [t5621-clone-revision.sh]=12
    [t6401-merge-criss-cross.sh]=3
    [t6404-recursive-merge.sh]=4
    [t6405-merge-symlinks.sh]=6
    [t6408-merge-up-to-date.sh]=7
    [t6409-merge-subtree.sh]=7
    [t6411-merge-filemode.sh]=17
    [t6413-merge-crlf.sh]=2
    [t7007-show.sh]=10
    [t7113-post-index-change-hook.sh]=4
    [t7603-merge-reduce-heads.sh]=11
    [t7606-merge-custom.sh]=2
    [t7816-grep-binary-pattern.sh]=145
    # wave-61 target coverage enrollment: non-timeout rows from the remaining
    # non-risk target scripts.
    [t0000-basic.sh]=88
    [t0004-unwritable.sh]=5
    [t0005-signals.sh]=2
    [t0010-racy-git.sh]=10
    [t0013-sha1dc.sh]=1
    [t0040-parse-options.sh]=94
    [t0051-windows-named-pipe.sh]=0
    [t0052-simple-ipc.sh]=9
    [t0061-run-command.sh]=24
    [t0080-unit-test-output.sh]=1
    [t0202-gettext-perl.sh]=1
    [t0203-gettext-setlocale-sanity.sh]=2
    [t0204-gettext-reencode-sanity.sh]=8
    [t0210-trace2-normal.sh]=14
    [t0211-trace2-perf.sh]=17
    [t0212-trace2-event.sh]=11
    [t0213-trace2-ancestry.sh]=5
    [t0300-credentials.sh]=52
    # t0301 FULL 52 (2026-07-07): spawn_daemon retry loop + SUN_LEN chdir bind fix (142e31a6).
    [t0301-credential-cache.sh]=52
    [t0303-credential-external.sh]=23
    [t0450-txt-doc-vs-help.sh]=788
    [t0500-progress-display.sh]=16
    [t0611-reftable-httpd.sh]=1
    [t0612-reftable-jgit-compatibility.sh]=0
    [t1304-default-acl.sh]=4
    [t3302-notes-index-expensive.sh]=12
    [t3401-rebase-and-am-rename.sh]=7
    [t3402-rebase-merge.sh]=13
    [t3405-rebase-malformed.sh]=5
    [t3407-rebase-abort.sh]=17
    [t3408-rebase-multi-line.sh]=2
    [t3409-rebase-environ.sh]=3
    [t3413-rebase-hook.sh]=15
    [t3416-rebase-onto-threedots.sh]=18
    [t3417-rebase-whitespace-fix.sh]=1
    [t3419-rebase-patch-id.sh]=8
    [t3423-rebase-reword.sh]=3
    [t3425-rebase-topology-merges.sh]=13
    [t3427-rebase-subtree.sh]=1
    [t3428-rebase-signoff.sh]=6
    [t3429-rebase-edit-todo.sh]=7
    [t3431-rebase-fork-point.sh]=22
    [t3433-rebase-across-mode-change.sh]=2
    [t3434-rebase-i18n.sh]=6
    [t3435-rebase-gpg-sign.sh]=0
    [t3436-rebase-more-options.sh]=19
    [t3438-rebase-broken-files.sh]=1
    [t3440-rebase-trailer.sh]=1
    [t3450-history.sh]=0
    [t3451-history-reword.sh]=0
    [t3452-history-split.sh]=0
    [t3503-cherry-pick-root.sh]=6
    [t3504-cherry-pick-rerere.sh]=4
    [t3506-cherry-pick-ff.sh]=11
    [t3508-cherry-pick-many-commits.sh]=11
    [t3509-cherry-pick-merge-df.sh]=6
    [t3511-cherry-pick-x.sh]=14
    [t3514-cherry-pick-revert-gpg.sh]=0
    [t3602-rm-sparse-checkout.sh]=4
    [t3910-mac-os-precompose.sh]=29
    [t4067-diff-partial-clone.sh]=1
    [t5532-fetch-proxy.sh]=4
    [t5580-unc-paths.sh]=0
    [t5614-clone-submodules-shallow.sh]=4
    [t5617-clone-submodules-remote.sh]=5
    [t5620-backfill.sh]=4
    [t5703-upload-pack-ref-in-want.sh]=10
    [t5710-promisor-remote-capability.sh]=2
    [t5730-protocol-v2-bundle-uri-file.sh]=6
    [t5731-protocol-v2-bundle-uri-git.sh]=4
    [t5801-remote-helpers.sh]=4
    [t5812-proto-disable-http.sh]=28
    [t5815-submodule-protos.sh]=8
    [t5900-repo-selection.sh]=8
    [t6403-merge-file.sh]=22
    [t6416-recursive-corner-cases.sh]=25
    [t6419-merge-ignorecase.sh]=0
    [t6421-merge-partial-clone.sh]=0
    [t6424-merge-unrelated-index-changes.sh]=16
    [t6425-merge-rename-delete.sh]=1
    [t6426-merge-skip-unneeded-updates.sh]=8
    [t6428-merge-conflicts-sparse.sh]=1
    [t6429-merge-sequence-rename-caching.sh]=3
    [t6431-merge-criscross.sh]=2
    [t6433-merge-toplevel.sh]=9
    [t6435-merge-sparse.sh]=6
    [t6436-merge-overwrite.sh]=11
    [t6437-submodule-merge.sh]=22
    [t7005-editor.sh]=10
    [t7008-filter-branch-null-sha1.sh]=3
    [t7010-setup.sh]=16
    [t7011-skip-worktree-reading.sh]=15
    [t7012-skip-worktree-writing.sh]=11
    [t7301-clean-interactive.sh]=6
    [t7402-submodule-rebase.sh]=6
    [t7409-submodule-detached-work-tree.sh]=3
    [t7411-submodule-config.sh]=20
    [t7413-submodule-is-active.sh]=10
    [t7414-submodule-mistakes.sh]=5
    [t7417-submodule-path-url.sh]=5
    [t7418-submodule-sparse-gitmodules.sh]=9
    [t7419-submodule-set-branch.sh]=9
    [t7420-submodule-set-url.sh]=3
    [t7421-submodule-summary-add.sh]=5
    [t7422-submodule-output.sh]=18
    [t7423-submodule-symlinks.sh]=6
    [t7503-pre-commit-and-pre-merge-commit-hooks.sh]=22
    [t7504-commit-msg-hook.sh]=29
    [t7505-prepare-commit-msg-hook.sh]=23
    [t7514-commit-patch.sh]=3
    [t7516-commit-races.sh]=0
    [t7517-per-repo-email.sh]=16
    [t7518-ident-corner-cases.sh]=5
    [t7520-ignored-hook-warning.sh]=5
    [t7521-ignored-mode.sh]=12
    [t7524-commit-summary.sh]=2
    [t7526-commit-pathspec-file.sh]=11
    [t7609-mergetool--lib.sh]=1
    [t7612-merge-verify-signatures.sh]=16
    [t7614-merge-signoff.sh]=4
    [t7615-diff-algo-with-mergy-operations.sh]=3
    [t7812-grep-icase-non-ascii.sh]=18
    [t7813-grep-icase-iso.sh]=2
    [t7815-grep-binary.sh]=15
    [t8009-blame-vs-topicbranches.sh]=1
    [t9002-column.sh]=1
    [t9003-help-autocorrect.sh]=1
    [t9210-scalar.sh]=8
    [t9211-scalar-clone.sh]=2
    [t9300-fast-import.sh]=256
    [t9301-fast-import-notes.sh]=14
    [t9302-fast-import-unpack-limit.sh]=3
    [t9303-fast-import-compression.sh]=8
    [t9304-fast-import-marks.sh]=7
    [t9305-fast-import-signatures.sh]=21
    [t9306-fast-import-signed-tags.sh]=19
    [t9350-fast-export.sh]=44
    [t9351-fast-export-anonymize.sh]=7
    [t9700-perl-git.sh]=3
    [t9901-git-web--browse.sh]=0
    # round5 completion (2026-07-07, off main f312ed0d): machine-readable helper
    # surfaces for bash completion (parse-options rendering, config/ref helpers,
    # helper-only command routing) + a `**` double-star fix in for-each-ref glob
    # matching (paired `refs/heads/*`/`refs/heads/*/**` patterns from __git_refs
    # now reach nested refs while single `*` still stays within a segment).
    # 185->258. Guards held: t0012-help 164/164, t0450-txt-doc-vs-help 794/794;
    # for_each_ref standalone parity tests green. Measured against /tmp/git-src
    # 2.55.0 source oracle (verify on next scheduled Linux run).
    [t9902-completion.sh]=258
    [t9903-bash-prompt.sh]=54

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
