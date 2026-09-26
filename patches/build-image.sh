#!/usr/bin/env bash
# Build the patched fork, layer its server binary onto the matching official
# Remux image (pinned by digest), and import that image on every cluster node.
# The image lives in no registry, so every node needs it for IfNotPresent.
# Roll out through GitOps afterwards.
#
# Usage: patches/build-image.sh UPSTREAM_VERSION [PATCH_REVISION]
# Example: patches/build-image.sh 0.34.0 1
#
# Rebase this branch onto the matching upstream tag before each Remux upgrade.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:?usage: patches/build-image.sh UPSTREAM_VERSION [PATCH_REVISION]}"
PATCH_REVISION="${2:-1}"
UPSTREAM_TAG="v${VERSION}"
IMAGE="ai13f/remux-patched:${VERSION}-patched.${PATCH_REVISION}"
CONTROL="abm-admin@192.168.1.5"
SSH=(ssh -o BatchMode=yes -i "$HOME/.ssh/k8s-home")
KUBECTL="sudo KUBECONFIG=/etc/kubernetes/admin.conf kubectl"

NODES_JSON=$("${SSH[@]}" "$CONTROL" "$KUBECTL get nodes -o json")
TARGET_ARCH=$(jq -r \
  '[.items[].status.nodeInfo.architecture] | unique | if length == 1 then .[0] else empty end' \
  <<<"$NODES_JSON")
NODE_IPS=$(jq -r \
  '.items[].status.addresses[] | select(.type == "InternalIP") | .address' \
  <<<"$NODES_JSON")
case "$TARGET_ARCH" in
  amd64) TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  arm64) TARGET_TRIPLE="aarch64-unknown-linux-gnu" ;;
  *)
    echo "error: cluster nodes must share a supported architecture" >&2
    exit 1
    ;;
esac
if [[ -z "$NODE_IPS" ]]; then
  echo "error: no Kubernetes node IPs found" >&2
  exit 1
fi

BASE_IMAGE="ghcr.io/lostb1t/remux:${VERSION}@$(
  docker buildx imagetools inspect "ghcr.io/lostb1t/remux:${VERSION}" \
    --format '{{json .Manifest}}' | jq -r .digest
)"
echo "==> base image ${BASE_IMAGE}"

git rev-parse --verify "${UPSTREAM_TAG}^{commit}" >/dev/null
if ! git merge-base --is-ancestor "$UPSTREAM_TAG" HEAD; then
  echo "error: this branch is not based on ${UPSTREAM_TAG}" >&2
  exit 1
fi

echo "==> compiling remux-server ${VERSION} for linux/${TARGET_ARCH}"
mkdir -p .build/image
docker run --rm --platform "linux/${TARGET_ARCH}" \
  -e "VERSION=$VERSION" \
  -e "TARGET_TRIPLE=$TARGET_TRIPLE" \
  -e DEBIAN_FRONTEND=noninteractive \
  -v "$PWD":/src:ro \
  -v "$PWD/.build/image":/out \
  -v remux-cargo-registry:/usr/local/cargo/registry \
  -v "remux-cargo-target-${TARGET_ARCH}:/work/target" \
  rust:1-trixie \
  sh -eu -c '
    apt-get update -qq >/dev/null
    apt-get install -y -qq pkg-config libssl-dev protobuf-compiler >/dev/null
    tar --exclude=.git --exclude=target --exclude=.build -C /src -cf - . |
      tar -C /work -xf -
    sed -i "0,/^version = \"0.0.0\"/s//version = \"${VERSION}\"/" \
      /work/crates/remux-server/Cargo.toml
    cd /work
    cargo build --release --target "${TARGET_TRIPLE}" -p remux-server
    cp "target/${TARGET_TRIPLE}/release/remux-server" /out/remux-server
  '

BINARY_DESCRIPTION=$(file .build/image/remux-server)
case "${TARGET_ARCH}:${BINARY_DESCRIPTION}" in
  amd64:*x86-64* | arm64:*ARM\ aarch64*) ;;
  *)
    echo "error: compiled binary does not match cluster architecture: ${BINARY_DESCRIPTION}" >&2
    exit 1
    ;;
esac

echo "==> layering binary onto ${BASE_IMAGE}"
sed "s|BASE_IMAGE|${BASE_IMAGE}|" patches/Dockerfile.patched > .build/image/Dockerfile
docker buildx build --platform "linux/${TARGET_ARCH}" --load -t "$IMAGE" .build/image

echo "==> exporting ${IMAGE}"
docker save "$IMAGE" | gzip -1 > .build/remux-image.tar.gz

echo "==> importing ${IMAGE} on every cluster node"

for ip in $NODE_IPS; do
  echo "==> importing image on ${ip}"
  "${SSH[@]}" "abm-admin@${ip}" \
    "gunzip | sudo ctr -n k8s.io images import -" < .build/remux-image.tar.gz
done

echo "==> ${IMAGE} is on every node; roll it out by setting it in the GitOps"
echo "    remux Deployment (ai13f/anthos-nuc-mgmt) and syncing Argo app remux."
