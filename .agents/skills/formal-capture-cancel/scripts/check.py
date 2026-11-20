#!/usr/bin/env python3
"""Run the bounded cancellation model without writing generated files in the checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

SKILL = Path(__file__).resolve().parents[1]
ROOT = SKILL.parents[2]
MODEL = 'CaptureCancel'
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


def run(java, jar, evidence, label, scenario='all', flags=(), invariant=None, mutation=None):
    run_dir = evidence / label
    run_dir.mkdir()
    model = (SKILL / 'CaptureCancel.tla').read_text()
    if mutation:
        old, new = mutation
        if model.count(old) != 1:
            raise RuntimeError(f'{label}: model mutation no longer matches; update it from the code')
        model = model.replace(old, new)
    (run_dir / 'CaptureCancel.tla').write_text(model)
    config = (SKILL / 'CaptureCancel.cfg').read_text()
    config = config.replace('Scenario = "all"', f'Scenario = "{scenario}"')
    for flag in flags:
        old = f'{flag} = FALSE'
        if config.count(old) != 1:
            raise RuntimeError(f'{label}: {flag} no longer matches the faithful config')
        config = config.replace(old, f'{flag} = TRUE')
    if invariant:
        config = re.sub(r'INVARIANT .*', f'INVARIANT {invariant}', config)
    (run_dir / 'CaptureCancel.cfg').write_text(config)
    command = [java, '-Xmx512m', '-XX:+UseParallelGC', '-cp', str(jar),
               'tlc2.TLC', '-workers', '1', '-seed', '1', '-cleanup',
               '-metadir', str(run_dir / 'states'), 'CaptureCancel']
    with (run_dir / 'tlc.txt').open('w') as log:
        done = subprocess.run(command, cwd=run_dir, stdout=log, stderr=subprocess.STDOUT,
                              timeout=120, check=False)
    output = (run_dir / 'tlc.txt').read_text()
    violation = re.search(r'Invariant (\w+) is violated\.', output)
    passed = done.returncode == 0 and 'Model checking completed. No error has been found.' in output
    if not violation and not passed:
        raise RuntimeError(f'TLC did not produce a verdict; read {run_dir / "tlc.txt"}')
    print(f'{label}: {"COUNTEREXAMPLE " + violation.group(1) + " (" + str(run_dir / "tlc.txt") + ")" if violation else "PASS"}', flush=True)
    for line in output.splitlines():
        if 'states generated' in line or line.startswith('Finished in'):
            print(line, flush=True)
    return violation.group(1) if violation else None


def chart(java, jar, output):
    """Dumps the faithful model's states under TypeOK alone and draws chart.mmd from view.py."""
    scratch = Path(tempfile.mkdtemp(prefix='thalvi-formal-chart-', dir='/var/tmp'))
    shutil.copyfile(SKILL / f'{MODEL}.tla', scratch / f'{MODEL}.tla')
    config = re.sub(r'^INVARIANTS? .*$', 'INVARIANT TypeOK', (SKILL / f'{MODEL}.cfg').read_text(),
                    flags=re.M)
    (scratch / f'{MODEL}.cfg').write_text(config)
    dot = scratch / 'states.dot'
    command = [java, '-Xmx1g', '-XX:+UseParallelGC', '-cp', str(jar),
               'tlc2.TLC', '-workers', '1', '-seed', '1', '-cleanup', '-deadlock',
               '-dump', 'dot,actionlabels', str(dot), '-metadir', str(scratch / 'states'), MODEL]
    with (scratch / 'tlc.txt').open('w') as log:
        subprocess.run(command, cwd=scratch, stdout=log, stderr=subprocess.STDOUT,
                       timeout=600, check=False)
    if 'Model checking completed. No error has been found.' not in (scratch / 'tlc.txt').read_text():
        print(f'CHART failed: TLC did not finish the state dump; read {scratch / "tlc.txt"}')
        return 2
    drawn = subprocess.run([sys.executable, '-I', str(SKILL / 'scripts' / 'chart.py'),
                            '--dot', str(dot), '--view', str(SKILL / 'view.py'),
                            '--out', str(output)], check=False)
    shutil.rmtree(scratch)
    return drawn.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true', help='check guard-removal negative controls')
    parser.add_argument('command', nargs='?', choices=['chart'],
                        help='draw chart.mmd from the model for review; never a verdict')
    parser.add_argument('--out', type=Path, default=SKILL / 'chart.mmd',
                        help='output path for the chart command')
    args = parser.parse_args()
    if args.command == 'chart':
        return chart(java_command(), fetch(TLC_NAME, TLC_URL, TLC_SHA), args.out)
    source_check()
    jar = fetch(TLC_NAME, TLC_URL, TLC_SHA)
    java = java_command()
    evidence = Path(tempfile.mkdtemp(prefix='thalvi-formal-', dir='/var/tmp'))
    keep = True
    try:
        if args.self_test:
            if run(java, jar, evidence, 'faithful'):
                raise RuntimeError('faithful model unexpectedly fails')
            controls = [
                ('removed-completion-guard', 'NoDeliveryAfterStop', ('DropCompletionGuard',), None),
                ('forgot-earlier-stop', 'NoDeliveryAfterStop', ('ForgetEarlierStop',), None),
                ('leaked-extra-session', 'ExtrasNeverLeftOpen', ('LeakExtraOnCancel',), None),
                ('cancel-keeps-extra', 'ReleasedWhenCancelled', ('LeakExtraOnCancel',), None),
                ('late-without-merge', 'LateOnlyAfterMerge', (),
                 ('THEN "released" ELSE x', 'THEN "late" ELSE x')),
                ('accept-before-load', 'AcceptedOnlyAfterLoad', (),
                 ('phase = "active" /\\ ~starting /\\ ~cancelPending /\\ (~cleaning \\/ secondary)',
                  'phase = "active" /\\ ~cancelPending /\\ (~cleaning \\/ secondary)')),
            ]
            for label, invariant, flags, mutation in controls:
                caught = run(java, jar, evidence, label, flags=flags,
                             invariant=invariant, mutation=mutation)
                if caught != invariant:
                    raise RuntimeError(f'{label} should break {invariant}, got {caught or "a pass"}')
            print('SELF-TEST PASS: faithful model passed; all six controls broke their named invariant.')
            return 0
        violation = run(java, jar, evidence, 'faithful')
        keep = violation
        print(f'FAIL: {violation} is broken; replay the trace using SKILL.md.' if violation
              else 'PASS: all five invariants hold in this bounded model; see SKILL.md for scope.')
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
