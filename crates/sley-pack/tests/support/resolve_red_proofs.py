"""Revert one security guard at a time; require an executed public-API RED.

Run from the repository root. Originals are restored even if a probe fails.
Logs and the result table go under the supplied scratch directory.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import time

scratch = Path(sys.argv[1] if len(sys.argv) > 1 else '/tmp/sley267-r5-proofs')
scratch.mkdir(parents=True, exist_ok=True)
rp = Path('crates/sley-pack/src/resolved.rs')
pp = Path('crates/sley-pack/src/parallel_index.rs')
lp = Path('crates/sley-pack/src/limits.rs')
original = {p: p.read_text() for p in (rp, pp, lp)}
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
add('worker-arena-bound', 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory', rp, lambda s: s.replace('.min(self.entries.len()).min(slots)', '.min(self.entries.len())'))
add('flight-lease-lifetime', 'in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory', rp, lambda s: s.replace('let _flight = if self.limits.is_unlimited()', 'let _ = if self.limits.is_unlimited()'))
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
    return s.replace('let (oid, object_type, crc32) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;', 'let Some((oid, object_type, crc32)) = entry else { continue };')
add('thin-completion', 'thin_pack_is_an_unresolved_base_error', rp, thin)
add('caller-verifier-error', 'caller_admission_rejection_is_preserved_without_returning_a_build', rp, lambda s: s.replace('.map_err(ResolveError::Visitor)?;', '.ok();'))
# Round 5: the same byte-derived bound on both policies, with the existing
# 64K count cap retained so the physical-input assertion must detect the defect.
add('physical-preallocation-bound', 'lying_maximum_object_count_over_one_kib_has_bounded_allocation', lp, lambda s: s.replace('.min(entry_bytes / MIN_PACK_ENTRY_BYTES as usize)', ''))
for test in ['object_count_budget_is_typed', 'object_bytes_budget_is_typed', 'decoded_bytes_budget_is_typed', 'workspace_fit_budget_is_typed']:
    add('typed-' + test, test, rp, lambda s: region(s, 'fn budget(', '\nfn invalid(', lambda b: b[:b.index('    GitError::ResourceLimit')] + '    invalid(&format!("{kind}: limit {limit}, attempted {attempted}"))\n}'))
add('legacy-ops-index-typing', 'legacy_operation_budget_remains_typed', rp, lambda s: s.replace('budget(ResourceLimitKind::ResolveOps, limit, attempted)', 'invalid(&format!("resolve_ops {limit} {attempted}"))'))
add('legacy-ops-parse-typing', 'legacy_operation_budget_remains_typed', pp, lambda s: region(s, '        ResolveError::ResolveOps {', '\n    })?;', lambda b: '        ResolveError::ResolveOps { limit, attempted } => GitError::InvalidFormat(format!("resolve_ops {limit} {attempted}")),'))
add('trusted-cancel-isolation', 'trusted_errors_leave_process_interrupt_clear', rp, lambda s: s.replace('        self.stop.cancel();', '        self.stop.cancel();\n        if let Some(source) = self.cancel.source() { source.cancel(); }'))
add('limited-fast-path-isolation', 'finite_limits_cannot_enter_unaccounted_trusted_discovery', rp, lambda s: region(s, '    fn is_unlimited(self)', '    /// Trusted legacy policy:', lambda b: '    fn is_unlimited(self) -> bool { true }\n'))
def old_metadata(s):
    # Restore the original 48-byte root and 104-byte output layouts, plus the
    # original root lifetime. The meter counts the actually restored live root
    # allocation during conversion; the coefficient assertion is unchanged.
    s = s.replace('type Output = (ObjectId, ObjectType, u32);', 'type Output = (PackIndexEntry, PackIndexedObject);')
    s = s.replace('roots: &[u32]', 'roots: &[Root<\'_>]')
    s = s.replace('None => Root::Entry(roots[index] as usize)', 'None => roots[index]')
    s = s.replace('None => roots.push(index as u32)', 'None => roots.push(Root::Entry(index))')
    a = s.index('        state.output[index] = Some(('); b = s.index('        drop(state);', a)
    s = s[:a] + '''        state.output[index] = Some((PackIndexEntry { oid, offset: d.offset as u64, crc32: crc32fast::hash(&self.pack[d.offset..d.end]) }, PackIndexedObject { oid, object_type, offset: d.offset as u64, size: body.len() as u64 }));
''' + s[b:]
    s = s.replace('    drop(roots);\n', '')
    s = s.replace('            buffers\n                + entries.len()', '            buffers + allocation(&roots)\n                + entries.len()')
    s = s.replace('let (oid, object_type, crc32) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;', 'let (old_index, old_object) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;\n        let (oid, object_type, crc32) = (old_index.oid, old_object.object_type, old_index.crc32);')
    s = s.replace('    drop(entries);', '    drop(entries);\n    drop(roots);')
    return s
# Restore the duplicated per-entry output fields; the meter must catch their cost.
def duplicate_metadata(s):
    s = s.replace('type Output = (ObjectId, ObjectType, u32);', 'type Output = (PackIndexEntry, PackIndexedObject);')
    a = s.index('        state.output[index] = Some(('); b = s.index('        drop(state);', a)
    s = s[:a] + '''        state.output[index] = Some((
            PackIndexEntry { oid, offset: d.offset as u64, crc32: crc32fast::hash(&self.pack[d.offset..d.end]) },
            PackIndexedObject { oid, offset: d.offset as u64, object_type, size: body.len() as u64 },
        ));
''' + s[b:]
    return s.replace('let (oid, object_type, crc32) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;',
        'let (old_index, old_object) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;\n        let (oid, object_type, crc32) = (old_index.oid, old_object.object_type, old_index.crc32);')
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

add('subtree-stealing', 'one_hot_family_is_stolen_by_multiple_workers', rp, lambda s: s.replace('item = pop((worker + other) % workers, true)?;', 'let _ = other; item = None;'))
add('panic-unwind-stop', 'panicking_visitor_stops_idle_workers_before_watchdog', rp, lambda s: s.replace('let _unwind = UnwindStop(&self.stop);', 'let _unwind = ();'))
add('dependent-release-order', 'last_dependent_releases_base_before_visiting_child', rp, lambda s: s.replace('        parent.release()?;\n', '').replace('        let base = self.replay(&parent)?;', '        parent.release()?;\n        let base = self.replay(&parent)?;'))

results = []
try:
    for name, test, path, mutate, features in probes:
        if len(sys.argv) > 2 and name not in sys.argv[2:]: continue
        for p, s in original.items(): p.write_text(s)
        changed = mutate(original[path])
        assert changed != original[path], name + ': mutation did not apply'
        path.write_text(changed)
        runenv = env.copy()
        if name == 'deprecated-atomic': runenv['RUSTFLAGS'] = '-D warnings'
        args = ['cargo', 'test', '--locked', '-p', 'sley-pack', '--test', 'resolved']
        if features: args += ['--features', features]
        args += [test, '--', '--exact', '--nocapture']
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
    for p, s in original.items(): p.write_text(s)
