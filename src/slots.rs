use std::alloc::{Layout, handle_alloc_error};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::ops::{Bound, Deref, Range, RangeBounds};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32};

use allocator_api2::alloc::{AllocError, Allocator};
use bytemuck::Zeroable;
use libc::{MAP_ANON, MAP_PRIVATE, MAP_SHARED, PROT_NONE, PROT_READ, PROT_WRITE};

const PAGE: usize = 16384;
const NIL: u32 = u32::MAX;

const EMPTY: u8 = 0;
const LOADING: u8 = 1;
const READY: u8 = 2;

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Zeroable)]
pub struct Md5Digest(pub u128);

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Zeroable)]
pub struct BlockKey {
    pub digest: Md5Digest,
    pub block: u64,
}

impl BlockKey {
    fn hash(&self) -> u64 {
        let d = self.digest.0 as u128;
        (d as u64) ^ ((d >> 64) as u64) ^ self.block
    }
}

#[repr(C)]
struct Header {
    lock: AtomicU32,
    capacity: u32,
    block_size: usize,
    head: u32,
    tail: u32,
    free: u32,
    len: u32,
    nodes_off: usize,
    buckets_off: usize,
    states_off: usize,
    pins_off: usize,
    blocks_off: usize,
}

#[repr(C)]
#[derive(Copy, Clone, Zeroable)]
struct Node {
    key: BlockKey,
    prev: u32,
    next: u32,
    chain: u32,
}

struct RegionLayout {
    layout: Layout,
    nodes_off: usize,
    buckets_off: usize,
    states_off: usize,
    pins_off: usize,
    blocks_off: usize,
}

impl RegionLayout {
    fn new(capacity: u32, block_size: usize) -> RegionLayout {
        assert!(capacity > 0 && capacity != NIL, "bad capacity");
        assert!(
            block_size != 0 && block_size % PAGE == 0,
            "block_size must be a non-zero page multiple",
        );

        let cap = capacity as usize;
        let page = |l: Layout| l.align_to(PAGE).unwrap().pad_to_align();

        // [ Header ]<pad to page>[ nodes | buckets | states | pins ]<pad to page>[ blocks ]
        let l = page(Layout::new::<Header>());
        let (l, nodes_off) = l.extend(Layout::array::<Node>(cap).unwrap()).unwrap();
        let (l, buckets_off) = l.extend(Layout::array::<u32>(cap).unwrap()).unwrap();
        let (l, states_off) = l.extend(Layout::array::<AtomicU8>(cap).unwrap()).unwrap();
        let (l, pins_off) = l.extend(Layout::array::<AtomicU16>(cap).unwrap()).unwrap();
        let (l, blocks_off) = page(l)
            .extend(Layout::array::<u8>(cap * block_size).unwrap())
            .unwrap();
        let layout = l.pad_to_align();

        RegionLayout {
            layout,
            nodes_off,
            buckets_off,
            states_off,
            pins_off,
            blocks_off,
        }
    }
}

pub struct Cache<A: Allocator> {
    base: *mut u8,
    layout: Layout,
    allocator: A,
}

unsafe impl<A: Allocator> Send for Cache<A> {}
unsafe impl<A: Allocator> Sync for Cache<A> {}

impl<A: Allocator> Cache<A> {
    unsafe fn from_raw(base: *mut u8, capacity: u32, block_size: usize, alloc: A) -> Cache<A> {
        let RegionLayout { layout, .. } = RegionLayout::new(capacity, block_size);

        let cache = Cache {
            base,
            layout,
            allocator: alloc,
        };
        unsafe {
            let hdr = &*cache.hdr();
            assert_eq!(hdr.capacity, capacity);
            assert_eq!(hdr.block_size, block_size);
        };

        cache
    }

