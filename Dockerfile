FROM alpine:latest AS builder

RUN apk add --no-cache cargo musl-dev openssl-dev pkgconfig ffmpeg-dev clang19-dev

RUN mkdir /src
COPY ./ /src/rustvideoplatform-processor

ENV RUSTFLAGS="-C target-cpu=x86-64-v2"
RUN cd /src/rustvideoplatform-processor && cargo build --release


FROM alpine:latest
COPY --from=builder /src/rustvideoplatform-processor/target/release/rustvideoplatform-processor /opt/rustvideoplatform-processor

RUN apk add --no-cache ffmpeg libva libva-utils mesa-dri-gallium mesa-va-gallium intel-media-driver

EXPOSE 8080
STOPSIGNAL SIGTERM

ENTRYPOINT ["/opt/rustvideoplatform-processor"]