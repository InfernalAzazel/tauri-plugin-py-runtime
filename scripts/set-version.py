"""同步设置三个包的版本号：`python3 scripts/set-version.py 0.2.0`。

需同步的位置：Cargo.toml、package.json、python-sdk/pyproject.toml 与 SDK_VERSION
（CLI 按 SDK_VERSION 安装同版本的 Rust crate 与前端包）。
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TARGETS = [
    ("Cargo.toml", r'^version = "[^"]*"', 'version = "{v}"'),
    ("package.json", r'^  "version": "[^"]*"', '  "version": "{v}"'),
    ("python-sdk/pyproject.toml", r'^version = "[^"]*"', 'version = "{v}"'),
    ("python-sdk/tauri_runtime/protocol.py", r'^SDK_VERSION = "[^"]*"', 'SDK_VERSION = "{v}"'),
]


def main() -> int:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        print("usage: python3 scripts/set-version.py X.Y.Z", file=sys.stderr)
        return 1
    version = sys.argv[1]
    for path, pattern, replacement in TARGETS:
        file = ROOT / path
        text, count = re.subn(pattern, replacement.format(v=version), file.read_text(encoding="utf-8"), count=1, flags=re.M)
        if count != 1:
            print(f"version not found in {path}", file=sys.stderr)
            return 1
        file.write_text(text, encoding="utf-8")
        print(f"{path}: {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
