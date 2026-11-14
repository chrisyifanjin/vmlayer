#!/usr/bin/env sh
# Regenerate the Python gRPC stubs from proto/. Run from the repo root.
# The generated package path (vmlayer/v1) lands inside the SDK package, so
# the stubs' own imports (`from vmlayer.v1 import ...`) resolve unchanged.
set -eu
cd "$(dirname "$0")/.."
cd sdk/python
uv run --group dev python -m grpc_tools.protoc \
  -I ../../proto \
  --python_out=. --grpc_python_out=. --pyi_out=. \
  ../../proto/vmlayer/v1/common.proto \
  ../../proto/vmlayer/v1/sandboxes.proto \
  ../../proto/vmlayer/v1/processes.proto \
  ../../proto/vmlayer/v1/files.proto \
  ../../proto/vmlayer/v1/volumes.proto \
  ../../proto/vmlayer/v1/system.proto
touch vmlayer/v1/__init__.py
echo "generated into sdk/python/vmlayer/v1"
