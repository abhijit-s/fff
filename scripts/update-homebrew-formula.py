#!/usr/bin/env python3
"""Update version and sha256 values in the Homebrew formula."""

import re
import sys

formula_path, version, arm64_sha, x86_sha = sys.argv[1:]

with open(formula_path) as f:
    text = f.read()

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

with open(formula_path, "w") as f:
    f.write(text)

print(f"Updated formula: version={version} arm64={arm64_sha[:8]}... x86_64={x86_sha[:8]}...")
