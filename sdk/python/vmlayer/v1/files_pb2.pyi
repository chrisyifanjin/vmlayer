from vmlayer.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ReadFileRequest(_message.Message):
    __slots__ = ("sandbox_id", "path")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ...) -> None: ...

class FileChunk(_message.Message):
    __slots__ = ("data",)
    DATA_FIELD_NUMBER: _ClassVar[int]
    data: bytes
    def __init__(self, data: _Optional[bytes] = ...) -> None: ...

class WriteFileRequest(_message.Message):
    __slots__ = ("header", "data")
    class Header(_message.Message):
        __slots__ = ("sandbox_id", "path", "mode")
        SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
        PATH_FIELD_NUMBER: _ClassVar[int]
        MODE_FIELD_NUMBER: _ClassVar[int]
        sandbox_id: str
        path: str
        mode: int
        def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ..., mode: _Optional[int] = ...) -> None: ...
    HEADER_FIELD_NUMBER: _ClassVar[int]
    DATA_FIELD_NUMBER: _ClassVar[int]
    header: WriteFileRequest.Header
    data: bytes
    def __init__(self, header: _Optional[_Union[WriteFileRequest.Header, _Mapping]] = ..., data: _Optional[bytes] = ...) -> None: ...

class StatRequest(_message.Message):
    __slots__ = ("sandbox_id", "path")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ...) -> None: ...

class ListFilesRequest(_message.Message):
    __slots__ = ("sandbox_id", "path")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ...) -> None: ...

class ListFilesResponse(_message.Message):
    __slots__ = ("entries",)
    ENTRIES_FIELD_NUMBER: _ClassVar[int]
    entries: _containers.RepeatedCompositeFieldContainer[_common_pb2.FileInfo]
    def __init__(self, entries: _Optional[_Iterable[_Union[_common_pb2.FileInfo, _Mapping]]] = ...) -> None: ...

class MkdirRequest(_message.Message):
    __slots__ = ("sandbox_id", "path")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ...) -> None: ...

class RemoveRequest(_message.Message):
    __slots__ = ("sandbox_id", "path", "recursive")
    SANDBOX_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    RECURSIVE_FIELD_NUMBER: _ClassVar[int]
    sandbox_id: str
    path: str
    recursive: bool
    def __init__(self, sandbox_id: _Optional[str] = ..., path: _Optional[str] = ..., recursive: _Optional[bool] = ...) -> None: ...
