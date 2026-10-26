#!/usr/bin/env python3
"""Bounded local experiment, not a swarm benchmark. Leaves evidence under .local/.
Copies sources once; two workers use that same source snapshot and separate targets.
Only probe comments in the copied sources are changed. Requires prebuilt brick CLI.
"""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
RUN = ROOT / '.local' / ('isolation-' + uuid.uuid4().hex[:10])
SOURCE = RUN / 'source'
CARGO = shutil.which('cargo')
ENV = dict(os.environ, RUSTC_WRAPPER='')
RESULTS = {'scope': 'one host, two build workers, synthetic source edits, no swarm throughput claim',
           'cache_wrapper': 'disabled for experiment', 'source_snapshot': str(SOURCE), 'phases': {}}


def measured(label, command, env=None):
    print(label, flush=True)
    stdout = RUN / (label + '.stdout')
    stderr = RUN / (label + '.stderr')
    started = time.monotonic()
    with stdout.open('w') as out, stderr.open('w') as err:
        process = subprocess.Popen(['/usr/bin/time', '-l'] + list(command), cwd=SOURCE,
                                   env=env or ENV, stdout=out, stderr=err, start_new_session=True)
        try:
            code = process.wait(timeout=300)
        except BaseException:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            raise
    phase = {'elapsed_seconds': round(time.monotonic() - started, 3), 'exit_code': code}
    for line in stderr.read_text().splitlines():
        if 'maximum resident set size' in line:
            phase['time_reported_max_rss_bytes'] = int(line.split()[0])
    artifacts = []
    for line in stdout.read_text().splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'compiler-artifact':
            artifacts.append(item)
    if artifacts:
        phase['rebuilt_local_targets'] = sorted(set(a['target']['name'] for a in artifacts
            if not a['fresh'] and a['target']['src_path'].startswith(str(SOURCE))))
        phase['all_local_packages'] = sorted(set(a['package_id'].split('#')[-1].split('@')[0] if '@' in a['package_id'].split('#')[-1] else a['package_id'].split('#')[0].split('/')[-1]
            for a in artifacts if a['target']['src_path'].startswith(str(SOURCE))))
    RESULTS['phases'][label] = phase
    assert code == 0, stderr.read_text()[-4000:]
    return artifacts


def build(label, package, target, tests=False, jobs=2):
    command = [CARGO, 'test' if tests else 'build', '--locked', '--offline', '-p', package,
               '--target-dir', str(target), '--message-format=json', '-j', str(jobs)]
    if tests:
        command.append('--no-run')
    return measured(label, command)


def test_executables(artifacts):
    return [a['executable'] for a in artifacts if a.get('executable') and a['profile']['test']]


def run_tests(label, executables):
    # Runtime PATH is empty: the test provider cannot find protoc or other tools.
    env = dict(ENV, PATH='', PROTOC='/unavailable/protoc')
    for index, executable in enumerate(executables):
        measured(label + '-' + str(index), [executable, '--test-threads=1'], env)


def disk_bytes(path):
    # Allocated target size, rather than logical file lengths.
    return int(subprocess.check_output(['/usr/bin/du', '-sk', str(path)]).split()[0]) * 1024


