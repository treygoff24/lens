#!/usr/bin/env bash
set -euo pipefail

version=0.32.0
case "${RUNNER_OS:-$(uname -s)}-${RUNNER_ARCH:-$(uname -m)}" in
  Linux-X64|Linux-x86_64)
    target=x86_64-unknown-linux-gnu
    sha256=eb52f9fae0d0506774e9f1801c1168f87fa2c87a45e2d64d3ae7c89401929946
    ;;
  Linux-ARM64|Linux-aarch64)
    target=aarch64-unknown-linux-gnu
    sha256=d29bcffeb3f8b0c517b4ce0dd2470926ed5cb0bb29d78c6bdd5f88d76ee14a6a
    ;;
  macOS-X64|Darwin-x86_64)
    target=x86_64-apple-darwin
    sha256=6243464a8389e006b9256ee548bc795638f1a17113c1b6669c0e05ce89fd05c5
    ;;
  macOS-ARM64|Darwin-arm64)
    target=aarch64-apple-darwin
    sha256=aa343b2ff78ec2981f17a65140250c5ad6062c74072163f68c5c2686d94763a7
    ;;
  *)
    echo "unsupported cargo-dist host: ${RUNNER_OS:-$(uname -s)}-${RUNNER_ARCH:-$(uname -m)}" >&2
    exit 1
    ;;
esac

archive="cargo-dist-${target}.tar.xz"
tmp_dir=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/cargo-dist.XXXXXX")
curl --proto '=https' --tlsv1.2 -LsSf \
  "https://github.com/axodotdev/cargo-dist/releases/download/v${version}/${archive}" \
  -o "$tmp_dir/$archive"
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\n' "$sha256" "$tmp_dir/$archive" | sha256sum --check
else
  printf '%s  %s\n' "$sha256" "$tmp_dir/$archive" | shasum -a 256 --check
fi
tar -xJf "$tmp_dir/$archive" -C "$tmp_dir"
mkdir -p "$HOME/.cargo/bin"
install -m 0755 "$tmp_dir/cargo-dist-${target}/dist" "$HOME/.cargo/bin/dist"
