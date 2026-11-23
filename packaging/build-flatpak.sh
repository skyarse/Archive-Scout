#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
app=io.github.skyarse.ArchiveSearch
platform=org.gnome.Platform
version=50
builder=${FLATPAK:-flatpak}
output=${1:-"$project_dir/Archive-Scout-0.1.0-x86_64.flatpak"}
binary=${ARCHIVE_SEARCH_BIN:-"$project_dir/target/release/archive-search"}
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT HUP INT TERM

if [ -z "${ARCHIVE_SEARCH_BIN:-}" ]; then
    cargo build --locked --release --manifest-path "$project_dir/Cargo.toml"
fi
test "$(uname -m)" = x86_64
"$builder" build-init --type=app "$build/app" "$app" "$platform" "$platform" "$version"
install -Dm755 "$binary" "$build/app/files/bin/archive-search"
install -Dm644 "$project_dir/packaging/$app.desktop" "$build/app/files/share/applications/$app.desktop"
install -Dm644 "$project_dir/packaging/$app.svg" "$build/app/files/share/icons/hicolor/scalable/apps/$app.svg"
"$builder" build-finish --no-inherit-permissions --command=archive-search \
    --require-version=1.17.0 --socket=wayland --socket=fallback-x11 --share=ipc \
    --device=dri --filesystem=host-root:ro --talk-name=org.freedesktop.FileManager1 \
    "$build/app"
"$builder" build-export "$build/repo" "$build/app" dev
"$builder" build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$build/repo" "$output" "$app" dev
