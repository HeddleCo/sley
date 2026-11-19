"""Revert one security guard at a time; require an executed public-API RED.

Run from the root of an isolated git-archive source export using the configured target. Wait for its build lock.
Do not share a checkout with concurrent checks. Originals are restored on failure.
Logs and the result table go under the supplied scratch directory.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import time
import tempfile

scratch = Path(sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix='resolve-proofs-'))
scratch.mkdir(parents=True, exist_ok=True)
rp = Path('crates/sley-pack/src/resolved.rs')
pp = Path('crates/sley-pack/src/parallel_index.rs')
lp = Path('crates/sley-pack/src/limits.rs')
cp = Path('crates/sley-core/src/lib.rs')
original = {p: p.read_text() for p in (rp, pp, lp, cp)}
env = os.environ.copy()
env['RUSTUP_TOOLCHAIN'] = 'stable'
probes = []
def add(name, test, path, mutate, features=None):
    probes.append((name, test, path, mutate, features))
def region(s, begin, end, mutate):
    a = s.index(begin); b = s.index(end, a)
    return s[:a] + mutate(s[a:b]) + s[b:]
def gate_off(s):
    return s.replace('.min(self.entries.len()).min(slots)', '.min(self.entries.len())').replace('.filter(|next| *next <= self.limits.max_in_flight_bytes)', '.filter(|_| true)')
add('deprecated-atomic', 'real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions', rp, lambda s: region(s, '    fn retain<', '    fn replay<', lambda b: b.replace('.try_update(', '.fetch_update(')))
add('trusted-default-policy', 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply', pp, lambda s: s.replace('resolve_limits: ResolveLimits::unlimited()', 'resolve_limits: ResolveLimits::default()'))
for name, start, end in [('legacy-index-route', 'pub(crate) fn build_parallel_index', 'pub(crate) fn parse_parallel_pack'), ('legacy-parse-route', 'pub(crate) fn parse_parallel_pack', 'pub(crate) fn check_delta_depth')]:
    add(name, 'trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply', pp, lambda s, a=start, b=end: region(s, a, b, lambda block: block.replace('options.resolve_limits(),', 'ResolveLimits { max_total_decoded: 128, ..ResolveLimits::default() },')))
add('shared-flight-gates', 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory', rp, gate_off)
def per_item_leases(s):
    # Round 7 replaces per-root leases with one worker-entry lease. Revert that
    # policy together with the worker cap to reproduce the old arena fan-out.
    s = s.replace('.min(self.entries.len()).min(slots)', '.min(self.entries.len())')
    a=s.index('                        _flight = if self.limits.is_unlimited()'); b=s.index('                        let work =',a)
    s=s[:a] + "                        _flight = None::<Flight<'_>>;\n" + s[b:]
    lease='let _item_flight = if self.limits.is_unlimited() { None } else { Some(self.flight()?) };'
    s=s.replace('                                        let root = if i < roots.len()', lease+'\n                                        let root = if i < roots.len()')
    return s.replace('                                if let Some((node, body)) = item {', '                                if let Some((node, body)) = item {\n                                    '+lease)
add('worker-arena-bound', 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory', rp, per_item_leases)
add('flight-lease-lifetime', 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory', rp, lambda s: s.replace('                        let work =', '                        drop(_flight);\n                        let work ='))
add('flight-error-release', 'flight_reservations_release_after_errors_and_waiters_observe_cancel', rp, lambda s: s.replace('self.used.fetch_sub(self.bytes, AtomicOrdering::AcqRel);', 'self.used.fetch_sub(0, AtomicOrdering::AcqRel);'))
add('delta-stream-workspace', 'flight_reservations_release_after_errors_and_waiters_observe_cancel', rp, lambda s: s.replace('.and_then(|size| size.checked_add(d.header.size))', '.and_then(Some)'))
add('replay-base-workspace', 'flight_reservations_release_after_errors_and_waiters_observe_cancel', rp, lambda s: s.replace('max_body\n                .checked_add(d.result_size)', 'Some(d.result_size)'))
add('resolve-op-ceiling', 'million_tiny_leaves_stop_at_resolve_op_budget', rp, lambda s: s.replace('.filter(|next| *next <= limit)', '.filter(|_| true)'))
add('discovery-op-charge', 'resolve_ops_counts_discovery_inflate_apply_and_replay', rp, lambda s: s.replace('        resolve_op(metrics, limits.max_resolve_ops)?;', ''))
add('inflate-op-charge', 'resolve_ops_counts_discovery_inflate_apply_and_replay', rp, lambda s: region(s, '    fn inflate(&self,', '    fn apply(&self,', lambda b: b.replace('        self.operation()?;', '')))
add('apply-op-charge', 'resolve_ops_counts_discovery_inflate_apply_and_replay', rp, lambda s: region(s, '    fn apply(&self,', '    fn retain<', lambda b: b.replace('        self.operation()?;', '')))
add('bounded-preallocation', 'declared_count_preallocation_is_capped_before_parsing', lp, lambda s: s.replace('.min(PACK_OBJECT_COUNT_PREALLOC_CAP)', ''))
def thin(s):
    a = s.index('    if count != entries.len() as u64 {'); b = s.index('    cancel.check()?;', a)
    s = s[:a] + s[b:]
    return s.replace('let (oid, object_type, crc32, _) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;', 'let Some((oid, object_type, crc32, _)) = entry else { continue };')
add('thin-completion', 'thin_pack_is_an_unresolved_base_error', rp, thin)
add('caller-verifier-error', 'caller_admission_rejection_is_preserved_without_returning_a_build', rp, lambda s: s.replace('.map_err(ResolveError::Visitor)?;', '.ok();'))
# Round 5: the same byte-derived bound on both policies, with the existing
# 64K count cap retained so the physical-input assertion must detect the defect.
add('physical-preallocation-bound', 'lying_maximum_object_count_over_one_kib_has_bounded_allocation', lp, lambda s: s.replace('.min(entry_bytes / MIN_PACK_ENTRY_BYTES as usize)', ''))
for test in ['object_count_budget_is_typed', 'object_bytes_budget_is_typed', 'decoded_bytes_budget_is_typed', 'workspace_fit_budget_is_typed']:
    add('typed-' + test, test, rp, lambda s: region(s, 'fn budget(', '\nfn invalid(', lambda b: b[:b.index('    GitError::ResourceLimit')] + '    invalid(&format!("{kind}: limit {limit}, attempted {attempted}"))\n}'))
add('legacy-ops-index-typing', 'legacy_operation_budget_remains_typed', rp, lambda s: s.replace('budget(ResourceLimitKind::ResolveOps, limit, attempted)', 'invalid(&format!("resolve_ops {limit} {attempted}"))'))
add('legacy-ops-parse-typing', 'legacy_operation_budget_remains_typed', pp, lambda s: s.replace('.map_err(|e| resolved::into_git_error(e, |e| e))?', '.map_err(|_| GitError::InvalidFormat("untyped budget".into()))?'))
add('trusted-cancel-isolation', 'trusted_errors_leave_process_interrupt_clear', rp, lambda s: s.replace('        self.stop.cancel();', '        self.stop.cancel();\n        if let Some(source) = self.cancel.source() { source.cancel(); }'))
add('limited-fast-path-isolation', 'finite_limits_cannot_enter_unaccounted_trusted_discovery', rp, lambda s: region(s, '    fn is_unlimited(self)', '    /// Trusted legacy policy:', lambda b: '    fn is_unlimited(self) -> bool { true }\n'))
# Restore the duplicated per-entry output fields; the meter must catch their cost.
def duplicate_metadata(s):
    # Restore the removed per-object index/object copies while preserving the
    # shortest-depth field added since round 7. The capacity meter sees their cost.
    s=s.replace('type Output = (ObjectId, ObjectType, u32, u16);', 'type Output = (ObjectId, ObjectType, u32, u16, Option<(ObjectId, u64, u32)>, Option<(ObjectId, ObjectType, u64, u64)>);')
    s=s.replace('            depth,\n        ));', '            depth,\n            Some((oid, d.offset as u64, crc32fast::hash(&self.pack[d.offset..d.end]))),\n            Some((oid, object_type, body.len() as u64, d.offset as u64)),\n        ));',1)
    s=s.replace('let (oid, ty, _, depth) =', 'let (oid, ty, _, depth, _, _) =').replace('Some((oid, ty, crc, old)) =', 'Some((oid, ty, crc, old, _, _)) =')
    s=s.replace('Some((oid, ty, crc, depth));', 'Some((oid, ty, crc, depth, state.output[i].and_then(|o| o.4), state.output[i].and_then(|o| o.5)));')
    return s.replace('let (oid, object_type, crc32, _) = entry.ok_or_else', 'let (oid, object_type, crc32, _, _, _) = entry.ok_or_else')
add('original-metadata-layout', 'million_objects_fit_metadata_coefficient', rp, duplicate_metadata)
for stage in ['Inflate', 'DeltaResolution']:
    add('profile-' + stage, 'trusted_index_restores_inflate_and_delta_profile_spans', rp, lambda s, stage=stage: s.replace('sley_core::fetch_profile::Stage::' + stage, 'sley_core::fetch_profile::Stage::ObjectStoreWrite'), 'fetch-profile')

def progress_off(s):
    a = s.index('                if *count == initial && next > initial + 1 {')
    b = s.index('                *count = next;', a)
    return s[:a] + s[b:]
add('small-pack-progress', 'trusted_small_packs_keep_intermediate_progress', rp, progress_off)

add('untrusted-speculation', 'embedded_zlib_members_have_linear_discovery_cpu_cost', rp, lambda s: region(s, 'fn discover<E>(', '// Only an explicitly unlimited', lambda b: b.replace('if limits.is_unlimited() {', 'if true {')))

add('workspace-overflow-typing', 'workspace_arithmetic_overflow_stays_a_typed_budget_error', rp, lambda s: region(s, '    let overflow = ||', '    let max_body =', lambda b: '    let overflow = || invalid("pack workspace size overflow");\n'))

add('subtree-stealing', 'one_hot_family_is_stolen_by_multiple_workers', rp, lambda s: s.replace('item = pop((worker + other) % workers, true)?\n                                            .map(|node| (node, None));', 'let _ = other; item = None;'))
add('panic-unwind-stop', 'panicking_visitor_stops_idle_workers_before_watchdog', rp, lambda s: s.replace('let _unwind = UnwindStop(|| self.fail(ResolveError::Panic));', 'let _unwind = ();'))
def release_before_base(s):
    s=region(s, '    fn child<', '    // Shortest depth', lambda b: b.replace('        parent.release()?;\n',''))
    return s.replace('                                    let base = match body {', '                                    node.release()?;\n                                    let base = match body {')
add('dependent-release-order', 'last_dependent_releases_base_before_visiting_child', rp, release_before_base)

# Original hostile-input contracts retained by the scheduler change.
add('shared-live-gate', 'live_base_bound_is_shared_and_replay_stays_within_peak_memory', rp, lambda s: s.replace('.filter(|sum| *sum <= self.limits.max_live_base_bytes)', '.filter(|_| true)'))
for name,test in [('declared-body-bomb','declared_size_bomb_is_refused_before_allocation'), ('declared-result-bomb','delta_result_bomb_and_running_budgets_are_checked_before_visits')]:
    add(name,test,rp,lambda s: region(s,'fn size_check(', 'fn charge(',lambda b:b.replace('if size > limit {','if false {')))
add('depth-before-apply','depth_51_is_refused_before_visiting_the_deep_object',rp,lambda s:s.replace('if parent.depth as usize >= self.limits.max_delta_depth.min(u16::MAX as usize) {','if false {'))
add('truncated-discovery','truncation_and_trailing_bytes_are_errors',rp,lambda s:s.replace('return Err(invalid("truncated pack zlib stream"));','d.end = d.data + z.total_in() as usize; return Ok(());'))
def cancel_off(s):
    s=s.replace('        self.cancel.check()?;\n        self.stop_flag().check()', '        self.stop_flag().check()')
    return s.replace('                if let Err(error) = self.cancel.check() {\n                    self.fail(error.into());\n                }','')
add('cancel-polls','cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error',rp,cancel_off)
add('replayed-result-work','replayed_delta_result_budget_is_shared_across_workers',rp,lambda s:s.replace('self.work(0, 0, plan.result_size)?;', 'self.work(0, 0, 0)?;'))
add('compressed-replay-work','low_output_deflate_replay_spends_encoded_work_budget',rp,lambda s:s.replace('self.work((d.end - d.data) as u64, d.header.size, decoded)?;', 'self.work(0, d.header.size, decoded)?;'))

add('iterative-parent-cleanup', 'custom_deep_limit_releases_parent_chain_without_recursive_drop', rp, lambda s: region(s, '// Custom depth limits may exceed', "impl Node<'_> {", lambda _: ''))

def release_before_apply(s):
    return region(s, '    fn child<', '    // Shortest depth', lambda b: b.replace('        parent.release()?;\n','').replace('        let body = self.apply(index, base.as_ref(), &delta, false)?;', '        parent.release()?;\n        let body = self.apply(index, base.as_ref(), &delta, false)?;'))
add('dependent-release-before-apply', 'last_dependent_releases_base_before_visiting_child', rp, release_before_apply)

add('resource-kind-nonexhaustive', 'ResourceLimitKind', cp, lambda s: s.replace('#[non_exhaustive]\npub enum ResourceLimitKind {', 'pub enum ResourceLimitKind {'), 'core-doc')

# D1: removing only sibling-replay charges must reopen each amplification route.
def uncharged_replay(s):
    def skip(b):
        b=b.replace('        self.operation()?;', '        if !_replay { self.operation()?; }')
        b=b.replace('        self.work((d.end - d.data) as u64, d.header.size, decoded)?;', '        if !_replay { self.work((d.end - d.data) as u64, d.header.size, decoded)?; }')
        return b.replace('        self.work(0, 0, plan.result_size)?;', '        if !_replay { self.work(0, 0, plan.result_size)?; }')
    return region(s, '    fn inflate(&self,', '    fn retain<', skip)
for test in ['replay_work_cannot_exceed_the_shared_decoded_budget', 'low_output_deflate_replay_spends_encoded_work_budget', 'replayed_delta_result_budget_is_shared_across_workers', 'million_tiny_leaves_stop_at_resolve_op_budget', 'duplicate_ref_base_sibling_amplification_spends_the_shared_budget']:
    add('D1-' + test, test, rp, uncharged_replay)
add('maximum-depth-overflow', 'maximum_custom_depth_does_not_saturate_and_retry_forever', rp, lambda s: s.replace('state.output[base].and_then(|o| o.3.checked_add(1))', 'state.output[base].map(|o| o.3.saturating_add(1))'))
add('coordinator-unwind-stop', 'progress_panic_stops_workers_before_the_watchdog', rp, lambda s: s.replace('let _coordinator = UnwindStop(|| self.fail(ResolveError::Panic));', 'let _coordinator = ();'))
add('panic-error-kind', 'panicking_visitor_stops_idle_workers_before_watchdog', rp, lambda s: s.replace('let _unwind = UnwindStop(|| self.fail(ResolveError::Panic));', 'let _unwind = UnwindStop(|| self.fail(GitError::Cancelled.into()));'))
def first_duplicate(s):
    return region(s, '    fn prefer(', '\n}\nstruct Engine', lambda _: '    fn prefer(&mut self, oid: &ObjectId, index: usize, _depth: u16) {\n        if let Some(slot) = self.ref_slot(oid) && self.refs[slot].2.is_none() { self.refs[slot].2 = Some(index as u32); }\n    }')
add('shallowest-duplicate', 'shallowest_duplicate_wins_even_when_the_deep_copy_publishes_first', rp, first_duplicate)
add('profile-excludes-replays', 'trusted_index_restores_inflate_and_delta_profile_spans', rp, lambda s: s.replace('        if !_replay {\n            sley_core::fetch_profile::add_count(', '        if true {\n            sley_core::fetch_profile::add_count('), 'fetch-profile')
add('chain-local-workspace', 'chain_acceptance_is_independent_of_retention_and_scheduling', rp, lambda s: s.replace('                                        Some(body) => body,', '                                        Some(body) => { drop(body); self.replay(&node)? },'))

def release_after_visit(s):
    return region(s, '    fn child<', '    // Shortest depth', lambda b: b.replace('        parent.release()?;\n','').replace('        let body = self.retain(&mut node, body)?;', '        if let Some(parent) = &node.parent { parent.release()?; }\n        let body = self.retain(&mut node, body)?;'))
add('dependent-release-before-visitor', 'last_dependent_releases_base_before_visiting_child', rp, release_after_visit)

results = []
try:
    for name, test, path, mutate, features in probes:
        if len(sys.argv) > 2 and name not in sys.argv[2:]: continue
        for p, s in original.items():
            if p.read_text() != s: p.write_text(s)
        changed = mutate(original[path])
        assert changed != original[path], name + ': mutation did not apply'
        path.write_text(changed)
        runenv = env.copy()
        if name == 'deprecated-atomic': runenv['RUSTFLAGS'] = '-D warnings'
        args = ['cargo', 'test', '--locked', '-p', 'sley-pack', '--test', 'resolved']
        if features == 'core-doc':
            args = ['cargo','test','--locked','-p','sley-core','--doc']
        elif features: args += ['--features', features]
        args += [test, '--', '--nocapture'] if features == 'core-doc' else [test, '--', '--exact', '--nocapture']
        start = time.monotonic()
        logpath = scratch / (name + '.log')
        with logpath.open('w') as log:
            r = subprocess.run(args, env=runenv, stdout=log, stderr=subprocess.STDOUT, timeout=180)
        log = logpath.read_text()
        red = r.returncode == 101 and ('deprecated method' in log if name == 'deprecated-atomic' else 'FAILED' in log and 'could not compile' not in log)
        row = dict(probe=name, test=test, exit=r.returncode, RED=red, seconds=round(time.monotonic()-start, 2))
        results.append(row)
        (scratch / 'results.json').write_text(json.dumps(results, indent=2))
        print(json.dumps(row), flush=True)
        if not red: raise RuntimeError(name + ': expected executed RED missing')
finally:
    for p, s in original.items():
        if p.read_text() != s: p.write_text(s)
