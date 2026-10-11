#!/usr/bin/env python3
"""Package a built FerriteS100 executable as a relocatable native macOS app."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    root = Path(__file__).resolve().parents[1]
    parser.add_argument('--binary', type=Path, default=root / 'target/release-fast/ferrite-s100')
    parser.add_argument('--output', type=Path, default=root / 'target/macos/FerriteS100.app')
    parser.add_argument('--inventory', type=Path, default=root.parent / 'S101-Catalogues')
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('Packaging requires macOS (sips, iconutil and codesign).')
    output = args.output.absolute()
    if output.suffix != '.app' or output.exists():
        parser.error('Output must be a new .app path; existing apps are preserved.')
    if not args.binary.is_file() or not args.inventory.is_dir():
        parser.error('Build the executable and prepare the catalogue inventory first.')
    version = re.search(r'(?m)^version = "([0-9.]+)"$', (root / 'Cargo.toml').read_text()).group(1)
    linked = subprocess.check_output(['otool', '-L', str(args.binary)], text=True)
    dependencies = [line.strip().split(' (')[0] for line in linked.splitlines()[1:]]
    if any(not p.startswith(('/System/Library/', '/usr/lib/')) for p in dependencies):
        parser.error('Executable has external dynamic dependencies; bundle them before packaging.')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.ferrite-package-', dir=output.parent) as temp:
        stage = Path(temp) / output.name
        contents = stage / 'Contents'
        resources = contents / 'Resources'
        executable = contents / 'MacOS/ferrite-s100'
        executable.parent.mkdir(parents=True)
        resources.mkdir()
        shutil.copy2(args.binary, executable)
        executable.chmod(0o755)
        # Actual copies allow relocation without the repository or symbolic links.
        for name in ['Catalogues', 'Trust']:
            shutil.copytree(root / name, resources / name)
        inventory_versions = sorted(p for p in args.inventory.iterdir()
                                    if p.is_dir() and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", p.name)
                                    and (p / 'FC').is_dir() and (p / 'PC').is_dir())
        if not inventory_versions:
            parser.error('No FC/PC version pairs found in the inventory.')
        for source in inventory_versions:
            for component in ['FC', 'PC']:
                shutil.copytree(source / component,
                                resources / 'S101-Catalogues' / source.name / component)
        icon_source = root / 'icon.ico'
        if not icon_source.is_file():
            parser.error('The repository icon.ico is required for the application icon.')
        shutil.copy2(icon_source, resources / 'icon.ico')
        icon_png = Path(temp) / 'repository-icon.png'
        subprocess.run(['sips', '-s', 'format', 'png', str(icon_source), '--out', str(icon_png)],
                       check=True, stdout=subprocess.DEVNULL)
        iconset = Path(temp) / 'FerriteS100.iconset'
        iconset.mkdir()
        for size in [16, 32, 128, 256, 512]:
            for multiplier in [1, 2]:
                suffix = '@2x' if multiplier == 2 else ''
                dest = iconset / f'icon_{size}x{size}{suffix}.png'
                subprocess.run(['sips', '-z', str(size * multiplier), str(size * multiplier),
                                str(icon_png), '--out', str(dest)], check=True,
                               stdout=subprocess.DEVNULL)
        subprocess.run(['iconutil', '-c', 'icns', str(iconset), '-o', str(resources / 'FerriteS100.icns')], check=True)
        info = {
            'CFBundleName': 'FerriteS100', 'CFBundleDisplayName': 'FerriteS100',
            'CFBundleIdentifier': 'io.semanticwave.FerriteS100',
            'CFBundleExecutable': 'ferrite-s100', 'CFBundlePackageType': 'APPL',
            'CFBundleShortVersionString': version, 'CFBundleVersion': version,
            'CFBundleIconFile': 'FerriteS100.icns', 'NSHighResolutionCapable': True,
            'NSPrincipalClass': 'NSApplication', 'LSMinimumSystemVersion': '12.0',
        }
        with (contents / 'Info.plist').open('wb') as handle:
            plistlib.dump(info, handle)
        (contents / 'PkgInfo').write_bytes(b'APPL????')
        digest = hashlib.sha256(executable.read_bytes()).hexdigest()
        (resources / 'package-manifest.json').write_text(json.dumps({
            'version': version, 'build_binary_sha256': digest,
            'icon_source': 'icon.ico', 'icon_source_sha256': hashlib.sha256(icon_source.read_bytes()).hexdigest(),
            'catalogue_versions': [p.name for p in inventory_versions],
            'dynamic_dependencies': dependencies, 'signing': 'local ad hoc',
        }, indent=2) + '\n')
        subprocess.run(['codesign', '--force', '--sign', '-', str(stage)], check=True)
        subprocess.run(['codesign', '--verify', '--strict', str(stage)], check=True)
        subprocess.run(['plutil', '-lint', str(contents / 'Info.plist')], check=True)
        os.rename(stage, output)
    print(output)


if __name__ == '__main__':
    main()
