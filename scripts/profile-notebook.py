#!/usr/bin/env python3
"""Measure nvb process-tree CPU/RSS in a private, disposable tmux server."""
import argparse
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import subprocess
import tempfile
import time

INTERVAL = 0.5


def command(args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=15, **kwargs).stdout.strip()


def cpu_seconds(value):
    days, clock = value.split('-', 1) if '-' in value else ('0', value)
    result = 0.0
    for part in clock.split(':'):
        result = result * 60 + float(part)
    return int(days) * 86400 + result


def process_table():
    raw = command(['ps', '-axo', 'pid=,ppid=,lstart=,time=,rss=,args='])
    rows = []
    for line in raw.splitlines():
        parts = line.split(None, 9)
        if len(parts) < 10:
            raise RuntimeError(f'cannot parse ps row: {line}')
        rows.append(dict(pid=int(parts[0]), ppid=int(parts[1]), started=' '.join(parts[2:7]),
                         cpu_s=cpu_seconds(parts[7]), cpu_time=parts[7], rss_kib=int(parts[8]),
                         command=parts[9]))
    return rows


def identity(row):
    return f"{row['pid']}:{row['started']}"


def descendants(rows, roots):
    owned = set(roots)
    while True:
        children = {r['pid'] for r in rows if r['ppid'] in owned}
        if children <= owned:
            return owned
        owned |= children


def wait_for(description, predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f'timed out waiting for {description}')
        # Polling cadence only: progress is decided by the predicate, never this delay.
        time.sleep(min(0.1, remaining))


def synthetic(path, workload, seconds):
    started, done = path.parent / 'started', path.parent / 'done'
    prelude = f"from pathlib import Path\nimport time\nPath({str(started)!r}).touch()\n"
    if workload == 'stream':
        source = prelude + f"for i in range({math.ceil((seconds + 2) * 50)}):\n    print(f'profile stream {{i:06d}} ' + 's' * 72, flush=True)\n    time.sleep(0.02)\n"
    elif workload == 'plots':
        source = prelude + """import struct, zlib
from IPython.display import Image, display
def png(frame):
    w, h = 320, 180
    row = bytes((frame % 256, 120, 200)) * w
    raw = (b'\\0' + row) * h
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
    return b'\\x89PNG\\r\\n\\x1a\\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')
handle = display(Image(data=png(0)), display_id=True)
""" + f"for i in range({math.ceil((seconds + 2) * 10)}):\n    handle.update(Image(data=png(i)))\n    time.sleep(0.1)\n"
    else:
        source = 'profile_value = 1'
    if workload in ('stream', 'plots'):
        source += f"Path({str(done)!r}).touch()\n"
    output = [{'output_type': 'stream', 'name': 'stdout', 'text': 'saved output ' + ('x' * 86) + '\n'}]
    if workload == 'edit-save':
        output[0]['text'] *= 50_000
    cells = [dict(cell_type='code', id='first', metadata={}, source=source,
                  outputs=output if workload == 'edit-save' else [], execution_count=None)]
    if workload in ('idle', 'scroll'):
        cells += [dict(cell_type='code', id=f'cell{i}', metadata={}, source=f'profile_value_{i} = {i}',
                       outputs=[], execution_count=None) for i in range(120)]
    notebook = dict(nbformat=4, nbformat_minor=5, cells=cells, metadata={
        'kernelspec': {'name': 'nvb-profile', 'display_name': 'Profile Python', 'language': 'python'},
        'language_info': {'name': 'python'}})
    path.write_text(json.dumps(notebook) + '\n')
    return started, done, hashlib.sha256(output[0]['text'].encode()).hexdigest()


