# syntax=docker/dockerfile:1
# SAG-Wiki 文件解析服务器镜像
#
# 多架构：linux/amd64、linux/arm64。构建时交叉编译（不需要 QEMU 模拟），
#   docker buildx build --platform linux/amd64,linux/arm64 -t sag-parse-server .
# 本机构建：docker compose up -d --build（按宿主机架构自动选择）
#
# 镜像自带全部解析依赖，开箱即用，容器内不再下载安装：
# - Docling 文档组件（Python 3.12 + docling + 布局/TableFormer/RapidOCR 模型）：
#   html、msg、zip 等格式与 PDF/Office 的本地降级都靠它。
# - Docling 音视频组件（openai-whisper + whisper-tiny 模型 + 内置 ffmpeg）：
#   音视频走 Docling media 时用它；ffmpeg/ffprobe 另由 apt 提供。
# - anydoc 内置解析、纯文本直读：随 sag 二进制自带。
# - MinerU 是云端服务，不在镜像里；配 key 后走官方接口（见 docker-compose.yml）。
#
# 首次构建说明：docling 预装阶段要从 GitHub / PyPI / HuggingFace 下载约 6GB，
# 耗时约 15–40 分钟；容器首次使用组件时只做本地复制（/opt/sag → /data），
# 不再联网下载。日常 `docker compose up -d` 不带 --build 不会重建。
# 注意：docling 预装阶段在目标架构上运行（要执行目标架构的 sag 与 Python），
# amd64 宿主机上构建 arm64 镜像会走 QEMU 模拟，安装步骤明显更慢；arm64 建议
# 用原生 ARM 构建机或 buildx 远程 ARM 节点。
#
# 运行期含 sag 二进制 + ffmpeg/ffprobe + CA 证书 + 预装的 Docling 组件，
# 不含 Rust 工具链。

ARG RUST_VERSION=1.99

# ---------- 构建阶段（始终在宿主机架构上跑，交叉编译目标架构）----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder

ARG TARGETARCH
WORKDIR /src

COPY . .

# TARGETARCH 由 BuildKit 传入：amd64 / arm64
# arm64 用 aarch64 交叉工具链编译；amd64 直接本机编译。
# 注：刻意不用 BuildKit 缓存挂载（--mount=type=cache），它在 overlay 套 overlay
# 的环境里会挂载失败；改用普通层缓存，换取在各类宿主机上都能直接构建。
RUN set -eux; \
  case "$TARGETARCH" in \
    amd64) TARGET=x86_64-unknown-linux-gnu ;; \
    arm64) TARGET=aarch64-unknown-linux-gnu ;; \
    *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
  esac; \
  if [ "$TARGETARCH" = "arm64" ]; then \
    rustup target add aarch64-unknown-linux-gnu; \
    apt-get update && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu; \
    rm -rf /var/lib/apt/lists/*; \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
  fi; \
  cargo build --release --locked --target "$TARGET" --manifest-path cli/Cargo.toml; \
  mkdir -p /out; \
  cp "cli/target/$TARGET/release/sag" /out/sag; \
  /out/sag --version

# ---------- Docling 预装阶段（在目标架构上运行，执行目标架构的二进制）----------
# 用刚编好的 sag 跑 `sag docling install`（生产环境的同一套安装代码：uv 装
# Python 3.12 → pip 装 hash 锁定的依赖 → 下载识别模型 → 写清单并验收），
# 再把激活后的组件目录收进 /opt/sag，供运行阶段通过环境变量指过去。
# 组件目录是自包含的（Python、site-packages、模型、manifest.json 都在里面），
# 换位置后仍可用；容器首次使用时服务把它复制进 /data，不再联网。
FROM --platform=$TARGETPLATFORM rust:${RUST_VERSION}-bookworm AS docling

COPY --from=builder /out/sag /usr/local/bin/sag

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

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

# ffmpeg/ffprobe：音视频抽音用；curl：健康检查用；ca-certificates：MinerU 等 HTTPS 上游用。
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 sag \
    && mkdir -p /data \
    && chown sag:sag /data

COPY --from=builder /out/sag /usr/local/bin/sag

# 预装的 Docling 组件：文档组件与音视频组件。服务首次使用时按环境变量
# 把它们复制进 /data/docling（本地复制，不下载）；组件已在 /data 里时不再复制。
COPY --from=docling /opt/sag/docling-documents /opt/sag/docling-documents
COPY --from=docling /opt/sag/docling-media /opt/sag/docling-media
RUN chmod -R a+rX /opt/sag/docling-documents /opt/sag/docling-media

ENV SAG_DOCLING_COMPONENT_DIR=/opt/sag/docling-documents
ENV SAG_DOCLING_MEDIA_COMPONENT_DIR=/opt/sag/docling-media

USER sag
VOLUME /data
EXPOSE 3001

HEALTHCHECK --interval=30s --timeout=5s --retries=3 --start-period=10s \
    CMD curl -fsS http://127.0.0.1:3001/health || exit 1

ENTRYPOINT ["/usr/local/bin/sag"]
CMD ["parse-server", "--host", "0.0.0.0", "--port", "3001", "--data-dir", "/data"]
