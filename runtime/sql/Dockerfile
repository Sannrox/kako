# Runtime image for the SQL runner. DuckDB is bundled in the kako binary.
FROM debian:bookworm-slim
COPY kako /usr/local/bin/kako
ENTRYPOINT ["/usr/local/bin/kako"]