def service(args, env):
    process = subprocess.Popen(args, cwd=SOURCE, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            assert selector.select(15), 'service startup timed out'
            line = process.stdout.readline()
        if not line:
            raise RuntimeError(process.stderr.read())
        return process, json.loads(line)['address']
    except BaseException:
        stop(process)
        raise


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    assert process.returncode == 0, process.stderr.read()


def main():
    assert sys.platform == 'darwin', 'RSS measurement currently uses macOS time -l'
    assert CARGO
    client = ROOT / 'target/debug/brick'
    assert client.exists(), 'Build brick-cli first (outside the measured experiment)'
    SOURCE.mkdir(parents=True)
    for name in ['crates', 'contracts', 'interfaces', 'fixtures', '.cargo']:
        shutil.copytree(ROOT / name, SOURCE / name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(ROOT / name, SOURCE / name)
    manifest = ''.join(hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + p.relative_to(SOURCE).as_posix() + '\n'
                       for p in sorted(SOURCE.rglob('*')) if p.is_file())
    (RUN / 'source-initial.sha256').write_text(manifest)
    RESULTS['source_initial_manifest_sha256'] = hashlib.sha256(manifest.encode()).hexdigest()
    script_bytes = Path(__file__).read_bytes()
    (RUN / 'measurement-script.py').write_bytes(script_bytes)
    RESULTS['measurement_script_sha256'] = hashlib.sha256(script_bytes).hexdigest()
    RESULTS['rustc'] = subprocess.check_output(['rustc', '-Vv'], text=True)
    RESULTS['cargo'] = subprocess.check_output([CARGO, '-V'], text=True)
    gt = RUN / 'generator-target'
    ct = RUN / 'compiler-target'
    artifacts = build('generator-cold', 'brick-generator', gt, tests=True)
    packages = RESULTS['phases']['generator-cold']['all_local_packages']
    assert set(packages) <= {'brick-generator', 'brick-contract', 'brick-rpc', 'brick-build', 'brick-compiler-contract'}, packages
    assert not list(gt.glob('debug/deps/libbrick_compiler-*')), 'compiler implementation leaked into build'
    run_tests('generator-without-protoc', test_executables(artifacts))
    build('compiler-cold', 'brick-compiler', ct)
    # Two disjoint edits in the SAME copied codebase. No Git worktree per worker.
    for package in ['brick-generator', 'brick-compiler']:
        path = SOURCE / 'crates' / package / 'src/lib.rs'
        with path.open('a') as out:
            out.write('\n// Isolation experiment: independent implementation edit.\n')
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        compiler_job = pool.submit(build, 'compiler-edit-concurrent', 'brick-compiler', ct, False, 1)
        run_tests('generator-existing-tests-during-compiler-build', test_executables(artifacts))
        RESULTS['compiler_worker_active_after_existing_tests'] = not compiler_job.done()
        generator_job = pool.submit(build, 'generator-edit-concurrent', 'brick-generator', gt, True, 1)
        updated = generator_job.result()
        run_tests('generator-verify-concurrent', test_executables(updated))
        compiler_job.result()
    assert set(RESULTS['phases']['generator-edit-concurrent']['rebuilt_local_targets']) <= {'brick_generator', 'brick-generator', 'isolation'}
    # A later compiler-only edit must not rebuild any generator target.
    with (SOURCE / 'crates/brick-compiler/src/lib.rs').open('a') as out:
        out.write('\n// A second compiler-only change.\n')
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        other = pool.submit(build, 'compiler-only-edit', 'brick-compiler', ct, False, 1)
        clean = pool.submit(build, 'generator-after-compiler-edit', 'brick-generator', gt, True, 1)
        clean.result()
        other.result()
    assert not RESULTS['phases']['generator-after-compiler-edit']['rebuilt_local_targets']
    # Swap the dependency to the real standalone compiler without changing generator.
    fixtures = SOURCE / 'fixtures'
    compiler, address = service([str(ct / 'debug/brick-compiler'), '--contract', str(fixtures / 'compiler.bundle.json')], ENV)
    generator = None
    try:
        source = json.loads((fixtures / 'compiler.bundle.json').read_text())['source']
        # Exact compiler authored schema is already recorded as the shadow output contract.
        # Extract via the seed CLI check's identity output would require compiler work;
        # use the known multiline literal in the authored interface instead.
        schema = source.split("'''", 2)[1]
        if schema.startswith('\n'):
            schema = schema[1:]
        pin = hashlib.sha256(schema.encode()).hexdigest()
        generator, gen_address = service([str(gt / 'debug/brick-generator'), '--contract', str(fixtures / 'generate-bundle.bundle.json'),
            '--compiler-contract', str(fixtures / 'compiler.bundle.json'), '--compiler-id', pin,
            '--compiler-address', address], dict(ENV, PATH=''))
        output = RUN / 'real-generated-echo.json'
        measured('real-dependency-swap', [str(client), 'generate', str(SOURCE / 'interfaces/echo.brick.toml'),
            '--via', gen_address, '--contract', str(fixtures / 'generate-bundle.bundle.json'), '--output', str(output)], dict(ENV, PATH=''))
        measured('real-output-shadow-verification', [str(client), 'verify', str(output)], dict(ENV, PATH=''))
        RESULTS['real_artifact_digest'] = json.loads(output.read_text())['digest']
    finally:
        try:
            if generator is not None:
                stop(generator)
        finally:
            stop(compiler)
    RESULTS['allocated_target_bytes'] = {'generator': disk_bytes(gt), 'compiler': disk_bytes(ct)}
    RESULTS['limitation'] = 'Separate targets duplicate dependency artifacts. RSS is time-reported per command, not simultaneous host peak. Source edits are synthetic comments; this proves invalidation scope, not productive 100-agent throughput.'
    report = RUN / 'report.json'
    report.write_text(json.dumps(RESULTS, indent=2) + '\n')
    print(str(report), flush=True)


if __name__ == '__main__':
    main()
