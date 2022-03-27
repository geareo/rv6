/// Driver for qemu's virtio disk device.
/// Uses qemu's mmio interface to virtio.
/// qemu presents a "legacy" virtio interface.
///
/// qemu ... -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
use core::array::IntoIter;
use core::marker::PhantomPinned;
use core::mem;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{fence, Ordering};

use array_macro::array;
use arrayvec::ArrayVec;
use bitmaps::Bitmap;
use cfg_if::cfg_if;
use const_zero::const_zero;
use pin_project::pin_project;
use static_assertions::const_assert;

use super::{
    MmioRegs, VirtIOFeatures, VirtIOStatus, VirtqAvail, VirtqDesc, VirtqDescFlags, VirtqUsed, NUM,
    VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};
use crate::{
    addr::{PGSHIFT, PGSIZE},
    bio::Buf,
    kernel::KernelRef,
    lock::{SleepableLock, SleepableLockGuard},
    param::BSIZE,
    proc::KernelCtx,
};

cfg_if! {
    if #[cfg(feature = "lfs")] {
        use crate::param::SEGSIZE;
        // Sequential write in a unit of one segment.
        const MAX_SEQ_WRITE: usize = SEGSIZE;
    } else {
        // Do not support sequential write.
        // Thus, `SleepableLock<VirtioDisk>::write_sequential` is the same as a normal write.
        const MAX_SEQ_WRITE: usize = 1;
    }
}

// It must be page-aligned.
// It needs repr(C) because it is read by device.
// https://github.com/kaist-cp/rv6/issues/52
#[repr(C, align(4096))]
#[pin_project]
pub struct VirtioDisk {
    /// The first region is a set (not a ring) of DMA descriptors, with which
    /// the driver tells the device where to read and write individual disk
    /// operations. There are NUM descriptors. Most commands consist of a
    /// "chain" (a linked list) of a couple of these descriptors.
    desc: [VirtqDesc; NUM],

    /// The next is a ring in which the driver writes descriptor numbers that
    /// the driver would like the device to process. It only includes the head
    /// descriptor of each chain. The ring has NUM elements.
    avail: VirtqAvail,

    /// Finally a ring in which the device writes descriptor numbers that the
    /// device has finished processing (just the head of each chain). There are
    /// NUM used ring entries.
    used: VirtqUsed,

    #[pin]
    info: DiskInfo,
}

// It must be page-aligned because a virtqueue (desc + avail + used) occupies
// two or more physically-contiguous pages.
#[repr(align(4096))]
#[pin_project]
struct DiskInfo {
    /// is a descriptor allocated?
    allocated: Bitmap<NUM>,

    /// we've looked this far in used.
    used_idx: u16,

    /// Track info about in-flight operations, for use when completion
    /// interrupt arrives. Indexed by first descriptor index of chain.
    inflight: [InflightInfo; NUM],

    /// Disk command headers. One-for-one with descriptors, for convenience.
    ops: [VirtIOBlockOutHeader; NUM],

    #[pin]
    _marker: PhantomPinned,
}

/// # Safety
///
/// `b` refers to a valid `Buf` unless it is null.
#[derive(Copy, Clone)]
struct InflightInfo {
    b: *mut Buf,
    status: bool,
}

/// The format of the first descriptor in a disk request. To be followed by two
/// more descriptors containing the block, and a one-byte status.
// It needs repr(C) because it is read by device.
// https://github.com/kaist-cp/rv6/issues/52
#[repr(C)]
#[derive(Copy, Clone)]
struct VirtIOBlockOutHeader {
    typ: u32,
    reserved: u32,
    sector: usize,
}

impl VirtioDisk {
    /// # Safety
    ///
    /// It must be used only after initializing it with `VirtioDisk::init`.
    pub const unsafe fn new() -> Self {
        Self {
            desc: [VirtqDesc::new(); NUM],
            avail: VirtqAvail::new(),
            used: VirtqUsed::new(),
            info: DiskInfo::new(),
        }
    }
}

impl DiskInfo {
    const fn new() -> Self {
        Self {
            // SAFETY: bitmap is safe to be zero-initialized.
            allocated: unsafe { const_zero!(Bitmap::<NUM>) },
            used_idx: 0,
            inflight: [InflightInfo::new(); NUM],
            ops: [VirtIOBlockOutHeader::default(); NUM],
            _marker: PhantomPinned,
        }
    }
}

