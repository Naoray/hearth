#!/usr/bin/env python3
"""Update Hearth's Homebrew formula for one release tag."""

from __future__ import annotations

import re
import sys
from pathlib import Path


TAG_PATTERN = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.]+)?$")
SHA_PATTERN = re.compile(r"^[0-9a-fA-F]{64}$")
VERSION_PATTERN = re.compile(
    r'(?m)^(?P<indent>[ \t]*)version\s+"[^"]+"[ \t]*$'
)


def replace_once(text: str, pattern: re.Pattern[str], replacement, label: str) -> str:
    updated, count = pattern.subn(replacement, text)
    if count != 1:
        raise ValueError(f"expected exactly one {label}, found {count}")
    return updated


def architecture_pattern(architecture: str) -> re.Pattern[str]:
    return re.compile(
        r'(?m)^(?P<url_indent>[ \t]*)url\s+"'
        r"https://github\.com/Naoray/hearth/releases/download/[^\"\n]+/"
        r"hearth-[^\"\n]+-"
        + re.escape(architecture)
        + r'\.tar\.gz"[ \t]*\n'
        r'(?P<sha_indent>[ \t]*)sha256\s+"[0-9a-fA-F]{64}"[ \t]*$'
    )


def update_formula(path: Path, tag: str, arm_sha: str, x86_sha: str) -> None:
    if not TAG_PATTERN.fullmatch(tag):
        raise ValueError(f"invalid release tag: {tag}")
    if not SHA_PATTERN.fullmatch(arm_sha):
        raise ValueError("invalid arm64 sha256")
    if not SHA_PATTERN.fullmatch(x86_sha):
        raise ValueError("invalid x86_64 sha256")

    original = path.read_text(encoding="utf-8")
    version = tag[1:]
    updated = replace_once(
        original,
        VERSION_PATTERN,
        lambda match: f'{match.group("indent")}version "{version}"',
        "version declaration",
    )

    for architecture, sha in (
        ("aarch64-apple-darwin", arm_sha.lower()),
        ("x86_64-apple-darwin", x86_sha.lower()),
    ):
        url = (
            "https://github.com/Naoray/hearth/releases/download/"
            f"{tag}/hearth-{tag}-{architecture}.tar.gz"
        )
        updated = replace_once(
            updated,
            architecture_pattern(architecture),
            lambda match, url=url, sha=sha: (
                f'{match.group("url_indent")}url "{url}"\n'
                f'{match.group("sha_indent")}sha256 "{sha}"'
            ),
            f"{architecture} URL/sha256 block",
        )

    path.write_text(updated, encoding="utf-8")


def main() -> int:
    if len(sys.argv) != 5:
        print(
            f"usage: {sys.argv[0]} <formula> <tag> <arm-sha256> <x86-sha256>",
            file=sys.stderr,
        )
        return 2

    try:
        update_formula(Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4])
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
