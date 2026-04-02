# syntax=docker/dockerfile:1
FROM alpine:edge AS builder

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

RUN apk add --no-cache cargo musl-dev openssl-dev pkgconfig ffmpeg-dev clang21-dev

ARG TARGETARCH

# Pre-build dependencies (cached layer - only invalidated when Cargo.toml changes)
COPY Cargo.toml /src/rustvideoplatform-processor/
RUN mkdir -p /src/rustvideoplatform-processor/src && echo 'fn main() {}' > /src/rustvideoplatform-processor/src/main.rs
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/root/.cargo/registry \
    --mount=type=cache,id=rustvideoplatform-processor-target-${TARGETARCH},target=/src/rustvideoplatform-processor/target \
    case "$TARGETARCH" in \
        amd64)   export RUSTFLAGS="-C target-cpu=x86-64-v2"; FEATURES="--features pdf" ;; \
        ppc64le) export RUSTFLAGS="-C target-cpu=pwr8" ;; \
    esac && \
    cd /src/rustvideoplatform-processor && cargo build --release $FEATURES 2>/dev/null ; true

# Build actual project
COPY ./ /src/rustvideoplatform-processor
# Touch source files to ensure cargo detects changes over the dummy pre-build
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/root/.cargo/registry \
    --mount=type=cache,id=rustvideoplatform-processor-target-${TARGETARCH},target=/src/rustvideoplatform-processor/target \
    find /src/rustvideoplatform-processor/src -name '*.rs' -exec touch {} + && \
    case "$TARGETARCH" in \
        amd64)   export RUSTFLAGS="-C target-cpu=x86-64-v2"; FEATURES="--features pdf" ;; \
        ppc64le) export RUSTFLAGS="-C target-cpu=pwr8" ;; \
    esac && \
    cd /src/rustvideoplatform-processor && cargo build --release $FEATURES && \
    cp target/release/rustvideoplatform-processor /rustvideoplatform-processor

FROM alpine:edge

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

WORKDIR /app

COPY --from=builder /rustvideoplatform-processor /opt/rustvideoplatform-processor

ARG TARGETARCH
RUN apk add --no-cache \
        ffmpeg libva libva-utils libgcc blender py3-numpy \
        # Mesa OpenGL (libGL.so.1 / libOpenGL.so.0) — needed by EEVEE classic fallback
        mesa-gl \
        # Mesa Gallium DRI/VA drivers (video + raster)
        mesa-dri-gallium mesa-va-gallium \
        # Vulkan stack — EEVEE Next uses Vulkan for headless rendering
        vulkan-loader \
        mesa-vulkan-intel \   # Intel ANV (Gen9+)
        mesa-vulkan-ati \     # AMD RADV
        mesa-vulkan-swrast \  # Lavapipe: pure-CPU Vulkan, always available as fallback
        ; \
    # NVIDIA: proprietary drivers must be supplied by the host via the
    # NVIDIA Container Toolkit (--gpus flag / nvidia-container-runtime).
    # No additional packages required inside the image.
    case "$TARGETARCH" in \
        amd64) apk add --no-cache intel-media-driver onevpl-intel-gpu ;; \
    esac; \
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
