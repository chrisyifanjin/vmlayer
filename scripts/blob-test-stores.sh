#!/usr/bin/env bash
# Throwaway S3 (MinIO) and Azure Blob (Azurite) endpoints for the blob store
# integration tests. Prints the environment the tests read.
#
#   eval "$(scripts/blob-test-stores.sh start)"
#   cargo test -p vmlayer-blob --test remote
#   scripts/blob-test-stores.sh stop
#
# S3_PORT / AZURITE_PORT pick host ports (defaults 9000 / 10000).
set -euo pipefail
S3_PORT="${S3_PORT:-9000}"
AZURITE_PORT="${AZURITE_PORT:-10000}"
MINIO_IMAGE="${MINIO_IMAGE:-cgr.dev/chainguard/minio:latest}"
AZURITE_IMAGE="${AZURITE_IMAGE:-mcr.microsoft.com/azure-storage/azurite:3.35.0}"
# Test-only credentials for the local MinIO container.
S3_KEY=vmlayer-test
S3_SECRET=vmlayer-test-secret
# Azurite's built-in development account; Microsoft publishes this key.
AZ_ACCOUNT=devstoreaccount1
AZ_KEY='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='

stop() {
  docker rm -f vmlayer-test-minio vmlayer-test-azurite >/dev/null 2>&1 || true
}

wait_http() {
  for _ in $(seq 1 60); do
    curl -s -o /dev/null "$1" && return 0
    sleep 0.5
  done
  echo "timed out waiting for $1" >&2
  exit 1
}

start() {
  stop
  docker pull -q "$MINIO_IMAGE" >/dev/null
  docker pull -q "$AZURITE_IMAGE" >/dev/null
  docker run -d --name vmlayer-test-minio -p "127.0.0.1:$S3_PORT:9000" \
    -e MINIO_ROOT_USER="$S3_KEY" -e MINIO_ROOT_PASSWORD="$S3_SECRET" \
    "$MINIO_IMAGE" server /data >/dev/null
  docker run -d --name vmlayer-test-azurite -p "127.0.0.1:$AZURITE_PORT:10000" \
    "$AZURITE_IMAGE" azurite-blob --blobHost 0.0.0.0 --skipApiVersionCheck >/dev/null
  wait_http "http://127.0.0.1:$S3_PORT/minio/health/live"
  wait_http "http://127.0.0.1:$AZURITE_PORT/"
  uv run --quiet --no-project --with boto3 --with azure-storage-blob python - <<PY >&2
import boto3
from azure.storage.blob import BlobServiceClient
boto3.client("s3", endpoint_url="http://127.0.0.1:$S3_PORT", region_name="us-east-1",
             aws_access_key_id="$S3_KEY", aws_secret_access_key="$S3_SECRET").create_bucket(Bucket="vmlayer-test")
conn = ("DefaultEndpointsProtocol=http;AccountName=$AZ_ACCOUNT;AccountKey=$AZ_KEY;"
        "BlobEndpoint=http://127.0.0.1:$AZURITE_PORT/$AZ_ACCOUNT;")
BlobServiceClient.from_connection_string(conn).create_container("vmlayer-test")
print("created bucket and container vmlayer-test")
PY
  cat <<ENV
export VMLAYER_TEST_S3_URL=s3://vmlayer-test/ci
export AWS_ENDPOINT=http://127.0.0.1:$S3_PORT
export AWS_ALLOW_HTTP=true
export AWS_REGION=us-east-1
export AWS_ACCESS_KEY_ID=$S3_KEY
export AWS_SECRET_ACCESS_KEY=$S3_SECRET
export VMLAYER_TEST_AZURE_URL=az://vmlayer-test/ci
export AZURE_STORAGE_USE_EMULATOR=true
export AZURITE_BLOB_STORAGE_URL=http://127.0.0.1:$AZURITE_PORT
ENV
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
