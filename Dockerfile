FROM rust:1.99-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends make binaryen && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY . .
# CARGO_BUILD_JOBS keeps the build from starving a Base node on the same machine.
ARG CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    make wasm && cargo build --locked --release -p plainscreener && cp target/release/plainscreener /plainscreener

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates fonts-inter fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /plainscreener /usr/local/bin/plainscreener
ENV DATA_DIR=/data LISTEN=0.0.0.0:8080
EXPOSE 8080
STOPSIGNAL SIGTERM
CMD ["plainscreener"]