    fn new(capacity: u32, block_size: usize, allocator: A) -> Cache<A> {
        let RegionLayout {
            layout,
            nodes_off,
            buckets_off,
            states_off,
            pins_off,
            blocks_off,
        } = RegionLayout::new(capacity, block_size);
        let cap = capacity as usize;

        unsafe {
            let Ok(base) = allocator.allocate(layout) else {
                handle_alloc_error(layout)
            };

            let base = base.as_ptr() as *mut u8;

            // Zero only the header + index region [0, blocks_off). The block
            // pages are left untouched so they stay lazily zero and aren't
            // faulted/committed until a block is actually fetched.
            std::ptr::write_bytes(base, 0, blocks_off);

            (base as *mut Header).write(Header {
                lock: AtomicU32::new(0),
                capacity,
                block_size,
                head: NIL,
                tail: NIL,
                free: 0,
                len: 0,
                nodes_off,
                buckets_off,
                states_off,
                pins_off,
                blocks_off,
            });

            let cache = Cache {
                base,
                layout,
                allocator,
            };
            // states/pins are EMPTY / 0 from the write_bytes above
            let (nodes, buckets) = (cache.nodes(), cache.buckets());
            for i in 0..cap {
                nodes[i] = Node {
                    key: Zeroable::zeroed(),
                    prev: NIL,
                    next: if i + 1 < cap { (i + 1) as u32 } else { NIL },
                    chain: NIL,
                };
                buckets[i] = NIL;
            }
            cache
        }
    }

    fn hdr(&self) -> *mut Header {
        self.base as *mut Header
    }

    fn nodes(&self) -> &mut [Node] {
        unsafe {
            let h = &*self.hdr();
            std::slice::from_raw_parts_mut(
                self.base.add(h.nodes_off) as *mut Node,
                h.capacity as usize,
            )
        }
    }

    fn buckets(&self) -> &mut [u32] {
        unsafe {
            let h = &*self.hdr();
            std::slice::from_raw_parts_mut(
                self.base.add(h.buckets_off) as *mut u32,
                h.capacity as usize,
            )
        }
    }

    fn state(&self, i: u32) -> &AtomicU8 {
        unsafe { &*(self.base.add((*self.hdr()).states_off) as *const AtomicU8).add(i as usize) }
    }

    fn pin(&self, i: u32) -> &AtomicU16 {
        unsafe { &*(self.base.add((*self.hdr()).pins_off) as *const AtomicU16).add(i as usize) }
    }

    fn head(&self) -> u32 {
        unsafe { (*self.hdr()).head }
    }
    fn set_head(&self, v: u32) {
        unsafe { (*self.hdr()).head = v }
    }
    fn tail(&self) -> u32 {
        unsafe { (*self.hdr()).tail }
    }
    fn set_tail(&self, v: u32) {
        unsafe { (*self.hdr()).tail = v }
    }
    fn free(&self) -> u32 {
        unsafe { (*self.hdr()).free }
    }
    fn set_free(&self, v: u32) {
        unsafe { (*self.hdr()).free = v }
    }
    fn len(&self) -> u32 {
        unsafe { (*self.hdr()).len }
    }
    fn set_len(&self, v: u32) {
        unsafe { (*self.hdr()).len = v }
    }

    pub fn block_size(&self) -> usize {
        unsafe { (*self.hdr()).block_size }
    }

    fn on_disk_size(&self) -> usize {
        self.layout.size()
    }

    fn block(&self, i: u32) -> *mut u8 {
        unsafe {
            let h = &*self.hdr();
            self.base.add(h.blocks_off + i as usize * h.block_size)
        }
    }

    fn lock(&self) {
        let l = unsafe { &(*self.hdr()).lock };
        while l.compare_exchange_weak(0, 1, Acquire, Relaxed).is_err() {
            std::hint::spin_loop();
        }
    }

    fn unlock(&self) {
        unsafe { (*self.hdr()).lock.store(0, Release) }
    }

    fn find(&self, key: BlockKey) -> u32 {
        let (nodes, buckets) = (self.nodes(), self.buckets());
        let mut cur = buckets[(key.hash() % buckets.len() as u64) as usize];
        while cur != NIL {
            if nodes[cur as usize].key == key {
                return cur;
            }
            cur = nodes[cur as usize].chain;
        }
        NIL
    }

