#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
architecture=""
build_type="binary"
output_dir="$root/dist"

usage() {
    cat <<EOF
Usage: scripts/build-deb.sh [--architecture amd64|arm64] [--build any|all|binary] [--output-dir DIR]

Builds the Debian packages with debhelper entirely inside an Ubuntu 26.04
container. Requires Docker with the Buildx plugin.

  --build any     only the architecture-dependent daemon package
  --build all     only the architecture-independent extension package
  --build binary  both packages (default)
EOF
}

while (($#)); do
    case "$1" in
        --architecture)
            architecture="${2:?missing value for --architecture}"
            shift 2
            ;;
        --build)
            build_type="${2:?missing value for --build}"
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

case "$build_type" in
    any | all | binary) ;;
    *)
        echo "Unsupported --build type: $build_type (expected any, all, or binary)" >&2
        exit 1
        ;;
esac

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

case "$architecture" in
    amd64 | arm64) ;;
    *)
        echo "Unsupported Debian architecture: $architecture" >&2
        exit 1
        ;;
esac

if ! docker buildx version >/dev/null 2>&1; then
    echo "Docker with the Buildx plugin is required" >&2
    exit 1
fi

source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$root" --no-pager show -s --no-show-signature --format=%ct HEAD 2>/dev/null || date +%s)}"
mkdir -p "$output_dir"
output_dir="$(cd -- "$output_dir" && pwd)"

docker buildx build \
    --file "$root/packaging/Dockerfile" \
    --target artifact \
    --build-arg "DEB_HOST_ARCH=$architecture" \
    --build-arg "DEB_BUILD_TYPE=$build_type" \
    --build-arg "SOURCE_DATE_EPOCH=$source_date_epoch" \
    --output "type=local,dest=$output_dir" \
    "$root"