impl InflightInfo {
    const fn new() -> Self {
        Self {
            b: ptr::null_mut(),
            status: false,
        }
    }
}

impl VirtIOBlockOutHeader {
    fn new(write: bool, sector: usize) -> Self {
        let typ = if write {
            VIRTIO_BLK_T_OUT
        } else {
            VIRTIO_BLK_T_IN
        };

        Self {
            typ,
            reserved: 0,
            sector,
        }
    }
}

impl const Default for VirtIOBlockOutHeader {
    fn default() -> Self {
        Self {
            typ: 0,
            reserved: 0,
            sector: 0,
        }
    }
}

/// A descriptor allocated by driver.
#[derive(Debug)]
struct Descriptor {
    idx: usize,
}

impl Descriptor {
    fn new(idx: usize) -> Self {
        Self { idx }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // HACK(@efenniht): we really need linear type here:
        // https://github.com/rust-lang/rfcs/issues/814
        panic!("Descriptor must never drop. Use Disk::free instead.");
    }
}

impl SleepableLock<VirtioDisk> {
    /// Return a locked Buf with the `latest` contents of the indicated block.
    /// If buf.valid is true, we don't need to access Disk.
    pub fn read(self: Pin<&Self>, dev: u32, blockno: u32, ctx: &KernelCtx<'_, '_>) -> Buf {
        let mut buf = ctx.kernel().bcache().get_buf(dev, blockno).lock(ctx);
        if !buf.deref_inner().valid {
            VirtioDisk::rw(&mut self.pinned_lock(), &mut buf, false, ctx);
            buf.deref_inner_mut().valid = true;
        }
        buf
    }

    pub fn write(self: Pin<&Self>, b: &mut Buf, ctx: &KernelCtx<'_, '_>) {
        VirtioDisk::rw(&mut self.pinned_lock(), b, true, ctx)
    }

    pub fn write_sequential(
        self: Pin<&Self>,
        barray: &mut [Option<Buf>; MAX_SEQ_WRITE],
        ctx: &KernelCtx<'_, '_>,
    ) {
        VirtioDisk::write_seq(&mut self.pinned_lock(), barray, ctx)
    }
}

impl VirtioDisk {
    pub fn init(self: Pin<&Self>) {
        let mut status: VirtIOStatus = VirtIOStatus::empty();

        // MMIO registers are located below KERNBASE, while kernel text and data
        // are located above KERNBASE, so we can safely read/write MMIO registers.
        MmioRegs::check_virtio_disk();
        status.insert(VirtIOStatus::ACKNOWLEDGE);
        MmioRegs::set_status(&status);
        status.insert(VirtIOStatus::DRIVER);
        MmioRegs::set_status(&status);

        // Negotiate features
        let features = MmioRegs::get_features()
            - (VirtIOFeatures::BLK_F_RO
                | VirtIOFeatures::BLK_F_SCSI
                | VirtIOFeatures::BLK_F_CONFIG_WCE
                | VirtIOFeatures::BLK_F_MQ
                | VirtIOFeatures::F_ANY_LAYOUT
                | VirtIOFeatures::RING_F_EVENT_IDX
                | VirtIOFeatures::RING_F_INDIRECT_DESC);

        MmioRegs::set_features(&features);

        // Tell device that feature negotiation is complete.
        status.insert(VirtIOStatus::FEATURES_OK);
        MmioRegs::set_status(&status);

        // Tell device we're completely ready.
        status.insert(VirtIOStatus::DRIVER_OK);
        MmioRegs::set_status(&status);
        // SAFETY: page size is `PGSIZE`.
        unsafe {
            MmioRegs::set_pg_size(PGSIZE as _);
        }

        // Initialize queue 0.
        unsafe {
            MmioRegs::select_and_init_queue(
                0,
                NUM as _,
                (self.desc.as_ptr() as usize >> PGSHIFT) as _,
            );
        }

        // plic.rs and trap.rs arrange for interrupts from VIRTIO0_IRQ.
    }

