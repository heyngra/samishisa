# Builds samishisa from source into a small image. Releases also publish a ready-made image: ghcr.io/heyngra/samishisa.
# The build compiles wasmtime, so expect several minutes and about 2 GB of RAM.
#
#   docker build -t samishisa .
#   docker run -p 3000:3000 -v ./config:/config -v samishisa-data:/data samishisa
FROM rust:1-bookworm AS build
RUN rustup target add wasm32-wasip2
WORKDIR /src
COPY . .
RUN cargo build --release --locked --target wasm32-wasip2 -p hello \
 && SAMISHISA_HELLO_WASM=/src/target/wasm32-wasip2/release/hello.wasm cargo build --release --locked -p samishisa-server \
 && mkdir -p /out/config /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/samishisa /usr/local/bin/samishisa
COPY --from=build --chown=65532:65532 /out/config /config
COPY --from=build --chown=65532:65532 /out/data /data
WORKDIR /config
ENV SAMISHISA_DATA=/data
VOLUME ["/config", "/data"]
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["samishisa", "healthcheck"]
ENTRYPOINT ["samishisa"]
CMD ["run"]
