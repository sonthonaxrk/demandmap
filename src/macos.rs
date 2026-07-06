#![allow(unsafe_op_in_unsafe_fn)]

use std::cmp::min;
use std::fmt::Debug;
use std::{collections::BTreeMap, sync::Arc};

use mach2::exception_types::exception_type_t;
use mach2::message::{
    MACH_MSG_TYPE_MAKE_SEND, MACH_RCV_MSG, mach_msg_body_t, mach_msg_port_descriptor_t,
    mach_msg_type_number_t,
};
use mach2::ndr::NDR_record_t;
use mach2::vm::mach_vm_remap;
use mach2::vm_inherit::VM_INHERIT_NONE;
use mach2::vm_prot::vm_prot_t;
use mach2::vm_statistics::VM_FLAGS_OVERWRITE;
use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t};
use mach2::{
    exc::__Reply__exception_raise_t,
    exception_types::{EXC_MASK_BAD_ACCESS, EXCEPTION_DEFAULT, MACH_EXCEPTION_CODES},
    kern_return::KERN_SUCCESS,
    mach_port::{mach_port_allocate, mach_port_insert_right},
    message::{
        MACH_MSG_TIMEOUT_NONE, MACH_MSGH_BITS_REMOTE_MASK, MACH_SEND_MSG, mach_msg,
        mach_msg_header_t,
    },
    port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t},
    task::task_set_exception_ports,
    thread_status::THREAD_STATE_NONE,
};

use crate::slots::{BlockKey, Cache, DiskAlloc, FixedSizeByteAlloc, PtrRange, ReqData};
use crate::{AlignDown, PAGE_SIZE};

#[repr(C, packed(4))]
#[allow(dead_code, non_snake_case)]
#[derive(Copy, Clone, Debug)]
pub struct MemFaultExceptionRaise {
    pub Head: mach_msg_header_t,
    /* start of the kernel processed data */
    pub msgh_body: mach_msg_body_t,
    pub thread: mach_msg_port_descriptor_t,
    pub task: mach_msg_port_descriptor_t,
    /* end of the kernel processed data */
    pub NDR: NDR_record_t,
    pub exception: exception_type_t,
    pub codeCnt: mach_msg_type_number_t,
    pub code: (u64, *mut u8),
}

fn get_containing<R: Clone>(
    map: &std::sync::Mutex<BTreeMap<PtrRange, R>>,
    p: *const u8,
) -> Option<(PtrRange, R)> {
    let map = map.lock().unwrap();
    let (&range, v) = map
        .range(..=PtrRange(p, usize::MAX as *const u8))
        .next_back()?;
    let PtrRange(start, end) = range;
    (start <= p && p < end).then(|| (range, v.clone()))
}

impl<R: std::fmt::Debug + Send + Sync + 'static> DiskAlloc<R> {
    pub unsafe fn init_faulter(
        _base: *const u8,
        req: Arc<std::sync::Mutex<BTreeMap<PtrRange, Arc<ReqData<R>>>>>,
        cache: Arc<Cache<FixedSizeByteAlloc>>,
    ) {
        let mut port: mach_port_t = 0;
        let task = mach2::traps::mach_task_self();
        mach_port_allocate(task, MACH_PORT_RIGHT_RECEIVE, &mut port);
        mach_port_insert_right(task, port, port, MACH_MSG_TYPE_MAKE_SEND);
        task_set_exception_ports(
            task,
            EXC_MASK_BAD_ACCESS,
            port,
            (EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES) as _,
            THREAD_STATE_NONE,
        );

        let _t = std::thread::spawn(move || {
            let res = std::panic::catch_unwind(|| {
                loop {
                    let mut buf = [0u8; 1024];
                    let hdr = buf.as_mut_ptr() as *mut mach_msg_header_t;

                    let _res = mach_msg(
                        hdr,
                        MACH_RCV_MSG,
                        0,
                        buf.len() as _,
                        port,
                        MACH_MSG_TIMEOUT_NONE,
                        MACH_PORT_NULL,
                    );

                    let resp = hdr as *mut MemFaultExceptionRaise;
                    let resp = &*resp;
                    let fault = resp.code.1;
                    let page = fault.align_down(PAGE_SIZE);
                    let (ptr_range, data) = get_containing(&req, page).unwrap();
                    let block_size = cache.block_size();

                    let base = ptr_range.0 as usize;
                    let total = ptr_range.1 as usize - base;
                    let block_number = (page as usize - base) / block_size;
                    let start = block_number * block_size;
                    let end = min(total, start + block_size);


                    // println!("{:p}", page);
                    // println!("Block_number {} {:x}", block_number, data.md5_digest.0);

                    let _slice = std::slice::from_raw_parts_mut(page, block_size as _);
                    //
                    // println!("created slice");

                    let cache_page = cache.get_or_fetch(
                        BlockKey {
                            digest: data.md5_digest,
                            block: block_number as _,
                        },
                        |buf| {
                            (data.cb)(&data.data, start..end - 1, &mut buf[..end - start]);
                        },
                    );
                    //
                    // println!("got cache page");

                    let src = cache_page.ptr as mach_vm_address_t;
                    let mut dst = (base + start) as mach_vm_address_t;
                    let remap = min(block_size, (total - start + PAGE_SIZE - 1) & !(PAGE_SIZE - 1));
                    let size = remap as mach_vm_size_t;

                    // println!("{size}");
                    let mut cur: vm_prot_t = 0;
                    let mut max: vm_prot_t = 0;

                    let kr = mach_vm_remap(
                        task,
                        &mut dst,
                        size,
                        0,
                        VM_FLAGS_OVERWRITE,
                        task,
                        src,
                        // copy on write - means things hold the page
                        1,
                        &mut cur,
                        &mut max,
                        VM_INHERIT_NONE,
                    );

                    // println!("vm_remap");

                    assert_eq!(kr, KERN_SUCCESS, "vm_remap failed: {kr}");

                    drop(cache_page);

                    let mut reply = __Reply__exception_raise_t {
                        Head: mach_msg_header_t {
                            msgh_bits: resp.Head.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK,
                            msgh_size: size_of::<__Reply__exception_raise_t>() as _,
                            msgh_remote_port: resp.Head.msgh_remote_port,
                            msgh_local_port: MACH_PORT_NULL,
                            msgh_voucher_port: MACH_PORT_NULL,
                            // literally part of the api spec for "return"
                            msgh_id: 2505,
                        },
                        NDR: resp.NDR,
                        RetCode: KERN_SUCCESS,
                    };

                    let _kr = mach_msg(
                        &mut reply.Head,
                        MACH_SEND_MSG,
                        size_of::<__Reply__exception_raise_t>() as _,
                        0,
                        MACH_PORT_NULL,
                        MACH_MSG_TIMEOUT_NONE,
                        MACH_PORT_NULL,
                    );
                }
            });


            dbg!(res.unwrap_err());
        });
    }
}
