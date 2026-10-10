#!/usr/bin/env bash
# Build release binaries for Linux (amd64, arm64, armv7) and Windows (amd64) into ./dist,
# cross-compiled in Docker with cargo-zigbuild. Needs only Docker with buildx.
set -euo pipefail
cd "$(dirname "$0")/.."
docker buildx build --target dist --output type=local,dest=dist .
ls -l dist
