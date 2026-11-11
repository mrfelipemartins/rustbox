FROM rust:1.99-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY openapi.json ./
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/app/target cargo build --release --locked && cp /app/target/release/rustbox /app/rustbox
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 rustbox
COPY --from=build /app/rustbox /usr/local/bin/rustbox
USER rustbox
EXPOSE 8080
ENTRYPOINT ["rustbox"]
