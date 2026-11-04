FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN groupadd --gid 10001 ronnia \
    && useradd --uid 10001 --gid 10001 --no-create-home ronnia \
    && mkdir /data && chown ronnia:ronnia /data
COPY --from=build /src/target/release/ronnia /usr/local/bin/ronnia
USER 10001:10001
WORKDIR /data
ENV DATABASE_PATH=/data/ronnia.sqlite3 BIND_IP=0.0.0.0 BIND_PORT=8080
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/ronnia"]
CMD ["serve"]
