# rdesk-server (the private server: accounts, sign-in, relay). See docs/SERVER.md.
#
#   docker build -t rdesk-server .
#   docker compose -f deploy/docker-compose.yml up -d

FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rdesk-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rdesk-server /usr/local/bin/rdesk-server
ENV RDESK_SERVER_DATA=/data
VOLUME /data
# HTTPS (sign-in, API, relay) and the relay's QUIC address discovery.
EXPOSE 443/tcp 7842/udp
ENTRYPOINT ["rdesk-server"]
CMD ["run"]