    fn detach(&self, i: u32) {
        let nodes = self.nodes();
        let (prev, next) = (nodes[i as usize].prev, nodes[i as usize].next);
        if prev != NIL {
            nodes[prev as usize].next = next;
        } else {
            self.set_head(next);
        }
        if next != NIL {
            nodes[next as usize].prev = prev;
        } else {
            self.set_tail(prev);
        }
    }

    fn push_front(&self, i: u32) {
        let nodes = self.nodes();
        let old = self.head();
        nodes[i as usize].prev = NIL;
        nodes[i as usize].next = old;
        if old != NIL {
            nodes[old as usize].prev = i;
        } else {
            self.set_tail(i);
        }
        self.set_head(i);
    }

    fn promote(&self, i: u32) {
        self.detach(i);
        self.push_front(i);
    }

    fn unchain(&self, i: u32) {
        let (nodes, buckets) = (self.nodes(), self.buckets());
        let key = nodes[i as usize].key;
        let b = (key.hash() % buckets.len() as u64) as usize;
        let (mut cur, mut prev) = (buckets[b], NIL);
        while cur != NIL {
            if cur == i {
                let next = nodes[cur as usize].chain;
                if prev == NIL {
                    buckets[b] = next;
                } else {
                    nodes[prev as usize].chain = next;
                }
                return;
            }
            prev = cur;
            cur = nodes[cur as usize].chain;
        }
    }

    fn pop_free(&self) -> Option<u32> {
        let free = self.free();
        if free == NIL {
            return None;
        }
        self.set_free(self.nodes()[free as usize].next);
        self.set_len(self.len() + 1);
        Some(free)
    }

    fn evict(&self) -> Option<u32> {
        let victim = {
            let nodes = self.nodes();
            let mut cur = self.tail();
            loop {
                if cur == NIL {
                    break NIL;
                }
                if self.pin(cur).load(SeqCst) == 0 && self.state(cur).load(SeqCst) == READY {
                    break cur;
                }
                cur = nodes[cur as usize].prev;
            }
        };
        if victim == NIL {
            return None;
        }
        self.detach(victim);
        self.unchain(victim);
        Some(victim)
    }

    fn insert(&self, key: BlockKey) -> Option<u32> {
        let i = match self.pop_free() {
            Some(i) => i,
            None => self.evict()?,
        };
        {
            let (nodes, buckets) = (self.nodes(), self.buckets());
            let b = (key.hash() % buckets.len() as u64) as usize;
            nodes[i as usize].key = key;
            nodes[i as usize].chain = buckets[b];
            buckets[b] = i;
        }
        self.state(i).store(LOADING, SeqCst);
        self.pin(i).store(0, SeqCst);
        self.push_front(i);
        Some(i)
    }

    pub fn get_or_fetch<F: FnOnce(&mut [u8])>(&self, key: BlockKey, fetch: F) -> Block<'_, A> {
        loop {
            self.lock();
            let i = self.find(key);
            if i != NIL {
                if self.state(i).load(SeqCst) == READY {
                    self.promote(i);
                    self.pin(i).fetch_add(1, SeqCst);
                    self.unlock();
                    return Block::new(self, i);
                }
                self.unlock();
                while self.state(i).load(SeqCst) == LOADING {
                    std::hint::spin_loop();
                }
                continue;
            }

            match self.insert(key) {
                Some(i) => {
                    self.pin(i).fetch_add(1, SeqCst);
                    self.unlock();

                    let (ptr, len) = (self.block(i), self.block_size());

                    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
                    slice.fill(0);
                    fetch(slice);
                    self.state(i).store(READY, SeqCst);

                    return Block::new(self, i);
                }
                None => {
                    self.unlock();
                    std::hint::spin_loop();
                }
            }
        }
    }

    fn get(&self, key: BlockKey) -> Option<Block<'_, A>> {
        self.lock();
        let i = self.find(key);
        if i == NIL || self.state(i).load(SeqCst) != READY {
            self.unlock();
            return None;
        }
        self.promote(i);
        self.pin(i).fetch_add(1, SeqCst);
        self.unlock();
        Some(Block::new(self, i))
    }
}

