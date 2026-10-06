"""Content guard for cooperative builds sharing one Cargo target directory.

Cargo remains responsible for compiler/profile/features/environment fingerprints.
The lock covers the entire Cargo invocation. Direct cargo calls bypass this guard.
Only files inside this checkout are admitted; external symlink inputs are refused.
"""
import contextlib
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile
import time

VERSION = 3
EXCLUDED_DIRS = {'.git', 'target', '__pycache__', '.pytest_cache', 'outputs', 'work'}
# These root folders are runtime data, not Cargo inputs. Do not even traverse
# them: ChartData may link to a large external dataset collection. Nested
# source folders with these names still receive the usual symlink checks.
ROOT_RUNTIME_DIRS = {'ChartData', 'TestData', 'Trust', 'TestResults',
                     'WindowsTestResults', 'logs', 'plugins_out'}


def source_files(root, target):
    root, target = Path(root).resolve(), Path(target).resolve()
    files = {}
    for directory, dirs, names in os.walk(root, followlinks=False):
        directory = Path(directory)
        retained = []
        for name in sorted(dirs):
            child = directory / name
            if (name in EXCLUDED_DIRS
                    or (directory == root and name in ROOT_RUNTIME_DIRS)
                    or child.resolve() == target):
                continue
            if child.is_symlink():
                raise RuntimeError(f'Symlink source directory is not admitted: {child}')
            retained.append(name)
        dirs[:] = retained
        for name in sorted(names):
            path = directory / name
            if name == '.DS_Store' or name.endswith(('.log', '.pyc')):
                continue
            if path.is_symlink():
                raise RuntimeError(f'Symlink source input is not admitted: {path}')
            if not path.is_file():
                raise RuntimeError(f'Non-regular source input: {path}')
            files[path.relative_to(root).as_posix()] = path
    return files


def inventory(root, target):
    root = Path(root).resolve()
    files = source_files(root, target)
    # Build scripts may inspect arbitrary files in their src directory; include
    # that directory conservatively, not unrelated workspace README/runtime files.
    build_roots = [Path(name).parent / 'src' for name in files if Path(name).name == 'build.rs']
    selected = {name for name in files if name.endswith(('.rs', '.wgsl', '.c', '.h', '.cc', '.cpp', '.m', '.mm', '.s', '.S'))
                or Path(name).name in {'Cargo.toml', 'Cargo.lock'}
                or name.startswith(('.cargo/', 'assets/', 'Catalogues/'))
                or any(Path(name).is_relative_to(directory) for directory in build_roots)}
    for name in files:
        if Path(name).name != 'build.rs':
            continue
        for literal in re.findall(r'rerun-if-changed=([^"{}\n]+)"', files[name].read_text(errors='replace')):
            path = (files[name].parent / literal).resolve()
            if not path.is_relative_to(root):
                raise RuntimeError(f'External build-script input is not admitted: {path}')
            if path.relative_to(root).parts and path.relative_to(root).parts[0] in ROOT_RUNTIME_DIRS:
                raise RuntimeError(f'Missing/excluded build-script input: {path.relative_to(root)}')
            if path.is_dir():
                selected.update(key for key, file in files.items() if file.is_relative_to(path))
            else:
                key = path.relative_to(root).as_posix()
                if key not in files:
                    raise RuntimeError(f'Missing/excluded build-script input: {key}')
                selected.add(key)
    pending = list(selected)
    # Cargo's dep-info tracks include inputs; literal embedded non-source assets
    # (including README/XML/JSON/font) enter this content manifest as well.
    pattern = re.compile(r'include_(?:str|bytes)?!\s*\(\s*"([^"\n]+)"')
    while pending:
        name = pending.pop()
        if not name.endswith('.rs'):
            continue
        for literal in pattern.findall(files[name].read_text(errors='replace')):
            path = (files[name].parent / literal).resolve()
            if not path.is_relative_to(root):
                raise RuntimeError(f'External embedded input is not admitted: {path}')
            key = path.relative_to(root).as_posix()
            if key not in files:
                raise RuntimeError(f'Missing/excluded embedded source input: {key}')
            if key not in selected:
                selected.add(key); pending.append(key)
    result = {}
    for name in sorted(selected):
        digest = hashlib.sha256()
        with files[name].open('rb') as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b''):
                digest.update(block)
        result[name] = digest.hexdigest()
    return result


def package_for(root, name):
    directory = Path(root, name).parent
    root = Path(root)
    while directory.is_relative_to(root):
        if (directory / 'Cargo.toml').is_file():
            return directory
        if directory == root:
            break
        directory = directory.parent
    return root