class Application:
    def __init__(self, args, directory, workload):
        self.args, self.directory = args, directory
        self.socket = directory / 'tmux.sock'
        self.path = directory / 'profile.ipynb'
        self.started, self.done, self.saved_hash = synthetic(self.path, workload, args.seconds)
        self.known = {}
        self.root = None
        self.root_identity = None
        self.tmux_prefix = ['tmux', '-S', str(self.socket)]
        self.env = os.environ.copy()
        self.env.pop('TMUX', None)
        self.env.pop('TMUX_PANE', None)
        self.env['JUPYTER_PATH'] = str(directory / 'jupyter')
        self.env['JUPYTER_RUNTIME_DIR'] = str(directory / 'runtime')
        self.env['TERM'] = 'xterm-256color'
        if args.config == 'clean':
            for key in ('CONFIG', 'DATA', 'STATE', 'CACHE'):
                self.env[f'XDG_{key}_HOME'] = str(directory / f'xdg-{key.lower()}')
        spec = directory / 'jupyter/kernels/nvb-profile'
        spec.mkdir(parents=True)
        (spec / 'kernel.json').write_text(json.dumps(dict(
            argv=[str(args.python), '-m', 'ipykernel_launcher', '-f', '{connection_file}'],
            display_name='Profile Python', language='python')))

    def tmux(self, *args):
        return command(self.tmux_prefix + list(args), env=self.env)

    def screen(self):
        return self.tmux('capture-pane', '-p', '-t', 'profile:0.0')

    def keys(self, *keys, literal=False):
        self.tmux('send-keys', '-t', 'profile:0.0', *(['-l'] if literal else []), *keys)

    def tree(self):
        rows = process_table()
        root = next((r for r in rows if r['pid'] == self.root), None)
        if self.root_identity is None and root:
            self.root_identity = identity(root)
        roots = {r['pid'] for r in rows if identity(r) in self.known}
        if root and identity(root) == self.root_identity:
            roots.add(self.root)
        # Follow reparented observed descendants, but exclude reused process identities.
        owned = descendants(rows, roots)
        selected = [r for r in rows if r['pid'] in owned]
        for row in selected:
            self.known[identity(row)] = row
        return selected

    def role(self, row):
        if row['pid'] == self.root:
            return 'nvb'
        if row['ppid'] == self.root and '--embed' in row['command'].split():
            return 'nvim'
        if ' -m ipykernel_launcher ' in f" {row['command']} ":
            return 'kernel'
        return 'other'

    def launch(self):
        argv = [str(self.args.binary), '--nvim', str(self.args.nvim)]
        if self.args.config == 'clean':
            argv.append('--clean')
        argv.append(str(self.path))
        # The shell execs the application; there are no persistent shell or agent panes.
        self.root = int(self.tmux('-f', '/dev/null', 'new-session', '-d', '-s', 'profile',
                                  '-P', '-F', '#{pane_pid}', '-x', '140', '-y', '40',
                                  '-c', str(self.directory), 'exec ' + shlex.join(argv)))
        wait_for('notebook NAV and kernel process', lambda: ' NAV' in self.screen()
                 and any(self.role(r) == 'kernel' for r in self.tree()))
        wait_for('kernel idle', lambda: 'idle' in self.screen())

    def cleanup(self):
        if self.root is not None:
            try:
                self.tree()
            except (OSError, subprocess.SubprocessError):
                pass
        # Graceful notebook exit first; bounded forced cleanup covers failed startup too.
        if self.socket.exists():
            try:
                self.keys('Escape', 'Escape', ':q!', 'Enter')
            except subprocess.SubprocessError:
                pass
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            alive = [r for r in process_table() if identity(r) in self.known]
            if not alive:
                break
            time.sleep(0.1)
        for sig in (signal.SIGTERM, signal.SIGKILL):
            for row in process_table():
                if identity(row) in self.known:
                    try:
                        os.kill(row['pid'], sig)
                    except ProcessLookupError:
                        pass
        subprocess.run(self.tmux_prefix + ['kill-server'], capture_output=True, timeout=5, check=False)


