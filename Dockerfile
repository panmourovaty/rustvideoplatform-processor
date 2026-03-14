FROM alpine:edge AS builder

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

RUN apk add --no-cache cargo musl-dev openssl-dev pkgconfig ffmpeg-dev clang21-dev

RUN mkdir /src
COPY ./ /src/rustvideoplatform-processor

ARG TARGETARCH
RUN case "$TARGETARCH" in \
        amd64)   export RUSTFLAGS="-C target-cpu=x86-64-v3" ;; \
        ppc64le) export RUSTFLAGS="-C target-cpu=pwr8" ;; \
    esac && \
    cd /src/rustvideoplatform-processor && cargo build --release

FROM alpine:edge

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

COPY --from=builder /src/rustvideoplatform-processor/target/release/rustvideoplatform-processor /opt/rustvideoplatform-processor

ARG TARGETARCH
RUN apk add --no-cache ffmpeg libva libva-utils mesa-dri-gallium mesa-va-gallium \
    && case "$TARGETARCH" in \
         amd64) \
           apk add --no-cache intel-media-driver onevpl-intel-gpu \
           && wget -q "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-musl-x64.tgz" -O /tmp/pdfium.tgz \
           && mkdir -p /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium \
           && cp /tmp/pdfium/lib/libpdfium.so /usr/lib/ \
           && rm -rf /tmp/pdfium /tmp/pdfium.tgz ;; \
         arm64) \
           wget -q "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-musl-arm64.tgz" -O /tmp/pdfium.tgz \
           && mkdir -p /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium \
           && cp /tmp/pdfium/lib/libpdfium.so /usr/lib/ \
           && rm -rf /tmp/pdfium /tmp/pdfium.tgz ;; \
       esac

EXPOSE 8080
STOPSIGNAL SIGTERM

ENTRYPOINT ["/opt/rustvideoplatform-processor"]
