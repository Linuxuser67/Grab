#!/usr/bin/env sh
# Build the Cargo project and copy the binary to the Meson output path.
# Usage: cargo.sh <source-root> <output-bin> <profile>
# NOTE: cds to the source root first so cargo picks up .cargo/config.toml
# (vendored crates for offline Flatpak builds); Meson invokes this with CWD
# set to its own (out-of-tree) build dir, where the config is invisible.
set -eu
src="$1"; out1="$2"; profile="${3:-release}"
case "$out1" in
  /*) ;;
  *) out1="$PWD/$out1" ;;
esac
cd "$src"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"
# NOTE: cargo has no "--profile debug"; the dev profile is the default.
if [ "$profile" = "release" ]; then
  profile_flag="--release"
else
  profile_flag=""
  profile="debug"
fi
# shellcheck disable=SC2086
cargo build --offline $profile_flag \
  ${CARGO_TARGET_DIR:+--target-dir "$CARGO_TARGET_DIR"}
target_dir="${CARGO_TARGET_DIR:-$src/target}/$profile"
cp "$target_dir/grab" "$out1"
