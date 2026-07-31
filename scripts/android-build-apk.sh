#!/usr/bin/env bash
set -euo pipefail

resolve_java_home() {
    if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/java" ]]; then
        major="$( "$JAVA_HOME/bin/java" -version 2>&1 | head -n 1 | sed -E 's/.*version "([0-9]+).*/\1/' )"
        if [[ "$major" =~ ^[0-9]+$ ]] && (( major >= 17 && major <= 23 )); then
            echo "$JAVA_HOME"
            return
        fi
    fi
    for jbr in \
        "/Applications/Android Studio.app/Contents/jbr/Contents/Home" \
        "$HOME/Applications/Android Studio.app/Contents/jbr/Contents/Home"
    do
        if [[ -x "$jbr/bin/java" ]]; then
            echo "$jbr"
            return
        fi
    done
    echo ""
}

export JAVA_HOME="$(resolve_java_home)"
if [[ -z "$JAVA_HOME" ]]; then
    echo "未找到兼容的 JDK (17-23), 请设置 JAVA_HOME" >&2
    exit 1
fi

if [[ -z "${ANDROID_HOME:-}" ]]; then
    for root in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do
        if [[ -d "$root" ]]; then
            export ANDROID_HOME="$root"
            break
        fi
    done
fi
if [[ -z "${ANDROID_HOME:-}" || ! -d "$ANDROID_HOME" ]]; then
    echo "ANDROID_HOME 未设置, 且未在默认位置找到 Android SDK" >&2
    exit 1
fi

cd android
./gradlew assembleDebug
