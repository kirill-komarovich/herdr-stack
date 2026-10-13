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

# The docker CLI and its compose plugin, with no daemon: the suite only asks them about an unreachable
# one.
ARG DOCKER_VERSION=29.9.0
ARG COMPOSE_VERSION=5.6.0
RUN curl -fsSL "https://download.docker.com/linux/static/stable/$(uname -m)/docker-${DOCKER_VERSION}.tgz" \
      | tar -xz -C /usr/local/bin --strip-components=1 docker/docker \
 && mkdir -p /usr/local/lib/docker/cli-plugins \
 && curl -fsSL -o /usr/local/lib/docker/cli-plugins/docker-compose \
      "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-$(uname -m)" \
 && chmod +x /usr/local/lib/docker/cli-plugins/docker-compose

ENV HERDR_STACK_IN_CONTAINER=1

WORKDIR /src
COPY . .
RUN cargo build --release --locked && cargo test --locked --features herdr-tests --test herdr --no-run

CMD ["cargo", "test", "--locked", "--features", "herdr-tests", "--test", "herdr"]
