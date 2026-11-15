#!/usr/bin/env sh
# Regenerate the Python gRPC stubs from proto/. Run from anywhere.
#
# The stubs are generated with a deliberately OLD, pinned grpcio-tools and
# the protobuf "runtime must be >= gencode" check is stripped, so one set of
# checked-in stubs imports under protobuf 5.x through 7.x and grpcio >= 1.68.
# Agent frameworks pin protobuf to different majors (AutoGen: 5.x); see
# docs/decisions/0010-python-stub-compatibility.md.
set -eu
cd "$(dirname "$0")/.."
TOOLS="grpcio-tools==1.68.1"
cd sdk/python
uv run --no-project --python 3.12 --with "$TOOLS" python -m grpc_tools.protoc \
  -I ../../proto \
  --python_out=. --grpc_python_out=. --pyi_out=. \
  ../../proto/vmlayer/v1/common.proto \
  ../../proto/vmlayer/v1/sandboxes.proto \
  ../../proto/vmlayer/v1/processes.proto \
  ../../proto/vmlayer/v1/files.proto \
  ../../proto/vmlayer/v1/volumes.proto \
  ../../proto/vmlayer/v1/system.proto \
  ../../proto/vmlayer/guest/v1/guest.proto
touch vmlayer/v1/__init__.py vmlayer/guest/__init__.py vmlayer/guest/v1/__init__.py
# Strip the gencode/runtime version pin from every *_pb2.py.
uv run --no-project --python 3.12 python - <<'PY'
import pathlib, re
pattern = re.compile(
    r"^from google\.protobuf import runtime_version as _runtime_version\n|"
    r"^_runtime_version\.ValidateProtobufRuntimeVersion\(\n(?:.*\n)*?\)\n",
    re.M,
)
for path in sorted(pathlib.Path("vmlayer").rglob("*_pb2.py")):
    text = path.read_text()
    new, n = pattern.subn("", text)
    assert n == 2, f"{path}: expected import + call, removed {n}"
    path.write_text(new)
PY
echo "generated into sdk/python/vmlayer/{v1,guest/v1} with $TOOLS"
