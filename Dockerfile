# syntax=docker/dockerfile:1
FROM alpine:edge AS builder

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

RUN apk add --no-cache cargo musl-dev openssl-dev pkgconfig ffmpeg-dev clang21-dev

ARG TARGETARCH

# Pre-build dependencies (cached layer - only invalidated when Cargo.toml changes)
COPY Cargo.toml /src/rustvideoplatform-processor/
RUN mkdir -p /src/rustvideoplatform-processor/src && echo 'fn main() {}' > /src/rustvideoplatform-processor/src/main.rs
RUN FEATURES=""; \
    if [ "$TARGETARCH" = "amd64" ] || [ "$TARGETARCH" = "arm64" ]; then FEATURES="--features pdf"; fi; \
    if [ "$TARGETARCH" = "amd64" ]; then export RUSTFLAGS="-C target-cpu=x86-64-v2"; fi \
    && cd /src/rustvideoplatform-processor && cargo build --release $FEATURES 2>/dev/null ; true

# Build actual project
COPY ./ /src/rustvideoplatform-processor
RUN FEATURES=""; \
    if [ "$TARGETARCH" = "amd64" ] || [ "$TARGETARCH" = "arm64" ]; then FEATURES="--features pdf"; fi; \
    if [ "$TARGETARCH" = "amd64" ]; then export RUSTFLAGS="-C target-cpu=x86-64-v2"; fi \
    && cd /src/rustvideoplatform-processor && cargo build --release $FEATURES

FROM alpine:edge

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

COPY --from=builder /src/rustvideoplatform-processor/target/release/rustvideoplatform-processor /opt/rustvideoplatform-processor

ARG TARGETARCH
RUN apk add --no-cache ffmpeg libva libva-utils mesa-dri-gallium mesa-va-gallium intel-media-driver onevpl-intel-gpu; \
    PDFIUM_ARCH=""; \
    case "$TARGETARCH" in \
        amd64) PDFIUM_ARCH="x64" ;; \
        arm64) PDFIUM_ARCH="arm64" ;; \
    esac; \
    if [ -n "$PDFIUM_ARCH" ]; then \
        wget -q "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-musl-${PDFIUM_ARCH}.tgz" -O /tmp/pdfium.tgz \
        && mkdir -p /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium \
        && cp /tmp/pdfium/lib/libpdfium.so /usr/lib/ \
        && rm -rf /tmp/pdfium /tmp/pdfium.tgz; \
    fi

EXPOSE 8080
STOPSIGNAL SIGTERM

ENTRYPOINT ["/opt/rustvideoplatform-processor"]
