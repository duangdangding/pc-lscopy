# 构建阶段（rust alpine 即可，纯 Rust 依赖，无需 openssl）
FROM rust:1-alpine AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

# 运行阶段：极简 alpine
FROM alpine:3.20
RUN adduser -D relay
COPY --from=build /app/target/release/relay-server /usr/local/bin/relay-server
USER relay
EXPOSE 8780
ENV RELAY_PORT=8780 \
    RELAY_DATA_DIR=/data
VOLUME ["/data"]
ENTRYPOINT ["relay-server"]
