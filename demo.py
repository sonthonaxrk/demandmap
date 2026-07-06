import demandmap
import math
import operator
import numpy as np
from numpy.lib import format as npfmt
import polars as pl


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

alloc = demandmap.S3Alloc(
    "./cache.bin",
    capacity=512,
    block_size=1048576
)

buf1 = alloc.get("https://rollo-testing.lon1.digitaloceanspaces.com/big_col.npz.npy")
buf2 = alloc.get("https://rollo-testing.lon1.digitaloceanspaces.com/big_col2.npz.npy")

# Both over 400mb
assert buf1.nbytes > 400000000
assert buf2.nbytes > 400000000
col1 = ndarray_from_npy_buffer(buf1)
col2 = ndarray_from_npy_buffer(buf2)

# But this takes ~100ms
df = pl.DataFrame([
    ndarray_from_npy_buffer(buf1),
    ndarray_from_npy_buffer(buf2)
])
# shape: (50_000_000, 2)
# ┌──────────┬──────────┐
# │ column_0 ┆ column_1 │
# │ ---      ┆ ---      │
# │ i64      ┆ i64      │
# ╞══════════╪══════════╡
# │ 0        ┆ 1000     │
# │ 1        ┆ 1001     │
# │ 2        ┆ 1002     │
# │ 3        ┆ 1003     │
# │ 4        ┆ 1004     │
# │ …        ┆ …        │
# │ 49999995 ┆ 50000995 │
# │ 49999996 ┆ 50000996 │
# │ 49999997 ┆ 50000997 │
# │ 49999998 ┆ 50000998 │
# │ 49999999 ┆ 50000999 │
# └──────────┴──────────┘