def summarize(samples):
    processes, roles = {}, {role: dict(cpu_delta_s=0.0, peak_rss_kib=0) for role in ('nvb', 'nvim', 'kernel', 'other')}
    for sample in samples:
        totals = {role: 0 for role in roles}
        for row in sample['processes']:
            key = identity(row)
            totals[row['role']] += row['rss_kib']
            if key not in processes:
                processes[key] = dict(pid=row['pid'], started=row['started'], role=row['role'],
                                      command=row['command'], cpu_delta_s=0.0, peak_rss_kib=0,
                                      observations=0, last_cpu_s=row['cpu_s'])
            p = processes[key]
            delta = row['cpu_s'] - p['last_cpu_s']
            if delta < 0:
                raise RuntimeError(f'CPU counter moved backwards for {key}')
            p['cpu_delta_s'] += delta
            roles[p['role']]['cpu_delta_s'] += delta
            p['last_cpu_s'] = row['cpu_s']
            p['peak_rss_kib'] = max(p['peak_rss_kib'], row['rss_kib'])
            p['last_rss_kib'] = row['rss_kib']
            p['observations'] += 1
        for role, rss in totals.items():
            roles[role]['peak_rss_kib'] = max(roles[role]['peak_rss_kib'], rss)
    wall = samples[-1]['elapsed_s'] - samples[0]['elapsed_s']
    for p in [*processes.values(), *roles.values()]:
        p['cpu_percent_one_core'] = 100 * p['cpu_delta_s'] / wall
        p.pop('last_cpu_s', None)
    return dict(wall_s=wall, processes=list(processes.values()), roles=roles)


