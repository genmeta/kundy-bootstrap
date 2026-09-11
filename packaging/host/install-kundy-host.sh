#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary="/usr/bin/kundy"
binary_explicit=false
runtime_user="${SUDO_USER:-}"
local_systemd_dir="/etc/systemd/system"
systemd_source_dir="${script_dir}/systemd"
install_static_units=true

# deb 包拥有静态 units，setup 只生成依赖所选用户的主服务。
#
# The deb owns the static units; setup only renders the main service that
# depends on the selected user.
if [[ "${script_dir}" == "/usr/libexec/kundy" ]]; then
    systemd_source_dir="/usr/lib/kundy/systemd"
    install_static_units=false
fi

usage() {
    cat <<'USAGE'
Usage: sudo kundy setup --user USER

Source-tree usage: sudo ./packaging/host/install-kundy-host.sh [options]

Options:
  --binary PATH  Install this prebuilt Kundy binary (default: /usr/bin/kundy).
  --user USER    Run Kundy as this existing non-root user.
  -h, --help     Show this help.

Source-tree usage defaults to the original SUDO_USER when --user is omitted.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            [[ $# -ge 2 ]] || { echo "--binary requires a path" >&2; exit 2; }
            binary="$2"
            binary_explicit=true
            shift 2
            ;;
        --user)
            [[ $# -ge 2 ]] || { echo "--user requires a user name" >&2; exit 2; }
            runtime_user="$2"
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

if [[ "${binary_explicit}" == false && ( ! -x "${binary}" || ! -f "${binary}" ) ]]; then
    for candidate in \
        "${script_dir}/../../target/release/kundy" \
        "${script_dir}/../../target/x86_64-unknown-linux-musl/release/kundy" \
        "${script_dir}/../../target/x86_64-unknown-linux-gnu/release/kundy"; do
        if [[ -x "${candidate}" && -f "${candidate}" ]]; then
            binary="${candidate}"
            break
        fi
    done
fi

service_template="${systemd_source_dir}/kundy.service.in"
if [[ ! -f "${service_template}" ]]; then
    echo "Kundy systemd service template is missing: ${service_template}" >&2
    exit 1
fi

if [[ ${EUID} -ne 0 ]]; then
    echo "Run this installer with sudo." >&2
    exit 1
fi
if [[ -z "${runtime_user}" || "${runtime_user}" == "root" ]]; then
    echo "Select an existing non-root Kundy runtime user." >&2
    exit 1
fi
if [[ ! "${runtime_user}" =~ ^[a-z_][a-z0-9_-]*[$]?$ ]]; then
    echo "The Kundy runtime user name is invalid." >&2
    exit 1
fi
if [[ ! -x "${binary}" || ! -f "${binary}" ]]; then
    echo "Kundy binary not found or not executable: ${binary}" >&2
    exit 1
fi

for path in /usr/bin/genmeta /usr/bin/getent /usr/bin/pishoo /usr/bin/systemctl /usr/local/bin/k3s; do
    if [[ ! -x "${path}" ]]; then
        echo "Required executable is missing at its supported path: ${path}" >&2
        exit 1
    fi
done

passwd_record="$(getent passwd "${runtime_user}")" || {
    echo "The selected Kundy runtime user does not exist: ${runtime_user}" >&2
    exit 1
}
IFS=: read -r _ _ runtime_uid runtime_gid _ runtime_home _ <<<"${passwd_record}"
if [[ "${runtime_uid}" == "0" || ! "${runtime_gid}" =~ ^[0-9]+$ || ! "${runtime_home}" =~ ^/ || ! -d "${runtime_home}" ]]; then
    echo "The selected Kundy runtime user has an invalid home directory." >&2
    exit 1
fi

for command in genmeta getent install pishoo sed systemctl usermod; do
    command -v "${command}" >/dev/null || {
        echo "Required command is missing: ${command}" >&2
        exit 1
    }
done
getent group dhttp >/dev/null || {
    echo "The dhttp group is missing; install the Pishoo package first." >&2
    exit 1
}

if ! id -nG "${runtime_user}" | tr ' ' '\n' | grep -Fxq dhttp; then
    usermod --append --groups dhttp "${runtime_user}"
fi

# ProtectHome 的写例外只能作用于已存在的路径；首次启动前由 root 创建空目录，
# 但绝不修改已有 identity 目录的内容或权限。
#
# ProtectHome write exceptions only apply to existing paths. Create empty directories before
# the first start without changing an existing identity directory's contents or permissions.
for private_dir in "${runtime_home}/.kundy" "${runtime_home}/.dhttp"; do
    if [[ -e "${private_dir}" ]]; then
        if [[ -L "${private_dir}" || ! -d "${private_dir}" ]]; then
            echo "Kundy runtime path must be a real directory: ${private_dir}" >&2
            exit 1
        fi
    else
        install -d -m 0700 -o "${runtime_uid}" -g "${runtime_gid}" "${private_dir}"
    fi
done

destination="/usr/bin/kundy"
if [[ "$(readlink -f "${binary}")" != "$(readlink -m "${destination}")" ]]; then
    install -D -m 0755 "${binary}" "${destination}"
fi

temp_dir="$(mktemp -d)"
trap 'rm -rf -- "${temp_dir}"' EXIT
escaped_user="$(printf '%s' "${runtime_user}" | sed 's/[&|\\]/\\&/g')"
escaped_home="$(printf '%s' "${runtime_home}" | sed 's/[&|\\]/\\&/g')"
sed \
    -e "s|@KUNDY_USER@|${escaped_user}|g" \
    -e "s|@KUNDY_HOME@|${escaped_home}|g" \
    "${service_template}" \
    >"${temp_dir}/kundy.service"

install -m 0644 "${temp_dir}/kundy.service" "${local_systemd_dir}/kundy.service"
if [[ "${install_static_units}" == true ]]; then
    for unit in \
        kundy-runtime-config-apply.path \
        kundy-runtime-config-apply.service \
        kundy-pishoo-reload.path \
        kundy-pishoo-reload.service; do
        install -m 0644 "${systemd_source_dir}/${unit}" "${local_systemd_dir}/${unit}"
    done
fi

systemctl daemon-reload
systemctl enable kundy.service kundy-runtime-config-apply.path kundy-pishoo-reload.path
systemctl restart kundy.service
systemctl start kundy-runtime-config-apply.path kundy-pishoo-reload.path
systemctl reload pishoo.service

echo "Kundy host integration installed for ${runtime_user}."
echo "Run 'kundy activate' as ${runtime_user} to activate this appliance."
