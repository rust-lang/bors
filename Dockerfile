FROM rust:1.88 AS base

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

RUN cargo install cargo-chef

FROM base AS planner

WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM base AS build

WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY rust-toolchain.toml .
COPY Cargo.toml .
COPY Cargo.lock .
COPY migrations migrations
COPY .sqlx .sqlx
COPY src src
COPY templates templates

RUN cargo build --release

FROM ubuntu:22.04 AS runtime

WORKDIR /

# curl is needed for healthcheck
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates curl

COPY --from=build /app/target/release/bors .

EXPOSE 80

HEALTHCHECK --timeout=10s --start-period=10s \
    CMD curl -f http://localhost/health || exit 1

ENTRYPOINT ["./bors"]
