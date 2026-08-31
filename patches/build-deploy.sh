#!/usr/bin/env bash
# Build the patched fork, layer its server binary onto the matching official
# Remux image, import that image on every cluster node, and update the live
# Deployment.
#
# Usage: patches/build-deploy.sh UPSTREAM_VERSION [PATCH_REVISION]
# Example: patches/build-deploy.sh 0.27.0 1
# BASE_IMAGE defaults to the live image when it is the matching official digest.
# Set BASE_IMAGE explicitly when upgrading from an already patched deployment.
#
# Rebase this branch onto the matching upstream tag before each Remux upgrade.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:?usage: patches/build-deploy.sh UPSTREAM_VERSION [PATCH_REVISION]}"
PATCH_REVISION="${2:-1}"
UPSTREAM_TAG="v${VERSION}"
IMAGE="ai13f/remux-patched:${VERSION}-autoplay-groups.${PATCH_REVISION}"
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

PREVIOUS_IMAGE=$("${SSH[@]}" "$CONTROL" "$KUBECTL -n remux get deploy remux -o json" |
  jq -r '.spec.template.spec.containers[] | select(.name == "remux") | .image')
BASE_IMAGE="${BASE_IMAGE:-$PREVIOUS_IMAGE}"
if [[ "$BASE_IMAGE" != "ghcr.io/lostb1t/remux:${VERSION}@sha256:"* ]]; then
  echo "error: set BASE_IMAGE to the digest-pinned official ${VERSION} image" >&2
  exit 1
fi

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

echo "==> updating deployment/remux"
"${SSH[@]}" "$CONTROL" \
  "$KUBECTL -n remux set image deployment/remux remux=${IMAGE}"
if ! "${SSH[@]}" "$CONTROL" \
  "$KUBECTL -n remux rollout status deployment/remux --timeout=300s"; then
  echo "==> rollout failed; restoring ${PREVIOUS_IMAGE}" >&2
  "${SSH[@]}" "$CONTROL" \
    "$KUBECTL -n remux set image deployment/remux remux=${PREVIOUS_IMAGE}"
  "${SSH[@]}" "$CONTROL" \
    "$KUBECTL -n remux rollout status deployment/remux --timeout=300s"
  exit 1
fi

echo "==> live image: ${IMAGE}"
echo "==> rollback image: ${PREVIOUS_IMAGE}"