def measure(app, workload, record):
    app.launch()
    if workload in ('stream', 'plots'):
        app.keys('r')
        wait_for('kernel workload start marker', app.started.exists)
    if workload == 'edit-save':
        app.keys('Enter')
        wait_for('cell EDIT mode', lambda: ' EDIT' in app.screen())
    samples, actions, saves = record['samples'], record['actions'], 0
    start = time.monotonic()
    next_sample = start
    next_action = start
    pending_save = None
    while True:
        now = time.monotonic()
        elapsed = now - start
        if now >= next_sample:
            before = time.monotonic()
            rows = app.tree()
            after = time.monotonic()
            samples.append(dict(elapsed_s=(before + after) / 2 - start,
                                collected_utc=dt.datetime.now(dt.timezone.utc).isoformat(),
                                ps_wall_s=after - before,
                                processes=[dict(r, role=app.role(r)) for r in rows]))
            if not any(r['pid'] == app.root for r in rows):
                raise RuntimeError('nvb exited during measurement')
            next_sample = after + INTERVAL
            if elapsed >= app.args.seconds:
                break
        if workload == 'scroll' and now >= next_action:
            key = 'C-e' if int(elapsed / 2) % 2 == 0 else 'C-y'
            app.keys(key)
            actions.append(dict(elapsed_s=elapsed, action=key))
            next_action = time.monotonic() + 0.1
        if workload == 'edit-save':
            if pending_save and app.path.stat().st_mtime_ns != pending_save[0]:
                actions.append(dict(elapsed_s=elapsed, action='save-confirmed'))
                saves += 1
                pending_save = None
            if pending_save and now > pending_save[1]:
                raise RuntimeError('timed out waiting for notebook save')
            if not pending_save and now >= next_action:
                previous = app.path.stat().st_mtime_ns
                app.keys('i')
                app.keys(f'# profile edit {saves:06d}\n', literal=True)
                app.keys('Escape', ':w', 'Enter')
                pending_save = (previous, time.monotonic() + 15)
                next_action = time.monotonic() + 0.5
        time.sleep(max(0, min(0.05, next_sample - time.monotonic())))
    if workload in ('stream', 'plots'):
        wait_for('bounded workload completion', app.done.exists, app.args.seconds + 20)
        wait_for('kernel returns idle', lambda: 'idle' in app.screen())
    if workload == 'edit-save':
        if pending_save:
            wait_for('final save', lambda: app.path.stat().st_mtime_ns != pending_save[0], 15)
            saves += 1
        notebook = json.loads(app.path.read_text())
        first = notebook['cells'][0]
        source = first['source']
        source = ''.join(source) if isinstance(source, list) else source
        if saves == 0 or '# profile edit ' not in source:
            raise RuntimeError('editing/saving workload did not change the notebook')
        text = first['outputs'][0]['text']
        text = ''.join(text) if isinstance(text, list) else text
        if hashlib.sha256(text.encode()).hexdigest() != app.saved_hash:
            raise RuntimeError('saved output changed during editing/saving')
    record.update(confirmed_saves=saves, final_screen=app.screen(),
                  notebook_bytes=app.path.stat().st_size, **summarize(samples))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'nvim', 'python'):
        parser.add_argument('--' + name, required=True, type=Path)
    parser.add_argument('--config', choices=('clean', 'user'), required=True)
    parser.add_argument('--seconds', type=float, default=10, help='measurement seconds per workload (at least 2)')
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or args.seconds < 2:
        parser.error('--seconds must be finite and at least 2')
    for name in ('binary', 'nvim', 'python'):
        path = getattr(args, name).absolute()
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f'--{name} must name an executable file')
        setattr(args, name, path)
    args.output = args.output.absolute()
    if args.output.suffix != '.json' or args.output.exists() or args.output.is_symlink():
        parser.error('--output must be a new .json path (existing paths are refused)')
    if not args.output.parent.is_dir():
        parser.error('--output parent directory must exist')
    if not shutil.which('tmux'):
        parser.error('tmux is required')
    command([str(args.python), '-c', 'import ipykernel, IPython'])
    metadata = dict(platform=platform.platform(), machine=platform.machine(), logical_cpus=os.cpu_count(),
                    config=args.config, requested_seconds=args.seconds, sample_interval_s=INTERVAL,
                    binary=str(args.binary), binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                    nvim=str(args.nvim), python=str(args.python), cwd=str(Path.cwd()),
                    versions={name: command(argv) for name, argv in {
                        'nvb': [str(args.binary), '--version'], 'nvim': [str(args.nvim), '--version'],
                        'python': [str(args.python), '--version'], 'tmux': ['tmux', '-V']}.items()},
                    inherited_environment={key: os.environ.get(key) for key in
                        ('TERM', 'COLORTERM', 'TERM_PROGRAM', 'VIRTUAL_ENV', 'NVIM_APPNAME',
                         'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME')})
    result = dict(schema_version=1, created_utc=dt.datetime.now(dt.timezone.utc).isoformat(),
                  metadata=metadata, workloads=[], limits=[
                      'ps cumulative CPU deltas cover observed lifetimes only; startup, exit tails and children wholly between samples are not measured.',
                      'CPU percent uses one logical core as 100%; short windows and ps time rounding are noisy. RSS is sampled resident memory, not private/unique memory.',
                      'Detached tmux uses a fixed 140x40 xterm-256color pane; it does not measure a real terminal graphics renderer.',
                      'Harness ps/capture/send overhead and host contention affect timing but are excluded from the application process totals.',
                      'User configuration can create external processes or writes; only observed app descendants are attributed/cleaned. No performance thresholds.'])
    try:
        with tempfile.TemporaryDirectory(prefix='nvb-profile-') as temp:
            for workload in ('idle', 'scroll', 'stream', 'plots', 'edit-save'):
                directory = Path(temp).resolve() / workload
                directory.mkdir()
                app = Application(args, directory, workload)
                print(f'profiling {workload} ({args.config})', flush=True)
                record = dict(workload=workload, samples=[], actions=[])
                result['workloads'].append(record)
                try:
                    measure(app, workload, record)
                except Exception as error:
                    record['error'] = f'{type(error).__name__}: {error}'
                    try:
                        record['final_screen'] = app.screen()
                    except subprocess.SubprocessError:
                        pass
                    raise
                finally:
                    app.cleanup()
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        with args.output.open('x') as output:
            output.write(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
