FROM rust:1-slim-bookworm

ARG HERDR_VERSION=0.9.3

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl lsof procps python3 \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/herdrdev/herdr/releases/download/v${HERDR_VERSION}/herdr-linux-$(uname -m)" \
 && chmod +x /usr/local/bin/herdr

# Units spawn through ~/.local/bin/mise, which is where this installer puts it.
RUN curl -fsSL https://mise.run | sh

ENV HERDR_STACK_IN_CONTAINER=1

WORKDIR /src
COPY . .
RUN cargo build --release --locked && cargo test --locked --features herdr-tests --test herdr --no-run

CMD ["cargo", "test", "--locked", "--features", "herdr-tests", "--test", "herdr"]
