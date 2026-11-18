# syntax=docker/dockerfile:1
# SAG-Wiki 文件解析服务器镜像
#
# 多架构：linux/amd64、linux/arm64。应用镜像从已构建的解析运行时基础镜像
# 继承 Docling 组件；日常源码更新只重建 sag 应用二进制，不重装模型。
# 基础镜像需单独构建：docker compose -f docker-compose.docling-base.yml build
# 部署/更新应用：docker compose up -d --build
#
# 镜像自带全部解析依赖，开箱即用，容器内不再下载安装：
# - Docling 文档组件（Python 3.12 + docling + 布局/TableFormer/RapidOCR 模型）：
#   html、msg、zip 等格式与 PDF/Office 的本地降级都靠它。
# - Docling 音视频组件（openai-whisper + whisper-tiny 模型 + 内置 ffmpeg）：
#   音视频走 Docling media 时用它；ffmpeg/ffprobe 另由 apt 提供。
# - anydoc 内置解析、纯文本直读：随 sag 二进制自带。
# - MinerU 是云端服务，不在镜像里；配 key 后走官方接口（见 docker-compose.yml）。
#
# 首次构建基础镜像时要从 GitHub / PyPI / HuggingFace 下载约 6GB，耗时约
# 15–40 分钟。Rust crates 默认使用 rsproxy.cn，Python 包使用清华 PyPI 镜像，
# Python 运行时使用 npmmirror，PyTorch CPU wheel 使用交大镜像，
# HuggingFace 模型使用 https://hf-mirror.net。
# 可在 .env 覆盖镜像地址。基础镜像构建成功后，应用源码更新不会重装组件。
# 容器首次启动会先将组件本地复制到具名数据卷（/opt/sag → /data），完成后
# 才启动 API；健康检查为首次复制预留 30 分钟。
#
# 运行期含 sag 二进制 + ffmpeg/ffprobe + CA 证书 + 预装的 Docling 组件，
# 不含 Rust 工具链。

ARG RUST_VERSION=1.99
ARG SAG_RUNTIME_BASE_IMAGE=sag-parse-server-base:a801f28
ARG SAG_CARGO_REGISTRY_MIRROR=sparse+https://rsproxy.cn/index/
ARG PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
ARG UV_PYTHON_INSTALL_MIRROR=https://registry.npmmirror.com/-/binary/python-build-standalone
ARG UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
ARG SAG_TORCH_CPU_INDEX_URL=https://mirror.sjtu.edu.cn/pytorch-wheels/cpu
ARG HF_ENDPOINT=https://hf-mirror.net

# ---------- 构建阶段（始终在宿主机架构上跑；同架构时原生编译）----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder

ARG BUILDARCH
ARG TARGETARCH
ARG SAG_CARGO_REGISTRY_MIRROR
WORKDIR /src

# Cargo dependencies are build-only. Use the China mirror by default to avoid
# slow or blocked direct crates.io access on mainland deployment hosts.
RUN mkdir -p /usr/local/cargo \
  && printf '[source.crates-io]\nreplace-with = "china-mirror"\n[source.china-mirror]\nregistry = "%s"\n' \
    "$SAG_CARGO_REGISTRY_MIRROR" > /usr/local/cargo/config.toml

COPY . .

# BUILDARCH / TARGETARCH 由 BuildKit 传入：amd64 / arm64。
# 构建机与目标机架构相同时原生编译，不安装交叉工具链；仅跨架构时安装 linker。
# 注：刻意不用 BuildKit 缓存挂载（--mount=type=cache），它在 overlay 套 overlay
# 的环境里会挂载失败；改用普通层缓存，换取在各类宿主机上都能直接构建。
RUN set -eux; \
  case "$TARGETARCH" in \
    amd64) TARGET=x86_64-unknown-linux-gnu; CROSS_LINKER=x86_64-linux-gnu-gcc ;; \
    arm64) TARGET=aarch64-unknown-linux-gnu; CROSS_LINKER=aarch64-linux-gnu-gcc ;; \
    *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
  esac; \
  if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
    rustup target add "$TARGET"; \
    case "$TARGETARCH" in \
      arm64) CROSS_PACKAGE=gcc-aarch64-linux-gnu ;; \
      amd64) CROSS_PACKAGE=gcc-x86-64-linux-gnu ;; \
    esac; \
    apt-get update && apt-get install -y --no-install-recommends "$CROSS_PACKAGE"; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
      arm64) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$CROSS_LINKER" ;; \
      amd64) export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$CROSS_LINKER" ;; \
    esac; \
  fi; \
  cargo build --release --locked --target "$TARGET" --manifest-path cli/Cargo.toml; \
  mkdir -p /out; \
  cp "cli/target/$TARGET/release/sag" /out/sag; \
  /out/sag --version

# ---------- uv 运行环境管理器（PyPI 镜像提供，避免依赖 GitHub Release）----------
FROM python:3.12-slim AS uv-bootstrap

ARG PIP_INDEX_URL
RUN python -m pip install --no-cache-dir --index-url "$PIP_INDEX_URL" \
      --target /tmp/uv-package uv==0.12.1 \
    && install -D -m 0755 /tmp/uv-package/bin/uv /out/uv

