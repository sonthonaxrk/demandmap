pub mod macos;
pub mod python;
pub mod s3;
pub mod slots;

pub trait AlignDown {
    fn align_down(self, align: usize) -> Self;
}

impl<T> AlignDown for *const T {
    fn align_down(self, align: usize) -> Self {
        self.map_addr(|a| a & !(align - 1))
    }
}

impl<T> AlignDown for *mut T {
    fn align_down(self, align: usize) -> Self {
        self.map_addr(|a| a & !(align - 1))
    }
}

const PAGE_SIZE: usize = 2 << 13;
const ONE_MB: usize = 1048576;
