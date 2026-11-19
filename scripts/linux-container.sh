#!/usr/bin/env bash
# Build, test and smoke devq on Linux with Apple's `container` CLI (macOS 26+,
# `brew install container`). Source is mounted read-only; builds happen in a copy.
# The image is pinned by version AND digest; bump both together.
set -eu
cd "$(dirname "$0")/.."
IMAGE="docker.io/library/rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073"
container run --rm -c 4 -m 4G -v "$PWD":/src:ro "$IMAGE" bash -c '
  set -eu
  mkdir /b && cd /src && tar --exclude=./target --exclude=./.git -cf - . | tar -xf - -C /b && cd /b
  RUSTFLAGS="-D warnings" cargo build --locked --release
  RUSTFLAGS="-D warnings" cargo test --locked
  apt-get update -qq >/dev/null
  apt-get install -y -qq curl procps >/dev/null
  scripts/smoke-linux.sh target/release/devq
'
