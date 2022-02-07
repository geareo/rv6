use core::cell::UnsafeCell;
use core::mem;
use core::ops::Deref;

use pin_project::pin_project;
use spin::Once;

use super::{
    FcntlFlags, FileName, FileSystem, Inode, InodeGuard, InodeType, Itable, Path, RcInode, Stat, Tx,
};
use crate::util::strong_pin::StrongPin;
use crate::{
    bio::Buf,
    file::{FileType, InodeFileType},
    hal::hal,
    param::BSIZE,
    proc::KernelCtx,
};

mod inode;
mod segment;
mod superblock;

pub use inode::{Dinode, Dirent, InodeInner, DIRENT_SIZE, DIRSIZ};
pub use segment::Segment;
pub use superblock::{Superblock, IPB};

/// root i-number
const ROOTINO: u32 = 1;

/// the number of segments
/// TODO: supposed to be DISK_SIZE / SEGSIZE
const NSEGMENT: usize = 10;

const NDIRECT: usize = 12;
const NINDIRECT: usize = BSIZE.wrapping_div(mem::size_of::<u32>());
const MAXFILE: usize = NDIRECT.wrapping_add(NINDIRECT);

#[pin_project]
pub struct Lfs {
    /// Initializing superblock should run only once because forkret() calls FileSystem::init().
    /// There should be one superblock per disk device, but we run with only one device.
    superblock: Once<Superblock>,

    /// Segments to save updates
    #[allow(dead_code)]
    segments: [Segment; NSEGMENT]
}

impl Tx<'_, Lfs> {
    /// Caller has modified b->data and is done with the buffer.
    /// Record the block number and pin in the cache by increasing refcnt.
    /// commit()/write_log() will do the disk write.
    ///
    /// write() replaces write(); a typical use is:
    ///   bp = kernel.fs().disk.read(...)
    ///   modify bp->data[]
    ///   write(bp)
    fn write(&self, _b: Buf, _ctx: &KernelCtx<'_, '_>) {
        // self.fs.log().lock().write(b, ctx);
        todo!()
    }

    /// Zero a block.
    #[allow(dead_code)]
    fn bzero(&self, dev: u32, bno: u32, ctx: &KernelCtx<'_, '_>) {
        let mut buf = ctx.kernel().bcache().get_buf(dev, bno).lock(ctx);
        buf.deref_inner_mut().data.fill(0);
        buf.deref_inner_mut().valid = true;
        self.write(buf, ctx);
    }

    /// Blocks.
    /// Allocate a zeroed disk block.
    #[allow(dead_code)]
    fn balloc(&self, _dev: u32, _ctx: &KernelCtx<'_, '_>) -> u32 {
        todo!()
        // panic!("balloc: out of blocks");
    }

    /// Free a disk block.
    #[allow(dead_code)]
    fn bfree(&self, _dev: u32, _b: u32, _ctx: &KernelCtx<'_, '_>) {
        todo!()
    }
}

impl Lfs {
    #[allow(dead_code)]
    pub const fn new() -> Self {
        Self {
            superblock: Once::new(),
            segments: [Segment::default(); NSEGMENT]
        }
    }

    #[allow(dead_code)]
    fn superblock(&self) -> &Superblock {
        self.superblock.get().expect("superblock")
    }

    #[allow(clippy::needless_lifetimes)]
    pub fn imap<'s>(self: StrongPin<'s, Self>) -> StrongPin<'s, Itable<Self>> {
        todo!()
    }
}

impl FileSystem for Lfs {
    type Dirent = Dirent;
    type InodeInner = InodeInner;

    fn init(&self, dev: u32, ctx: &KernelCtx<'_, '_>) {
        if !self.superblock.is_completed() {
            let buf = hal().disk().read(dev, 1, ctx);
            let _superblock = self.superblock.call_once(|| Superblock::new(&buf));
            buf.free(ctx);
        }
    }

    fn root(self: StrongPin<'_, Self>) -> RcInode<Self> {
        todo!()
    }

