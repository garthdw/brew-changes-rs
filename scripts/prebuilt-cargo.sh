#!/usr/bin/env bash
# Stands in for cargo in .goreleaser.yaml: GoReleaser OSS cannot import
# prebuilt binaries, so this stages the release workflow's matrix builds
# from prebuilt/brew-changes-<target>/ where the rust builder expects them.
set -euo pipefail

target=""
while (($#)); do
  case "$1" in
    --target=*) target="${1#--target=}" ;;
    --target) shift; target="${1:-}" ;;
  esac
  shift
done

if [[ -z "$target" ]]; then
  echo "prebuilt-cargo: no --target given" >&2
  exit 1
fi

source="prebuilt/brew-changes-$target/brew-changes"
if [[ ! -f "$source" ]]; then
  echo "prebuilt-cargo: missing $source" >&2
  exit 1
fi

mkdir -p "target/$target/release"
cp "$source" "target/$target/release/brew-changes"
chmod +x "target/$target/release/brew-changes"
