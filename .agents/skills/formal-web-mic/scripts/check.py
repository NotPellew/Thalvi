#!/usr/bin/env python3
"""Model-check the web client's microphone around a conversation's end, outside the checkout."""
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
MODEL = 'WebMic'
CACHE = (Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'thalvi-formal').resolve()
TLC_NAME = 'tla2tools-1.7.4.jar'
TLC_URL = 'https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar'
TLC_SHA = '936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88'
JRE_NAME = 'temurin-21.0.9+10-jre.tar.gz'
JRE_URL = ('https://github.com/adoptium/temurin21-binaries/releases/download/'
           'jdk-21.0.9%2B10/OpenJDK21U-jre_x64_linux_hotspot_21.0.9_10.tar.gz')
JRE_SHA = 'aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431'
PROPERTIES = ['NoOrphanMic', 'MicOnlyWithVoice', 'NoMicAfterEnd']
# Each mutation breaks a guard the code has, and the invariant it must break.
MUTATIONS = [
    ('OpenMicLeaksTracks', 'NoOrphanMic'),
    ('KeepsMicAfterEnd', 'NoMicAfterEnd'),
    ('ReleaseKeepsMic', 'MicOnlyWithVoice'),
    ('MicStoppedKeepsVoice', 'MicOnlyWithVoice'),
]


def steps(output):
    """The counterexample as numbered steps, one action each."""
    actions = re.findall(r'^State \d+: <(\w+)', output, re.M)[1:]
    return [f'{number}. {action}' for number, action in enumerate(actions, 1)]


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


def run(java, jar, evidence, label, flags=(), invariants=None):
    """Runs TLC once; returns the violated invariant's name, or None on a pass."""
    run_dir = evidence / label
    run_dir.mkdir()
    shutil.copyfile(SKILL / f'{MODEL}.tla', run_dir / f'{MODEL}.tla')
    config = (SKILL / f'{MODEL}.cfg').read_text()
    for flag in flags:
        config = config.replace(f'{flag} = FALSE', f'{flag} = TRUE')
    if invariants:
        config = re.sub(r'INVARIANTS .*', 'INVARIANTS ' + ' '.join(invariants), config)
    (run_dir / f'{MODEL}.cfg').write_text(config)
    command = [java, '-Xmx512m', '-XX:+UseParallelGC', '-cp', str(jar),
               'tlc2.TLC', '-workers', '1', '-seed', '1', '-cleanup',
               '-metadir', str(run_dir / 'states'), MODEL]
    with (run_dir / 'tlc.txt').open('w') as log:
        done = subprocess.run(command, cwd=run_dir, stdout=log, stderr=subprocess.STDOUT,
                              timeout=120, check=False)
    output = (run_dir / 'tlc.txt').read_text()
    violated = re.search(r'Invariant (\w+) is violated\.', output)
    passed = done.returncode == 0 and 'Model checking completed. No error has been found.' in output
    if not violated and not passed:
        raise RuntimeError(f'TLC did not produce a verdict; read {run_dir / "tlc.txt"}')
    verdict = f'COUNTEREXAMPLE {violated.group(1)}' if violated else 'PASS'
    print(f'{label}: {verdict} ({run_dir / "tlc.txt"})', flush=True)
    for line in output.splitlines():
        if 'states generated' in line or line.startswith('Finished in'):
            print(line, flush=True)
    if violated:
        print('\n'.join(steps(output)), flush=True)
    return violated.group(1) if violated else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true',
                        help='check that the mutations are caught')
    args = parser.parse_args()
    source_check()
    jar = fetch(TLC_NAME, TLC_URL, TLC_SHA)
    java = java_command()
    evidence = Path(tempfile.mkdtemp(prefix='thalvi-formal-web-mic-', dir='/var/tmp'))
    print(f'EVIDENCE={evidence}', flush=True)
    if args.self_test:
        if run(java, jar, evidence, 'faithful'):
            raise RuntimeError('the code as it is should pass every invariant')
        for mutation, invariant in MUTATIONS:
            caught = run(java, jar, evidence, mutation, [mutation], [invariant])
            if caught != invariant:
                raise RuntimeError(f'{mutation} should break {invariant}, got {caught or "a pass"}')
        print('SELF-TEST PASS: the code as it is passed and every mutation broke its '
              'invariant. Run without --self-test for the verdict on the code as it is.')
        return 0
    broken = [p for p in PROPERTIES if run(java, jar, evidence, f'faithful-{p}', invariants=[p])]
    print(f'FAIL: {", ".join(broken)} broken; replay each trace using SKILL.md.' if broken
          else 'PASS: every invariant holds in this bounded model; see SKILL.md for scope.')
    return 1 if broken else 0

if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f'BLOCKED: {error}', file=sys.stderr)
        sys.exit(2)
