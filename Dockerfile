# ---- 构建阶段 ----
FROM rust:1.92-alpine AS builder

# rustls(ring) 在 musl 目标下编译需要 C 工具链
RUN apk add --no-cache musl-dev perl make

WORKDIR /app
# 先复制依赖清单以利用 Docker 层缓存（Cargo.lock 存在则锁定版本，可复现构建）
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# 使用 rustls（--no-default-features），避免 musl 下依赖 OpenSSL
RUN cargo build --release --no-default-features

# ---- 运行阶段 ----
FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/target/release/kiro-mini-proxy /app/kiro-mini-proxy

VOLUME ["/app/config"]

EXPOSE 8991

CMD ["./kiro-mini-proxy", "-c", "/app/config/config.json"]