    fn namei(
        self: StrongPin<'_, Self>,
        path: &Path,
        tx: &Tx<'_, Self>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<RcInode<Self>, ()> {
        // name-to-inode translation
        self.imap().namei(path, tx, ctx)
    }

    fn link(
        self: StrongPin<'_, Self>,
        inode: RcInode<Self>,
        path: &Path,
        tx: &Tx<'_, Self>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        // Create another name `path` by linking to inode
        let inode = scopeguard::guard(inode, |ptr| ptr.free((tx, ctx)));
        let ip = inode.lock(ctx);
        let mut ip = scopeguard::guard(ip, |ip| ip.free(ctx));
        if ip.deref_inner().typ == InodeType::Dir {
            return Err(());
        }
        ip.deref_inner_mut().nlink += 1;
        ip.update(tx, ctx);
        drop(ip);

        if let Ok((ptr2, name)) = self.imap().nameiparent(path, tx, ctx) {
            let ptr2 = scopeguard::guard(ptr2, |ptr| ptr.free((tx, ctx)));
            let dp = ptr2.lock(ctx);
            let mut dp = scopeguard::guard(dp, |ip| ip.free(ctx));
            if dp.dev == inode.dev && dp.dirlink(name, inode.inum, tx, ctx).is_ok() {
                return Ok(());
            }
        }

        let ip = inode.lock(ctx);
        let mut ip = scopeguard::guard(ip, |ip| ip.free(ctx));
        ip.deref_inner_mut().nlink -= 1;
        ip.update(tx, ctx);
        Err(())
    }

    fn unlink(
        self: StrongPin<'_, Self>,
        path: &Path,
        tx: &Tx<'_, Self>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        // remove a file with `path`
        let (ptr, name) = self.imap().nameiparent(path, tx, ctx)?;
        let ptr = scopeguard::guard(ptr, |ptr| ptr.free((tx, ctx)));
        let dp = ptr.lock(ctx);
        let mut dp = scopeguard::guard(dp, |ip| ip.free(ctx));

        // Cannot unlink "." or "..".
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            return Err(());
        }

        let (ptr2, off) = dp.dirlookup(name, ctx)?;
        let ptr2 = scopeguard::guard(ptr2, |ptr| ptr.free((tx, ctx)));
        let ip = ptr2.lock(ctx);
        let mut ip = scopeguard::guard(ip, |ip| ip.free(ctx));
        assert!(ip.deref_inner().nlink >= 1, "unlink: nlink < 1");

        if ip.deref_inner().typ == InodeType::Dir && !ip.is_dir_empty(ctx) {
            return Err(());
        }