impl<A: Allocator> Drop for Cache<A> {
    fn drop(&mut self) {
        unsafe {
            self.allocator
                .deallocate(NonNull::new(self.base).unwrap(), self.layout)
        }
    }
}

pub struct Block<'a, A: Allocator> {
    cache: &'a Cache<A>,
    pub slot: u32,
    pub ptr: *mut u8,
    pub len: usize,
}

impl<'a, A: Allocator> Block<'a, A> {
    fn new(cache: &'a Cache<A>, slot: u32) -> Block<'a, A> {
        Block {
            cache,
            slot,
            ptr: cache.block(slot),
            len: cache.block_size(),
        }
    }
}

impl<A: Allocator> Deref for Block<'_, A> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<A: Allocator> Drop for Block<'_, A> {
    fn drop(&mut self) {
        // thread safety while updating
        self.cache.pin(self.slot).fetch_sub(1, SeqCst);
    }
}

pub struct FixedSizeByteAlloc {
    data: NonNull<[u8]>,
    used: AtomicBool,
}

unsafe impl Allocator for FixedSizeByteAlloc {
    fn allocate(
        &self,
        _layout: Layout,
    ) -> Result<std::ptr::NonNull<[u8]>, allocator_api2::alloc::AllocError> {
        if !self.used.swap(true, std::sync::atomic::Ordering::AcqRel) {
            Ok(self.data)
        } else {
            Err(AllocError)
        }
    }

    unsafe fn deallocate(&self, _ptr: std::ptr::NonNull<u8>, _layout: Layout) {}
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PtrRange(pub *const u8, pub *const u8);

impl RangeBounds<*const u8> for PtrRange {
    fn start_bound(&self) -> Bound<&*const u8> {
        Bound::Included(&self.0)
    }
    fn end_bound(&self) -> Bound<&*const u8> {
        Bound::Excluded(&self.1)
    }
}

unsafe impl Send for PtrRange {}

pub struct ReqData<Req> {
    pub cb: fn(&Req, Range<usize>, &mut [u8]),
    pub data: Req,
    pub md5_digest: Md5Digest,
}

pub struct DiskAlloc<Req> {
    path: PathBuf,
    cache: Arc<Cache<FixedSizeByteAlloc>>,
    pub requests: Arc<std::sync::Mutex<BTreeMap<PtrRange, Arc<ReqData<Req>>>>>,
}

impl<R: std::fmt::Debug + Send + Sync + 'static> DiskAlloc<R> {
    pub fn new(path: impl AsRef<Path>, cap: u32, block_size: usize) -> Self {
        let path = path.as_ref().to_path_buf();
        let region = RegionLayout::new(cap, block_size);
        let sz = region.layout.size();

        let (file, created) = if !path.exists() {
            (File::create_new(&path).unwrap(), true)
        } else {
            (
                File::options().read(true).write(true).open(&path).unwrap(),
                false,
            )
        };

        file.set_len(sz as _).unwrap();

        let reqs: Arc<std::sync::Mutex<BTreeMap<PtrRange, Arc<ReqData<R>>>>> = Default::default();

        let cache = unsafe {
            let ptr = libc::mmap(
                null_mut(),
                sz,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            );

            let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, sz);
            let alloc = FixedSizeByteAlloc {
                data: NonNull::from(slice),
                used: Default::default(),
            };

            let cache = if created {
                Arc::new(Cache::new(cap, block_size, alloc))
            } else {
                Arc::new(Cache::from_raw(
                    alloc.data.as_ptr() as *mut _,
                    cap,
                    block_size,
                    alloc,
                ))
            };

            Self::init_faulter(ptr as *const u8, reqs.clone(), cache.clone());

            cache
        };

        Self {
            path,
            cache,
            requests: reqs,
        }
    }

    pub fn callback_buffer(
        &self,
        size: usize,
        data: R,
        md5: Md5Digest,
        cb: fn(&R, Range<usize>, &mut [u8]),
    ) -> &[u8] {
        unsafe {
            let ptr =
                libc::mmap(null_mut(), size, PROT_NONE, MAP_ANON | MAP_PRIVATE, -1, 0) as *const u8;

            let res = std::slice::from_raw_parts(ptr, size);
            let rng = res.as_ptr_range();
            let start = rng.start;
            let end = rng.end;
            let mut reqs = self.requests.lock().unwrap();

            (&mut *reqs).insert(
                PtrRange(start, end),
                Arc::new(ReqData {
                    cb,
                    data,
                    md5_digest: md5,
                }),
            );
            res
        }
    }
}


