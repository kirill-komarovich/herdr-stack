FROM rust:1-slim-bookworm

ARG HERDR_VERSION=0.9.3

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl direnv lsof procps \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/herdrdev/herdr/releases/download/v${HERDR_VERSION}/herdr-linux-$(uname -m)" \
 && chmod +x /usr/local/bin/herdr

# The suite runs units under real mise and direnv; direnv comes from apt above.
RUN curl -fsSL https://mise.run | sh

ENV HERDR_STACK_IN_CONTAINER=1

WORKDIR /src
COPY . .
RUN cargo build --release --locked && cargo test --locked --features herdr-tests --test herdr --no-run

CMD ["cargo", "test", "--locked", "--features", "herdr-tests", "--test", "herdr"]