    // This method reads and writes disk by reading and writing MMIO registers.
    // By the construction of the kernel page table in KernelMemory::new, the
    // virtual addresses of the MMIO registers are mapped to the proper physical
    // addresses. Therefore, this method is safe.
    fn rw(
        guard: &mut SleepableLockGuard<'_, Self>,
        b: &mut Buf,
        write: bool,
        ctx: &KernelCtx<'_, '_>,
    ) {
        let sector: usize = (*b).blockno as usize * (BSIZE / 512);

        // The spec's Section 5.2 says that legacy block operations use
        // three descriptors: one for type/reserved/sector, one for the
        // data, one for a 1-byte status result.

        // Allocate the three descriptors.
        let desc = loop {
            match guard.get_pin_mut().alloc_three_descriptors() {
                Some(idx) => break idx,
                // We do not need wakeup for the None case:
                // * alloc_three_descriptors can be executed by one thread at
                //   once. Thus, we do not need to consider interleaving of
                //   alloc_three_descriptors.
                // * If alloc_three_descriptors fails, it frees only the
                //   descriptors that it created. It does not increase the
                //   number of free descriptors. Therefore, sleeping threads
                //   do not need to wake up, as alloc_three_descriptors will
                //   still fail.
                None => guard.sleep(ctx),
            }
        };

        let mut this = guard.get_pin_mut().project();
        let mut info = this.info.project();

        // Format the three descriptors.
        // qemu's virtio-blk.c reads them.

        // 1. Set the first descriptor.
        let buf0 = &mut info.ops[desc[0].idx];
        *buf0 = VirtIOBlockOutHeader::new(write, sector);

        this.desc[desc[0].idx] = VirtqDesc {
            addr: buf0 as *const _ as _,
            len: mem::size_of::<VirtIOBlockOutHeader>() as _,
            flags: VirtqDescFlags::NEXT,
            next: desc[1].idx as _,
        };

        // 2. Set the second descriptor.
        // Device reads/writes b->data
        this.desc[desc[1].idx] = VirtqDesc {
            addr: b.deref_inner().data.as_ptr() as _,
            len: BSIZE as _,
            flags: if write {
                VirtqDescFlags::NEXT
            } else {
                VirtqDescFlags::NEXT | VirtqDescFlags::WRITE
            },
            next: desc[2].idx as _,
        };

        // 3. Set the third descriptor.
        // device writes 0 on success
        info.inflight[desc[0].idx].status = true;

        // Device writes the status
        this.desc[desc[2].idx] = VirtqDesc {
            addr: &info.inflight[desc[0].idx].status as *const _ as _,
            len: 1,
            flags: VirtqDescFlags::WRITE,
            next: 0,
        };

        // Record struct Buf for virtio_disk_intr().
        b.deref_inner_mut().disk = true;
        // It does not break the invariant because b is &mut Buf, which refers
        // to a valid Buf.
        info.inflight[desc[0].idx].b = b;

        // Tell the device the first index in our chain of descriptors.
        let ring_idx = this.avail.idx as usize % NUM;
        this.avail.ring[ring_idx] = desc[0].idx as _;

        fence(Ordering::SeqCst);

        // Tell the device another avail ring entry is available.
        this.avail.idx += 1;

        fence(Ordering::SeqCst);

        // SAFETY: the all three descriptors' fields are well set.
        // Value is queue number.
        unsafe {
            MmioRegs::notify_queue(0);
        }

        // Wait for virtio_disk_intr() to say request has finished.
        b.vdisk_request_waitchannel.sleep(guard, ctx);

        // As it assigns null, the invariant of inflight is maintained even if
        // b: &mut Buf becomes invalid after this method returns.
        guard.get_pin_mut().project().info.project().inflight[desc[0].idx].b = ptr::null_mut();
        IntoIter::new(desc).for_each(|desc| guard.get_pin_mut().free(desc));
        guard.wakeup(ctx.kernel());
    }

