#!/usr/bin/env bash
set -euo pipefail

target="x86_64-unknown-linux-gnu"
output_dir=""
build_jobs="${KUNDY_BUILD_JOBS:-1}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(cd -- "${script_dir}/.." && pwd -P)"

usage() {
    cat <<'USAGE'
Usage: ./scripts/build-deb.sh [options]

Options:
  --target TRIPLE  Rust target to package (default: x86_64-unknown-linux-gnu).
                   Supported: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
                   armv7-unknown-linux-gnueabihf, i686-unknown-linux-gnu.
  --output DIR     Directory for .deb artifacts (default: target/deb/<triple>).
  -h, --help       Show this help.

Environment:
  KUNDY_BUILD_JOBS Number of concurrent Cargo jobs (default: 1).

Before building, place the bootstrap private key at
tls/kundy.dhttp.net.key.pem. The path is ignored by Git and is read directly
by the container build.

The build runs in a Debian container and requires Podman or Docker. Podman is
preferred when both are installed; set CONTAINER_ENGINE=podman or docker to
choose explicitly. The package installs Kundy and its setup assets; host user
selection and service startup are performed explicitly with kundy setup.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            [[ $# -ge 2 ]] || { echo "--target requires a Rust target" >&2; exit 2; }
            target="$2"
            shift 2
            ;;
        --output)
            [[ $# -ge 2 ]] || { echo "--output requires a directory" >&2; exit 2; }
            output_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$target" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|armv7-unknown-linux-gnueabihf|i686-unknown-linux-gnu)
        ;;
    *)
        echo "Unsupported target: $target" >&2
        exit 2
        ;;
esac

if [[ ! "$build_jobs" =~ ^[1-9][0-9]*$ ]]; then
    echo "KUNDY_BUILD_JOBS must be a positive integer" >&2
    exit 2
fi

if [[ -z "$output_dir" ]]; then
    output_dir="$repo_dir/target/deb/$target"
elif [[ "$output_dir" != /* ]]; then
    output_dir="$repo_dir/$output_dir"
fi

bootstrap_private_key="$repo_dir/tls/kundy.dhttp.net.key.pem"
if [[ ! -r "$bootstrap_private_key" ]]; then
    echo "missing bootstrap TLS private key: $bootstrap_private_key" >&2
    echo "place the private key at that ignored path before building" >&2
    exit 1
fi

if [[ -n "${CONTAINER_ENGINE:-}" ]]; then
    container_engine="$CONTAINER_ENGINE"
elif [[ -n "${PODMAN:-}" ]]; then
    container_engine="$PODMAN"
elif [[ -n "${DOCKER:-}" ]]; then
    container_engine="$DOCKER"
elif command -v podman >/dev/null 2>&1; then
    container_engine=podman
elif command -v docker >/dev/null 2>&1; then
    container_engine=docker
else
    echo "Podman or Docker CLI is required" >&2
    exit 1
fi

command -v "$container_engine" >/dev/null 2>&1 || {
    echo "Container engine is unavailable: $container_engine" >&2
    exit 1
}

mkdir -p "$output_dir"
output_dir="$(cd -- "$output_dir" && pwd -P)"
image="kundy-deb-builder:${target}"

echo "Building Kundy deb for $target"
"$container_engine" build \
    --pull \
    --build-arg "KUNDY_RELEASE_TARGET=$target" \
    --tag "$image" \
    --file "$repo_dir/packaging/deb/Dockerfile" \
    "$repo_dir"

run_args=(
    run
    --rm
    --user "$(id -u):$(id -g)"
)
case "$container_engine" in
    podman|*/podman)
        run_args+=(--userns=keep-id)
        ;;
esac
run_args+=(
    --env "KUNDY_RELEASE_TARGET=$target"
    --env "KUNDY_BUILD_JOBS=$build_jobs"
    --env KUNDY_RELEASE_OUT_DIR=/output
    --env HOME=/tmp
    --env CARGO_HOME=/tmp/cargo
    --env RUSTUP_HOME=/usr/local/rustup
    --mount "type=bind,src=$repo_dir,dst=/workspace,readonly"
    --mount "type=bind,src=$output_dir,dst=/output"
    "$image"
    /opt/kundy/deb/package.sh
)
"$container_engine" "${run_args[@]}"

echo
echo "Debian artifacts: $output_dir"
find "$output_dir" -maxdepth 1 -type f -name '*.deb' -print
