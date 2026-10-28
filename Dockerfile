# syntax=docker/dockerfile:1.7

FROM rust:1.90-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p shuttle-cli && \
    install -m 0755 target/release/shuttle /usr/local/bin/shuttle && \
    /usr/local/bin/shuttle --version

FROM scratch AS binary-export
COPY --from=builder /usr/local/bin/shuttle /shuttle

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS core-builder
COPY --from=builder /usr/local/bin/shuttle /usr/local/bin/shuttle
RUN set -eux; \
    mkdir -p /core-root/usr/local/bin /core-root/etc /core-root/home/shuttle /core-root/workspace /core-root/tmp; \
    install -m 0755 /usr/local/bin/shuttle /core-root/usr/local/bin/shuttle; \
    ldd /usr/local/bin/shuttle | awk '/=> \/|^[[:space:]]*\// { for (i=1; i<=NF; i++) if ($i ~ /^\//) print $i }' | \
      while IFS= read -r library; do \
        destination="/core-root${library}"; \
        mkdir -p "$(dirname "$destination")"; \
        cp -L "$library" "$destination"; \
      done; \
    printf 'root:x:0:0:root:/root:/sbin/nologin\nea:x:65532:65532:Shuttle:/home/shuttle:/sbin/nologin\n' > /core-root/etc/passwd; \
    printf 'root:x:0:\nea:x:65532:\n' > /core-root/etc/group; \
    chown -R 65532:65532 /core-root/home/shuttle /core-root/workspace; \
    chmod 1777 /core-root/tmp

FROM scratch AS core
ARG VERSION=dev
ARG VCS_REF=unknown
ARG SOURCE_URL=https://github.com/fankainang/shuttle
ENV HOME=/home/shuttle
LABEL org.opencontainers.image.title="shuttle-core" \
      org.opencontainers.image.description="Minimal Shuttle execution kernel" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="MIT"
COPY --from=core-builder /core-root/ /
USER 65532:65532
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/shuttle"]
CMD ["--help"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/shuttle", "--version"]

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS tools-builder
ARG TARGETARCH
ARG VERSION=dev
ARG DEBIAN_SNAPSHOT=20260824T000000Z
RUN printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list && \
    printf 'deb-src [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list && \
    rm -f /etc/apt/sources.list.d/debian.sources && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
      'ca-certificates=20230311+deb12u1' \
      'python3=3.11.2-1+b1'
COPY release/external-tools-contract.json /release/external-tools-contract.json
COPY release/external_tools_bundle.py /release/external_tools_bundle.py
RUN apt-get install -y --no-install-recommends \
      'autoconf=2.71-3' \
      'automake=1:1.16.5-1.3' \
      'build-essential=12.9' \
      'libjansson-dev=2.14-2' \
      'make=4.3-4.1' \
      'pkg-config=1.8.1-1' && \
    mkdir -p /release/packages /release/sources && \
    python3 /release/external_tools_bundle.py preflight-inputs \
      --contract /release/external-tools-contract.json \
      --package-archives /release/packages \
      --source-archives /release/sources \
      --arch "$TARGETARCH" \
      --snapshot "$DEBIAN_SNAPSHOT" && \
    apt-get install -y --no-install-recommends \
      /release/packages/git_*.deb \
      /release/packages/ripgrep_*.deb \
      /release/packages/universal-ctags_*.deb \
      /release/packages/tar_*.deb \
      /release/packages/procps_*.deb \
      'patchelf=0.14.3-1+b1' && \
    mkdir /release/ctags-src && \
    tar -xzf /release/sources/universal-ctags_5.9.20210829.0.orig.tar.gz \
      --strip-components=1 -C /release/ctags-src && \
    cd /release/ctags-src && \
    ./autogen.sh && \
    ./configure --disable-xml --enable-json --disable-yaml --disable-seccomp && \
    make -j2 && \
    strip ctags && \
    install -m 0755 ctags /usr/local/bin/shuttle-ctags
RUN SHUTTLE_DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT" python3 /release/external_tools_bundle.py stage \
      --root /opt/shuttle-tools \
      --contract /release/external-tools-contract.json \
      --package-archives /release/packages \
      --version "$VERSION" \
      --arch "$TARGETARCH" \
      --ctags-source /usr/local/bin/shuttle-ctags \
      --source-archive-root /release/sources/runtime

FROM scratch AS tools
ARG VERSION=dev
ARG VCS_REF=unknown
ARG SOURCE_URL=https://github.com/fankainang/shuttle
LABEL org.opencontainers.image.title="shuttle-tools" \
      org.opencontainers.image.description="Immutable Shuttle external tool bundle" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="LicenseRef-Bundle-Notices"
COPY --from=tools-builder /opt/shuttle-tools/ /opt/shuttle-tools/

FROM scratch AS tools-bundle-export
COPY --from=tools /opt/shuttle-tools/ /

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS compat-builder
ARG DEBIAN_SNAPSHOT=20260824T000000Z
RUN printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list && \
    rm -f /etc/apt/sources.list.d/debian.sources && \
    apt-get update && \
    apt-get install -y --no-install-recommends 'ca-certificates=20230311+deb12u1' && \
    mkdir -p /compat-root/bin /compat-root/etc/ssl/certs && \
    install -m 0755 /bin/dash /compat-root/bin/dash && \
    ln -s dash /compat-root/bin/sh && \
    install -m 0644 /etc/ssl/certs/ca-certificates.crt /compat-root/etc/ssl/certs/ca-certificates.crt

FROM scratch AS full
ARG VERSION=dev
ARG VCS_REF=unknown
ARG SOURCE_URL=https://github.com/fankainang/shuttle
ENV HOME=/home/shuttle \
    PATH=/opt/shuttle-tools/bin:/usr/local/bin:/usr/bin:/bin \
    GIT_EXEC_PATH=/opt/shuttle-tools/libexec/git-core \
    GIT_TEMPLATE_DIR=/opt/shuttle-tools/share/git-core/templates
LABEL org.opencontainers.image.title="shuttle" \
      org.opencontainers.image.description="Shuttle with digest-bound external tools" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="MIT AND LicenseRef-Bundle-Notices"
COPY --from=core / /
COPY --from=compat-builder /compat-root/ /
COPY --from=tools /opt/shuttle-tools/ /opt/shuttle-tools/
USER 65532:65532
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/shuttle"]
CMD ["--help"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/shuttle", "--version"]

# Backwards-compatible build target and image tags remain the full runtime.
FROM full AS runtime
