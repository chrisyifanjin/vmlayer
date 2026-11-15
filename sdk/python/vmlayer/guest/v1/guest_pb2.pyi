import datetime

from google.protobuf import duration_pb2 as _duration_pb2
from vmlayer.v1 import common_pb2 as _common_pb2
from vmlayer.v1 import processes_pb2 as _processes_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ReadyResponse(_message.Message):
    __slots__ = ("agent_version", "kernel", "uptime")
    AGENT_VERSION_FIELD_NUMBER: _ClassVar[int]
    KERNEL_FIELD_NUMBER: _ClassVar[int]
    UPTIME_FIELD_NUMBER: _ClassVar[int]
    agent_version: str
    kernel: str
    uptime: float
    def __init__(self, agent_version: _Optional[str] = ..., kernel: _Optional[str] = ..., uptime: _Optional[float] = ...) -> None: ...

class MountRequest(_message.Message):
    __slots__ = ("source", "path", "fstype", "read_only")
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    FSTYPE_FIELD_NUMBER: _ClassVar[int]
    READ_ONLY_FIELD_NUMBER: _ClassVar[int]
    source: str
    path: str
    fstype: str
    read_only: bool
    def __init__(self, source: _Optional[str] = ..., path: _Optional[str] = ..., fstype: _Optional[str] = ..., read_only: _Optional[bool] = ...) -> None: ...

class UnmountRequest(_message.Message):
    __slots__ = ("path",)
    PATH_FIELD_NUMBER: _ClassVar[int]
    path: str
    def __init__(self, path: _Optional[str] = ...) -> None: ...

class EntrypointRequest(_message.Message):
    __slots__ = ("argv", "cwd", "env")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    ARGV_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    argv: _containers.RepeatedScalarFieldContainer[str]
    cwd: str
    env: _containers.ScalarMap[str, str]
    def __init__(self, argv: _Optional[_Iterable[str]] = ..., cwd: _Optional[str] = ..., env: _Optional[_Mapping[str, str]] = ...) -> None: ...

class WaitForPortRequest(_message.Message):
    __slots__ = ("port", "timeout")
    PORT_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    port: int
    timeout: _duration_pb2.Duration
    def __init__(self, port: _Optional[int] = ..., timeout: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...
