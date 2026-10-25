# The archiver (crates/archiver): a static binary in an otherwise empty image. Built from the
# repository root (wrangler.toml's image_build_context), so the workspace and its Cargo.lock
# are in the build; .dockerignore keeps secrets and build output out of it.
FROM docker.io/library/rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p terminal-worker-archiver \
    && cp target/release/terminal-worker-archiver /archiver

FROM scratch
COPY --from=build /archiver /archiver
EXPOSE 8080
ENTRYPOINT ["/archiver"]
