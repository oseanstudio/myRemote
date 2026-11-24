#!/usr/bin/env bash
set -euo pipefail

version=${1:?Pass the Cargo package version}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
app="$staging/myRemote.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" dist
export MACOSX_DEPLOYMENT_TARGET=14.0

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target add "$target"
    cargo build --locked --release --target "$target" --bin myremote
done
lipo -create \
    "$target_dir/aarch64-apple-darwin/release/myremote" \
    "$target_dir/x86_64-apple-darwin/release/myremote" \
    -output "$app/Contents/MacOS/myremote"
lipo "$app/Contents/MacOS/myremote" -verify_arch arm64 x86_64

cp packaging/macos/Info.plist "$app/Contents/Info.plist"
cp LICENSE "$app/Contents/Resources/"
iconset="$staging/AppIcon.iconset"
mkdir -p "$iconset"
swift packaging/macos/icon.swift "$iconset/icon_512x512@2x.png"
for pixels in 16 32 128 256 512; do
    sips -z "$pixels" "$pixels" "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${pixels}x${pixels}.png" >/dev/null
    if [[ "$pixels" != 512 ]]; then
        retina=$((pixels * 2))
        sips -z "$retina" "$retina" "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${pixels}x${pixels}@2x.png" >/dev/null
    fi
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
test -s "$app/Contents/Resources/AppIcon.icns"
rm -r "$iconset"
python3 packaging/dependency-notices.py aarch64-apple-darwin "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
test "$("$app/Contents/MacOS/myremote" --version)" = "myRemote $version"
if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    python3 - "$app" "$staging/input-permission" <<'PY'
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

app, result = sys.argv[1:]
signature = subprocess.run(
    ["/usr/bin/codesign", "--display", "--verbose=2", app],
    check=True,
    capture_output=True,
    text=True,
).stderr.splitlines()
assert {"Identifier=com.oseanstudio.myremote", "Signature=adhoc"} <= set(signature)

binary = str(Path(app) / "Contents/MacOS/myremote")
with tempfile.TemporaryDirectory() as state:
    with subprocess.Popen(
        [binary, "--dry-run", "--ip", "127.0.0.1", "--state-dir", state],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    ) as receiver:
        try:
            # A fresh probe must answer while the original signed executable is still running.
            assert select.select([receiver.stdout], [], [], 10)[0], "Receiver did not start"
            assert b" is listening on " in receiver.stdout.readline()
            Path(result).touch(exist_ok=False)
            subprocess.run(
                ["/usr/bin/open", "-n", "-g", "-j", "--stdout", result, app,
                 "--args", "--probe-input-permission"],
                check=True,
                timeout=8,
            )
            deadline = time.monotonic() + 3
            while not (answer := Path(result).read_bytes()).endswith(b"\n"):
                assert time.monotonic() < deadline, "Fresh permission probe did not answer"
                time.sleep(0.04)
            assert answer in {b"granted\n", b"accessibility\n", b"denied\n"}
            assert receiver.poll() is None
        finally:
            receiver.kill()
            Path(result).unlink(missing_ok=True)

# The helper must wait for its parent to close the pipe before launching another instance.
with subprocess.Popen(
    [binary, "--relaunch-after-accessibility", "--probe-input-permission"],
    stdin=subprocess.PIPE,
) as helper:
    try:
        helper.wait(timeout=0.5)
    except subprocess.TimeoutExpired:
        pass
    else:
        raise AssertionError("Accessibility relaunch helper did not wait for its parent")
    try:
        helper.communicate(timeout=15)
    except subprocess.TimeoutExpired:
        helper.kill()
        helper.communicate()
        raise
    assert helper.returncode == 0
PY
fi
ln -s /Applications "$staging/Applications"
hdiutil create -ov -format UDZO -volname "myRemote $version" -srcfolder "$staging" "dist/myRemote-macOS-Universal.dmg"
for attempt in 1 2 3; do
    if hdiutil verify "dist/myRemote-macOS-Universal.dmg"; then
        exit 0
    fi
    if [[ "$attempt" != 3 ]]; then
        sleep 2
    fi
done
exit 1
