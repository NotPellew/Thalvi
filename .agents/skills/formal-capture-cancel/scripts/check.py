#!/usr/bin/env python3
"""Run the bounded cancellation model without writing generated files in the checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

SKILL = Path(__file__).resolve().parents[1]
ROOT = SKILL.parents[2]
CACHE = (Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'thalvi-formal').resolve()
TLC_NAME = 'tla2tools-1.7.4.jar'
TLC_URL = 'https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar'
TLC_SHA = '936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88'
JRE_NAME = 'temurin-21.0.9+10-jre.tar.gz'
JRE_URL = ('https://github.com/adoptium/temurin21-binaries/releases/download/'
           'jdk-21.0.9%2B10/OpenJDK21U-jre_x64_linux_hotspot_21.0.9_10.tar.gz')
JRE_SHA = 'aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431'


def fetch(name, url, digest):
    CACHE.mkdir(parents=True, exist_ok=True)
    target = CACHE / name
    if not target.exists():
        with tempfile.NamedTemporaryFile(dir=CACHE, delete=False) as downloaded:
            partial = Path(downloaded.name)
        try:
            with urllib.request.urlopen(url, timeout=60) as response, partial.open('wb') as sink:
                shutil.copyfileobj(response, sink)
            if hashlib.sha256(partial.read_bytes()).hexdigest() != digest:
                raise RuntimeError(f'checksum mismatch: {url}')
            partial.replace(target)
        finally:
            partial.unlink(missing_ok=True)
    if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
        raise RuntimeError(f'checksum mismatch: {target}; remove it and rerun')
    return target


def java_command():
    java = shutil.which('java')
    if java:
        return java
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        raise RuntimeError('install Java 11+ in your user account and put java on PATH')
    archive = fetch(JRE_NAME, JRE_URL, JRE_SHA)
    java = CACHE / 'jdk-21.0.9+10-jre/bin/java'
    if not java.exists():
        with tarfile.open(archive) as bundle:
            bundle.extractall(CACHE, filter='data')
    return str(java)


def source_check():
    if CACHE.is_relative_to(ROOT):
        raise RuntimeError('XDG_CACHE_HOME must point outside the repository')
    inventory = json.loads((SKILL / 'source.json').read_text())
    changed = [p for p, item in inventory['files'].items()
               if not (ROOT / p).is_file()
               or hashlib.sha256((ROOT / p).read_bytes()).hexdigest() != item['sha256']]
    if changed:
        raise RuntimeError('SOURCE DRIFT: read the skill, update the model from these files, '
                           'then update source.json: ' + ', '.join(changed))
    print('Source matches recorded hashes; sync commit ' + inventory['sync_commit'], flush=True)


def run(java, jar, evidence, label, scenario='all', drop=False, forget=False):
    run_dir = evidence / label
    run_dir.mkdir()
    shutil.copyfile(SKILL / 'CaptureCancel.tla', run_dir / 'CaptureCancel.tla')
    config = (SKILL / 'CaptureCancel.cfg').read_text()
    config = config.replace('Scenario = "all"', f'Scenario = "{scenario}"')
    if drop:
        config = config.replace('DropCompletionGuard = FALSE', 'DropCompletionGuard = TRUE')
    if forget:
        config = config.replace('ForgetEarlierStop = FALSE', 'ForgetEarlierStop = TRUE')
    (run_dir / 'CaptureCancel.cfg').write_text(config)
    command = [java, '-Xmx512m', '-XX:+UseParallelGC', '-cp', str(jar),
               'tlc2.TLC', '-workers', '1', '-seed', '1', '-cleanup',
               '-metadir', str(run_dir / 'states'), 'CaptureCancel']
    with (run_dir / 'tlc.txt').open('w') as log:
        done = subprocess.run(command, cwd=run_dir, stdout=log, stderr=subprocess.STDOUT,
                              timeout=120, check=False)
    output = (run_dir / 'tlc.txt').read_text()
    violation = 'Invariant NoDeliveryAfterStop is violated.' in output
    passed = done.returncode == 0 and 'Model checking completed. No error has been found.' in output
    if not violation and not passed:
        raise RuntimeError(f'TLC did not produce a verdict; read {run_dir / "tlc.txt"}')
    print(f'{label}: {"COUNTEREXAMPLE (" + str(run_dir / "tlc.txt") + ")" if violation else "PASS"}', flush=True)
    for line in output.splitlines():
        if 'states generated' in line or line.startswith('Finished in'):
            print(line, flush=True)
    return violation


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true', help='check guard-removal negative controls')
    args = parser.parse_args()
    source_check()
    jar = fetch(TLC_NAME, TLC_URL, TLC_SHA)
    java = java_command()
    evidence = Path(tempfile.mkdtemp(prefix='thalvi-formal-', dir='/var/tmp'))
    keep = True
    try:
        if args.self_test:
            if run(java, jar, evidence, 'faithful-fence', scenario='fence'):
                raise RuntimeError('faithful fence control unexpectedly fails')
            if not run(java, jar, evidence, 'removed-completion-guard', scenario='fence', drop=True):
                raise RuntimeError('removed completion guard escaped the invariant')
            if not run(java, jar, evidence, 'forgot-earlier-stop', scenario='fence', forget=True):
                raise RuntimeError('overwritten drain stop escaped the invariant')
            print('SELF-TEST PASS: faithful fence control passed; both mutations failed. '
                  'Run without --self-test for the full model verdict.')
            return 0
        violation = run(java, jar, evidence, 'faithful')
        keep = violation
        print('FAIL: cancellation can reach delivery; replay the trace using SKILL.md.' if violation
              else 'PASS: no delivery after a stop in this bounded model; see SKILL.md for scope.')
        return 1 if violation else 0
    finally:
        if keep:
            print(f'EVIDENCE={evidence}', flush=True)
        else:
            shutil.rmtree(evidence)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f'BLOCKED: {error}', file=sys.stderr)
        sys.exit(2)
