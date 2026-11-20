# ct-mock: the Connect Trade mock as an image. Build from the repository root:
#   docker build -f crates/ct-mock/Dockerfile -t ct-mock .
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p ct-mock

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/ct-mock /ct-mock
ENV PORT=8098
EXPOSE 8098
ENTRYPOINT ["/ct-mock"]
