#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target="${RUST_TARGET:?RUST_TARGET is required}"
architecture="${DEB_ARCH:?DEB_ARCH is required}"
debian_revision="${DEB_REVISION:-1}"
output_dir="$root/dist"

case "$architecture:$target" in
    amd64:x86_64-unknown-linux-gnu | arm64:aarch64-unknown-linux-gnu) ;;
    *)
        echo "Debian architecture $architecture does not match Rust target $target" >&2
        exit 1
        ;;
esac

package_id="$(cargo pkgid --manifest-path "$root/Cargo.toml" --package keylightd)"
version="${package_id##*@}"
if [[ "$version" == "$package_id" ]]; then
    echo "Cannot determine package version from cargo pkgid" >&2
    exit 1
fi
package_version="${version}-${debian_revision}"

cargo build --manifest-path "$root/Cargo.toml" --release --locked --target "$target"

binary="$root/target/$target/release/keylightd"
if [[ ! -x "$binary" ]]; then
    echo "Release binary not found: $binary" >&2
    exit 1
fi

case "$target" in
    x86_64-unknown-linux-gnu) strip --remove-section=.comment "$binary" ;;
    aarch64-unknown-linux-gnu) aarch64-linux-gnu-strip --remove-section=.comment "$binary" ;;
    *)
        echo "No strip tool configured for Rust target $target" >&2
        exit 1
        ;;
esac

staging="$(mktemp -d "$root/.deb-build.XXXXXX")"
chmod 0755 "$staging"
cleanup() {
    case "$staging" in
        "$root"/.deb-build.*) rm -rf -- "$staging" ;;
        *) echo "Refusing to remove unexpected staging path: $staging" >&2 ;;
    esac
}
trap cleanup EXIT

install -Dm 0755 "$binary" "$staging/usr/bin/keylightd"
install -Dm 0644 "$root/systemd/keylightd-camera.service" \
    "$staging/usr/lib/systemd/system/keylightd-camera.service"
install -Dm 0644 "$root/systemd/keylightd.service" \
    "$staging/usr/lib/systemd/user/keylightd.service"
install -Dm 0644 "$root/config.example.toml" \
    "$staging/usr/share/doc/keylightd/config.example.toml"
install -Dm 0644 "$root/README.md" "$staging/usr/share/doc/keylightd/README.md"
install -Dm 0644 "$root/SPEC.md" "$staging/usr/share/doc/keylightd/SPEC.md"
install -Dm 0644 "$root/packaging/debian/copyright" \
    "$staging/usr/share/doc/keylightd/copyright"
install -Dm 0644 "$root/packaging/debian/lintian-overrides" \
    "$staging/usr/share/lintian/overrides/keylightd"
install -Dm 0755 "$root/packaging/debian/postinst" "$staging/DEBIAN/postinst"
install -Dm 0755 "$root/packaging/debian/prerm" "$staging/DEBIAN/prerm"
install -Dm 0755 "$root/packaging/debian/postrm" "$staging/DEBIAN/postrm"
install -Dm 0644 "$root/packaging/keylightd.1" "$staging/usr/share/man/man1/keylightd.1"
gzip -9n "$staging/usr/share/man/man1/keylightd.1"

release_date="$(date --utc --date="@${SOURCE_DATE_EPOCH:-0}" --rfc-email)"
{
    printf 'keylightd (%s) unstable; urgency=medium\n\n' "$package_version"
    printf '  * Initial release.\n\n'
    printf ' -- Moritz Heiber <moritz@heiber.im>  %s\n' "$release_date"
} | gzip -9n >"$staging/usr/share/doc/keylightd/changelog.Debian.gz"

installed_size="$(du -sk "$staging/usr" | cut -f1)"
cat >"$staging/DEBIAN/control" <<EOF
Package: keylightd
Version: $package_version
Section: utils
Priority: optional
Architecture: $architecture
Maintainer: Moritz Heiber <moritz@heiber.im>
Installed-Size: $installed_size
Depends: libc6, libgcc-s1, systemd
Homepage: https://github.com/moritzheiber/keylightd
Description: Automatic Elgato Key Light control
 Turns an Elgato Key Light on when a V4L2 camera produces frames,
 chooses brightness from ambient-light calibration, and restores the
 previous power and brightness state after capture stops.
EOF

(
    cd "$staging"
    find usr -type f -print0 | sort -z | xargs -0 md5sum
) >"$staging/DEBIAN/md5sums"

mkdir -p "$output_dir"
package="$output_dir/keylightd_${package_version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$staging" "$package"
echo "$package"
