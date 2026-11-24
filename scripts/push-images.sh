#!/usr/bin/env bash
# Pushes the images of scripts/build-images.sh from this machine to a k3d registry, and prints the
# IMAGE_PREFIX that the cluster pulls them by:
#   scripts/push-images.sh <k3d registry> <prefix> <tag> <pushed tag>
# <k3d registry> is the registry container that `k3d cluster create --registry-create <name>`
# starts, k3d-<name>. Give each run its own <pushed tag>: the chart's pull policy is IfNotPresent.
set -euo pipefail

registry="${1:?usage: push-images.sh <k3d registry> <prefix> <tag> <pushed tag>}"
prefix="${2:?usage: push-images.sh <k3d registry> <prefix> <tag> <pushed tag>}"
tag="${3:?usage: push-images.sh <k3d registry> <prefix> <tag> <pushed tag>}"
pushed="${4:?usage: push-images.sh <k3d registry> <prefix> <tag> <pushed tag>}"

# The registry listens on port 5000 in the cluster and on a port of the host's loopback.
# `docker port` prints 0.0.0.0:<port>, one line per address.
ports=$(docker port "$registry" 5000/tcp)
host_port="${ports%%$'\n'*}"
host_port="${host_port##*:}"
[[ -n $host_port ]] || { echo "$registry has no host port" >&2; exit 1; }
for name in builder builder-job builder-railpack; do
  target="$prefix/$name:$pushed"
  docker tag "$prefix/$name:$tag" "localhost:$host_port/$target" >&2
  docker push --quiet "localhost:$host_port/$target" >&2
done
echo "$registry:5000/$prefix"
