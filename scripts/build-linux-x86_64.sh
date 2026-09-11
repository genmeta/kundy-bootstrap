#!/usr/bin/env bash
set -euo pipefail

target="x86_64-unknown-linux-musl"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
jobs="${KUNDY_BUILD_JOBS:-2}"

if [[ ! "${jobs}" =~ ^[1-9][0-9]*$ ]]; then
    echo "KUNDY_BUILD_JOBS must be a positive integer." >&2
    exit 2
fi
if ! command -v cross >/dev/null 2>&1; then
    echo "cross is required. Install it with: cargo install cross" >&2
    exit 1
fi

cd "${repo_dir}"
echo "Building Kundy for ${target} with ${jobs} parallel jobs..."
if ! cross build \
    --locked \
    --release \
    --target "${target}" \
    --jobs "${jobs}"; then
    echo >&2
    echo "Linux build failed." >&2
    echo "If rustc ended with SIGKILL, the cross VM ran out of memory." >&2
    echo "Retry with KUNDY_BUILD_JOBS=1 or allocate more memory to the container VM." >&2
    exit 1
fi

binary="${repo_dir}/target/${target}/release/kundy"
if [[ ! -x "${binary}" ]]; then
    echo "Build finished without producing an executable at ${binary}." >&2
    exit 1
fi

file "${binary}"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${binary}"
else
    shasum -a 256 "${binary}"
fi

echo
echo "Linux artifact: ${binary}"
