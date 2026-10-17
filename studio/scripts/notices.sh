#!/usr/bin/env bash
# Writes JAI Studio's licence notices for one target: JAI Studio's own licence,
# then every Rust package the `jai` executable is built from on that target,
# with its licence text and copyright notices (studio/DESIGN.md section 14).
# The JavaScript that the executable embeds (dist/studio) is jai-tools' own
# code, under jai-tools' licence.
#
#   studio/scripts/notices.sh <target-triple> <output file>
#   studio/scripts/notices.sh x86_64-pc-windows-msvc dist/studio/THIRD-PARTY-NOTICES.txt
#
# Needs cargo-about 0.9 (`cargo install cargo-about --locked --features cli`)
# and network access to crates.io for packages not fetched yet. Set
# JAI_THIRD_PARTY_NOTICES to the output file when building, and the build
# embeds it (`jai licenses` prints it).
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <target-triple> <output file>" >&2
  exit 2
fi
target="$1"
out="$2"
studio="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tools="$(dirname "$studio")"
config="$studio/scripts/notices"

if ! cargo about --version >/dev/null 2>&1; then
  echo "error: cargo-about is missing: cargo install cargo-about --locked --features cli" >&2
  exit 1
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
cargo about generate --locked --target "$target" \
  --manifest-path "$studio/app/Cargo.toml" \
  --config "$config/about.toml" "$config/notices.hbs" -o "$tmp"
mkdir -p "$(dirname "$out")"
{
  printf 'JAI Studio\n==========\n\n'
  cat "$tools/LICENSE"
  printf '\n\n'
  cat "$tmp"
} > "$out"
echo "==> $out"