def structural_anchors(root, before, structural):
    anchors = set()
    for name in structural:
        package = package_for(root, name)
        # Build scripts regenerate package data; lib/main/bin/example entrypoints
        # trigger module/include traversal when stale dep-info lacks new inputs.
        for entry in before:
            p = Path(root, entry)
            if package_for(root, entry) == package and (
                p.name in {'lib.rs', 'main.rs', 'build.rs', 'Cargo.toml'}
                or p.parent == package / 'src/bin'
                or p.parent == package / 'examples'):
                anchors.add(entry)
        # Root global inputs may be consumed by multiple package build scripts.
        if package == Path(root):
            anchors.update(entry for entry in before if Path(entry).name == 'build.rs')
    return anchors


@contextlib.contextmanager
def target_lock(target):
    """Kernel advisory lock, released on process death; no stale lock deletion."""
    target = Path(target).resolve()
    target.mkdir(parents=True, exist_ok=True)
    with (target / '.ferrite-content-guard.lock').open('a+b') as handle:
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b'0')
            handle.flush()
        handle.seek(0)
        if os.name == 'nt':
            import msvcrt
            # LK_LOCK has a finite retry limit; retry until cooperative owner exits.
            while True:
                try:
                    msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    time.sleep(0.1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            handle.seek(0)
            if os.name == 'nt':
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def load_state(path):
    try:
        state = json.loads(path.read_text())
        if state.get('version') == VERSION and isinstance(state.get('inputs'), dict):
            return state
    except (OSError, ValueError, AttributeError):
        pass
    return None


def invalidate(root, target, before, previous, owner_changed):
    """Touch changed inputs and affected structural anchors; never Cargo's artifacts.

    Touching Rust anchors is necessary for removed/new include inputs not present
    in prior dep-info. Retain all-source invalidation for unknown cache/owner.
    No temporary content modifications are made, so no false content identity is
    registered from a restored file after compilation.
    """
    old = previous['inputs'] if previous else {}
    changed = {name for name in before if old.get(name) != before[name]}
    unknown = previous is None or owner_changed
    if not unknown and old == before:
        return []
    structural = set(old) ^ set(before)
    selected = set(before) if unknown else changed | structural_anchors(root, before, structural)
    # A coarse filesystem timestamp must be strictly later than prior mtimes.
    # Wait instead of installing future timestamps that precede Cargo start time.
    latest = max((Path(root, name).stat().st_mtime_ns for name in selected), default=0)
    # Cargo records invocation timestamps per profile/target fingerprint. Old
    # cloned artifacts may have newer mtimes than the current checkout.
    for timestamp in Path(target).glob('**/.fingerprint/*/invoked.timestamp'):
        latest = max(latest, timestamp.stat().st_mtime_ns)
    if latest > time.time_ns() + 2_000_000_000:
        raise RuntimeError('Future source mtime; repair checkout timestamps before guarded build')
    while time.time_ns() <= latest + 1_000_000_000:
        time.sleep(0.02)
    stamp = time.time_ns()
    for name in sorted(selected):
        path = Path(root, name)
        st = path.stat()
        os.utime(path, ns=(st.st_atime_ns, stamp))
    # Coarse mtime resolution can still defeat Cargo: reject, do not publish.
    if any(Path(root, name).stat().st_mtime_ns <= latest for name in selected):
        raise RuntimeError('Source filesystem cannot represent monotonic invalidation mtime')
    return sorted(selected)


def publish(path, value):
    fd, tmp = tempfile.mkstemp(prefix='.ferrite-guard-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as handle:
            json.dump(value, handle, sort_keys=True)
            handle.write('\n')
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def guarded_build(root, target, command, run=None):
    root, target = Path(root).resolve(), Path(target).resolve()
    if target == root or root.is_relative_to(target):
        raise RuntimeError('Target must not equal or contain source checkout')
    run = run or (lambda cmd: subprocess.call(cmd, cwd=root))
    with target_lock(target):
        state_path = target / '.ferrite-content-guard.json'
        before = inventory(root, target)
        previous = load_state(state_path)
        touched = invalidate(root, target, before, previous, previous is not None and previous.get('owner') != str(root))
        # Invalidate old state before Cargo: failed/interrupted compilation may
        # replace some artifacts, making the previous success receipt invalid.
        if state_path.exists():
            state_path.unlink()
        if inventory(root, target) != before:
            raise RuntimeError('Source content changed during guard preparation')
        # Detect normal edit-and-restore (ABA) changes as well as different bytes.
        stamps = {name: (Path(root, name).stat().st_mtime_ns, Path(root, name).stat().st_size)
                  for name in before}
        status = run(command)
        after = inventory(root, target)
        after_stamps = {name: (Path(root, name).stat().st_mtime_ns, Path(root, name).stat().st_size)
                        for name in after}
        if after != before or after_stamps != stamps:
            raise RuntimeError('Source content changed during build; guard state not published')
        if status == 0:
            publish(state_path, {'version': VERSION, 'owner': str(root), 'inputs': before,
                                 'command': command, 'touched': touched})
        return status
