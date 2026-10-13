#!/bin/sh
# Usage: tests/container/run.sh [herdr-version...]
set -eu

cd "$(dirname "$0")/../.."

[ $# -gt 0 ] || set -- 0.9.3

for version in "$@"; do
  image=herdr-stack-container-tests:$version
  docker build -q -t "$image" --build-arg HERDR_VERSION="$version" -f tests/container/Dockerfile . >/dev/null
  echo "herdr $version"
  docker run --rm "$image"
done
