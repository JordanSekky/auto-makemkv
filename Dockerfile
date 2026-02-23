FROM rust:bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

FROM automaticrippingmachine/arm-dependencies AS runtime
COPY --from=builder /app/target/release/auto-makemkv /usr/local/bin/
ENTRYPOINT ["auto-makemkv"]