        dp.write_kernel(&Dirent::default(), off, tx, ctx)
            .expect("unlink: writei");
        if ip.deref_inner().typ == InodeType::Dir {
            dp.deref_inner_mut().nlink -= 1;
            dp.update(tx, ctx);
        }
        drop(dp);
        drop(ptr);
        ip.deref_inner_mut().nlink -= 1;
        ip.update(tx, ctx);
        Ok(())
    }

    fn create<F, T>(
        self: StrongPin<'_, Self>,
        path: &Path,
        typ: InodeType,
        tx: &Tx<'_, Self>,
        ctx: &KernelCtx<'_, '_>,
        f: F,
    ) -> Result<(RcInode<Self>, T), ()>
    where
        F: FnOnce(&mut InodeGuard<'_, Self>) -> T,
    {
        // create a new file with `path`
        let (ptr, name) = self.imap().nameiparent(path, tx, ctx)?;
        let ptr = scopeguard::guard(ptr, |ptr| ptr.free((tx, ctx)));
        let dp = ptr.lock(ctx);
        let mut dp = scopeguard::guard(dp, |ip| ip.free(ctx));
        if let Ok((ptr2, _)) = dp.dirlookup(name, ctx) {
            let ptr2 = scopeguard::guard(ptr2, |ptr| ptr.free((tx, ctx)));
            drop(dp);
            if typ != InodeType::File {
                return Err(());
            }
            let ip = ptr2.lock(ctx);
            let mut ip = scopeguard::guard(ip, |ip| ip.free(ctx));
            if let InodeType::None | InodeType::Dir = ip.deref_inner().typ {
                return Err(());
            }
            let ret = f(&mut ip);
            drop(ip);
            return Ok((scopeguard::ScopeGuard::into_inner(ptr2), ret));
        }

        let ptr2 = self.imap().alloc_inode(dp.dev, typ, tx, ctx);
        let ip = ptr2.lock(ctx);
        let mut ip = scopeguard::guard(ip, |ip| ip.free(ctx));
        ip.deref_inner_mut().nlink = 1;
        ip.update(tx, ctx);

        // Create . and .. entries.
        if typ == InodeType::Dir {
            // for ".."
            dp.deref_inner_mut().nlink += 1;
            dp.update(tx, ctx);

            let inum = ip.inum;
            // No ip->nlink++ for ".": avoid cyclic ref count.
            // SAFETY: b"." does not contain any NUL characters.
            ip.dirlink(unsafe { FileName::from_bytes(b".") }, inum, tx, ctx)
                // SAFETY: b".." does not contain any NUL characters.
                .and_then(|_| ip.dirlink(unsafe { FileName::from_bytes(b"..") }, dp.inum, tx, ctx))
                .expect("create dots");
        }
        dp.dirlink(name, ip.inum, tx, ctx).expect("create: dirlink");
        let ret = f(&mut ip);
        drop(ip);
        Ok((ptr2, ret))
    }

    fn open(
        self: StrongPin<'_, Self>,
        path: &Path,
        omode: FcntlFlags,
        tx: &Tx<'_, Self>,
        ctx: &mut KernelCtx<'_, '_>,
    ) -> Result<usize, ()> {
        // open a file with `path`
        let (ip, typ) = if omode.contains(FcntlFlags::O_CREATE) {
            self.create(path, InodeType::File, tx, ctx, |ip| ip.deref_inner().typ)?
        } else {
            let ptr = self.imap().namei(path, tx, ctx)?;
            let ptr = scopeguard::guard(ptr, |ptr| ptr.free((tx, ctx)));
            let ip = ptr.lock(ctx);
            let ip = scopeguard::guard(ip, |ip| ip.free(ctx));
            let typ = ip.deref_inner().typ;

            if typ == InodeType::Dir && omode != FcntlFlags::O_RDONLY {
                return Err(());
            }
            drop(ip);
            (scopeguard::ScopeGuard::into_inner(ptr), typ)
        };

        let filetype = match typ {
            InodeType::Device { major, .. } => FileType::Device { ip, major },
            _ => {
                FileType::Inode {
                    inner: InodeFileType {
                        ip,
                        off: UnsafeCell::new(0),
                    },
                }
            }
        };

        let f = ctx.kernel().ftable().alloc_file(
            filetype,
            !omode.intersects(FcntlFlags::O_WRONLY),
            omode.intersects(FcntlFlags::O_WRONLY | FcntlFlags::O_RDWR),
        )?;

        if omode.contains(FcntlFlags::O_TRUNC) && typ == InodeType::File {
            match &f.typ {
                // It is safe to call itrunc because ip.lock() is held
                FileType::Device { ip, .. }
                | FileType::Inode {
                    inner: InodeFileType { ip, .. },
                } => {
                    let mut ip = ip.lock(ctx);
                    ip.trunc(tx, ctx);
                    ip.free(ctx);
                }
                _ => panic!("sys_open : Not reach"),
            };
        }
        let fd = f.fdalloc(ctx)?;
        Ok(fd as usize)
    }

    fn chdir(
        self: StrongPin<'_, Self>,
        inode: RcInode<Self>,
        tx: &Tx<'_, Self>,
        ctx: &mut KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        // change the current directory
        let ip = inode.lock(ctx);
        let typ = ip.deref_inner().typ;
        ip.free(ctx);
        if typ != InodeType::Dir {
            inode.free((tx, ctx));
            return Err(());
        }

        mem::replace(ctx.proc_mut().cwd_mut(), inode).free((tx, ctx));
        Ok(())
    }

    fn tx_begin(&self, _ctx: &KernelCtx<'_, '_>) {
        // TODO: begin transaction
        // self.log().begin_op(ctx);
    }

    unsafe fn tx_end(&self, _ctx: &KernelCtx<'_, '_>) {
        // TODO: commit and end transaction
        // self.log().end_op(ctx);
    }

    #[inline]
    fn inode_read<
        'id,
        's,
        K: Deref<Target = KernelCtx<'id, 's>>,
        F: FnMut(u32, &[u8], &mut K) -> Result<(), ()>,
    >(
        guard: &mut InodeGuard<'_, Self>,
        mut off: u32,
        mut n: u32,
        mut f: F,
        mut k: K,
    ) -> Result<usize, ()> {
        // read inode
        let inner = guard.deref_inner();
        if off > inner.size || off.wrapping_add(n) < off {
            return Ok(0);
        }
        if off + n > inner.size {
            n = inner.size - off;
        }
        let mut tot: u32 = 0;
        while tot < n {
            let bp = hal()
                .disk()
                .read(guard.dev, guard.disk(off as usize / BSIZE, &k), &k);
            let m = core::cmp::min(n - tot, BSIZE as u32 - off % BSIZE as u32);
            let begin = (off % BSIZE as u32) as usize;
            let end = begin + m as usize;
            let res = f(tot, &bp.deref_inner().data[begin..end], &mut k);
            bp.free(&k);
            res?;
            tot += m;
            off += m;
        }
        Ok(tot as usize)
    }

    fn inode_write<
        'id,
        's,
        K: Deref<Target = KernelCtx<'id, 's>>,
        F: FnMut(u32, &mut [u8], &mut K) -> Result<(), ()>,
    >(
        guard: &mut InodeGuard<'_, Self>,
        mut off: u32,
        n: u32,
        mut f: F,
        tx: &Tx<'_, Lfs>,
        mut k: K,
    ) -> Result<usize, ()> {
        // write the inode
        if off > guard.deref_inner().size {
            return Err(());
        }
        if off.checked_add(n).ok_or(())? as usize > MAXFILE * BSIZE {
            return Err(());
        }
        let mut tot: u32 = 0;
        while tot < n {
            let mut bp = hal().disk().read(
                guard.dev,
                guard.disk_or_alloc(off as usize / BSIZE, tx, &k),
                &k,
            );
            let m = core::cmp::min(n - tot, BSIZE as u32 - off % BSIZE as u32);
            let begin = (off % BSIZE as u32) as usize;
            let end = begin + m as usize;

            if f(tot, &mut bp.deref_inner_mut().data[begin..end], &mut k).is_ok() {
                // tx.write(bp, &k);
            } else {
                bp.free(&k);
                break;
            }

            tot += m;
            off += m;
        }

        if off > guard.deref_inner().size {
            guard.deref_inner_mut().size = off;
        }

        // Write the i-node back to disk even if the size didn't change
        // because the loop above might have called bmap() and added a new
        // block to self->addrs[].
        guard.update(tx, &k);
        Ok(tot as usize)
    }

    fn inode_trunc(
        _guard: &mut InodeGuard<'_, Self>,
        _tx: &Tx<'_, Self>,
        _ctx: &KernelCtx<'_, '_>,
    ) {
        todo!()
    }

    fn inode_lock<'a>(_inode: &'a Inode<Self>, _ctx: &KernelCtx<'_, '_>) -> InodeGuard<'a, Self> {
        todo!()
    }

    fn inode_finalize<'a, 'id: 'a>(
        _inode: &mut Inode<Self>,
        _tx: &'a Tx<'a, Self>,
        _ctx: &'a KernelCtx<'id, 'a>,
    ) {
        todo!()
    }

    fn inode_stat(_inode: &Inode<Self>, _ctx: &KernelCtx<'_, '_>) -> Stat {
        todo!()
    }
}