    // This method writes the data in the given buffers to disk sequentially, therefore,
    // minimizing seek-time overhead of the disk. It is designed for using with `Lfs`.
    // Calling this method when using `Ufs` is effectively not different from calling
    // `VirtioDisk::rw` and may incur additional performance overhead.
    fn write_seq(
        guard: &mut SleepableLockGuard<'_, Self>,
        barray: &mut [Option<Buf>; MAX_SEQ_WRITE],
        ctx: &KernelCtx<'_, '_>,
    ) {
        // Maximum # of blocks to be sequentially written to the disk must be less than
        // the total # of available descriptors excluding two descriptors, which will be
        // allocated for the header and tailer descriptors.
        const_assert!(MAX_SEQ_WRITE + 2 <= NUM);

        let mut darray: [Option<Descriptor>; MAX_SEQ_WRITE] = array![_ => None; MAX_SEQ_WRITE];
        let mut prev_blk_idx = 0usize;
        let mut next_write = 0usize;
        let mut header: Option<Descriptor> = None; // To reserve a header descriptor.
        let mut tailer: Option<Descriptor> = None; // To reserve a tailer descriptor.

        for i in 0..MAX_SEQ_WRITE {
            if barray[i].is_some() {
                let next_desc = loop {
                    // Allocate a header and tailer descriptor if not already allocated.
                    // Invariant: `header` and `tailer` are always allocated together.
                    if tailer.is_none() {
                        // `None` means this is the beginning of a new descriptor chain.
                        match guard.get_pin_mut().alloc() {
                            None => {
                                guard.sleep(ctx);
                                continue;
                            }
                            idx => header = idx,
                        }
                        match guard.get_pin_mut().alloc() {
                            None => {
                                guard.get_pin_mut().as_mut().free(header.unwrap());
                                header = None;
                                continue;
                            }
                            idx => tailer = idx,
                        }
                    }

                    // Allocate a descriptor for the buffer.
                    match guard.get_pin_mut().alloc() {
                        Some(idx) => break idx,
                        // Write the previously allocated descriptors to the disk.
                        // If `Self::finalize_write_seq` returns false, zero buffer descriptor can be allocated,
                        // thus resulting in deadlock.
                        None => {
                            assert!(
                                Self::finalize_write_seq(
                                    guard,
                                    &mut darray[next_write..=prev_blk_idx],
                                    &mut barray[next_write..=prev_blk_idx],
                                    header.take().unwrap(),
                                    tailer.take().unwrap(),
                                    ctx,
                                ),
                                "could not allocate buffer descriptors"
                            );
                            next_write = prev_blk_idx + 1;
                        }
                    };
                };

                let buf = &mut barray[i].as_mut().unwrap();

                // Set the descriptor of the buffer
                let this = guard.get_pin_mut().project();
                this.desc[tailer.as_ref().unwrap().idx] = VirtqDesc {
                    addr: buf.deref_inner().data.as_ptr() as _,
                    len: BSIZE as _,
                    flags: VirtqDescFlags::NEXT,
                    next: 0,
                };

                // Chain with the previous descriptor
                if let Some(prev_d) = &darray[prev_blk_idx] {
                    this.desc[prev_d.idx].next = tailer.as_ref().unwrap().idx as _;
                }

                buf.deref_inner_mut().disk = true;

                darray[i] = tailer.replace(next_desc);
                prev_blk_idx = i;
            }
        }

        assert!(
            Self::finalize_write_seq(
                guard,
                &mut darray[next_write..=prev_blk_idx],
                &mut barray[next_write..=prev_blk_idx],
                header.take().unwrap(),
                tailer.take().unwrap(),
                ctx,
            ),
            "could not finalize the last set of buffers"
        );
    }

