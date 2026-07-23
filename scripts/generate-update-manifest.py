#!/usr/bin/env python3
"""Generate the deterministic ClipIt GitHub Release update manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def artifact(path: Path, url: str) -> dict[str, object]:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return {
        "url": url,
        "sha256": digest.hexdigest(),
        "size": path.stat().st_size,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    version = args.version.removeprefix("v")
    tag = f"v{version}"
    names = {
        "windows-x86_64-exe": f"clip-it-{version}-windows-x86_64.exe",
        "macos-universal-zip": f"ClipIt-{version}-macos-universal.zip",
    }
    artifacts: dict[str, object] = {}
    for key, name in names.items():
        path = args.dist / name
        if not path.is_file():
            raise SystemExit(f"missing release artifact: {path}")
        url = f"https://github.com/{args.repository}/releases/download/{tag}/{name}"
        artifacts[key] = artifact(path, url)

    manifest = {"schema": 1, "version": version, "artifacts": artifacts}
    args.output.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
        newline="\n",
    )


if __name__ == "__main__":
    main()
