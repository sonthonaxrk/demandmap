use std::ffi::c_void;
use std::io::Read;
use std::ops::Range;

use pyo3::prelude::*;

use crate::slots::{DiskAlloc, Md5Digest};

fn fetch_s3(url: &String, range: Range<usize>, buf: &mut [u8]) {
    let mut reader = ureq::get(url)
        .header("Range", format!("bytes={}-{}", range.start, range.end - 1))
        .call()
        .unwrap()
        .into_body()
        .into_reader();
    let mut off = 0;
    while off < buf.len() {
        match reader.read(&mut buf[off..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => off += n,
        }
    }
    buf[off..].fill(0);
}

#[pyclass]
pub struct S3Alloc {
    disk: DiskAlloc<String>,
}

#[pymethods]
impl S3Alloc {
    #[new]
    fn new(path: String, capacity: u32, block_size: usize) -> Self {
        S3Alloc {
            disk: DiskAlloc::new(&path, capacity, block_size),
        }
    }

    fn get(slf: Bound<'_, Self>, url: String) -> PyResult<Buffer> {
        let head = ureq::head(&url)
            .call()
            .map_err(|e| pyo3::exceptions::PyIOError::new_err(e.to_string()))?;

        let size = head
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("no content-length"))?;

        let md5 = head
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| u128::from_str_radix(v.trim_start_matches("W/").trim_matches('"'), 16).ok())
            .map(Md5Digest)
            .unwrap_or(Md5Digest(0));

        let (base, len) = {
            let this = slf.borrow();
            let slice = this.disk.callback_buffer(size, url, md5, fetch_s3);
            (slice.as_ptr(), slice.len())
        };

        Ok(Buffer { _alloc: slf.unbind(), base, len })
    }
}

#[pyclass(unsendable)]
pub struct Buffer {
    _alloc: Py<S3Alloc>,
    base: *const u8,
    len: usize,
}

#[pymethods]
impl Buffer {
    #[getter]
    fn nbytes(&self) -> usize {
        self.len
    }

    unsafe fn __getbuffer__(
        slf: PyRefMut<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let ret = unsafe {
            pyo3::ffi::PyBuffer_FillInfo(
                view,
                slf.as_ptr(),
                slf.base as *mut c_void,
                slf.len as pyo3::ffi::Py_ssize_t,
                1,
                flags,
            )
        };
        if ret == -1 {
            Err(PyErr::take(slf.py())
                .unwrap_or_else(|| pyo3::exceptions::PyBufferError::new_err("fill failed")))
        } else {
            Ok(())
        }
    }

    unsafe fn __releasebuffer__(&self, _view: *mut pyo3::ffi::Py_buffer) {}
}

#[pymodule]
fn demandmap(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<S3Alloc>()?;
    m.add_class::<Buffer>()?;
    Ok(())
}
