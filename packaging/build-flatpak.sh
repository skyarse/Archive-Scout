#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
app=io.github.skyarse.ArchiveSearch
platform=org.gnome.Platform
runtime_version=50
channel=${ARCHIVE_SEARCH_CHANNEL:-dev}
builder=${FLATPAK:-flatpak}
output=${1:-"$project_dir/Archive-Scout-0.1.0-x86_64.flatpak"}
binary=${ARCHIVE_SEARCH_BIN:-"$project_dir/target/release/archive-search"}
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT HUP INT TERM

if [ -z "${ARCHIVE_SEARCH_BIN:-}" ]; then
    cargo build --locked --release --manifest-path "$project_dir/Cargo.toml"
fi
test "$(uname -m)" = x86_64
case "$channel" in
    dev|stable) ;;
    *) exit 2 ;;
esac
"$builder" build-init --type=app "$build/app" "$app" "$platform" "$platform" "$runtime_version"
install -Dm755 "$binary" "$build/app/files/bin/archive-search"
install -Dm644 "$project_dir/packaging/$app.desktop" "$build/app/files/share/applications/$app.desktop"
install -Dm644 "$project_dir/packaging/$app.svg" "$build/app/files/share/icons/hicolor/scalable/apps/$app.svg"
install -Dm644 "$project_dir/packaging/$app.metainfo.xml" "$build/app/files/share/metainfo/$app.metainfo.xml"
appstreamcli validate --no-net "$build/app/files/share/metainfo/$app.metainfo.xml"
appstreamcli compose --prefix=/ --origin="$app" --components="$app" \
    --result-root="$build/app/files" --data-dir="$build/app/files/share/app-info/xmls" \
    --icons-dir="$build/app/files/share/app-info/icons/flatpak" "$build/app/files"
test -s "$build/app/files/share/app-info/xmls/$app.xml.gz"
install -Dm644 "$project_dir/LICENSE" "$build/app/files/share/licenses/$app/LICENSE"
"$builder" build-finish --no-inherit-permissions --command=archive-search \
    --require-version=1.17.0 --socket=wayland --socket=fallback-x11 --share=ipc \
    --device=dri --filesystem=host-root:ro --talk-name=org.freedesktop.FileManager1 \
    "$build/app"
set --
if [ "${FLATPAK_ICON_SANDBOX:-1}" = 0 ]; then
    set -- --disable-sandbox
fi
"$builder" build-export "$@" "$build/repo" "$build/app" "$channel"
"$builder" build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$build/repo" "$output" "$app" "$channel"
