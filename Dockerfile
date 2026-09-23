# Agent2API headless 网关镜像（多阶段构建，amd64 + arm64）
#
# ── 为什么只构建 server crate ────────────────────────────────
# 仓库里有两个 crate：桌面端（tauri，Linux 下要 webkit2gtk 一整套系统库）
# 与网关本体（server/，无 GUI 依赖）。容器里只需要网关本体 ——
# `cargo build -p agent2api-server` 明确只编它。
#
# ── 多架构：交叉编译而不是 QEMU ─────────────────────────────
# builder 固定跑在构建机的原生架构（$BUILDPLATFORM）：buildx 构建 arm64
# 时不进 QEMU 模拟（Rust 编译会慢几十倍），而是 rustup 加一个 aarch64
# target + 装 aarch64 的 C 工具链交叉编译。转发链路全是 rustls（纯 Rust），
# 唯一的 C 代码是 rusqlite bundled 的 SQLite 源码，gcc-aarch64 工具链足够。
#
# ── 依赖缓存层 ──────────────────────────────────────────────
# 先用空的 lib/bin 桩把全部依赖编一遍（首次几分钟），之后只改源码重新
# 构建时这一层全部命中缓存。COPY 会保留源文件 mtime —— 它们早于桩层的
# 编译时刻，cargo 的增量判断会误判「没变」而跳过重编，所以真源码层先
# find touch 再编。

# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
WORKDIR /build

# 可选：容器内 cargo 拉取 crates.io 走宿主代理（按需传入，默认不用）：
#   docker build --build-arg HTTP_PROXY=http://host.docker.internal:7890 \
#                --build-arg HTTPS_PROXY=http://host.docker.internal:7890 .
ARG HTTP_PROXY=""
ARG HTTPS_PROXY=""

# TARGETARCH 由 buildx/BuildKit 自动注入：amd64 / arm64。arm64 需要
# aarch64 的 C 工具链（rusqlite bundled 编 SQLite 的 C 源码用）与链接器。
RUN if [ "$TARGETARCH" = "arm64" ]; then \
        rustup target add aarch64-unknown-linux-gnu \
        && apt-get update \
        && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu \
        && rm -rf /var/lib/apt/lists/*; \
    fi
# cc / cargo 按这两个变量找交叉工具（只在 arm64 构建时生效；amd64 原生用默认值）
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++

# 统一构建脚本：amd64 原生 / arm64 交叉，产物统一归位到 /build/agent2api-server
#（COPY --from 无法条件分支，所以在这里收拢路径）
RUN { echo '#!/bin/sh -e'; \
      echo 'cd /build/src-tauri'; \
      echo 'if [ "$TARGETARCH" = "arm64" ]; then'; \
      echo '  cargo build --release -p agent2api-server --target aarch64-unknown-linux-gnu --target-dir /build/target'; \
      echo '  cp /build/target/aarch64-unknown-linux-gnu/release/agent2api-server /build/agent2api-server'; \
      echo 'else'; \
      echo '  cargo build --release -p agent2api-server --target-dir /build/target'; \
      echo '  cp /build/target/release/agent2api-server /build/agent2api-server'; \
      echo 'fi'; \
    } > /build/cargo-build.sh && chmod +x /build/cargo-build.sh

COPY desktop-tauri/src-tauri/Cargo.toml desktop-tauri/src-tauri/Cargo.lock src-tauri/
COPY desktop-tauri/src-tauri/server/Cargo.toml src-tauri/server/
RUN mkdir -p src-tauri/server/src/bin src-tauri/src \
    && echo "" > src-tauri/server/src/lib.rs \
    && echo "fn main() {}" > src-tauri/server/src/bin/agent2api-server.rs \
    && echo "" > src-tauri/src/lib.rs \
    && echo "fn main() {}" > src-tauri/src/main.rs
# 桩依赖层：整棵依赖树编一遍，命中后成为之后每次构建的缓存底座
RUN /build/cargo-build.sh

COPY desktop-tauri/src-tauri/server/src src-tauri/server/src
RUN cd src-tauri \
    && find server/src -type f -exec touch {} + \
    && /build/cargo-build.sh

# ── 运行时 ──────────────────────────────────────────────────
# bookworm-slim + ca-certificates（出站 HTTPS）+ curl（HEALTHCHECK）。
# 网关链的是 rustls（纯 Rust），运行时没有任何额外的共享库要求。
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/agent2api-server /usr/local/bin/agent2api-server
COPY desktop-tauri/ui /app/ui

# 容器内的默认形态：全网卡监听 + 数据落卷 + 自托管面板。
# 鉴权：面板需要管理员（登录页注册或 env 预置）；未配置任何 API Key 时
# /v1/* 处于 fail-closed（登录面板创建第一把后自动恢复）。
# AGENT2API_CAPTCHA_ENABLED：登录页人机验证组件环境变量，默认为1开启，0为关闭。
ENV AGENT2API_HOST=0.0.0.0 \
    AGENT2API_PROXY_HOME=/data \
    AGENT2API_UI_DIR=/app/ui \
    AGENT2API_CAPTCHA_ENABLED=1
VOLUME ["/data"]
EXPOSE 3065
WORKDIR /app
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${AGENT2API_PROXY_PORT:-3065}/health" || exit 1
ENTRYPOINT ["/usr/local/bin/agent2api-server"]
