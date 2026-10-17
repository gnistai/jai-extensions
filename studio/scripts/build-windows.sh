#!/usr/bin/env bash
# Cross-compiles jai.exe from Linux or WSL with cargo-xwin and copies it to
# jai-tools/dist/studio/jai.exe (studio/DESIGN.md section 14).
#
# Needs Node with the jai-tools devDependencies installed, Rust with the
# x86_64-pc-windows-msvc target, cargo-xwin, and LLVM 13 or newer (clang,
# lld-link, llvm-rc, llvm-lib) in ~/.local/llvm/bin or on PATH. Extra
# arguments go to cargo, for example --locked.
#
# With cargo-about (`cargo install cargo-about --locked --features cli`) it
# also writes dist/studio/THIRD-PARTY-NOTICES.txt and embeds it (`jai
# licenses`). Without it the exe has no licence notices and must not be given
# to anyone.
set -euo pipefail

studio="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tools="$(dirname "$studio")"
target=x86_64-pc-windows-msvc

if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
export PATH="$HOME/.local/llvm/bin:$PATH"
# Keep the target dir on the Linux filesystem, not under /mnt/c.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/jai-studio-target}"

echo "==> npm run build"
(cd "$tools" && npm run build)

notices="$tools/dist/studio/THIRD-PARTY-NOTICES.txt"
if cargo about --version > /dev/null 2>&1; then
  echo "==> third-party notices"
  bash "$studio/scripts/notices.sh" "$target" "$notices"
  export JAI_THIRD_PARTY_NOTICES="$notices"
else
  unset JAI_THIRD_PARTY_NOTICES
  echo "warning: cargo-about is missing (cargo install cargo-about --locked --features cli):" >&2
  echo "warning: this jai.exe has no third-party licence notices; do not give it to anyone" >&2
fi

echo "==> cargo xwin build --release --target $target -p jai"
cargo xwin build --release --target "$target" -p jai --manifest-path "$studio/Cargo.toml" "$@"

mkdir -p "$tools/dist/studio"
cp "$CARGO_TARGET_DIR/$target/release/jai.exe" "$tools/dist/studio/jai.exe"
echo "==> $tools/dist/studio/jai.exe"
