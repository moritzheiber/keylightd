#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
architecture=""
target=""
output_dir="$root/dist"

usage() {
    cat <<EOF
Usage: scripts/build-deb.sh [--architecture amd64|arm64] [--target RUST_TARGET] [--output-dir DIR]

Builds the package entirely inside an Ubuntu 26.04 container.
Requires Docker with the Buildx plugin.
EOF
}

while (($#)); do
    case "$1" in
        --architecture)
            architecture="${2:?missing value for --architecture}"
            shift 2
            ;;
        --target)
            target="${2:?missing value for --target}"
            shift 2
            ;;
        --output-dir)
            output_dir="${2:?missing value for --output-dir}"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$architecture" ]]; then
    case "$(uname -m)" in
        x86_64) architecture="amd64" ;;
        aarch64|arm64) architecture="arm64" ;;
        *)
            echo "Cannot infer Debian architecture; pass --architecture" >&2
            exit 1
            ;;
    esac
fi

if [[ -z "$target" ]]; then
    case "$architecture" in
        amd64) target="x86_64-unknown-linux-gnu" ;;
        arm64) target="aarch64-unknown-linux-gnu" ;;
        *)
            echo "Unsupported Debian architecture: $architecture" >&2
            exit 1
            ;;
    esac
fi

case "$architecture:$target" in
    amd64:x86_64-unknown-linux-gnu | arm64:aarch64-unknown-linux-gnu) ;;
    *)
        echo "Debian architecture $architecture does not match Rust target $target" >&2
        exit 1
        ;;
esac

if ! docker buildx version >/dev/null 2>&1; then
    echo "Docker with the Buildx plugin is required" >&2
    exit 1
fi

source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || date +%s)}"
mkdir -p "$output_dir"
output_dir="$(cd -- "$output_dir" && pwd)"

docker buildx build \
    --file "$root/packaging/Dockerfile" \
    --target artifact \
    --build-arg "RUST_TARGET=$target" \
    --build-arg "DEB_ARCH=$architecture" \
    --build-arg "SOURCE_DATE_EPOCH=$source_date_epoch" \
    --output "type=local,dest=$output_dir" \
    "$root"
