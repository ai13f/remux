#!/usr/bin/env bash
# Build a Remux image with the patch(es) in this directory applied, and deploy
# it to the home Kubernetes cluster by layering the patched binary on top of
# the official image (web UI, ffmpeg and env stay stock).
#
# Usage:  patches/build-deploy.sh [UPSTREAM_VERSION]
#         e.g. patches/build-deploy.sh 0.27.0   (default: version from Cargo.toml)
#
# Update flow for a new Remux release:
#   1. git fetch upstream && git checkout vX.Y.Z
#   2. patches/build-deploy.sh 0.X.Y      (applies patches, builds, ships)
#   3. If a patch fails to apply, rebase it onto the new tag and regenerate
#      the patch file: git diff > patches/0001-...patch
set -euo pipefail
cd "$(dirname "$0")/.."

NODE="abm-admin@192.168.1.5"
SSH="ssh -i $HOME/.ssh/k8s-home"
KUBECTL="sudo KUBECONFIG=/etc/kubernetes/admin.conf kubectl"

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
BASE_IMAGE="ghcr.io/lostb1t/remux:${VERSION}"
TAG="ai13f/remux-patched:${VERSION}-autoplay-fix"
FIX="1"

echo "==> version: ${VERSION}"
echo "==> base:    ${BASE_IMAGE}"
echo "==> target:  ${TAG}"

echo "==> applying patches"
git apply --check patches/*.patch
git apply patches/*.patch
restore() { git checkout -- crates/; }
trap restore EXIT

echo "==> compiling remux-server (linux/arm64, cached cargo home)"
docker run --rm \
  -v "$PWD":/src -w /src \
  -v remux-cargo-registry:/usr/local/cargo/registry \
  -v remux-cargo-target:/src/target \
  rust:1-trixie \
  sh -c 'apt-get update -qq >/dev/null && apt-get install -y -qq pkg-config libssl-dev protobuf-compiler >/dev/null && cargo build --release -p remux-server'

echo "==> layering patched binary onto official image"
mkdir -p .build
cp target/release/remux-server .build/remux-server
sed "s|BASE_IMAGE|${BASE_IMAGE}|" patches/Dockerfile.patched > .build/Dockerfile
trap 'restore; rm -rf .build' EXIT
docker buildx build --platform linux/arm64 --load -t "$TAG" .build

echo "==> importing image into cluster node"
docker save "$TAG" | gzip | $SSH "$NODE" "gunzip | sudo ctr -n k8s.io images import -"

echo "==> deploying"
$SSH "$NODE" "$KUBECTL -n remux set image deploy/remux remux=${TAG}"
$SSH "$NODE" "$KUBECTL -n remux patch deploy/remux --type=json -p='[{\"op\":\"replace\",\"path\":\"/spec/template/spec/containers/0/imagePullPolicy\",\"value\":\"IfNotPresent\"}]'"
$SSH "$NODE" "$KUBECTL -n remux rollout status deploy/remux --timeout=300s"

echo "==> done: ${TAG} is live"
