# DEMAND MAP

How many times have you been *forced* to download a big file from S3? There's a criminally
underutilised API on modern systems that allows you to index arrays and _attach callbacks_
if the data isn't there, called userfaulting.  Unfortunately, barely anyone knows about this and the API differs on Windows, Linux, and macOS, respectively increasing in just how esoteric the API is.

This allows you to lazily download chunks of data, as you read the underlying array, while caching blocks to your file system. This makes it LIGHTING fast for probing data remotely as you only download what you need.

```python
alloc = demandmap.S3Alloc(
    "./cache.bin",
    # number of blocks
    capacity=512,
    # one megabyte block (per request chunk size)
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
```

–--

And the Rust API.

```rust
let buf = cache.callback_buffer(
    length,
    path,
    etag, 
    |url, range, mut buf| get(s3_url, range).read_into(buf));

assert_eq!(buf[10000..10010], b"my s3 data");
```

## Caching

This has a persistent memory mapped lru cache where blocks are downloaded to, making repeated runs and restarts as fast as they would be locally.

---

## TODOS

This is very very rough and not ready for production and only supports macOS.

- [x] Linux userfaultfd handling
- [x] Windows API
- [x] Signal and error handling
- [x] Prefaulting data.
- [x] More options around caching.
- [x] Nonblocking coroutines
