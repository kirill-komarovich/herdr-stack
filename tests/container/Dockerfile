FROM rust:1-slim-bookworm

ARG HERDR_VERSION=0.9.3

RUN apt-get update \
 && apt-get install -y --no-install-recommends bash ca-certificates curl direnv fish lsof procps zsh \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/herdrdev/herdr/releases/download/v${HERDR_VERSION}/herdr-linux-$(uname -m)" \
 && chmod +x /usr/local/bin/herdr

# Units run in fish, zsh and bash with real mise and direnv hooks; the rest comes from apt above.
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh

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
RUN cargo build --release --locked && cargo test --locked --features container-tests --test herdr --test shells --no-run

CMD ["cargo", "test", "--locked", "--features", "container-tests", "--test", "herdr", "--test", "shells"]
