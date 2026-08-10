# Multi-stage build: compile with musl, run on a minimal Alpine image.
FROM rust:1.97-alpine AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM alpine:3.22
RUN apk add --no-cache git ca-certificates
WORKDIR /opt/lemon-agent
COPY --from=builder /build/target/release/lemon-agent ./
COPY scripts ./scripts
COPY config.toml ./
RUN adduser -D -h /opt/lemon-agent lemon \
    && mkdir -p workspace \
    && chown -R lemon:lemon /opt/lemon-agent
USER lemon
VOLUME ["/opt/lemon-agent"]
# Override the LLM key at runtime; never bake it into an image.
ENV AGENT_API_KEY=""
ENTRYPOINT ["./lemon-agent"]
CMD ["--config", "config.toml"]
