FROM docker.io/library/debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY target-podman-linux/release/ferrosa-memory-mcp /usr/local/bin/ferrosa-memory-mcp

ENV FERROSA_MEMORY_CONFIG=/run/secrets/ferrosa-memory/ferrosa-memory-http-podman.toml

EXPOSE 8765

CMD ["ferrosa-memory-mcp"]
