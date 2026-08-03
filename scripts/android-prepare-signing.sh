#!/usr/bin/env bash
set -euo pipefail

output_dir="${1:-dist/keystore}"
required_vars=(
  SYNLY_ANDROID_KEYSTORE_BASE64
  SYNLY_ANDROID_KEYSTORE_PASSWORD
  SYNLY_ANDROID_KEY_ALIAS
  SYNLY_ANDROID_KEY_PASSWORD
)

for name in "${required_vars[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    printf '缺少签名 secret: %s\n' "$name" >&2
    exit 1
  fi
done

mkdir -p "$output_dir"
keystore="$output_dir/release.jks"
printf '%s' "$SYNLY_ANDROID_KEYSTORE_BASE64" | tr -d '\n\r' | base64 --decode > "$keystore"
if [[ ! -s "$keystore" ]]; then
  printf '签名 keystore 解码后为空: %s\n' "$keystore" >&2
  exit 1
fi

keystore_path="$(cd "$(dirname "$keystore")" && pwd)/$(basename "$keystore")"
printf '%s\n' "$keystore_path"