    // This method writes the buffers in the given range of the given array to the disk.
    fn finalize_write_seq(
        guard: &mut SleepableLockGuard<'_, Self>,
        darray: &mut [Option<Descriptor>],
        barray: &mut [Option<Buf>],
        hdesc: Descriptor,
        tdesc: Descriptor,
        ctx: &KernelCtx<'_, '_>,
    ) -> bool {
        let mut start = 0usize;
        let end = darray.len();
        assert_eq!(end, barray.len());

        // Asserts that the chain contains at least one buffer to be written.
        if end == 0 || darray[end - 1].is_none() {
            guard.get_pin_mut().as_mut().free(hdesc);
            guard.get_pin_mut().as_mut().free(tdesc);
            return false;
        }

        while darray[start].is_none() {
            start += 1;
        }

        let fbdesc = &darray[start].as_ref().unwrap();
        let lbdesc = &darray[end - 1].as_ref().unwrap();

        let mut this = guard.get_pin_mut().project();
        let mut info = this.info.project();
        let sector: usize = (barray[start].as_ref().unwrap()).blockno as usize * (BSIZE / 512);

        let buf0 = &mut info.ops[hdesc.idx];
        *buf0 = VirtIOBlockOutHeader::new(true, sector);

        // Set the header and tailer descriptors and finalize the chain.
        this.desc[hdesc.idx] = VirtqDesc {
            addr: buf0 as *const _ as _,
            len: mem::size_of::<VirtIOBlockOutHeader>() as _,
            flags: VirtqDescFlags::NEXT,
            next: fbdesc.idx as _,
        };
        info.inflight[hdesc.idx].status = true;
        this.desc[tdesc.idx] = VirtqDesc {
            addr: &info.inflight[hdesc.idx].status as *const _ as _,
            len: 1,
            flags: VirtqDescFlags::WRITE,
            next: 0,
        };
        this.desc[lbdesc.idx].next = tdesc.idx as _;

        // The request is complete when the last block is written.
        // TODO: This may require that the `VIRTIO_F_IN_ORDER` feature has been negotiated.
        info.inflight[hdesc.idx].b = &mut *barray[end - 1].as_mut().unwrap();

        let ring_idx = this.avail.idx as usize % NUM;
        this.avail.ring[ring_idx] = hdesc.idx as _;
        fence(Ordering::SeqCst);

        // Tell the device another avail ring entry is available.
        this.avail.idx += 1;

        fence(Ordering::SeqCst);

        // SAFETY: all the three descriptors' fields are well set.
        // Value is queue number.
        unsafe {
            MmioRegs::notify_queue(0);
        }

        // Wait for the disk to finishing writing the entire chain
        (&barray[end - 1].as_ref().unwrap())
            .vdisk_request_waitchannel
            .sleep(guard, ctx);

        // Clean up
        guard.get_pin_mut().project().info.project().inflight[hdesc.idx].b = ptr::null_mut();

        let mut this = guard.get_pin_mut();
        this.as_mut().free(hdesc);

        for i in start..end {
            if let Some(d) = darray[i].take() {
                this.as_mut().free(d);
                let buf = &mut barray[i].as_mut().unwrap();
                buf.deref_inner_mut().disk = false;
            }
        }

        this.as_mut().free(tdesc);

        guard.wakeup(ctx.kernel());

        return true;
    }

    pub fn intr(self: Pin<&mut Self>, kernel: KernelRef<'_, '_>) {
        // The device won't raise another interrupt until we tell it
        // we've seen this interrupt, which the following line does.
        // This may race with the device writing new entries to
        // the "used" ring, in which case we may process the new
        // completion entries in this interrupt, and have nothing to do
        // in the next interrupt, which is harmless.
        MmioRegs::intr_ack_all();

        fence(Ordering::SeqCst);

        // The device increments disk.used->idx when it
        // adds an entry to the used ring.

        let this = self.project();
        let info = this.info.project();

        while *info.used_idx != this.used.id {
            fence(Ordering::SeqCst);
            let id = this.used.ring[(*info.used_idx as usize) % NUM].id as usize;

            assert!(!info.inflight[id].status, "Disk::intr status");

            // SAFETY: from the invariant, b refers to a valid
            // buffer unless it is null.
            let buf = unsafe { &mut *info.inflight[id].b };

            // disk is done with buf
            buf.deref_inner_mut().disk = false;
            buf.vdisk_request_waitchannel.wakeup(kernel);

            *info.used_idx += 1;
        }
    }

    /// Find a free descriptor, mark it non-free, return its index.
    fn alloc(self: Pin<&mut Self>) -> Option<Descriptor> {
        let info = self.project().info.project();
        let idx = info.allocated.first_false_index()?;
        let _ = info.allocated.set(idx, true);
        Some(Descriptor::new(idx))
    }

    /// Allocate three descriptors (they need not be contiguous).
    /// Disk transfers always use three descriptors.
    fn alloc_three_descriptors(mut self: Pin<&mut Self>) -> Option<[Descriptor; 3]> {
        let mut descs = ArrayVec::<_, 3>::new();

        for _ in 0..3 {
            if let Some(desc) = self.as_mut().alloc() {
                descs.push(desc);
            } else {
                for desc in descs {
                    self.as_mut().free(desc);
                }
                return None;
            }
        }

        descs.into_inner().ok()
    }

    fn free(self: Pin<&mut Self>, desc: Descriptor) {
        let this = self.project();
        let idx = desc.idx;
        this.desc[idx].addr = 0;
        this.desc[idx].len = 0;
        this.desc[idx].flags = VirtqDescFlags::FREED;
        this.desc[idx].next = 0;
        assert!(this.info.project().allocated.set(idx, false), "Disk::free");
        mem::forget(desc);
    }
}
