# Runtime image for the SQL runner. DuckDB is bundled in the kako binary.
# package.sh compiles that binary on the GitHub runner and copies it in.
# This base must ship a glibc at least as new as the runner's, or the
# image will not start. package.sh runs the image with --version before
# saving it.
FROM debian:13-slim
COPY kako /usr/local/bin/kako
ENTRYPOINT ["/usr/local/bin/kako"]
