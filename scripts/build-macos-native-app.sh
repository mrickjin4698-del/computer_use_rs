#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
helper_dir="$repo_dir/macos-native-helper"
configuration=${1:-release}
binary="$helper_dir/.build/arm64-apple-macosx/$configuration/alice-computer-native"
app_dir="$repo_dir/target/$configuration/AliceComputerNative.app"

swift build --package-path "$helper_dir" -c "$configuration"

rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
cp "$binary" "$app_dir/Contents/MacOS/alice-computer-native"
chmod 755 "$app_dir/Contents/MacOS/alice-computer-native"
cp "$helper_dir/Resources/Info.plist" "$app_dir/Contents/Info.plist"

codesign --force --deep --sign - --requirements "$helper_dir/Resources/designated.req" "$app_dir"
printf '%s\n' "$app_dir"
