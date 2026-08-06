FROM rust:1.97.1-bookworm AS build

WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin cor-code

FROM debian:bookworm-slim

LABEL org.opencontainers.image.source="https://github.com/CorVous/CorCode"
LABEL org.opencontainers.image.description="CorCode core: the chat console and the container plane behind it."

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/cor-code /usr/local/bin/cor-code

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/cor-code"]
