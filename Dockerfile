# syntax=docker/dockerfile:1
FROM rust:1.97-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/steam-mcp /usr/local/bin/steam-mcp
ENV STEAM_MCP_HTTP_BIND=0.0.0.0:8080 \
    STEAM_MCP_CACHE_FILE=/var/cache/steam-mcp/cache.jsonl \
    STEAM_MCP_LOG_FORMAT=json
VOLUME ["/var/cache/steam-mcp"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/steam-mcp"]
CMD ["--http", "0.0.0.0:8080"]
