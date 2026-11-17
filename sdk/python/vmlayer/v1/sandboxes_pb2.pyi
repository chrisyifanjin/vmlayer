from google.protobuf import duration_pb2 as _duration_pb2
from vmlayer.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Iterable as _Iterable, Mapping as _Mapping, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class WorkspaceSpec(_message.Message):
    __slots__ = ("anonymous", "volume", "none")
    ANONYMOUS_FIELD_NUMBER: _ClassVar[int]
    VOLUME_FIELD_NUMBER: _ClassVar[int]
    NONE_FIELD_NUMBER: _ClassVar[int]
    anonymous: _common_pb2.Empty
    volume: str
    none: _common_pb2.Empty
    def __init__(self, anonymous: _Optional[_Union[_common_pb2.Empty, _Mapping]] = ..., volume: _Optional[str] = ..., none: _Optional[_Union[_common_pb2.Empty, _Mapping]] = ...) -> None: ...

class EntrypointSpec(_message.Message):
    __slots__ = ("image_default", "argv", "none")
    IMAGE_DEFAULT_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    NONE_FIELD_NUMBER: _ClassVar[int]
    image_default: _common_pb2.Empty
    argv: Argv
    none: _common_pb2.Empty
    def __init__(self, image_default: _Optional[_Union[_common_pb2.Empty, _Mapping]] = ..., argv: _Optional[_Union[Argv, _Mapping]] = ..., none: _Optional[_Union[_common_pb2.Empty, _Mapping]] = ...) -> None: ...

class Argv(_message.Message):
    __slots__ = ("argv",)
    ARGV_FIELD_NUMBER: _ClassVar[int]
    argv: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, argv: _Optional[_Iterable[str]] = ...) -> None: ...

class CreateSandboxRequest(_message.Message):
    __slots__ = ("image", "vcpus", "memory_mib", "disk_mib", "env", "workdir", "workspace", "mounts", "entrypoint", "ttl", "backend", "network", "labels", "egress", "limits", "lease", "reschedule")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    IMAGE_FIELD_NUMBER: _ClassVar[int]
    VCPUS_FIELD_NUMBER: _ClassVar[int]
    MEMORY_MIB_FIELD_NUMBER: _ClassVar[int]
    DISK_MIB_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    WORKDIR_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_FIELD_NUMBER: _ClassVar[int]
    MOUNTS_FIELD_NUMBER: _ClassVar[int]
    ENTRYPOINT_FIELD_NUMBER: _ClassVar[int]
    TTL_FIELD_NUMBER: _ClassVar[int]
    BACKEND_FIELD_NUMBER: _ClassVar[int]
    NETWORK_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    EGRESS_FIELD_NUMBER: _ClassVar[int]
    LIMITS_FIELD_NUMBER: _ClassVar[int]
    LEASE_FIELD_NUMBER: _ClassVar[int]
    RESCHEDULE_FIELD_NUMBER: _ClassVar[int]
    image: str
    vcpus: int
    memory_mib: int
    disk_mib: int
    env: _containers.ScalarMap[str, str]
    workdir: str
    workspace: WorkspaceSpec
    mounts: _containers.RepeatedCompositeFieldContainer[_common_pb2.Mount]
    entrypoint: EntrypointSpec
    ttl: _duration_pb2.Duration
    backend: str
    network: _common_pb2.NetworkMode
    labels: _containers.ScalarMap[str, str]
    egress: _common_pb2.EgressPolicy
    limits: _common_pb2.IoLimits
    lease: str
    reschedule: bool
    def __init__(self, image: _Optional[str] = ..., vcpus: _Optional[int] = ..., memory_mib: _Optional[int] = ..., disk_mib: _Optional[int] = ..., env: _Optional[_Mapping[str, str]] = ..., workdir: _Optional[str] = ..., workspace: _Optional[_Union[WorkspaceSpec, _Mapping]] = ..., mounts: _Optional[_Iterable[_Union[_common_pb2.Mount, _Mapping]]] = ..., entrypoint: _Optional[_Union[EntrypointSpec, _Mapping]] = ..., ttl: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ..., backend: _Optional[str] = ..., network: _Optional[_Union[_common_pb2.NetworkMode, str]] = ..., labels: _Optional[_Mapping[str, str]] = ..., egress: _Optional[_Union[_common_pb2.EgressPolicy, _Mapping]] = ..., limits: _Optional[_Union[_common_pb2.IoLimits, _Mapping]] = ..., lease: _Optional[str] = ..., reschedule: bool = ...) -> None: ...