# ---------- Docling 预装阶段（只在显式构建基础镜像时运行）----------
# 用刚编好的 sag 跑 `sag docling install`（生产环境的同一套安装代码：uv 装
# Python 3.12 → pip 装 hash 锁定的依赖 → 下载识别模型 → 写清单并验收），
# 再把激活后的组件目录收进 /opt/sag，供运行阶段通过环境变量指过去。
# 组件目录是自包含的（Python、site-packages、模型、manifest.json 都在里面），
# 换位置后仍可用；容器首次使用时服务把它复制进 /data，不再联网。
FROM --platform=$TARGETPLATFORM rust:${RUST_VERSION}-bookworm AS docling

ARG HF_ENDPOINT=https://hf-mirror.net
ARG UV_PYTHON_INSTALL_MIRROR
ARG UV_DEFAULT_INDEX
ARG SAG_TORCH_CPU_INDEX_URL
ENV HF_ENDPOINT=${HF_ENDPOINT} \
    UV_PYTHON_INSTALL_MIRROR=${UV_PYTHON_INSTALL_MIRROR} \
    UV_DEFAULT_INDEX=${UV_DEFAULT_INDEX} \
    SAG_TORCH_CPU_INDEX_URL=${SAG_TORCH_CPU_INDEX_URL}

COPY --from=builder /out/sag /usr/local/bin/sag
COPY --from=uv-bootstrap /out/uv /build-data/docling/uv-real
RUN printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'if [ "${1:-}" = "pip" ] && [ "${2:-}" = "install" ] && [ -n "${SAG_TORCH_CPU_INDEX_URL:-}" ]; then' \
    '  unset UV_TORCH_BACKEND' \
    '  exec /build-data/docling/uv-real "$@" --index-strategy unsafe-best-match --index "pytorch=$SAG_TORCH_CPU_INDEX_URL"' \
    'fi' \
    'exec /build-data/docling/uv-real "$@"' \
    > /build-data/docling/uv \
  && chmod 0755 /build-data/docling/uv \
  && mkdir -p /build-data/docling/media \
  && cp /build-data/docling/uv /build-data/docling/media/uv \
  && chmod 0755 /build-data/docling/media/uv

RUN set -eux; \
  sag docling install --data-dir /build-data --profile all; \
  doc_dir=$(ls -d /build-data/docling/components/documents-*); \
  media_dir=$(ls -d /build-data/docling/media/components/media-*); \
  mkdir -p /opt/sag/docling-documents /opt/sag/docling-media; \
  cp -a "$doc_dir/." /opt/sag/docling-documents/; \
  cp -a "$media_dir/." /opt/sag/docling-media/; \
  test -f /opt/sag/docling-documents/manifest.json; \
  test -f /opt/sag/docling-media/manifest.json; \
  du -sh /opt/sag/docling-documents /opt/sag/docling-media

# ---------- 解析运行时基础镜像：Debian 运行库 + 固定版本 Docling 组件 ----------
FROM debian:bookworm-slim AS runtime-base

# ffmpeg/ffprobe：音视频抽音用；curl：健康检查用；ca-certificates：MinerU 等 HTTPS 上游用。
# Use the China mirror for Debian packages as well as Python, Rust, and model downloads.
RUN sed -i \
      -e 's#http://deb.debian.org/debian-security#https://mirrors.tuna.tsinghua.edu.cn/debian-security#g' \
      -e 's#http://deb.debian.org/debian#https://mirrors.tuna.tsinghua.edu.cn/debian#g' \
      /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 sag \
    && mkdir -p /data \
    && chown sag:sag /data

# 预装的 Docling 组件：文档组件与音视频组件。服务首次使用时按环境变量
# 把它们复制进 /data/docling（本地复制，不下载）；组件已在 /data 里时不再复制。
COPY --from=docling /opt/sag/docling-documents /opt/sag/docling-documents
COPY --from=docling /opt/sag/docling-media /opt/sag/docling-media
RUN chmod -R a+rX /opt/sag/docling-documents /opt/sag/docling-media

# The process supervisor snapshots child RSS with /bin/ps. Debian slim omits it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends procps \
    && rm -rf /var/lib/apt/lists/*

ENV SAG_DOCLING_COMPONENT_DIR=/opt/sag/docling-documents
ENV SAG_DOCLING_MEDIA_COMPONENT_DIR=/opt/sag/docling-media

USER sag
VOLUME /data
EXPOSE 3001

# First boot copies the prebuilt components into /data before the HTTP server starts.
HEALTHCHECK --interval=30s --timeout=5s --retries=3 --start-period=30m \
    CMD curl -fsS http://127.0.0.1:3001/health || exit 1

# ---------- 应用镜像：源码更新只替换小体积 sag 二进制 ----------
FROM ${SAG_RUNTIME_BASE_IMAGE} AS app

COPY --from=builder /out/sag /usr/local/bin/sag
COPY --chmod=755 scripts/parse-server-entrypoint.sh /usr/local/bin/parse-server-entrypoint

ENTRYPOINT ["/usr/local/bin/parse-server-entrypoint"]
CMD ["parse-server", "--host", "0.0.0.0", "--port", "3001", "--data-dir", "/data"]
