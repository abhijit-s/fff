#!/usr/bin/env python3
"""Update version and sha256 values in the Homebrew formula."""

import re
import sys

formula_path, version, arm64_sha, x86_sha = sys.argv[1:]

with open(formula_path) as f:
    text = f.read()

# macOS ships both arches: clone the on_arm block into an on_intel sibling the
# first time (idempotent — a no-op once present) so brew install on an Intel Mac
# resolves the x86_64 bottle instead of falling back to a --HEAD source build.
if "on_intel" not in text:
    arm = re.search(r"([ \t]*)on_arm do\n(.*?\n)([ \t]*)end\n", text, re.DOTALL)
    if arm:
        indent, body, end_indent = arm.group(1), arm.group(2), arm.group(3)
        intel = (
            f"{indent}on_intel do\n"
            f"{body.replace('aarch64-apple-darwin', 'x86_64-apple-darwin')}"
            f"{end_indent}end\n"
        )
        text = text[: arm.end()] + intel + text[arm.end() :]

# version "x.y.z"
text = re.sub(r'version "[^"]*"', f'version "{version}"', text)

# URLs
text = re.sub(
    r'(releases/download/v)[^/]+(/fff-aarch64-apple-darwin\.tar\.gz)',
    rf'\g<1>{version}\g<2>',
    text,
)
text = re.sub(
    r'(releases/download/v)[^/]+(/fff-x86_64-apple-darwin\.tar\.gz)',
    rf'\g<1>{version}\g<2>',
    text,
)

# sha256 values — replace in order: arm64 block comes before intel block
arm64_done = False
def replace_sha(m):
    global arm64_done
    if not arm64_done:
        arm64_done = True
        return f'sha256 "{arm64_sha}"'
    return f'sha256 "{x86_sha}"'

text = re.sub(r'sha256 "[0-9a-f]{64}"', replace_sha, text)

# Refresh the pre-CI note: CI now ships an x86_64 bottle, so Intel Macs no
# longer need --HEAD. Matches any comment line mentioning Intel (plus adjacent
# comment lines); a no-op when absent, and idempotent on re-run.
def refresh_intel_comment(m):
    indent = m.group(1)
    return (
        f"{indent}# macOS ships pre-built bottles for both Apple Silicon (arm64) and Intel (x86_64).\n"
        f"{indent}# Linux installs via the APT repo or `brew install --HEAD` (source build).\n"
    )

text = re.sub(
    r"^([ \t]*)#[^\n]*\bIntel\b.*\n(?:[ \t]*#.*\n)*",
    refresh_intel_comment,
    text,
    count=1,
    flags=re.MULTILINE | re.IGNORECASE,
)

with open(formula_path, "w") as f:
    f.write(text)

print(f"Updated formula: version={version} arm64={arm64_sha[:8]}... x86_64={x86_sha[:8]}...")
