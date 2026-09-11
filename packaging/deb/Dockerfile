ARG KUNDY_RELEASE_RUST_IMAGE=rust:bookworm
FROM ${KUNDY_RELEASE_RUST_IMAGE}

ARG XTASK_RELEASE_TARGET
ARG KUNDY_RELEASE_TARGET
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/zig:$PATH

RUN set -eux; \
    apt-get update -qq; \
    apt-get install --assume-yes -qq \
        ca-certificates build-essential pkg-config libclang-dev wget \
        python3 dpkg-dev debhelper fakeroot; \
    target="${XTASK_RELEASE_TARGET:-${KUNDY_RELEASE_TARGET:-}}"; \
    case "$target" in \
        x86_64-unknown-linux-gnu) deb=amd64; gnu=x86_64-linux-gnu ;; \
        aarch64-unknown-linux-gnu) deb=arm64; gnu=aarch64-linux-gnu ;; \
        armv7-unknown-linux-gnueabihf) deb=armhf; gnu=arm-linux-gnueabihf ;; \
        i686-unknown-linux-gnu) deb=i386; gnu=i386-linux-gnu ;; \
        *) echo "unsupported target $target" >&2; exit 1 ;; \
    esac; \
    native_deb=$(dpkg --print-architecture); \
    if [ "$deb" != "$native_deb" ]; then \
        dpkg --add-architecture "$deb"; \
        apt-get update -qq; \
        apt-get install --assume-yes -qq "libc6-dev:$deb" "binutils-$gnu" || \
            apt-get install --assume-yes -qq "libc6-dev:$deb"; \
    fi; \
    rustup target add "$target"; \
    case "$(uname -m)" in \
        x86_64) zig_arch=x86_64 ;; \
        aarch64) zig_arch=aarch64 ;; \
        *) echo "unsupported build host architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    zig_archive="zig-$zig_arch-linux-0.16.0.tar.xz"; \
    wget -q --tries=5 --timeout=30 \
        "https://ziglang.org/download/0.16.0/$zig_archive"; \
    tar -xf "$zig_archive"; \
    mv "zig-$zig_arch-linux-0.16.0" /usr/local/zig; \
    ln -s /usr/local/zig/zig /usr/local/bin/zig; \
    rm "$zig_archive"; \
    CARGO_TARGET_DIR=/tmp/cargo-install-target cargo install --locked cargo-zigbuild; \
    chmod -R a+rX /usr/local/cargo /usr/local/rustup

COPY packaging/deb /opt/kundy/deb
COPY xtask/release/deb/package.sh /opt/genmeta-release/package
RUN chmod 0755 /opt/kundy/deb/package.sh /opt/kundy/deb/rules /opt/genmeta-release/package
