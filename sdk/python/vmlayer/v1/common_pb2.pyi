from google.protobuf import timestamp_pb2 as _timestamp_pb2
from google.protobuf import duration_pb2 as _duration_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Iterable as _Iterable, Mapping as _Mapping, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class SandboxState(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SANDBOX_STATE_UNSPECIFIED: _ClassVar[SandboxState]
    SANDBOX_STATE_CREATING: _ClassVar[SandboxState]
    SANDBOX_STATE_RUNNING: _ClassVar[SandboxState]
    SANDBOX_STATE_PAUSED: _ClassVar[SandboxState]
    SANDBOX_STATE_STOPPED: _ClassVar[SandboxState]
    SANDBOX_STATE_FAILED: _ClassVar[SandboxState]

class NetworkMode(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    NETWORK_MODE_UNSPECIFIED: _ClassVar[NetworkMode]
    NETWORK_MODE_NONE: _ClassVar[NetworkMode]
    NETWORK_MODE_EGRESS: _ClassVar[NetworkMode]
    NETWORK_MODE_EGRESS_AND_EXPOSE: _ClassVar[NetworkMode]

class OutputStream(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    OUTPUT_STREAM_UNSPECIFIED: _ClassVar[OutputStream]
    OUTPUT_STREAM_STDOUT: _ClassVar[OutputStream]
    OUTPUT_STREAM_STDERR: _ClassVar[OutputStream]
SANDBOX_STATE_UNSPECIFIED: SandboxState
SANDBOX_STATE_CREATING: SandboxState
SANDBOX_STATE_RUNNING: SandboxState
SANDBOX_STATE_PAUSED: SandboxState
SANDBOX_STATE_STOPPED: SandboxState
SANDBOX_STATE_FAILED: SandboxState
NETWORK_MODE_UNSPECIFIED: NetworkMode
NETWORK_MODE_NONE: NetworkMode
NETWORK_MODE_EGRESS: NetworkMode
NETWORK_MODE_EGRESS_AND_EXPOSE: NetworkMode
OUTPUT_STREAM_UNSPECIFIED: OutputStream
OUTPUT_STREAM_STDOUT: OutputStream
OUTPUT_STREAM_STDERR: OutputStream

class Capabilities(_message.Message):
    __slots__ = ("snapshot", "memory_snapshot", "fork", "pause", "pty", "expose_port", "volumes", "hot_attach", "egress_policy", "io_limits")
    SNAPSHOT_FIELD_NUMBER: _ClassVar[int]
    MEMORY_SNAPSHOT_FIELD_NUMBER: _ClassVar[int]
    FORK_FIELD_NUMBER: _ClassVar[int]
    PAUSE_FIELD_NUMBER: _ClassVar[int]
    PTY_FIELD_NUMBER: _ClassVar[int]
    EXPOSE_PORT_FIELD_NUMBER: _ClassVar[int]
    VOLUMES_FIELD_NUMBER: _ClassVar[int]
    HOT_ATTACH_FIELD_NUMBER: _ClassVar[int]
    EGRESS_POLICY_FIELD_NUMBER: _ClassVar[int]
    IO_LIMITS_FIELD_NUMBER: _ClassVar[int]
    snapshot: bool
    memory_snapshot: bool
    fork: bool
    pause: bool
    pty: bool
    expose_port: bool
    volumes: bool
    hot_attach: bool
    egress_policy: bool
    io_limits: bool
    def __init__(self, snapshot: bool = ..., memory_snapshot: bool = ..., fork: bool = ..., pause: bool = ..., pty: bool = ..., expose_port: bool = ..., volumes: bool = ..., hot_attach: bool = ..., egress_policy: bool = ..., io_limits: bool = ...) -> None: ...

class EgressPolicy(_message.Message):
    __slots__ = ("allow",)
    ALLOW_FIELD_NUMBER: _ClassVar[int]
    allow: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, allow: _Optional[_Iterable[str]] = ...) -> None: ...

class IoLimits(_message.Message):
    __slots__ = ("network_mbps", "disk_mbps", "disk_iops")
    NETWORK_MBPS_FIELD_NUMBER: _ClassVar[int]
    DISK_MBPS_FIELD_NUMBER: _ClassVar[int]
    DISK_IOPS_FIELD_NUMBER: _ClassVar[int]
    network_mbps: int
    disk_mbps: int
    disk_iops: int
    def __init__(self, network_mbps: _Optional[int] = ..., disk_mbps: _Optional[int] = ..., disk_iops: _Optional[int] = ...) -> None: ...

class Endpoint(_message.Message):
    __slots__ = ("host", "port")
    HOST_FIELD_NUMBER: _ClassVar[int]
    PORT_FIELD_NUMBER: _ClassVar[int]
    host: str
    port: int
    def __init__(self, host: _Optional[str] = ..., port: _Optional[int] = ...) -> None: ...

class Mount(_message.Message):
    __slots__ = ("volume_id", "guest_path", "read_only")
    VOLUME_ID_FIELD_NUMBER: _ClassVar[int]
    GUEST_PATH_FIELD_NUMBER: _ClassVar[int]
    READ_ONLY_FIELD_NUMBER: _ClassVar[int]
    volume_id: str
    guest_path: str
    read_only: bool
    def __init__(self, volume_id: _Optional[str] = ..., guest_path: _Optional[str] = ..., read_only: bool = ...) -> None: ...

class Sandbox(_message.Message):
    __slots__ = ("id", "backend", "state", "capabilities", "image", "workdir", "workspace_volume_id", "mounts", "created_at", "labels", "exposed")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    class ExposedEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: int
        value: Endpoint
        def __init__(self, key: _Optional[int] = ..., value: _Optional[_Union[Endpoint, _Mapping]] = ...) -> None: ...
    ID_FIELD_NUMBER: _ClassVar[int]
    BACKEND_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    CAPABILITIES_FIELD_NUMBER: _ClassVar[int]
    IMAGE_FIELD_NUMBER: _ClassVar[int]
    WORKDIR_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_VOLUME_ID_FIELD_NUMBER: _ClassVar[int]
    MOUNTS_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    EXPOSED_FIELD_NUMBER: _ClassVar[int]
    id: str
    backend: str
    state: SandboxState
    capabilities: Capabilities
    image: str
    workdir: str
    workspace_volume_id: str
    mounts: _containers.RepeatedCompositeFieldContainer[Mount]
    created_at: _timestamp_pb2.Timestamp
    labels: _containers.ScalarMap[str, str]
    exposed: _containers.MessageMap[int, Endpoint]
    def __init__(self, id: _Optional[str] = ..., backend: _Optional[str] = ..., state: _Optional[_Union[SandboxState, str]] = ..., capabilities: _Optional[_Union[Capabilities, _Mapping]] = ..., image: _Optional[str] = ..., workdir: _Optional[str] = ..., workspace_volume_id: _Optional[str] = ..., mounts: _Optional[_Iterable[_Union[Mount, _Mapping]]] = ..., created_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ..., labels: _Optional[_Mapping[str, str]] = ..., exposed: _Optional[_Mapping[int, Endpoint]] = ...) -> None: ...

class Volume(_message.Message):
    __slots__ = ("id", "name", "size_mib", "labels", "attached_to", "anonymous", "created_at")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    ID_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    SIZE_MIB_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    ATTACHED_TO_FIELD_NUMBER: _ClassVar[int]
    ANONYMOUS_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    id: str
    name: str
    size_mib: int
    labels: _containers.ScalarMap[str, str]
    attached_to: str
    anonymous: bool
    created_at: _timestamp_pb2.Timestamp
    def __init__(self, id: _Optional[str] = ..., name: _Optional[str] = ..., size_mib: _Optional[int] = ..., labels: _Optional[_Mapping[str, str]] = ..., attached_to: _Optional[str] = ..., anonymous: bool = ..., created_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class VolumeSnapshot(_message.Message):
    __slots__ = ("id", "volume_id", "guest_path", "created_at", "size_bytes", "durable", "labels", "local")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    ID_FIELD_NUMBER: _ClassVar[int]
    VOLUME_ID_FIELD_NUMBER: _ClassVar[int]
    GUEST_PATH_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    SIZE_BYTES_FIELD_NUMBER: _ClassVar[int]
    DURABLE_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    LOCAL_FIELD_NUMBER: _ClassVar[int]
    id: str
    volume_id: str
    guest_path: str
    created_at: _timestamp_pb2.Timestamp
    size_bytes: int
    durable: bool
    labels: _containers.ScalarMap[str, str]
    local: bool
    def __init__(self, id: _Optional[str] = ..., volume_id: _Optional[str] = ..., guest_path: _Optional[str] = ..., created_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ..., size_bytes: _Optional[int] = ..., durable: bool = ..., labels: _Optional[_Mapping[str, str]] = ..., local: bool = ...) -> None: ...

class Snapshot(_message.Message):
    __slots__ = ("id", "backend", "engine_version", "cpu_model", "created_at", "size_bytes", "includes_memory", "volumes", "labels", "durable")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    ID_FIELD_NUMBER: _ClassVar[int]
    BACKEND_FIELD_NUMBER: _ClassVar[int]
    ENGINE_VERSION_FIELD_NUMBER: _ClassVar[int]
    CPU_MODEL_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    SIZE_BYTES_FIELD_NUMBER: _ClassVar[int]
    INCLUDES_MEMORY_FIELD_NUMBER: _ClassVar[int]
    VOLUMES_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    DURABLE_FIELD_NUMBER: _ClassVar[int]
    id: str
    backend: str
    engine_version: str
    cpu_model: str
    created_at: _timestamp_pb2.Timestamp
    size_bytes: int
    includes_memory: bool
    volumes: _containers.RepeatedCompositeFieldContainer[VolumeSnapshot]
    labels: _containers.ScalarMap[str, str]
    durable: bool
    def __init__(self, id: _Optional[str] = ..., backend: _Optional[str] = ..., engine_version: _Optional[str] = ..., cpu_model: _Optional[str] = ..., created_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ..., size_bytes: _Optional[int] = ..., includes_memory: bool = ..., volumes: _Optional[_Iterable[_Union[VolumeSnapshot, _Mapping]]] = ..., labels: _Optional[_Mapping[str, str]] = ..., durable: bool = ...) -> None: ...

class ExitStatus(_message.Message):
    __slots__ = ("code", "signal", "duration", "timed_out")
    CODE_FIELD_NUMBER: _ClassVar[int]
    SIGNAL_FIELD_NUMBER: _ClassVar[int]
    DURATION_FIELD_NUMBER: _ClassVar[int]
    TIMED_OUT_FIELD_NUMBER: _ClassVar[int]
    code: int
    signal: int
    duration: _duration_pb2.Duration
    timed_out: bool
    def __init__(self, code: _Optional[int] = ..., signal: _Optional[int] = ..., duration: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ..., timed_out: bool = ...) -> None: ...

class OutputChunk(_message.Message):
    __slots__ = ("stream", "data")
    STREAM_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    stream: OutputStream
    data: bytes
    def __init__(self, stream: _Optional[_Union[OutputStream, str]] = ..., data: _Optional[bytes] = ...) -> None: ...

class ProcessInfo(_message.Message):
    __slots__ = ("pid", "argv", "started_at", "is_entrypoint")
    PID_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    STARTED_AT_FIELD_NUMBER: _ClassVar[int]
    IS_ENTRYPOINT_FIELD_NUMBER: _ClassVar[int]
    pid: int
    argv: _containers.RepeatedScalarFieldContainer[str]
    started_at: _timestamp_pb2.Timestamp
    is_entrypoint: bool
    def __init__(self, pid: _Optional[int] = ..., argv: _Optional[_Iterable[str]] = ..., started_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ..., is_entrypoint: bool = ...) -> None: ...

class FileInfo(_message.Message):
    __slots__ = ("path", "size", "is_dir", "mode", "modified_at")
    PATH_FIELD_NUMBER: _ClassVar[int]
    SIZE_FIELD_NUMBER: _ClassVar[int]
    IS_DIR_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    MODIFIED_AT_FIELD_NUMBER: _ClassVar[int]
    path: str
    size: int
    is_dir: bool
    mode: int
    modified_at: _timestamp_pb2.Timestamp
    def __init__(self, path: _Optional[str] = ..., size: _Optional[int] = ..., is_dir: bool = ..., mode: _Optional[int] = ..., modified_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class Empty(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...
