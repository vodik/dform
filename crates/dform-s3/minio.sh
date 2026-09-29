#!/bin/sh
# MinIO in rootless podman for tests/s3.rs's real run.
#
#   eval "$(crates/dform-s3/minio.sh start)"   # prints the DFORM_S3_* exports
#   cargo test --test s3
#   crates/dform-s3/minio.sh stop              # the container and its volume
#
# The image is quay.io/minio/minio, else (it is no longer public) Bitnami's
# archived build of MinIO, docker.io/bitnamilegacy/minio; DFORM_S3_TEST_IMAGE
# picks another. The port is published on 127.0.0.1:9000 (MinIO listens on
# the container's every address, where the published port arrives), or,
# when podman cannot publish one (rootless networking needs /dev/net/tun),
# the container shares the host's network and MinIO listens on
# 127.0.0.1:9000 only. The data is a podman volume, dform-minio-data.
set -eu

name=dform-minio
volume=dform-minio-data
user=dformtest
password=dformtest-secret

start() {
    podman rm -f "$name" >/dev/null 2>&1 || true
    image=${DFORM_S3_TEST_IMAGE:-}
    if [ -z "$image" ]; then
        for i in quay.io/minio/minio docker.io/bitnamilegacy/minio; do
            if podman image exists "$i" || podman pull -q "$i" >/dev/null 2>&1; then
                image=$i
                break
            fi
        done
    fi
    [ -n "$image" ] || { echo "minio.sh: no MinIO image could be pulled" >&2; exit 1; }
    binary=$(podman run --rm --network=none --entrypoint sh "$image" -c 'command -v minio')
    # run ADDRESS PODMAN-ARGS...
    run() {
        address=$1
        shift
        podman run -d --name "$name" --user 0:0 -v "$volume:/data" \
            -e MINIO_ROOT_USER="$user" -e MINIO_ROOT_PASSWORD="$password" \
            --entrypoint "$binary" "$@" "$image" \
            server /data --address "$address:9000" --console-address "$address:9001" >/dev/null
    }
    if ! run "" -p 127.0.0.1:9000:9000 2>/dev/null; then
        podman rm -f "$name" >/dev/null 2>&1 || true
        run 127.0.0.1 --network=host
    fi
    for _ in $(seq 100); do
        if curl -fs http://127.0.0.1:9000/minio/health/ready >/dev/null 2>&1; then
            echo "export DFORM_S3_TEST_ENDPOINT=http://127.0.0.1:9000"
            echo "export DFORM_S3_ACCESS_KEY_ID=$user"
            echo "export DFORM_S3_SECRET_ACCESS_KEY=$password"
            echo "# minio.sh: $image" >&2
            return
        fi
        sleep 0.2
    done
    podman logs "$name" >&2
    echo "minio.sh: MinIO did not come up" >&2
    exit 1
}

stop() {
    podman rm -f "$name" >/dev/null 2>&1 || true
    podman volume rm -f "$volume" >/dev/null 2>&1 || true
}

case "${1:-}" in
start) start ;;
stop) stop ;;
*) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
