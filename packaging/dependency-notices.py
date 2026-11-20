"""Collect Cargo dependency licenses for a packaged executable."""

import json
from pathlib import Path
import subprocess
import sys

target, output = sys.argv[1:]
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target]
))
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
root = metadata["resolve"]["root"]
dependencies = set()
pending = [root]
while pending:
    package_id = pending.pop()
    for dependency in nodes[package_id]["deps"]:
        if dependency["pkg"] not in dependencies and any(
            kind["kind"] is None for kind in dependency["dep_kinds"]
        ):
            dependencies.add(dependency["pkg"])
            pending.append(dependency["pkg"])

index = []
texts = {}
for package in sorted(metadata["packages"], key=lambda package: (package["name"], package["version"])):
    if package["id"] not in dependencies:
        continue
    label = f'{package["name"]} {package["version"]}'
    index.append(f'{label} — {package["license"] or "see license text"}')
    directory = Path(package["manifest_path"]).parent
    files = {path for path in directory.iterdir() if path.is_file()
             and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"))}
    if package["license_file"]:
        files.add(directory / package["license_file"])
    for path in sorted(files):
        text = path.read_text(encoding="utf-8", errors="replace").strip()
        if text:
            texts.setdefault(text, []).append(f"{label} ({path.name})")

with Path(output).open("w", encoding="utf-8") as file:
    file.write("myRemote dependencies\n\n" + "\n".join(index) + "\n")
    for text, labels in texts.items():
        file.write("\n" + ", ".join(labels) + "\n\n" + text + "\n")
