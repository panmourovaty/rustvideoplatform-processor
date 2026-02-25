FROM alpine:edge AS builder

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

RUN apk add --no-cache cargo musl-dev openssl-dev pkgconfig ffmpeg-dev clang21-dev

RUN mkdir /src
COPY ./ /src/rustvideoplatform-processor

ENV RUSTFLAGS="-C target-cpu=x86-64-v2"
RUN cd /src/rustvideoplatform-processor && cargo build --release

FROM alpine:edge

RUN echo "http://dl-cdn.alpinelinux.org/alpine/edge/testing" >> /etc/apk/repositories \
    && apk update

COPY --from=builder /src/rustvideoplatform-processor/target/release/rustvideoplatform-processor /opt/rustvideoplatform-processor

RUN apk add --no-cache ffmpeg libva libva-utils mesa-dri-gallium mesa-va-gallium intel-media-driver onevpl-intel-gpu \
    && wget -q "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-musl-x64.tgz" -O /tmp/pdfium.tgz \
    && mkdir -p /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium \
    && cp /tmp/pdfium/lib/libpdfium.so /usr/lib/ \
    && rm -rf /tmp/pdfium /tmp/pdfium.tgz

EXPOSE 8080
STOPSIGNAL SIGTERM

ENTRYPOINT ["/opt/rustvideoplatform-processor"]
