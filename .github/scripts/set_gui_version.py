"""Sets maki-gui's version in its manifest and in Cargo.lock.

The desktop app compares its own `CARGO_PKG_VERSION` with the latest
release tag, so the two must move together.
"""

import re
import sys
from pathlib import Path

MANIFEST = Path("maki-gui/Cargo.toml")
LOCK = Path("Cargo.lock")


def main() -> None:
    version = sys.argv[1]
    manifest = MANIFEST.read_text(encoding="utf-8")
    manifest, n = re.subn(r'(?m)^version = "[^"]*"', f'version = "{version}"', manifest, count=1)
    if n != 1:
        sys.exit(f"no version line in {MANIFEST}")
    MANIFEST.write_text(manifest, encoding="utf-8")

    lock = LOCK.read_text(encoding="utf-8")
    lock, n = re.subn(
        r'(name = "maki-gui"\nversion = )"[^"]*"', rf'\g<1>"{version}"', lock, count=1
    )
    if n != 1:
        sys.exit(f"no maki-gui entry in {LOCK}")
    LOCK.write_text(lock, encoding="utf-8")


if __name__ == "__main__":
    main()
