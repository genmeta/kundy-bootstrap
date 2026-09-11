#!/usr/bin/env bash
set -euo pipefail

target=${XTASK_RELEASE_TARGET:-${KUNDY_RELEASE_TARGET:-}}
output_dir=${XTASK_RELEASE_OUT_DIR:-${KUNDY_RELEASE_OUT_DIR:-/output}}
build_jobs=${KUNDY_BUILD_JOBS:-1}

if [[ -z "$target" ]]; then
    echo "XTASK_RELEASE_TARGET or KUNDY_RELEASE_TARGET is required" >&2
    exit 2
fi

if [[ ! "$build_jobs" =~ ^[1-9][0-9]*$ ]]; then
    echo "KUNDY_BUILD_JOBS must be a positive integer" >&2
    exit 2
fi

case "$target" in
    x86_64-unknown-linux-gnu) deb_arch=amd64; gnu_arch=x86_64-linux-gnu ;;
    aarch64-unknown-linux-gnu) deb_arch=arm64; gnu_arch=aarch64-linux-gnu ;;
    armv7-unknown-linux-gnueabihf) deb_arch=armhf; gnu_arch=arm-linux-gnueabihf ;;
    i686-unknown-linux-gnu) deb_arch=i386; gnu_arch=i386-linux-gnu ;;
    *) echo "unsupported target $target" >&2; exit 1 ;;
esac

work_dir=$(mktemp -d /tmp/kundy-deb.XXXXXX)
trap 'rm -rf -- "$work_dir"' EXIT
source_dir="$work_dir/src"
product_source="$work_dir/product-source"
mkdir -p "$source_dir/debian" "$product_source" "$output_dir"

repo_root=${XTASK_RELEASE_REPO_ROOT:-/workspace}
bootstrap_private_key="$repo_root/tls/kundy.dhttp.net.key.pem"

if [[ ! -r "$bootstrap_private_key" ]]; then
    echo "missing bootstrap TLS private key: $bootstrap_private_key" >&2
    exit 1
fi

cp -a \
    /opt/kundy/deb/control \
    /opt/kundy/deb/copyright \
    /opt/kundy/deb/rules \
    /opt/kundy/deb/source \
    "$source_dir/debian/"
tar -C "$repo_root" \
    --exclude='./.git' \
    --exclude='./target' \
    --exclude='./packaging/deb' \
    --exclude='./tls/kundy.dhttp.net.key.pem' \
    -cf - . | tar -C "$product_source" -xf -
install -D -m 0600 "$bootstrap_private_key" "$product_source/tls/kundy.dhttp.net.key.pem"

source_version=$(python3 - "$product_source/Cargo.toml" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as handle:
    manifest = tomllib.load(handle)
print(manifest["package"]["version"])
PY
)
if [ -z "$source_version" ]; then
    echo "unable to determine the Kundy version from Cargo.toml" >&2
    exit 1
fi
package_version=${XTASK_RELEASE_PACKAGE_VERSION:-}
if [ -z "$package_version" ]; then
    deb_version=${source_version/-/~}
    package_version="${deb_version}-1"
fi

printf 'kundy (%s) unstable; urgency=low\n\n  * release %s\n\n -- Genmeta Tech Limited <developer@genmeta.net>  %s\n' \
    "$package_version" "${XTASK_RELEASE_SOURCE_VERSION:-$source_version}" "$(date -R)" > "$source_dir/debian/changelog"

export HOME=/tmp
export CARGO_HOME=${CARGO_HOME:-/tmp/cargo}
export RUSTUP_HOME=${RUSTUP_HOME:-/usr/local/rustup}
export PATH="/usr/local/cargo/bin:/usr/local/zig:$PATH"
export TRIPLE=$target
export ZIG_TARGET=$target
profile=${XTASK_RELEASE_PROFILE:-release}
profile_args=${CARGO_PROFILE_ARGS:-}
if [[ -z "$profile_args" && "$profile" == release ]]; then
    profile_args=--release
fi
export BUILD_PROFILE=$profile
export CARGO_PROFILE_ARGS=$profile_args
export CARGO_BUILD_JOBS=$build_jobs
export DEB_HOST_MULTIARCH=$gnu_arch
export SOURCE_ROOT=$product_source

cd "$source_dir"
if ! dpkg-buildpackage -B -uc -us -d -a"$deb_arch"; then
    echo "Debian package build failed." >&2
    echo "If rustc was killed with SIGKILL, retry with KUNDY_BUILD_JOBS=1 or increase Podman machine memory." >&2
    exit 1
fi

shopt -s nullglob
artifacts=("$work_dir"/kundy_*.deb)
if (( ${#artifacts[@]} == 0 )); then
    echo "dpkg-buildpackage did not produce a Kundy deb" >&2
    exit 1
fi
for artifact in "${artifacts[@]}" "$work_dir"/kundy_*.buildinfo "$work_dir"/kundy_*.changes; do
    if [ -f "$artifact" ]; then
        cp -f "$artifact" "$output_dir/"
    fi
done
echo "Debian artifacts written to $output_dir"
