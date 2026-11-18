#!/bin/sh
# Regenerates licenses/THIRD-PARTY-LICENSES.md from Cargo.lock (cargo-about). The CI checks that it is up to date.
set -eu
cd "$(dirname "$0")/.."
cargo about generate --workspace --locked licenses/about.hbs -o licenses/THIRD-PARTY-LICENSES.md
