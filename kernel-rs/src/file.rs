//! Support functions for system calls that involve file descriptors.

use crate::{
    arena::{Arena, ArenaObject, ArrayArena, Rc},
    fs::RcInode,
    kernel::kernel,
    param::{BSIZE, MAXOPBLOCKS, NFILE},
    pipe::AllocatedPipe,
    proc::{myproc, Proc},
    spinlock::Spinlock,
    stat::Stat,
    vm::UVAddr,
};
use core::{cell::UnsafeCell, cmp, convert::TryFrom, mem, ops::Deref, slice};

pub struct File {
    pub typ: FileType,
    readable: bool,
    writable: bool,
}

// TODO: will be infered as we wrap *mut Pipe and *mut Inode.
unsafe impl Send for File {}

pub enum FileType {
    None,
    Pipe { pipe: AllocatedPipe },
    Inode { ip: RcInode, off: UnsafeCell<u32> },
    Device { ip: RcInode, major: u16 },
}

impl Default for FileType {
    fn default() -> Self {
        Self::None
    }
}

/// map major device number to device functions.
#[derive(Copy, Clone)]
pub struct Devsw {
    pub read: Option<unsafe fn(_: UVAddr, _: i32) -> i32>,
    pub write: Option<unsafe fn(_: UVAddr, _: i32) -> i32>,
}

#[derive(Clone)]
pub struct FTableTag {}

impl Deref for FTableTag {
    type Target = Spinlock<ArrayArena<File, NFILE>>;

    fn deref(&self) -> &Self::Target {
        &kernel().ftable
    }
}

pub type RcFile = Rc<<FTableTag as Deref>::Target, FTableTag>;

impl RcFile {
    /// Allocate a file structure.
    pub fn alloc(typ: FileType, readable: bool, writable: bool) -> Option<Self> {
        // TODO: idiomatic initialization.
        FTableTag {}.alloc(|p| {
            *p = File::new(typ, readable, writable);
        })
    }
}

impl File {
    pub const fn new(typ: FileType, readable: bool, writable: bool) -> Self {
        Self {
            typ,
            readable,
            writable,
        }
    }

    pub const fn zero() -> Self {
        Self::new(FileType::None, false, false)
    }

    /// Get metadata about file self.
    /// addr is a user virtual address, pointing to a struct stat.
    pub unsafe fn stat(&self, addr: UVAddr) -> Result<(), ()> {
        let p: *mut Proc = myproc();

        match &self.typ {
            FileType::Inode { ip, .. } | FileType::Device { ip, .. } => {
                let mut st = ip.stat();
                (*(*p).data.get()).pagetable.copyout(
                    addr,
                    slice::from_raw_parts_mut(
                        &mut st as *mut Stat as *mut u8,
                        mem::size_of::<Stat>() as usize,
                    ),
                )
            }
            _ => Err(()),
        }
    }

    /// Read from file self.
    /// addr is a user virtual address.
    pub unsafe fn read(&self, addr: UVAddr, n: i32) -> Result<usize, ()> {
        if !self.readable {
            return Err(());
        }

        match &self.typ {
            FileType::Pipe { pipe } => pipe.read(addr, usize::try_from(n).unwrap_or(0)),
            FileType::Inode { ip, off } => {
                let tx = kernel().fs().begin_transaction();
                let mut ip = ip.deref().lock(&tx);
                let curr_off = *off.get();
                let ret = ip.read(addr, curr_off, n as u32);
                if let Ok(v) = ret {
                    *off.get() = curr_off.wrapping_add(v as u32);
                }
                drop(ip);
                ret
            }
            FileType::Device { major, .. } => kernel()
                .devsw
                .get(*major as usize)
                .and_then(|dev| Some(dev.read?(addr, n) as usize))
                .ok_or(()),
            FileType::None => panic!("File::read"),
        }
    }
    /// Write to file self.
    /// addr is a user virtual address.
    pub unsafe fn write(&self, addr: UVAddr, n: i32) -> Result<usize, ()> {
        if !self.writable {
            return Err(());
        }

        match &self.typ {
            FileType::Pipe { pipe } => pipe.write(addr, usize::try_from(n).unwrap_or(0)),
            FileType::Inode { ip, off } => {
                // write a few blocks at a time to avoid exceeding
                // the maximum log transaction size, including
                // i-node, indirect block, allocation blocks,
                // and 2 blocks of slop for non-aligned writes.
                // this really belongs lower down, since write()
                // might be writing a device like the console.
                let max = (MAXOPBLOCKS - 1 - 1 - 2) / 2 * BSIZE;
                for bytes_written in (0..n).step_by(max) {
                    let bytes_to_write = cmp::min(n - bytes_written, max as i32);
                    let tx = kernel().fs().begin_transaction();
                    let mut ip = ip.deref().lock(&tx);
                    let curr_off = *off.get();
                    let bytes_written = ip
                        .write(
                            addr + bytes_written as usize,
                            curr_off,
                            bytes_to_write as u32,
                        )
                        .map(|v| {
                            *off.get() = curr_off.wrapping_add(v as u32);
                            v
                        });
                    assert!(
                        bytes_written? == bytes_to_write as usize,
                        "short File::write"
                    );
                }
                Ok(n as usize)
            }
            FileType::Device { major, .. } => kernel()
                .devsw
                .get(*major as usize)
                .and_then(|dev| Some(dev.write?(addr, n) as usize))
                .ok_or(()),
            FileType::None => panic!("File::read"),
        }
    }
}

impl ArenaObject for File {
    fn finalize<'s, A: Arena>(&'s mut self, guard: &'s mut A::Guard<'_>) {
        A::reacquire_after(guard, || {
            let typ = mem::replace(&mut self.typ, FileType::None);
            match typ {
                FileType::Pipe { mut pipe } => unsafe { pipe.close(self.writable) },
                FileType::Inode { ip, .. } | FileType::Device { ip, .. } => {
                    let _tx = kernel().fs().begin_transaction();
                    drop(ip);
                }
                _ => (),
            }
        });
    }
}
