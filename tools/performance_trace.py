"""Root-operated bounded hidden trace; never builds, attaches, or controls user apps.

Uses the existing native flat-eventloop/surface/GPU collectors and Cargo content
lock. CPU wall/entry cadence, GPU pass, device utilization are distinct metrics.
"""
from pathlib import Path
import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import threading
import time

CAP = 512 * 1024**2
FLOOR = 2 * 1024**3
TIMEOUT = 180


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def load(path):
    require(Path(path).stat().st_size <= CAP, 'JSON exceeds output budget')
    return json.loads(Path(path).read_text())


def dump(path, obj):
    Path(path).write_text(json.dumps(obj, indent=2, allow_nan=False) + '\n')


def summarize_ns(values):
    """Exact nearest-rank quantiles and rational budgets; never impute None as 0."""
    valid = [v for v in values if type(v) is int and v >= 0]
    require(all(v is None or type(v) is int and v >= 0 for v in values), 'Invalid ns sample')
    valid.sort()
    n = len(valid)
    quantile = lambda numerator: valid[(n * numerator + 99) // 100 - 1] if n else None
    return {'expected_samples': len(values), 'available_samples': n,
            'unavailable_samples': len(values) - n,
            'mean_ns': sum(valid) / n if n else None,
            'p50_ns': quantile(50), 'p95_ns': quantile(95), 'p99_ns': quantile(99),
            'over_budget': {str(hz): {'count': sum(v * hz > 1_000_000_000 for v in valid),
                'denominator': n,
                'fraction': sum(v * hz > 1_000_000_000 for v in valid) / n if n else None}
                for hz in (60, 144)}}


def summarize_ms(values):
    require(all(v is None or type(v) in (int, float) and math.isfinite(v) and v >= 0 for v in values), 'Invalid GPU sample')
    valid = sorted(v for v in values if v is not None)
    n = len(valid)
    return {'expected_samples': len(values), 'available_samples': n,
            'unavailable_samples': len(values) - n,
            'mean_ms': sum(valid) / n if n else None,
            'p95_ms': valid[(n * 95 + 99) // 100 - 1] if n else None,
            'p99_ms': valid[(n * 99 + 99) // 100 - 1] if n else None}


def validate_rows(rows):
    require(len(rows) == 500, 'Need exactly 500 interaction callbacks')
    for i, row in enumerate(rows):
        require(type(row['frame']) is int and row['frame'] == i, 'Missing/reordered callback')
        require(row['hidden'] is True and row['focused'] is False, 'Visible/focused child refused')
        require(row['trajectory'] == ('chart_relative' if i < 400 else 'outside_wide'), 'Wrong trajectory')
        if i < 400:
            require(row['chart_aabb_overlap_fraction'] > 0, 'Primary callback outside chart AABB')
        for key in ('prepare_wall_ns', 'render_through_present_wall_ns', 'handler_through_scheduling_wall_ns'):
            require(type(row[key]) is int and row[key] >= 0, 'Invalid callback clock')
    return rows


def summarize_rows(rows):
    validate_rows(rows)
    result = {}
    for name, first, last in [('first_chart100', 0, 100), ('warm_chart300', 100, 400), ('outside100', 400, 500)]:
        selected = rows[first:last]
        metrics = {key: summarize_ns([row[key] for row in selected]) for key in
                   ('prepare_wall_ns', 'render_through_present_wall_ns', 'handler_through_scheduling_wall_ns')}
        # Entry interval belongs to the current callback but includes the previous
        # callback. Drop boundary interval so warm statistics never include cold99.
        metrics['within_window_redraw_entry_interval_ns'] = summarize_ns(
            [row['redraw_entry_interval_ns'] for row in selected[1:]])
        result[name] = {'interaction_callbacks': len(selected), 'interval_denominator_expected': len(selected)-1,
                        'metrics': metrics}
    return result


def input_inventory(roots):
    result = {}
    for root in roots:
        root = Path(root).resolve(strict=True)
        require(root.is_dir(), 'Input root must be directory')
        for p in sorted(root.rglob('*')):
            require(not p.is_symlink(), 'Symlink input requires explicit resolved fixture')
            if p.is_file():
                result[str(p)] = sha(p)
    return result


def size_tree(root):
    return sum(p.stat().st_size for p in Path(root).rglob('*') if p.is_file() and not p.is_symlink())


def own_child(command, root, env, out):
    """Only terminates the Popen object created here; never PID/name searches."""
    started = time.monotonic()
    p = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    errors = []
    def drain():
        try:
            with (out / 'native.log').open('xb') as log:
                written = 0
                while True:
                    block = p.stdout.read(32768)
                    if not block:
                        break
                    written += len(block)
                    require(written <= CAP, 'Child stdout cap exceeded')
                    log.write(block)
        except BaseException as error:
            errors.append(str(error))
    reader = None
    try:
        reader = threading.Thread(target=drain, daemon=True)
        reader.start()
        dump(out / 'own-process.json', {'pid': p.pid, 'own_child': True, 'background_requested': True})
        while p.poll() is None:
            require(not errors, '; '.join(errors))
            require(time.monotonic() - started <= TIMEOUT, 'Own child timeout')
            require(size_tree(out.parent) <= CAP, 'Aggregate output cap exceeded')
            require(shutil.disk_usage(out).free >= FLOOR, 'Free space floor reached')
            time.sleep(0.25)
        reader.join(timeout=5)
        require(not reader.is_alive() and not errors, 'Output reader failed/incomplete')
        require(p.returncode == 0, f'Own child exit {p.returncode}')
        require(size_tree(out.parent) <= CAP, 'Final aggregate output cap exceeded')
        return {'exit': p.returncode, 'seconds': time.monotonic()-started}
    finally:
        if p.poll() is None:
            p.kill()
            p.wait(timeout=10)
        if p.stdout:
            p.stdout.close()
        if reader is not None:
            reader.join(timeout=5)


def qualify_endpoint(case, fixture):
    s = load(case / 'endpoint/snapshot.json')
    expected = {str(Path(p).resolve(strict=True)) for p in fixture['charts']}
    actual = [str(Path(p).resolve(strict=True)) for p in s['source_cells']]
    require(len(actual) == len(expected) and set(actual) == expected, 'Incomplete/duplicate loaded cells')
    require(s['background_test'] is True and s['native_window_visible'] is False and s['native_window_has_focus'] is False, 'Endpoint not hidden')
    require(s['profile'] == 'Day', 'Wrong palette')
    owners = s['optional_layer_policy']['cells']
    require(len(owners) == len(expected), 'Wrong actual owner count')
    identity = [{k: v[k] for k in ('cell_index', 'dataset_key', 'product', 'version', 'pc_source_digest')} for v in owners]
    require(all(v['product'] == 'S-101' and len(v['pc_source_digest']) == 64 for v in identity), 'Unsupported/missing actual PC owners')
    return identity


def run(args):
    # Reuse existing source scanner and kernel lock rather than duplicating Cargo rules.
    from build_content_guard import inventory, target_lock
    root = args.root.resolve(strict=True)
    exe = args.executable.resolve(strict=True)
    fixture = load(args.fixture)
    require(sha(args.fixture) == args.fixture_sha256, 'Fixture pin mismatch')
    require(len(fixture['charts']) == fixture['expected_counts']['total_S101'], 'Expected chart denominator mismatch')
    require(len(set(fixture['charts'])) == len(fixture['charts']), 'Duplicate fixture path')
    require(sha(args.source_inputs) == args.source_inputs_sha256, 'Source inventory file pin mismatch')
    source_inputs = load(args.source_inputs)
    expected_inputs = input_inventory(fixture['input_roots'])
    require(sha(args.build_receipt) == args.build_receipt_sha256, 'Build receipt pin mismatch')
    receipt = load(args.build_receipt)
    require(receipt['binary_sha256'] == args.executable_sha256, 'Receipt executable differs')
    require(receipt['source_manifest_sha256'] == args.source_inputs_sha256, 'Receipt source differs')
    out = args.output.resolve()
    require(not out.exists(), 'Refuse overwrite performance evidence')
    out.mkdir(parents=True)
    def guard():
        require(sha(exe) == args.executable_sha256, 'Executable changed')
        require(inventory(root, root/'target') == source_inputs, 'Build source drift')
        require(sha(args.build_receipt) == args.build_receipt_sha256, 'Receipt drift')
        require(sha(args.fixture) == args.fixture_sha256, 'Fixture drift')
        require(input_inventory(fixture['input_roots']) == expected_inputs, 'Runtime input drift')
    try:
        with target_lock(root/'target'):
            lock = root/'target/geometry-owner.lock'
            fd = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.write(fd, str(os.getpid()).encode()); os.close(fd)
            try:
                guard()
                dump(out/'fixture.json', fixture)
                dump(out/'runtime-inputs.json', expected_inputs)
                cases = []
                baseline_owners = None
                for i, latency in enumerate((1, 2, 2, 1)):
                    case = out / f'{i:02d}-latency{latency}'
                    case.mkdir()
                    require(shutil.disk_usage(case).free >= FLOOR, 'Free space floor reached')
                    env = {k: v for k, v in os.environ.items() if not k.startswith('FERRITE_')}
                    env.update(FERRITE_BACKGROUND_TEST='1', FERRITE_AUDIT_DIGEST_ONLY='1', FERRITE_NO_CACHE='1',
                        FERRITE_GPU_TIMESTAMP_AUDIT='1', FERRITE_GPU_FRAME_TIMESTAMPS='1' if args.gpu_timestamps else '0',
                        FERRITE_SURFACE_PACING_DIAGNOSTICS='1', FERRITE_FLAT_STAGE_TIMING='1' if args.stage_timing else '0',
                        FERRITE_SURFACE_FRAME_LATENCY=str(latency), FERRITE_FLAT_EVENTLOOP_PROFILE='Day',
                        FERRITE_FLAT_EVENTLOOP_AUDIT=str(case/'audit'))
                    command = [str(exe), '--fc', fixture['fc'], '--pc', fixture['pc'],
                        '--catalogue-inventory', fixture['catalogue_inventory'], '--center', fixture['center'], '--zoom', '1',
                        '--portrayal-audit', str(case/'endpoint'), '--screenshot', str(case/'final.png')]
                    for chart in fixture['charts']:
                        command += ['--chart', chart]
                    dump(case/'command.json', command)
                    dump(case/'flags.json', {k:v for k,v in env.items() if k.startswith('FERRITE_')})
                    terminal = own_child(command, root, env, case)
                    guard()
                    owners = qualify_endpoint(case, fixture)
                    if baseline_owners is None:
                        baseline_owners = owners
                    require(owners == baseline_owners, 'Actual owner identities differ across latency legs')
                    rows = load(case/'audit/flat-eventloop.json')['rows']
                    result = summarize_rows(rows)
                    pacing = load(case/'audit/surface-pacing.json')
                    require(pacing['complete_500_normal_surface_frames'] is True, 'Incomplete surface ledger')
                    require(pacing['requested_latency_hint'] == latency, 'Latency request not applied')
                    pacing_metrics = {name: summarize_ns([r[name] for r in pacing['rows']]) for name in
                        ('surface_acquire_wall_ns', 'queue_submit_call_wall_ns', 'present_call_wall_ns')}
                    gpu = None
                    if args.gpu_timestamps:
                        batch = load(case/'audit/gpu-timestamp-batch.json')
                        gpu = {'scope': batch['scope'], 'metrics': summarize_ms([r['gpu_chart_ms'] for r in batch['rows']]),
                               'adapter_backend': batch.get('adapter_backend'), 'gpu_name': batch.get('gpu_name'),
                               'same_population_as_cpu_warm300': False}
                    terminal.update(latency_requested=latency, cpu_windows=result, surface_host=pacing_metrics,
                        gpu_chart=gpu, gpu_device_utilization_percent=None,
                        source_manifest_sha256=args.source_inputs_sha256, binary_sha256=args.executable_sha256,
                        owner_identity=owners, performance_only=True,
                        correctness_scope='Actual endpoint source/PC-owner identity only; no pixel or intermediate visibility equivalence proof')
                    dump(case/'terminal.json', terminal)
                    cases.append(terminal)
                    dump(out/'progress.json', {'completed_legs': len(cases)})
                dump(out/'terminal.json', {'exit':0, 'cases':cases,
                    'scope':'Hidden own-child synthetic navigation; CPU callback/entry/surface wall and GPU chart separate. Not physical display FPS, input latency, full correctness, or device utilization. Source/PC versions remain source-bound. Latency request may be backend-clamped.'})
            finally:
                if lock.exists() and lock.read_text() == str(os.getpid()):
                    lock.unlink()
    except BaseException as error:
        dump(out/'failure.json', {'error':str(error), 'failed_evidence_retained':True})
        raise


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('root', 'executable', 'source-inputs', 'fixture', 'build-receipt', 'output'):
        p.add_argument('--'+name, type=Path, required=True)
    for name in ('executable', 'source-inputs', 'fixture', 'build-receipt'):
        p.add_argument('--'+name+'-sha256', required=True)
    p.add_argument('--stage-timing', action='store_true', help='Opt-in per-instruction attribution clocks; run a separate observer-cost pair')
    p.add_argument('--gpu-timestamps', action='store_true', help='Separate observer-enabled quartet; not comparable with observer-off timing')
    run(p.parse_args())


if __name__ == '__main__':
    main()
