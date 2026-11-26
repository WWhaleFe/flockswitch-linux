#!/usr/bin/env python3
"""Run actual platform-independent source on macOS/Linux, without Linux HID/UI linking.
The generated facades extract the real waveform constants and shortcut parser verbatim.
All Cargo operations are offline. No source or dependency is downloaded or rewritten.
"""
from pathlib import Path
import os
import re
import subprocess

root = Path(__file__).resolve().parent.parent
scratch = root / 'target/host-tests'
scratch.mkdir(parents=True, exist_ok=True)

def run(args):
    subprocess.run(args, cwd=root, check=True, env={**os.environ, 'CARGO_TARGET_DIR': str(root / 'target/host-check')})

def item(source, pattern, closing):
    start = re.search(pattern, source, re.M).start()
    end = source.index(closing, start) + len(closing)
    return source[start:end]

device = (root / 'src/device.rs').read_text()
hotkey = (root / 'src/hotkey.rs').read_text()
(scratch / 'facades.rs').write_text(
    'pub mod device {\n' + item(device, r'^pub mod waveform', '\n}') + '\n}\n'
    'pub mod hotkey {\nuse anyhow::{anyhow, Result};\nuse global_hotkey::hotkey::HotKey;\n'
    + item(hotkey, r'^pub const LOCATE:', ';') + '\n'
    + item(hotkey, r'^pub fn parse\(', '\n}') + '\n}\n')
# Run the existing updater and Homebrew path tests against their real pure helpers.
update = (root / 'src/update.rs').read_text()
tray = (root / 'src/tray.rs').read_text()
with (scratch / 'facades.rs').open('a') as out:
    out.write('mod update_checks {\nuse std::path::PathBuf;\n')
    for name in ['pick_asset_for', 'is_newer', 'is_homebrew_path']:
        out.write(item(update, r'^(?:pub )?fn ' + name + r"(?:<|\()", '\n}') + '\n')
    out.write(item(tray, r'^fn brew_link\(', '\n}') + '\n')
    for name in ['versions', 'assets', 'homebrew_paths']:
        out.write('#[test]\n' + item(update, r'^    fn ' + name + r'\(', '\n    }') + '\n')
    out.write('#[test]\n' + item(tray, r'^    fn homebrew_path_without_the_version\(', '\n    }') + '\n}\n')
modules = ['config', 'clipboard', 'i18n', 'net_logic', 'edge_logic', 'setup_logic', 'locate_logic', 'window_size']
(scratch / 'lib.rs').write_text(
    '// These modules expose runtime entry points unused by this logic-only harness.\n'
    '#![allow(dead_code)]\ninclude!("facades.rs");\n' + ''.join(
        f'#[path = "{root / "src" / (name + ".rs")}"] mod {name};\n' for name in modules))
(scratch / 'Cargo.toml').write_text(f'''[package]
name = "flockswitch-host-tests"
version = "2.0.0"
edition = "2021"
[lib]
path = "lib.rs"
[dependencies]
anyhow = "1"
arboard = {{ version = "3.7.0", default-features = false, features = ["image-data", "wayland-data-control"] }}
dirs = "5"
flock-net = {{ path = "{root / 'netcore'}" }}
global-hotkey = "0.9"
log = "0.4"
png = "0.18"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
sys-locale = "0.3.2"
toml = "0.8"
''')
run(['cargo', 'test', '--offline', '--manifest-path', str(scratch / 'Cargo.toml')])
