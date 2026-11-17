from google.protobuf import timestamp_pb2 as _timestamp_pb2
from vmlayer.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Iterable as _Iterable, Mapping as _Mapping, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class HostInfo(_message.Message):
    __slots__ = ("host_id", "endpoint", "daemon_version", "backends", "cpus", "memory_mib", "allocated_vcpus", "allocated_memory_mib", "sandbox_ids", "labels", "updated_at")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    HOST_ID_FIELD_NUMBER: _ClassVar[int]
    ENDPOINT_FIELD_NUMBER: _ClassVar[int]
    DAEMON_VERSION_FIELD_NUMBER: _ClassVar[int]
    BACKENDS_FIELD_NUMBER: _ClassVar[int]
    CPUS_FIELD_NUMBER: _ClassVar[int]
    MEMORY_MIB_FIELD_NUMBER: _ClassVar[int]
    ALLOCATED_VCPUS_FIELD_NUMBER: _ClassVar[int]
    ALLOCATED_MEMORY_MIB_FIELD_NUMBER: _ClassVar[int]
    SANDBOX_IDS_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    UPDATED_AT_FIELD_NUMBER: _ClassVar[int]
    host_id: str
    endpoint: str
    daemon_version: str
    backends: _containers.RepeatedScalarFieldContainer[str]
    cpus: int
    memory_mib: int
    allocated_vcpus: int
    allocated_memory_mib: int
    sandbox_ids: _containers.RepeatedScalarFieldContainer[str]
    labels: _containers.ScalarMap[str, str]
    updated_at: _timestamp_pb2.Timestamp
    def __init__(self, host_id: _Optional[str] = ..., endpoint: _Optional[str] = ..., daemon_version: _Optional[str] = ..., backends: _Optional[_Iterable[str]] = ..., cpus: _Optional[int] = ..., memory_mib: _Optional[int] = ..., allocated_vcpus: _Optional[int] = ..., allocated_memory_mib: _Optional[int] = ..., sandbox_ids: _Optional[_Iterable[str]] = ..., labels: _Optional[_Mapping[str, str]] = ..., updated_at: _Optional[_Union[_timestamp_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class ListHostsResponse(_message.Message):
    __slots__ = ("hosts",)
    HOSTS_FIELD_NUMBER: _ClassVar[int]
    hosts: _containers.RepeatedCompositeFieldContainer[HostInfo]
    def __init__(self, hosts: _Optional[_Iterable[_Union[HostInfo, _Mapping]]] = ...) -> None: ...

class CollectGarbageRequest(_message.Message):
    __slots__ = ("dry_run",)
    DRY_RUN_FIELD_NUMBER: _ClassVar[int]
    dry_run: bool
    def __init__(self, dry_run: bool = ...) -> None: ...

class CollectGarbageResponse(_message.Message):
    __slots__ = ("snapshots", "chunks", "referenced_chunks", "recent_chunks", "deleted_chunks", "deleted_bytes")
    SNAPSHOTS_FIELD_NUMBER: _ClassVar[int]
    CHUNKS_FIELD_NUMBER: _ClassVar[int]
    REFERENCED_CHUNKS_FIELD_NUMBER: _ClassVar[int]
    RECENT_CHUNKS_FIELD_NUMBER: _ClassVar[int]
    DELETED_CHUNKS_FIELD_NUMBER: _ClassVar[int]
    DELETED_BYTES_FIELD_NUMBER: _ClassVar[int]
    snapshots: int
    chunks: int
    referenced_chunks: int
    recent_chunks: int
    deleted_chunks: int
    deleted_bytes: int
    def __init__(self, snapshots: _Optional[int] = ..., chunks: _Optional[int] = ..., referenced_chunks: _Optional[int] = ..., recent_chunks: _Optional[int] = ..., deleted_chunks: _Optional[int] = ..., deleted_bytes: _Optional[int] = ...) -> None: ...

class PullImageRequest(_message.Message):
    __slots__ = ("image", "backend")
    IMAGE_FIELD_NUMBER: _ClassVar[int]
    BACKEND_FIELD_NUMBER: _ClassVar[int]
    image: str
    backend: str
    def __init__(self, image: _Optional[str] = ..., backend: _Optional[str] = ...) -> None: ...

class ImageInfo(_message.Message):
    __slots__ = ("image", "backend", "rootfs_kind", "rootfs_path", "entrypoint", "workdir")
    IMAGE_FIELD_NUMBER: _ClassVar[int]
    BACKEND_FIELD_NUMBER: _ClassVar[int]
    ROOTFS_KIND_FIELD_NUMBER: _ClassVar[int]
    ROOTFS_PATH_FIELD_NUMBER: _ClassVar[int]
    ENTRYPOINT_FIELD_NUMBER: _ClassVar[int]
    WORKDIR_FIELD_NUMBER: _ClassVar[int]
    image: str
    backend: str
    rootfs_kind: str
    rootfs_path: str
    entrypoint: _containers.RepeatedScalarFieldContainer[str]
    workdir: str
    def __init__(self, image: _Optional[str] = ..., backend: _Optional[str] = ..., rootfs_kind: _Optional[str] = ..., rootfs_path: _Optional[str] = ..., entrypoint: _Optional[_Iterable[str]] = ..., workdir: _Optional[str] = ...) -> None: ...

class BackendInfo(_message.Message):
    __slots__ = ("name", "engine_version", "capabilities", "unavailable_reason")
    NAME_FIELD_NUMBER: _ClassVar[int]
    ENGINE_VERSION_FIELD_NUMBER: _ClassVar[int]
    CAPABILITIES_FIELD_NUMBER: _ClassVar[int]
    UNAVAILABLE_REASON_FIELD_NUMBER: _ClassVar[int]
    name: str
    engine_version: str
    capabilities: _common_pb2.Capabilities
    unavailable_reason: str
    def __init__(self, name: _Optional[str] = ..., engine_version: _Optional[str] = ..., capabilities: _Optional[_Union[_common_pb2.Capabilities, _Mapping]] = ..., unavailable_reason: _Optional[str] = ...) -> None: ...

class SystemInfo(_message.Message):
    __slots__ = ("daemon_version", "backends", "default_backend", "data_dir", "blob_store")
    DAEMON_VERSION_FIELD_NUMBER: _ClassVar[int]
    BACKENDS_FIELD_NUMBER: _ClassVar[int]
    DEFAULT_BACKEND_FIELD_NUMBER: _ClassVar[int]
    DATA_DIR_FIELD_NUMBER: _ClassVar[int]
    BLOB_STORE_FIELD_NUMBER: _ClassVar[int]
    daemon_version: str
    backends: _containers.RepeatedCompositeFieldContainer[BackendInfo]
    default_backend: str
    data_dir: str
    blob_store: str
    def __init__(self, daemon_version: _Optional[str] = ..., backends: _Optional[_Iterable[_Union[BackendInfo, _Mapping]]] = ..., default_backend: _Optional[str] = ..., data_dir: _Optional[str] = ..., blob_store: _Optional[str] = ...) -> None: ...

class HealthResponse(_message.Message):
    __slots__ = ("ok",)
    OK_FIELD_NUMBER: _ClassVar[int]
    ok: bool
    def __init__(self, ok: bool = ...) -> None: ...

class ListSnapshotsRequest(_message.Message):
    __slots__ = ("labels", "remote")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    LABELS_FIELD_NUMBER: _ClassVar[int]
    REMOTE_FIELD_NUMBER: _ClassVar[int]
    labels: _containers.ScalarMap[str, str]
    remote: bool
    def __init__(self, labels: _Optional[_Mapping[str, str]] = ..., remote: bool = ...) -> None: ...

class ListSnapshotsResponse(_message.Message):
    __slots__ = ("snapshots",)
    SNAPSHOTS_FIELD_NUMBER: _ClassVar[int]
    snapshots: _containers.RepeatedCompositeFieldContainer[_common_pb2.Snapshot]
    def __init__(self, snapshots: _Optional[_Iterable[_Union[_common_pb2.Snapshot, _Mapping]]] = ...) -> None: ...

class DeleteSnapshotRequest(_message.Message):
    __slots__ = ("id", "local_only")
    ID_FIELD_NUMBER: _ClassVar[int]
    LOCAL_ONLY_FIELD_NUMBER: _ClassVar[int]
    id: str
    local_only: bool
    def __init__(self, id: _Optional[str] = ..., local_only: bool = ...) -> None: ...
