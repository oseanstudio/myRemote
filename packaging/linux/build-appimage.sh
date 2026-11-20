#!/usr/bin/env bash
set -euo pipefail

architecture=${1:?Pass x86_64 or aarch64}
version=${2:?Pass the Cargo package version}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
case "$architecture" in
    x86_64)
        tool_hash=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
        runtime_hash=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
        ;;
    aarch64)
        tool_hash=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
        runtime_hash=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
        ;;
    *) exit 2 ;;
esac
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
target="${architecture}-unknown-linux-musl"
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
app="$staging/myRemote.AppDir"

rustup target add "$target"
RUSTFLAGS="-C linker=rust-lld -C linker-flavor=ld.lld -C link-self-contained=yes" \
    cargo build --locked --release --target "$target" --bin myremote
install -Dm755 "$target_dir/$target/release/myremote" "$app/usr/bin/myremote"
install -Dm755 packaging/linux/AppRun "$app/AppRun"
install -Dm644 packaging/linux/myremote.desktop "$app/myremote.desktop"
install -Dm644 packaging/linux/myremote.svg "$app/myremote.svg"
install -Dm644 packaging/70-myremote-uinput.rules "$app/usr/share/myremote/70-myremote-uinput.rules"
install -Dm644 LICENSE "$app/usr/share/myremote/LICENSE"
python3 packaging/dependency-notices.py "$target" "$app/usr/share/myremote/THIRD-PARTY-NOTICES.txt"
tool="$staging/appimagetool"
runtime="$staging/runtime"
curl --fail --silent --show-error --location --retry 3 \
    "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$architecture.AppImage" -o "$tool"
curl --fail --silent --show-error --location --retry 3 \
    "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$architecture" -o "$runtime"
printf '%s  %s\n%s  %s\n' "$tool_hash" "$tool" "$runtime_hash" "$runtime" | sha256sum --check
curl --fail --silent --show-error --location --retry 3 \
    "https://raw.githubusercontent.com/AppImage/type2-runtime/20251108/LICENSE" \
    -o "$app/usr/share/myremote/AppImage-runtime-LICENSE"
chmod +x "$tool"
mkdir -p dist
asset="dist/myRemote-Linux-$architecture.AppImage"
ARCH="$architecture" APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --runtime-file "$runtime" "$app" "$asset"
chmod +x "$asset"
test "$(APPIMAGE_EXTRACT_AND_RUN=1 "$asset" --version)" = "myRemote $version"
