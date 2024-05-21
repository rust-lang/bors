FROM rust:1.76 as base

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

RUN cargo install cargo-chef

FROM base as planner

WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM base as build

WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml .
COPY Cargo.lock .
COPY migrations migrations
COPY .sqlx .sqlx
COPY src src

RUN cargo build --release

FROM ubuntu:22.04 as runtime

WORKDIR /

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y ca-certificates

COPY --from=build /app/target/release/bors .

EXPOSE 80

ENTRYPOINT ["./bors"]
