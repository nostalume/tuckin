#!/bin/sh
set -eu

tag=${1:-}
if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
    printf 'Usage: sh install.sh vMAJOR.MINOR.PATCH\n' >&2
    exit 2
fi

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) platform=linux-x86_64 ;;
    Darwin/arm64) platform=macos-aarch64 ;;
    *) printf 'No release binary for this platform. See Build from source.\n' >&2; exit 2 ;;
esac

repo=${TUCKIN_REPOSITORY:-nostalume/tuckin}
base="https://github.com/$repo/releases/download/$tag"
archive="tuckin-$platform.tar.gz"
bin_dir=${TUCKIN_INSTALL_DIR:-"$HOME/.local/bin"}
scratch=$(mktemp -d)
trap 'rm -f "$scratch/$archive" "$scratch/SHA256SUMS" "$scratch/tuckin"; rmdir "$scratch"' EXIT
trap 'exit 1' HUP INT TERM

curl -fsSL "$base/$archive" -o "$scratch/$archive"
curl -fsSL "$base/SHA256SUMS" -o "$scratch/SHA256SUMS"
expected=$(awk -v name="$archive" '$2 == name { print $1 }' "$scratch/SHA256SUMS")
if [ "$platform" = linux-x86_64 ]; then
    actual=$(sha256sum "$scratch/$archive")
else
    actual=$(shasum -a 256 "$scratch/$archive")
fi
if [ "$expected" != "${actual%% *}" ]; then
    printf 'SHA-256 mismatch for %s\n' "$archive" >&2
    exit 1
fi

tar -xzf "$scratch/$archive" -C "$scratch" tuckin
mkdir -p "$bin_dir"
install -m 755 "$scratch/tuckin" "$bin_dir/tuckin"
printf 'Installed %s\n' "$bin_dir/tuckin"
