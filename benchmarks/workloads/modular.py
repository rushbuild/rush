#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Compare one pinned Modular stdlib package through isolated CPU build lanes.
Run prepare, provision the pinned inputs, qualify, then measure. Keep raw logs
outside Git; they contain local paths. This script runs no GPU programs.
"""
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import signal
import statistics
import subprocess
import time

PIN = '4e87649567a82e3bdac178ea61d62daddc7477bb'
TARGET = '//mojo/stdlib/std:std'
EDIT = Path('mojo/stdlib/std/builtin/int.mojo')
PRODUCT = Path('mojo/stdlib/std/std.mojoc')
CPU = ['--platforms=@platforms//host', '--host_platform=@platforms//host',
       '--repo_env=MODULAR_SKIP_AUTO_PLATFORMS=1']
CONFIGURED_COMMANDS = ('build', 'cquery', 'aquery', 'fetch')
REPOSITORY_COMMANDS = ('build', 'cquery', 'aquery', 'fetch', 'sync', 'mod', 'query')
LOCAL = ['--remote_cache=', '--remote_executor=', '--bes_backend=',
         '--disk_cache=', '--noremote_accept_cached', '--noremote_upload_local_results']
MARKER = 'rush-modular-private-controller-v1'
MOJO_LOG = re.compile(r'^\[mojo\] std [0-9.]+s cached=(true|false) exit=(-?\d+) argv: ', re.M)


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(part)
    return h.hexdigest()


def git(source, *args):
    return subprocess.check_output(['git', '-C', str(source), *args],
                                   env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'},
                                   text=True).strip()


def toolchain_digest(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        relative = str(path.relative_to(root))
        if path.is_symlink():
            if not path.resolve().is_relative_to(root):
                raise RuntimeError('external toolchain symlink: ' + str(path))
            value = 'link:' + os.readlink(path)
        elif path.is_file():
            value = 'file:' + sha(path)
        elif path.is_dir():
            value = 'dir'
        else:
            raise RuntimeError('unsupported toolchain entry: ' + str(path))
        digest.update(f'{relative}\0{path.lstat().st_mode:o}\0{value}\0'.encode())
    return digest.hexdigest()


def write_json(path, obj):
    temp = path.with_suffix(path.suffix + '.tmp')
    temp.write_text(json.dumps(obj, indent=2, sort_keys=True) + '\n')
    temp.replace(path)


def invoke(argv, cwd, env, log, timeout):
    # The subprocess owns a process group so a timed-out CLI cannot continue
    # the current command's child compiler. Persistent daemons are stopped below.
    start = time.perf_counter_ns()
    with log.open('wb') as stream:
        process = subprocess.Popen([str(x) for x in argv], cwd=cwd, env=env,
                                   stdout=stream, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        descriptor = os.pidfd_open(process.pid)
        try:
            if not select.select([descriptor], [], [], timeout)[0]:
                raise TimeoutError('command timed out; see ' + str(log))
            rc = process.wait()
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            if not select.select([descriptor], [], [], 10)[0]:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait()
            raise
        finally:
            os.close(descriptor)
    elapsed = (time.perf_counter_ns() - start) / 1e9
    if rc:
        raise RuntimeError(f'command exited {rc}; see {log}')
    return elapsed


def bazel_spawns(path):
    """Read Bazel's JSON SpawnExec stream; reject unknown/missing schemas.

    Root must qualify this parser against the pinned Bazel on a real cold build.
    Empty execution log is valid only after that engine's cold count was checked.
    """
    if not path.is_file():
        raise RuntimeError('missing Bazel execution log: ' + str(path))
    raw = path.read_text()
    decoder, objects = json.JSONDecoder(), []
    while raw.strip():
        value, end = decoder.raw_decode(raw.lstrip())
        raw = raw.lstrip()[end:]
        if isinstance(value, list):
            objects.extend(value)
        else:
            objects.append(value)
    count = 0
    for obj in objects:
        if not isinstance(obj, dict) or 'commandArgs' not in obj:
            raise RuntimeError('unknown Bazel SpawnExec schema')
        args = obj['commandArgs']
        if not isinstance(args, list) or not all(isinstance(v, str) for v in args):
            raise RuntimeError('invalid Bazel commandArgs')
        if not args or Path(args[0]).name != 'mojo' or 'precompile' not in args:
            continue
        # Count actual local successful compiler execution, never cache hits.
        # Bazel JSON omits protobuf default fields (exitCode=0/cacheHit=false).
        if obj.get('remoteCacheHit', False):
            raise RuntimeError('reference accepted a remote cached Mojo action')
        runner = obj.get('runner', '')
        if not runner or any(x in runner.lower() for x in ('cache', 'remote')):
            raise RuntimeError('nonlocal/unknown Mojo runner: ' + repr(runner))
        if obj.get('exitCode', 0) != 0:
            raise RuntimeError('failed reference Mojo action')
        count += 1
    return count


class Engine:
    def __init__(self, args, name):
        self.args, self.name = args, name
        self.root = args.output / name
        self.ws = self.root / 'source'
        self.binary = getattr(args, name.replace('-', '_')) if name != 'bazel' else args.bazel
        self.runtime = self.root / 'runtime'
        self.env = {
            'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
            'HOME': str(self.root / 'home'), 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8',
            'XDG_CACHE_HOME': str(self.root / 'home/.cache'),
            'XDG_CONFIG_HOME': str(self.root / 'home/.config'),
            'XDG_RUNTIME_DIR': str(self.runtime),
            'TMPDIR': str(self.root / 'tmp'),
            'RUSH_CACHE_ROOT': str(self.root / 'action-cache'),
            'RUSH_WORKSPACE_CACHE_ROOT': str(self.root / 'workspace-cache'),
            'RUSH_WORKSPACE_OUTPUT_ROOT': str(self.root / 'rush-out'),
            'RUSH_MOJO': str(args.toolchain / 'bin/mojo'),
            'RUSH_MOJO_TIMING': '1',
            'RUSH_BAZEL_PATH': str(self.root / 'bazel-cpu'),
            'CUDA_VISIBLE_DEVICES': '', 'HIP_VISIBLE_DEVICES': '', 'ROCR_VISIBLE_DEVICES': '',
        }
        # Do not inherit user Rust/Mojo/Bazel/remote-cache settings or accelerator overrides.
        self.bazel_start = [str(args.bazel), '--nohome_rc', '--nosystem_rc',
                            '--output_user_root=' + str(self.root / 'bazel-user'),
                            '--output_base=' + str(self.root / 'bazel-output')]
        self.command = ([str(self.binary), 'build', TARGET, '-j', str(args.jobs), '--daemon=auto']
                        if name != 'bazel' else
                        self.bazel_start + ['build', TARGET, '--jobs=' + str(args.jobs)] + CPU + LOCAL)
        self.artifact = ((self.root / 'rush-out' / PRODUCT) if name != 'bazel' else
                         self.ws / 'bazel-bin' / PRODUCT)

    def prepare(self):
        self.root.mkdir()
        for folder in ('home', 'home/.cache', 'home/.config', 'runtime', 'logs', 'tmp'):
            (self.root / folder).mkdir(exist_ok=True, mode=0o700)
        subprocess.run(['git', 'clone', '--no-hardlinks', '--no-checkout', str(self.args.source), str(self.ws)],
                       check=True, stdout=subprocess.DEVNULL)
        subprocess.run(['git', '-C', str(self.ws), 'checkout', '--detach', PIN],
                       check=True, stdout=subprocess.DEVNULL)
        # This file is required by the tracked upstream rc; only ignored generated
        # files change. No disk-cache config, local.bazelrc, or tmpfs sandbox.
        wrapper_rc = self.ws / 'build/wrapper.bazelrc'
        if git(self.ws, 'ls-files', 'build/wrapper.bazelrc'):
            raise RuntimeError('refuse to overwrite tracked wrapper.bazelrc')
        wrapper_rc.parent.mkdir(parents=True, exist_ok=True)
        wrapper_rc.write_text('# Private CPU benchmark; no shared cache or tmpfs sandbox.\n')
        # Rush's helper supplies its own --output_base; retain and record it.
        # Command-specific flags avoid sending build options to mod/shutdown.
        proxy = self.root / 'bazel-cpu'
        proxy.write_text('#!/usr/bin/python3\nimport os,sys,json\nfrom pathlib import Path\na=sys.argv[1:]\n'
                         'commands={"build","fetch","mod","sync","info","query","cquery","aquery","shutdown","clean"}\n'
                         'i=next((i for i,x in enumerate(a) if x in commands),len(a))\n'
                         'if i==len(a): raise SystemExit("unrecognized helper command")\n'
                         'command=a[i]\nextra=[]\n'
                         'if command in ' + repr(CONFIGURED_COMMANDS) + ': extra+=' + repr(CPU[:2]) + '\n'
                         'if command in ' + repr(REPOSITORY_COMMANDS) + ': extra+=' + repr(CPU[2:]) + '\n'
                         'startup=a[:i]\nbase=None\n'
                         'for j,value in enumerate(startup):\n'
                         ' if value=="--output_base": base=startup[j+1]\n'
                         ' elif value.startswith("--output_base="): base=value.split("=",1)[1]\n'
                         'if base is None:\n'
                         ' base=' + repr(str(self.root / 'bazel-output')) + '\n'
                         ' a.insert(0,"--output_base="+base)\n'
                         'base=Path(base).resolve()\n'
                         'if not base.is_relative_to(Path(' + repr(str(self.root)) + ')): raise SystemExit("non-task output base")\n'
                         'fd=os.open(' + repr(str(self.root / 'bazel-output-bases.jsonl')) + ',os.O_CREAT|os.O_APPEND|os.O_WRONLY,0o600)\n'
                         'try: os.write(fd,(json.dumps({"output_base":str(base),"command":command})+"\\n").encode())\n'
                         'finally: os.close(fd)\n'
                         'os.execv(' + repr(str(self.args.bazel)) + ',[' + repr(str(self.args.bazel)) +
                         ',"--nohome_rc","--nosystem_rc",' +
                         repr('--output_user_root=' + str(self.root / 'bazel-user')) +
                         ']+a+extra)\n')
        proxy.chmod(0o700)
        write_json(self.root / 'environment.json', self.env)
        write_json(self.root / 'build-command.json', self.command)

    def stop_bazel_helpers(self):
        bases = set()
        registry = self.root / 'bazel-output-bases.jsonl'
        if registry.exists():
            for line in registry.read_text().splitlines():
                bases.add(Path(json.loads(line)['output_base']))
        # Include the expected shared fallback base and any task-local nested
        # fallback servers (older helper layouts), never global Bazel roots.
        bases.add(self.root / 'workspace-cache/bazel_output_base')
        for pidfile in (self.root / 'workspace-cache').rglob('server/server.pid.txt'):
            bases.add(pidfile.parent.parent)
        errors = []
        for number, base in enumerate(sorted(bases)):
            resolved = base.resolve()
            if base != resolved or not resolved.is_relative_to(self.root):
                errors.append('unsafe Bazel cleanup base: ' + str(base))
                continue
            if not (base / 'server').is_dir():
                continue
            try:
                invoke([str(self.args.bazel), '--nohome_rc', '--nosystem_rc',
                        '--output_user_root=' + str(self.root / 'bazel-user'),
                        '--output_base=' + str(base), 'shutdown'], self.ws, self.env,
                       self.root / f'logs/helper-shutdown-{number}.log', 60)
            except Exception as error:
                errors.append(str(error))
        if errors:
            raise RuntimeError('; '.join(errors))

    def stop(self):
        if self.name == 'bazel':
            invoke(self.bazel_start + ['shutdown'], self.ws, self.env,
                   self.root / 'logs/shutdown.log', 60)
            return
        errors = []
        for metadata in self.runtime.rglob('metadata.json'):
            try:
                self.stop_rush_daemon(metadata)
            except Exception as error:
                errors.append(str(error))
        try:
            self.stop_bazel_helpers()
        except Exception as error:
            errors.append(str(error))
        if errors:
            raise RuntimeError('; '.join(errors))

    def stop_rush_daemon(self, metadata):
        pid = json.loads(metadata.read_text())['pid']
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            proc = Path('/proc') / str(pid)
            if ((proc / 'exe').resolve() != self.binary or
                (proc / 'cwd').resolve() != self.ws or
                b'daemon' not in (proc / 'cmdline').read_bytes().split(b'\0')):
                raise RuntimeError('refuse to signal daemon outside this lane')
            signal.pidfd_send_signal(fd, signal.SIGTERM)
            if not select.select([fd], [], [], 15)[0]:
                raise RuntimeError('private daemon failed to stop')
        finally:
            os.close(fd)

    def clean(self, sample):
        if self.name == 'bazel':
            invoke(self.bazel_start + ['clean'], self.ws, self.env,
                   self.root / f'logs/clean-{sample}.log', self.args.timeout)
            self.stop()
        else:
            self.stop()
            # Preserve fetched external repos and analysis metadata, but remove
            # every compiled result/CAS/output and persistent no-op certificate.
            for path in (self.root / 'action-cache', self.root / 'rush-out',
                         self.root / 'workspace-cache/daemon-noop-cache', self.runtime):
                if path.exists():
                    if path.is_symlink() or not path.resolve().is_relative_to(self.root):
                        raise RuntimeError('unsafe cleanup path: ' + str(path))
                    shutil.rmtree(path)
            self.runtime.mkdir(mode=0o700)

    def build(self, mode, sample, expected):
        prefix = self.root / f'logs/{mode}-{sample}'
        command = list(self.command)
        if self.name == 'bazel':
            command.append('--execution_log_json_file=' + str(prefix) + '.spawns.json')
        daemon_offsets = {path: path.stat().st_size for path in self.runtime.rglob('rushd.log')}
        seconds = invoke(command, self.ws, self.env, Path(str(prefix) + '.log'), self.args.timeout)
        if self.name == 'bazel':
            compilers = bazel_spawns(Path(str(prefix) + '.spawns.json'))
        else:
            client_records = MOJO_LOG.findall(Path(str(prefix) + '.log').read_text(errors='replace'))
            daemon_records = []
            for path in self.runtime.rglob('rushd.log'):
                offset = daemon_offsets.get(path, 0)
                with path.open('rb') as stream:
                    stream.seek(offset)
                    daemon_records.extend(MOJO_LOG.findall(stream.read().decode(errors='replace')))
            if client_records and daemon_records and client_records != daemon_records:
                raise RuntimeError('client/server Mojo records disagree')
            records = daemon_records or client_records
            if any(int(exit_code) for _, exit_code in records):
                raise RuntimeError('failed Rush Mojo action')
            compilers = sum(cached == 'false' for cached, _ in records)
        if compilers != expected:
            raise RuntimeError(f'{self.name}/{mode}: expected {expected} real Mojo compiler calls, got {compilers}')
        if not self.artifact.is_file() or self.artifact.stat().st_size < 1024:
            raise RuntimeError('missing/nonmaterialized std.mojoc: ' + str(self.artifact))
        return {'engine': self.name, 'mode': mode, 'sample': sample, 'seconds': seconds,
                'mojo_precompiles': compilers, 'std_mojoc_sha256': sha(self.artifact),
                'std_mojoc_bytes': self.artifact.stat().st_size,
                'edit_sha256': sha(self.ws / EDIT), 'argv': command}


def validate_lane(engine):
    if git(engine.ws, 'rev-parse', 'HEAD') != PIN:
        raise RuntimeError('prepared lane changed revision: ' + engine.name)
    if git(engine.ws, 'status', '--porcelain', '--untracked-files=no'):
        raise RuntimeError('prepared lane has tracked changes: ' + engine.name)


@contextmanager
def claim_run(root, phase):
    lock = root / '.run.lock'
    descriptor = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    try:
        with (root / ('.' + phase + '.started')).open('x') as stream:
            stream.write(str(os.getpid()) + '\n')
        if phase == 'measure' and (root / 'results.json').exists():
            raise RuntimeError('measurement evidence already exists')
        yield
    finally:
        os.close(descriptor)
        lock.unlink()


def qualify(args, engines):
    destination = args.output / 'ROOT-QUALIFIED.json'
    if destination.exists():
        raise RuntimeError('qualification evidence already exists')
    rows, cleanup_errors = [], []
    failure = None
    try:
        for engine in engines:
            validate_lane(engine)
            engine.clean('qualification')
            rows.append(engine.build('qualification', 0, 1))
        if len({row['std_mojoc_sha256'] for row in rows}) != 1:
            raise RuntimeError('bootstrap compiler outputs differ')
    except BaseException as error:
        failure = error
    finally:
        for engine in engines:
            try:
                engine.stop()
                validate_lane(engine)
            except Exception as error:
                cleanup_errors.append(str(error))
    if cleanup_errors:
        raise RuntimeError('qualification cleanup failed: ' + '; '.join(cleanup_errors)) from failure
    if failure is not None:
        raise failure
    write_json(destination, {
        'inputs_sha256': sha(args.output / 'inputs.json'),
        'controller_sha256': sha(Path(__file__)),
        'scope': 'one CPU stdlib package; no runtime or GPU program',
        'rows': rows,
    })


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'toolchain', 'bazel', 'rush-before', 'rush-after', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--mode', choices=('prepare', 'qualify', 'measure'), required=True)
    parser.add_argument('--jobs', type=int, default=8, choices=(8,))
    parser.add_argument('--clean-samples', type=int, default=1)
    parser.add_argument('--noop-samples', type=int, default=5)
    parser.add_argument('--edit-samples', type=int, default=3)
    parser.add_argument('--timeout', type=int, default=1800)
    args = parser.parse_args()
    for name in ('source', 'toolchain', 'bazel', 'rush_before', 'rush_after', 'output'):
        setattr(args, name, getattr(args, name).expanduser().resolve())
    if min(args.clean_samples, args.noop_samples, args.edit_samples, args.timeout) < 1:
        parser.error('sample counts/timeouts must be positive')
    if git(args.source, 'rev-parse', 'HEAD') != PIN:
        raise RuntimeError('source must be checked out at exact pin ' + PIN)
    if git(args.source, 'status', '--porcelain', '--untracked-files=no'):
        raise RuntimeError('source has tracked changes')
    binaries = {'bazel': args.bazel, 'rush-before': args.rush_before,
                'rush-after': args.rush_after, 'mojo': args.toolchain / 'bin/mojo'}
    if not (args.toolchain / 'lib').is_dir():
        raise RuntimeError('toolchain must be the entire bin/ + lib/ redistributable')
    for name, binary in binaries.items():
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError('missing executable ' + name + ': ' + str(binary))
    identities = {name: {'path': str(path), 'sha256': sha(path)} for name, path in binaries.items()}
    identities['toolchain_tree'] = {'path': str(args.toolchain), 'sha256': toolchain_digest(args.toolchain)}
    allowed = sorted(os.sched_getaffinity(0))
    if len(allowed) < args.jobs:
        raise RuntimeError('CPU8 profile requires eight allowed CPUs')
    cpus = allowed[:args.jobs]
    os.sched_setaffinity(0, cpus)
    engines = [Engine(args, name) for name in ('rush-before', 'rush-after', 'bazel')]
    if args.mode == 'prepare':
        if args.output.exists():
            raise RuntimeError('prepare requires a new, absent output directory')
        args.output.mkdir(parents=True, mode=0o700)
        (args.output / 'OWNER').write_text(MARKER + '\n')
        for engine in engines:
            engine.prepare()
        write_json(args.output / 'inputs.json', {'schema': MARKER, 'pin': PIN, 'target': TARGET,
                   'binaries': identities, 'cpu_flags': CPU, 'reference_flags': LOCAL,
                   'cpus': cpus,
                   'clean_definition': 'cold compiled artifacts; fetched repositories/analysis metadata retained',
                   'state': 'prepared; root bootstrap and qualification required'})
        print('Prepared private lanes. Provision pinned dependencies, then run --mode qualify before --mode measure.')
        return
    if (args.output / 'OWNER').read_text().strip() != MARKER:
        raise RuntimeError('output is not owned by this controller')
    saved = json.loads((args.output / 'inputs.json').read_text())
    if saved['binaries'] != identities:
        raise RuntimeError('binary identity changed after preparation')
    if saved['cpus'] != cpus:
        raise RuntimeError('CPU affinity changed after preparation')
    for engine in engines:
        validate_lane(engine)
    with claim_run(args.output, args.mode):
        if args.mode == 'qualify':
            qualify(args, engines)
            print('Qualification passed; run --mode measure once in this output root.')
            return
        measure(args, engines, saved)


def measure(args, engines, saved):
    qualification = json.loads((args.output / 'ROOT-QUALIFIED.json').read_text())
    if qualification.get('inputs_sha256') != sha(args.output / 'inputs.json'):
        raise RuntimeError('root qualification does not bind these prepared inputs')
    if qualification.get('controller_sha256') != sha(Path(__file__)):
        raise RuntimeError('controller changed after qualification')
    originals = {e.name: (e.ws / EDIT).read_bytes() for e in engines}
    if len(set(originals.values())) != 1:
        raise RuntimeError('lane edit inputs differ')
    rows, digest = [], None
    summary = {'inputs': saved, 'qualification': qualification, 'complete': False, 'rows': rows}
    try:
        for mode, samples in (('clean', args.clean_samples), ('noop', args.noop_samples),
                              ('novel-edit', args.edit_samples)):
            for sample in range(samples):
                edit = (originals[engines[0].name] +
                        f'\n# private-rush-cpu-novel-{sample:06d}\n'.encode()
                        if mode == 'novel-edit' else originals[engines[0].name])
                # Balanced within-host ordering; hosts must be timed serially by root.
                ordered = engines[sample % 3:] + engines[:sample % 3]
                for engine in ordered:
                    path = engine.ws / EDIT
                    if path.read_bytes() != edit:
                        path.write_bytes(edit)
                    if mode == 'clean':
                        engine.clean(sample)
                    row = engine.build(mode, sample, 0 if mode == 'noop' else 1)
                    if digest is None:
                        digest = row['std_mojoc_sha256']
                    if row['std_mojoc_sha256'] != digest:
                        raise RuntimeError('std.mojoc differs across engines or comment-only edits')
                    rows.append(row)
                    write_json(args.output / 'results.json', summary)
        summary['complete'] = True
        summary['medians_seconds'] = {e.name: {mode: statistics.median(
            r['seconds'] for r in rows if r['engine'] == e.name and r['mode'] == mode)
            for mode in ('clean', 'noop', 'novel-edit')} for e in engines}
    finally:
        for engine in engines:
            try:
                (engine.ws / EDIT).write_bytes(originals[engine.name])
            except Exception as error:
                summary.setdefault('cleanup_errors', []).append(str(error))
                summary['complete'] = False
        for engine in engines:
            try:
                engine.stop()
                validate_lane(engine)
            except Exception as error:
                summary.setdefault('cleanup_errors', []).append(str(error))
                summary['complete'] = False
        summary['sources_restored'] = not summary.get('cleanup_errors')
        write_json(args.output / 'results.json', summary)
    if not summary['complete']:
        raise RuntimeError('incomplete result; inspect results.json')
    print(json.dumps(summary['medians_seconds'], indent=2))


if __name__ == '__main__':
    main()
