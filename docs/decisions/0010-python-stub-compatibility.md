# 0010. Python stubs: generated with an old pinned toolchain, version pin stripped

Status: accepted, 2026-11-15

## Context

The Python SDK ships checked-in protobuf/gRPC stubs so users never need
`protoc`. Generated `_pb2.py` files call
`ValidateProtobufRuntimeVersion(major, minor, patch)`, which refuses to
import when the installed `protobuf` is older than the generator, or more
than one major version newer.

Plugging the SDK into AutoGen exposed the cost: `autogen-core` pins
`protobuf~=5.29`, our stubs had been generated with the newest tools
(gencode 7.35), and `import vmlayer` failed in any environment that also had
AutoGen. Agent frameworks pin protobuf to different majors, and an
infrastructure SDK that dictates the major is one nobody can install next to
them. No single gencode version passes the check on protobuf 5, 6 and 7.

## Options

1. Generate with the newest tools and require `protobuf>=7`. Breaks
   coexistence with AutoGen today and with whichever framework lags next.
2. Drop gencode: load a serialized descriptor set at import time and build
   message classes with `message_factory`. Fully decoupled, but every
   message type becomes dynamic and the `.pyi` stubs go away.
3. Generate with the **oldest** toolchain we support and strip the version
   check. The generated code only registers a serialized descriptor and
   builds classes through `google.protobuf.internal.builder`, an API that is
   stable across these majors; our `.proto` files use only plain proto3
   (maps, oneofs, `optional`, well-known types).

## Decision

Option 3. `scripts/gen-proto.sh` pins `grpcio-tools==1.68.1` (protobuf 5.28
gencode) and removes the runtime-version import and call from each
`_pb2.py`, asserting it removed exactly those two things. The SDK declares
`protobuf>=5.26` and `grpcio>=1.68`. CI regenerates and fails on any diff,
and imports the SDK under protobuf 5.26, 6.x and 7.x.

## Consequences

- One wheel works next to frameworks on protobuf 5, 6 or 7.
- We give up protobuf's own guard, so the compatibility matrix is ours to
  test: the CI import check is not optional. If a future `.proto` needs a
  feature newer than the pinned generator (editions), revisit option 2.
- The gRPC stubs keep their own check (`grpcio>=1.68`), which is a floor
  only and conflicts with nothing surveyed.
