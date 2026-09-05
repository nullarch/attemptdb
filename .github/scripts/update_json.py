#!/usr/bin/env python3
"""RELEASE.toml + the tag -> update.json, the release policy clients read.

    python3 .github/scripts/update_json.py 0.2.8 nullarch/attemptdb > update.json

No TOML library on purpose: the file is two scalars and this must run on a
bare runner. A malformed value fails the release rather than publishing a
policy nobody meant."""
import json
import re
import sys

version, repo = sys.argv[1].lstrip("v"), sys.argv[2]
text = open("RELEASE.toml", encoding="utf-8").read()


def field(name, cast=str):
    m = re.search(rf'^\s*{name}\s*=\s*"?([^"\n#]+?)"?\s*(#.*)?$', text, re.M)
    return cast(m.group(1).strip()) if m else None


floor = field("required_below")
if floor is not None and not re.fullmatch(r"\d+\.\d+\.\d+", floor):
    sys.exit(f"RELEASE.toml: required_below {floor!r} is not a version")
policy = {
    "latest": version,
    "required_below": floor,
    "min_sync_version": field("min_sync_version", int),
    "notes": f"https://github.com/{repo}/releases/tag/v{version}",
}
print(json.dumps(policy, indent=2))
