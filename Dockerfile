# syntax=docker/dockerfile:1
# SAG-Wiki 文件解析服务器镜像
#
# 多架构：linux/amd64、linux/arm64。构建时交叉编译（不需要 QEMU 模拟），
#   docker buildx build --platform linux/amd64,linux/arm64 -t sag-parse-server .
# 本机构建：docker compose up -d --build（按宿主机架构自动选择）
#
# 运行期只含 sag 二进制 + ffmpeg/ffprobe + CA 证书，不含 Rust 工具链。

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

USER sag
VOLUME /data
EXPOSE 3001

HEALTHCHECK --interval=30s --timeout=5s --retries=3 --start-period=10s \
    CMD curl -fsS http://127.0.0.1:3001/health || exit 1

ENTRYPOINT ["/usr/local/bin/sag"]
CMD ["parse-server", "--host", "0.0.0.0", "--port", "3001", "--data-dir", "/data"]
