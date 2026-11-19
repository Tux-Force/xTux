#!/bin/sh
# Builds the native Linux packages of xTux (.deb, .rpm, Arch .pkg.tar.zst) from the release archive the CI builds.
#
# Usage: packaging/linux/build-packages.sh <xtux-VERSION-linux-x86_64.tar.gz> <output dir>
#
# Needs nfpm (https://nfpm.goreleaser.com) in PATH. The version comes from the archive's BUILD-INFO.json.
set -eu

archive=$(realpath "$1")
out=$(realpath -m "$2")
repo=$(cd "$(dirname "$0")/../.." && pwd)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

tar -C "$stage" -xzf "$archive"
version=$(sed -n 's/.*"version":"\([^"]*\)".*/\1/p' "$stage/BUILD-INFO.json")
test -n "$version" || { echo "no version in BUILD-INFO.json of $archive" >&2; exit 1; }

cp "$repo/packaging/linux/nfpm.yaml" "$stage/"
cp "$repo/packaging/flatpak/io.github.tux_force.xTux.desktop" "$stage/"
cp "$repo/packaging/flatpak/io.github.tux_force.xTux.metainfo.xml" "$stage/"
cp "$repo/crates/xtux-launcher/assets/icon-mascot.png" "$stage/io.github.tux_force.xTux.png"

mkdir -p "$out"
cd "$stage"
for packager in deb rpm archlinux; do
  VERSION="$version" nfpm package --config nfpm.yaml --packager "$packager" --target "$out/"
done
ls -l "$out"
