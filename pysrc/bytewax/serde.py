"""Serialization for recovery and worker exchange.

By default, Python's {py:obj}`pickle` is used for serialization.
"""

import pickle
from abc import ABC, abstractmethod
from typing import Any

from typing_extensions import override

__all__ = [
    "Serde",
    "PickleSerde",
]


class Serde(ABC):
    """A serialization encoder/decoder.

    This must support serializing arbitray Python objects and
    reconstituting them exactly. This means using things like
    `json.dumps` and `json.loads` directly will not work, as they do
    not support things like datetimes, integer keys, etc.

    Even if all of your dataflow's state is serializeable by a format,
    Bytewax generates Python objects to store internal data, and they
    must round-trip correctly or there will be errors.

    """

    @abstractmethod
    def ser(self, obj: Any) -> bytes:
        """Serialize the given object."""
        ...

    @abstractmethod
    def de(self, s: bytes) -> Any:
        """Deserialize the given object."""
        ...


class PickleSerde(Serde):
    """Serialize objects using `pickle`."""

    @override
    def ser(self, obj: Any) -> bytes:
        return pickle.dumps(obj)

    @override
    def de(self, s: bytes) -> Any:
        return pickle.loads(s)
