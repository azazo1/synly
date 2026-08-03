#!/usr/bin/env bash
set -euo pipefail

mode="${1:-auto}"
case "$mode" in
  auto|debug|release) ;;
  *)
    printf '用法: android-build.sh [auto|debug|release]\n' >&2
    exit 2
    ;;
esac

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
signing_env="$repo_root/secrets/synly-signing.env"
if [[ -f "$signing_env" ]]; then
  set -a
  source "$signing_env"
  set +a
fi

signing_vars=(
  SYNLY_ANDROID_KEYSTORE_PASSWORD
  SYNLY_ANDROID_KEY_ALIAS
  SYNLY_ANDROID_KEY_PASSWORD
)
keystore_source=""

if [[ -n "${SYNLY_ANDROID_KEYSTORE_BASE64:-}" ]]; then
  keystore_source="base64"
elif [[ -n "${SYNLY_ANDROID_KEYSTORE_FILE:-}" ]]; then
  keystore_source="file"
fi

missing=()
for name in "${signing_vars[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    missing+=("$name")
  fi
done

if [[ "$mode" == "auto" ]]; then
  if [[ -n "$keystore_source" || ${#missing[@]} -gt 0 ]]; then
    mode="release"
  else
    mode="debug"
  fi
fi

if [[ "$mode" == "release" ]]; then
  if [[ -z "$keystore_source" || ${#missing[@]} -gt 0 ]]; then
    printf 'Android 签名配置不完整, 缺少: keystore 或 %s\n' "${missing[*]}" >&2
    exit 1
  fi

  if [[ "$keystore_source" == "base64" ]]; then
    keystore_path="$(bash "$script_dir/android-prepare-signing.sh" dist/keystore)"
    export SYNLY_ANDROID_KEYSTORE_FILE="$keystore_path"
  elif [[ ! -f "$SYNLY_ANDROID_KEYSTORE_FILE" ]]; then
    printf 'Android 签名 keystore 不存在: %s\n' "$SYNLY_ANDROID_KEYSTORE_FILE" >&2
    exit 1
  fi

  printf '[android] building signed release APK\n'
  bash "$script_dir/android-gradle.sh" assembleRelease
else
  printf '[android] building debug APK\n'
  bash "$script_dir/android-gradle.sh" assembleDebug
fi
