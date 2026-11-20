"""Check the Git index before publishing myRemote source."""

from pathlib import PurePosixPath
import re
import subprocess


def git(*args):
    return subprocess.check_output(["git", *args])


allowed_files = {".gitattributes", ".gitignore", "Cargo.toml", "Cargo.lock", "README.md", "LICENSE"}
allowed_dirs = {".github", "docs", "packaging", "src", "tests"}
allowed_extensions = {".rs", ".md", ".py", ".sh", ".yml", ".plist", ".rules", ".desktop", ".svg"}
markers = re.compile(
    rb"-----BEGIN [A-Z ]*PRIVATE " rb"KEY-----"
    rb"|" rb"gh[pousr]_[A-Za-z0-9]{20,}"
    rb"|" rb"github_pat_[A-Za-z0-9_]{20,}"
    rb"|" rb"(?:AKIA|ASIA)[0-9A-Z]{16}"
    rb"|" rb"(?:[a-z]:[\\/]+Users[\\/]+|/Users/)[A-Za-z0-9_.-]+[\\/]",
    re.IGNORECASE,
)

for entry in git("ls-files", "--stage", "-z").split(b"\0"):
    if not entry:
        continue
    header, name = entry.split(b"\t", 1)
    mode, digest, stage = header.decode().split()
    path = PurePosixPath(name.decode("utf-8"))
    allowed = (
        path.as_posix() in allowed_files
        or path.as_posix() == "packaging/linux/AppRun"
        or (path.parts[0] in allowed_dirs and path.suffix in allowed_extensions)
    )
    ignored_name = any(
        part.startswith(("._", ".env")) or ".sync-conflict-" in part for part in path.parts
    )
    if not allowed or ignored_name or mode not in {"100644", "100755"} or stage != "0":
        raise SystemExit(f"Unexpected public file: {path}")
    content = git("cat-file", "blob", digest)
    if len(content) > 512_000 or b"\0" in content or markers.search(content):
        raise SystemExit(f"Review this file before publishing: {path}")

print("Public source check passed.")
