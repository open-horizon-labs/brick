#!/usr/bin/env python3
"""Exercise a scaffold, build-time schema enforcement and typed provider errors."""
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
RUN = ROOT / '.local' / ('scaffold-' + uuid.uuid4().hex[:10])
RUN.mkdir(parents=True)
PROJECT = RUN / 'demo'
REPORT = {}


def run(label, args, env=None, success=True):
    started = time.monotonic()
    with (RUN / (label + '.stdout')).open('w') as out, (RUN / (label + '.stderr')).open('w') as err:
        process = subprocess.Popen(args, cwd=PROJECT if PROJECT.exists() else ROOT,
                                   env=env, stdout=out, stderr=err, start_new_session=True)
        try:
            code = process.wait(timeout=240)
        except BaseException:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise
    REPORT[label] = {'elapsed_seconds': round(time.monotonic() - started, 3), 'exit_code': code}
    assert (code == 0) == success, (RUN / (label + '.stderr')).read_text()[-4000:]
    return (RUN / (label + '.stderr')).read_text()


run('new-module', [str(ROOT / 'target/debug/brick'), 'new', 'module', 'demo', '--path', str(PROJECT)])
blocker = RUN / 'bin'
blocker.mkdir()
marker = RUN / 'protoc-was-called'
(blocker / 'protoc').write_text('#!/bin/sh\ntouch ' + shlex.quote(str(marker)) + '\nexit 99\n')
(blocker / 'protoc').chmod(0o755)
env = dict(os.environ, RUSTC_WRAPPER='', PROTOC=str(blocker / 'protoc'),
           PATH=str(blocker) + os.pathsep + os.environ['PATH'],
           CARGO_TARGET_DIR=str(ROOT / '.local/scaffold-target'))
run('typed-real-and-shadow', ['cargo', 'test', '--offline'], env)
assert not marker.exists(), 'Cargo build invoked protoc'
provider = PROJECT / 'implementation/src/lib.rs'
original = provider.read_text()
try:
    provider.write_text(original.replace('contract::Reply', 'contract::Request'))
    diagnostic = run('wrong-provider-type', ['cargo', 'check', '--offline', '--locked'], env, success=False)
    assert 'Reply' in diagnostic and 'Request' in diagnostic
finally:
    provider.write_text(original)
definition = PROJECT / 'contract/interface.brick.toml'
original = definition.read_text()
try:
    definition.write_text(original.replace('service Module', 'service ChangedModule'))
    diagnostic = run('stale-contract-bundle', ['cargo', 'check', '--offline', '--locked'], env, success=False)
    assert 'contract changed' in diagnostic
finally:
    definition.write_text(original)
run('restored-module', ['cargo', 'test', '--offline', '--locked'], env)
REPORT['cargo_builds_invoked_protoc'] = marker.exists()
(RUN / 'report.json').write_text(json.dumps(REPORT, indent=2) + '\n')
print(RUN / 'report.json')
