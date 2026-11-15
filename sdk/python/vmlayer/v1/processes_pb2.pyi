from google.protobuf import duration_pb2 as _duration_pb2
from vmlayer.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Iterable as _Iterable, Mapping as _Mapping, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class PtySize(_message.Message):
    __slots__ = ("cols", "rows")
    COLS_FIELD_NUMBER: _ClassVar[int]
    ROWS_FIELD_NUMBER: _ClassVar[int]
    cols: int
    rows: int
    def __init__(self, cols: _Optional[int] = ..., rows: _Optional[int] = ...) -> None: ...

class ExecRequest(_message.Message):
    __slots__ = ("sandbox_id", "argv", "cwd", "env", "timeout", "stdin", "detach")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    STDIN_FIELD_NUMBER: _ClassVar[int]
    DETACH_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    cwd: str
    env: _containers.ScalarMap[str, str]
    timeout: _duration_pb2.Duration
    stdin: bytes
    detach: bool
    def __init__(self, sandbox_id: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., cwd: _Optional[str] = ..., env: _Optional[_Mapping[str, str]] = ..., timeout: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ..., stdin: _Optional[bytes] = ..., detach: bool = ...) -> None: ...

class ExecEvent(_message.Message):
    __slots__ = ("started", "output", "exited", "dropped_bytes")
    STARTED_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_FIELD_NUMBER: _ClassVar[int]
    EXITED_FIELD_NUMBER: _ClassVar[int]
    DROPPED_BYTES_FIELD_NUMBER: _ClassVar[int]
    started: Started
    output: _common_pb2.OutputChunk
    exited: _common_pb2.ExitStatus
    dropped_bytes: int
    def __init__(self, started: _Optional[_Union[Started, _Mapping]] = ..., output: _Optional[_Union[_common_pb2.OutputChunk, _Mapping]] = ..., exited: _Optional[_Union[_common_pb2.ExitStatus, _Mapping]] = ..., dropped_bytes: _Optional[int] = ...) -> None: ...

class Started(_message.Message):
    __slots__ = ("pid",)
    PID_FIELD_NUMBER: _ClassVar[int]
    pid: int
    def __init__(self, pid: _Optional[int] = ...) -> None: ...

class OutputRequest(_message.Message):
    __slots__ = ("sandbox_id", "pid")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    pid: int
    def __init__(self, sandbox_id: _Optional[str] = ..., pid: _Optional[int] = ...) -> None: ...

class StdinChunk(_message.Message):
    __slots__ = ("sandbox_id", "pid", "data")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    pid: int
    data: bytes
    def __init__(self, sandbox_id: _Optional[str] = ..., pid: _Optional[int] = ..., data: _Optional[bytes] = ...) -> None: ...

class WaitRequest(_message.Message):
    __slots__ = ("sandbox_id", "pid", "timeout")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    pid: int
    timeout: _duration_pb2.Duration
    def __init__(self, sandbox_id: _Optional[str] = ..., pid: _Optional[int] = ..., timeout: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ...) -> None: ...

class KillProcessRequest(_message.Message):
    __slots__ = ("sandbox_id", "pid", "signal")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    SIGNAL_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    pid: int
    signal: int
    def __init__(self, sandbox_id: _Optional[str] = ..., pid: _Optional[int] = ..., signal: _Optional[int] = ...) -> None: ...

class ListProcessesRequest(_message.Message):
    __slots__ = ("sandbox_id",)
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    def __init__(self, sandbox_id: _Optional[str] = ...) -> None: ...

class ListProcessesResponse(_message.Message):
    __slots__ = ("processes",)
    PROCESSES_FIELD_NUMBER: _ClassVar[int]
    processes: _containers.RepeatedCompositeFieldContainer[_common_pb2.ProcessInfo]
    def __init__(self, processes: _Optional[_Iterable[_Union[_common_pb2.ProcessInfo, _Mapping]]] = ...) -> None: ...

class PtyInput(_message.Message):
    __slots__ = ("open", "data", "resize")
    OPEN_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    RESIZE_FIELD_NUMBER: _ClassVar[int]
    open: PtyOpen
    data: bytes
    resize: PtySize
    def __init__(self, open: _Optional[_Union[PtyOpen, _Mapping]] = ..., data: _Optional[bytes] = ..., resize: _Optional[_Union[PtySize, _Mapping]] = ...) -> None: ...

class PtyOpen(_message.Message):
    __slots__ = ("sandbox_id", "argv", "cwd", "env", "size")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    SIZE_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    cwd: str
    env: _containers.ScalarMap[str, str]
    size: PtySize
    def __init__(self, sandbox_id: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., cwd: _Optional[str] = ..., env: _Optional[_Mapping[str, str]] = ..., size: _Optional[_Union[PtySize, _Mapping]] = ...) -> None: ...

class PtyOutput(_message.Message):
    __slots__ = ("started", "data", "exited")
    STARTED_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    EXITED_FIELD_NUMBER: _ClassVar[int]
    started: Started
    data: bytes
    exited: _common_pb2.ExitStatus
    def __init__(self, started: _Optional[_Union[Started, _Mapping]] = ..., data: _Optional[bytes] = ..., exited: _Optional[_Union[_common_pb2.ExitStatus, _Mapping]] = ...) -> None: ...
