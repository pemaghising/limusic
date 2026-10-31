#!/usr/bin/env python3
"""Check bundled Mach-O dependencies and architectures; optionally deduplicate rpaths."""
import argparse
from collections import Counter
from pathlib import Path
import plistlib
import re
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('app', type=Path)
parser.add_argument('--dedupe', action='store_true')
args = parser.parse_args()
app = args.app.resolve()
with (app / 'Contents/Info.plist').open('rb') as stream:
    executable = app / 'Contents/MacOS' / plistlib.load(stream)['CFBundleExecutable']


def output(*command):
    return subprocess.check_output(command, text=True)


def rpaths(path):
    return re.findall(r'cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset', output('otool', '-l', str(path)))


files = [p for p in (app / 'Contents').rglob('*') if p.is_file() and not p.is_symlink()
         and 'Mach-O' in output('file', '-b', str(p))]
errors = []
for path in files:
    if args.dedupe:
        for value, count in Counter(rpaths(path)).items():
            for _ in range(count - 1):
                subprocess.run(['install_name_tool', '-delete_rpath', value, str(path)], check=True)
    if len(rpaths(path)) != len(set(rpaths(path))):
        errors.append(f'{path.name}: duplicate rpaths')
    if 'x86_64' not in output('lipo', '-archs', str(path)).split():
        errors.append(f'{path.name}: missing Intel architecture')
    text = output('otool', '-L', str(path))
    for dependency in re.findall(r'^\s+(.+?) \(compatibility version', text, re.M):
        if dependency.startswith(('/System/Library/', '/usr/lib/', '/Library/Apple/')):
            continue
        def expand(value):
            return value.replace('@executable_path', str(executable.parent)).replace('@loader_path', str(path.parent))
        if dependency.startswith('@rpath/'):
            candidates = [Path(expand(rp)) / dependency[7:]
                          for rp in rpaths(path) + rpaths(executable)]
        else:
            candidates = [Path(expand(dependency))]
        if not any(p.is_relative_to(app) and p.exists() and p.resolve().is_relative_to(app)
                   for p in candidates):
            errors.append(f'{path.name}: unresolved or external dependency {dependency}')
    for rp in rpaths(path):
        if rp.startswith(('/opt/', '/usr/local/', '/Users/')):
            errors.append(f'{path.name}: external rpath {rp}')
if errors:
    raise SystemExit('\n'.join(errors))
print(f'PASS: {len(files)} Intel Mach-O files; all non-system dependencies resolve inside the app; no duplicate rpaths')