#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::{self, Read, Write},
        os::fd::AsRawFd,
        ptr::{NonNull, null_mut},
        sync::atomic::AtomicBool,
    };

    use allocator_api2::alloc::{AllocError, Global};
    use libc::{MAP_SHARED, PROT_NONE, PROT_READ, PROT_WRITE};
    use pretty_hex::PrettyHex;

    use crate::{ONE_MB, PAGE_SIZE};

    use super::*;

    #[test]
    fn alloc_test() {
        let cache = DiskAlloc::new("./test_f", 64, ONE_MB);
        let path = "https://rollo-testing.lon1.digitaloceanspaces.com/my_large_thing";

        let mut resp = ureq::head(path).call().unwrap();

        let length = resp
            .headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let etag = resp.headers().get("etag").unwrap().to_str().unwrap();
        let digest = Md5Digest(u128::from_str_radix(etag.trim_matches('"'), 16).unwrap());

        let buf = cache.callback_buffer(
            length,
            path,
            digest, 
            |p, range, mut buf| {
            ureq::get(*p)
                .header(
                    "Range",
                    format!("bytes={}-{}", range.start, range.end),
                )
                .call()
                .inspect_err(|e| {
                    // println!("{e:?}");

                })
                .unwrap()
                .into_body()
                .into_reader()
                .read_exact(buf);
        });

        let multi_page = &buf[ONE_MB * 2..ONE_MB * 3];
        let multi_page = &multi_page[0..100];
        std::hint::black_box(multi_page[0]);
        dbg!(multi_page.hex_dump());


        let multi_page = &buf[ONE_MB * 5..ONE_MB * 6];
        let multi_page = &multi_page[0..128];
        dbg!(multi_page.hex_dump());
    }

    #[test]
    fn cache() {
        // https://rollo-testing.lon1.digitaloceanspaces.com/my_large_thing

        let cache = Cache::new(2, PAGE, Global);
        let d = Md5Digest(1);
        let k = |block| BlockKey { digest: d, block };

        assert!(cache.get(k(0)).is_none());

        {
            let b = cache.get_or_fetch(k(0), |buf| buf[..5].copy_from_slice(b"aaaaa"));
            assert_eq!(&b[..5], b"aaaaa");
        }
        {
            let b = cache.get_or_fetch(k(1), |buf| buf[..5].copy_from_slice(b"bbbbb"));
            assert_eq!(&b[..5], b"bbbbb");
        }

        {
            let b = cache.get_or_fetch(k(0), |_| panic!("should already be cached"));
            assert_eq!(&b[..5], b"aaaaa");
        }

        {
            let b = cache.get_or_fetch(k(2), |buf| buf[..5].copy_from_slice(b"ccccc"));
            assert_eq!(&b[..5], b"ccccc");
        }

        assert!(cache.get(k(1)).is_none());
        assert_eq!(&cache.get(k(0)).unwrap()[..5], b"aaaaa");
        assert_eq!(&cache.get(k(2)).unwrap()[..5], b"ccccc");
    }

    #[test]
    fn parallel_dedup() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU32;

        let cache = Arc::new(Cache::new(4, PAGE, Global));
        let fetches = Arc::new(AtomicU32::new(0));
        let d = Md5Digest(7);

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let fetches = fetches.clone();
                std::thread::spawn(move || {
                    let key = BlockKey {
                        digest: d,
                        block: 0,
                    };
                    let b = cache.get_or_fetch(key, |buf| {
                        fetches.fetch_add(1, SeqCst);
                        buf[..3].copy_from_slice(b"xyz");
                    });
                    assert_eq!(&b[..3], b"xyz");
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        // all 8 threads asked for the same block; it must only be fetched once
        assert_eq!(fetches.load(SeqCst), 1);
    }
}