class RestoreSandboxRequest(_message.Message):
    __slots__ = ("snapshot_id", "ttl", "labels")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SNAPSHOT_ID_FIELD_NUMBER: _ClassVar[int]
    TTL_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    snapshot_id: str
    ttl: _duration_pb2.Duration
    labels: _containers.ScalarMap[str, str]
    def __init__(self, snapshot_id: _Optional[str] = ..., ttl: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ..., labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class GetSandboxRequest(_message.Message):
    __slots__ = ("id",)
    ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    def __init__(self, id: _Optional[str] = ...) -> None: ...

class ListSandboxesRequest(_message.Message):
    __slots__ = ("labels",)
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    LABELS_FIELD_NUMBER: _ClassVar[int]
    labels: _containers.ScalarMap[str, str]
    def __init__(self, labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class ListSandboxesResponse(_message.Message):
    __slots__ = ("sandboxes",)
    SANDBOXES_FIELD_NUMBER: _ClassVar[int]
    sandboxes: _containers.RepeatedCompositeFieldContainer[_common_pb2.Sandbox]
    def __init__(self, sandboxes: _Optional[_Iterable[_Union[_common_pb2.Sandbox, _Mapping]]] = ...) -> None: ...

class KillSandboxRequest(_message.Message):
    __slots__ = ("id",)
    ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    def __init__(self, id: _Optional[str] = ...) -> None: ...

class PauseSandboxRequest(_message.Message):
    __slots__ = ("id",)
    ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    def __init__(self, id: _Optional[str] = ...) -> None: ...

class ResumeSandboxRequest(_message.Message):
    __slots__ = ("id",)
    ID_FIELD_NUMBER: _ClassVar[int]
    id: str
    def __init__(self, id: _Optional[str] = ...) -> None: ...

class SnapshotSandboxRequest(_message.Message):
    __slots__ = ("id", "labels", "stay_paused")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    ID_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    STAY_PAUSED_FIELD_NUMBER: _ClassVar[int]
    id: str
    labels: _containers.ScalarMap[str, str]
    stay_paused: bool
    def __init__(self, id: _Optional[str] = ..., labels: _Optional[_Mapping[str, str]] = ..., stay_paused: bool = ...) -> None: ...

class ForkSandboxRequest(_message.Message):
    __slots__ = ("id", "count")
    ID_FIELD_NUMBER: _ClassVar[int]
    COUNT_FIELD_NUMBER: _ClassVar[int]
    id: str
    count: int
    def __init__(self, id: _Optional[str] = ..., count: _Optional[int] = ...) -> None: ...

class ForkSandboxResponse(_message.Message):
    __slots__ = ("children",)
    CHILDREN_FIELD_NUMBER: _ClassVar[int]
    children: _containers.RepeatedCompositeFieldContainer[_common_pb2.Sandbox]
    def __init__(self, children: _Optional[_Iterable[_Union[_common_pb2.Sandbox, _Mapping]]] = ...) -> None: ...

class ExposeRequest(_message.Message):
    __slots__ = ("id", "guest_port")
    ID_FIELD_NUMBER: _ClassVar[int]
    GUEST_PORT_FIELD_NUMBER: _ClassVar[int]
    id: str
    guest_port: int
    def __init__(self, id: _Optional[str] = ..., guest_port: _Optional[int] = ...) -> None: ...

class UnexposeRequest(_message.Message):
    __slots__ = ("id", "guest_port")
    ID_FIELD_NUMBER: _ClassVar[int]
    GUEST_PORT_FIELD_NUMBER: _ClassVar[int]
    id: str
    guest_port: int
    def __init__(self, id: _Optional[str] = ..., guest_port: _Optional[int] = ...) -> None: ...

class WaitForPortRequest(_message.Message):
    __slots__ = ("id", "port", "timeout")
    ID_FIELD_NUMBER: _ClassVar[int]
    PORT_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    id: str
    port: int
    timeout: _duration_pb2.Duration
    def __init__(self, id: _Optional[str] = ..., port: _Optional[int] = ..., timeout: _Optional[_Union[_duration_pb2.Duration, _Mapping]] = ...) -> None: ...

class AttachRequest(_message.Message):
    __slots__ = ("id", "mount")
    ID_FIELD_NUMBER: _ClassVar[int]
    MOUNT_FIELD_NUMBER: _ClassVar[int]
    id: str
    mount: _common_pb2.Mount
    def __init__(self, id: _Optional[str] = ..., mount: _Optional[_Union[_common_pb2.Mount, _Mapping]] = ...) -> None: ...

class DetachRequest(_message.Message):
    __slots__ = ("id", "guest_path")
    ID_FIELD_NUMBER: _ClassVar[int]
    GUEST_PATH_FIELD_NUMBER: _ClassVar[int]
    id: str
    guest_path: str
    def __init__(self, id: _Optional[str] = ..., guest_path: _Optional[str] = ...) -> None: ...
