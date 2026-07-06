import demandmap
import math
import operator
import numpy as np
from numpy.lib import format as npfmt


class BufferReader:
    def __init__(self, buf):
        self._mv = memoryview(buf).cast("B")
        self._pos = 0

    def read(self, n=-1):
        if n < 0:
            n = len(self._mv) - self._pos
        end = min(self._pos + n, len(self._mv))
        out = self._mv[self._pos:end].tobytes()  
        self._pos = end
        return out

    def tell(self):
        return self._pos


def ndarray_from_npy_buffer(buf, *, max_header_size=10_000):
    r = BufferReader(buf)

    version = npfmt.read_magic(r)
    if version == (1, 0):
        shape, fortran_order, dtype = npfmt.read_array_header_1_0(
            r,
            max_header_size=max_header_size,
        )
    elif version == (2, 0):
        shape, fortran_order, dtype = npfmt.read_array_header_2_0(
            r,
            max_header_size=max_header_size,
        )
    else:
        raise ValueError(f"unsupported .npy version: {version}")

    if dtype.hasobject:
        raise TypeError("object dtype .npy arrays are not mmap-compatible")

    offset = r.tell()
    order = "F" if fortran_order else "C"

    need = math.prod(shape) * dtype.itemsize
    have = len(memoryview(buf)) - offset
    if have < need:
        raise ValueError(f"buffer too small: need {need} payload bytes, have {have}")

    return np.ndarray(
        shape=shape,
        dtype=dtype,
        buffer=buf,
        offset=offset,
        order=order,
    )


arr = demandmap.S3Array("./cache.bin", "https://rollo-testing.lon1.digitaloceanspaces.com/big_col.npz.npy", 512, 1048576)
print(arr.nbytes, "bytes")

a = ndarray_from_npy_buffer(arr)

print(a[-1])
