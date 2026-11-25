#!/usr/bin/env python3
"""test/privacy_guard.py — private organisation, machine and account names must not enter the public tree.

The names are kept only as SHA-256 hashes of lowercase tokens, so this file does not publish them. Every tracked
text file is split into lowercase alphanumeric tokens (also joined across `-`), each token is hashed, and any hit
fails the check. Real home-directory paths are matched directly, since `/home/<user>` is not a secret name.
"""
import hashlib
import re
import subprocess
import sys
from pathlib import Path

DENY = {
    "b920f04de6728f0ceacd66306a7083363a38e73be32898a8a91a349aa2e05ab8",
    "2e20eef4d45a0fb1cef570492dd64d29a40b20691fa26a9fad6b44b137512782",
    "5a3b25dda3aa7df7b0f47cb5757ae21df0190e88f41ab0f71d7f6ac461f7b7f7",
    "20384179e3926b5931f92b6b8c128dcd4a25ea5ef31753a01af212eafffaeae4",
    "247dad7739d3ed4e2ff0368423dac665fb068fa31af114455f7c46946ddb3284",
    "85fee0d74a799463da1fc37db6c193a059167899b73baefd1c0a48e83ee1b738",
    "e6327c879d39ad8caed30b9a0a507577669560039f34b7474fc7f64ec2c0081a",
    "1389c8593ff39f5759cf666b02af8d59ba0852c436fcdb6433bf6120f1204c53",
    "940fa5e673fa44c93882522adbcfd13226740c932a85679b5de3dc13c1fafaa7",
}
HOME_PATH = re.compile(r"/home/blake(?![a-z0-9])")
WORD = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
SKIP = {"test/privacy_guard.py"}


def hashed(token: str) -> bool:
    return hashlib.sha256(token.encode()).hexdigest() in DENY


def scan(text: str):
    for lineno, line in enumerate(text.splitlines(), 1):
        low = line.lower()
        if HOME_PATH.search(low):
            yield lineno, "home-directory path"
            continue
        for match in WORD.finditer(low):
            whole = match.group(0)
            if hashed(whole) or any(hashed(part) for part in whole.split("-")):
                yield lineno, "private name"
                break


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    files = subprocess.run(["git", "-C", str(root), "ls-files", "-z"], capture_output=True, check=True).stdout
    failures = 0
    for rel in files.decode().split("\0"):
        if not rel or rel in SKIP:
            continue
        try:
            text = (root / rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
            continue
        for lineno, why in scan(text):
            print(f"{rel}:{lineno}: {why}")
            failures += 1
    if failures:
        print(f"privacy-guard: {failures} private name(s) above must not be committed", file=sys.stderr)
        return 1
    print("privacy-guard: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
