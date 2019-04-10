#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![feature(alloc)]
#![feature(const_str_len)]

extern crate alloc;
#[macro_use]
extern crate log;

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
    boxed::Box,
    vec,
};
use core::any::Any;
use core::fmt::{Debug, Error, Formatter};
use core::mem::uninitialized;

use bitvec::BitVec;
use spin::RwLock;

use rcore_fs::dev::Device;
use rcore_fs::dirty::Dirty;
use rcore_fs::util::*;
use rcore_fs::vfs::{self, FileSystem, FsError, INode, Timespec};

use self::structs::*;

mod structs;
#[cfg(test)]
mod tests;

trait DeviceExt: Device {
    fn read_block(&self, id: BlockId, offset: usize, buf: &mut [u8]) -> vfs::Result<()> {
        debug_assert!(offset + buf.len() <= BLKSIZE);
        match self.read_at(id * BLKSIZE + offset, buf) {
            Some(len) if len == buf.len() => Ok(()),
            _ => panic!("cannot read block {} offset {} from device", id, offset),
        }
    }
    fn write_block(&self, id: BlockId, offset: usize, buf: &[u8]) -> vfs::Result<()> {
        debug_assert!(offset + buf.len() <= BLKSIZE);
        match self.write_at(id * BLKSIZE + offset, buf) {
            Some(len) if len == buf.len() => Ok(()),
            _ => panic!("cannot write block {} offset {} to device", id, offset),
        }
    }
    /// Load struct `T` from given block in device
    fn load_struct<T: AsBuf>(&self, id: BlockId) -> vfs::Result<T> {
        let mut s: T = unsafe { uninitialized() };
        self.read_block(id, 0, s.as_buf_mut()).map(|_| s)
    }
}

impl DeviceExt for Device {}

pub trait SFSINode: INode {
    fn get_id(&self) -> INodeId;
    fn get_disk_inode(&self) -> &RwLock<Dirty<DiskINode>>;
    fn get_fs(&self) -> &Arc<SimpleFileSystem>;

    fn set_id(&self, id: INodeId) {
        unimplemented!();
    }

    fn set_disk_inode(&self, disk_inode: RwLock<Dirty<DiskINode>>) {
        unimplemented!();
    }

    fn set_fs(&self, fs: Arc<SimpleFileSystem>) {
        unimplemented!();
    }

    fn get_disk_block_id(&self, file_block_id: BlockId) -> vfs::Result<BlockId> {
        let disk_inode = self.get_disk_inode().read();
        match file_block_id {
            id if id >= disk_inode.blocks as BlockId => Err(FsError::InvalidParam),
            id if id < MAX_NBLOCK_DIRECT => Ok(disk_inode.direct[id] as BlockId),
            id if id < MAX_NBLOCK_INDIRECT => {
                let mut disk_block_id: u32 = 0;
                self.get_fs().device.read_block(
                    disk_inode.indirect as usize,
                    ENTRY_SIZE * (id - NDIRECT),
                    disk_block_id.as_buf_mut(),
                )?;
                Ok(disk_block_id as BlockId)
            }
            id if id < MAX_NBLOCK_DOUBLE_INDIRECT => {
                // double indirect
                let indirect_id = id - MAX_NBLOCK_INDIRECT;
                let mut indirect_block_id: u32 = 0;
                self.get_fs().device.read_block(
                    disk_inode.db_indirect as usize,
                    ENTRY_SIZE * (indirect_id / BLK_NENTRY),
                    indirect_block_id.as_buf_mut(),
                )?;
                assert!(indirect_block_id > 0);
                let mut disk_block_id: u32 = 0;
                self.get_fs().device.read_block(
                    indirect_block_id as usize,
                    ENTRY_SIZE * (indirect_id as usize % BLK_NENTRY),
                    disk_block_id.as_buf_mut(),
                )?;
                assert!(disk_block_id > 0);
                Ok(disk_block_id as BlockId)
            }
            _ => unimplemented!("triple indirect blocks is not supported"),
        }
    }
    fn set_disk_block_id(&self, file_block_id: BlockId, disk_block_id: BlockId) -> vfs::Result<()> {
        match file_block_id {
            id if id >= self.get_disk_inode().read().blocks as BlockId => Err(FsError::InvalidParam),
            id if id < MAX_NBLOCK_DIRECT => {
                self.get_disk_inode().write().direct[id] = disk_block_id as u32;
                Ok(())
            }
            id if id < MAX_NBLOCK_INDIRECT => {
                let disk_block_id = disk_block_id as u32;
                self.get_fs().device.write_block(
                    self.get_disk_inode().read().indirect as usize,
                    ENTRY_SIZE * (id - NDIRECT),
                    disk_block_id.as_buf(),
                )?;
                Ok(())
            }
            id if id < MAX_NBLOCK_DOUBLE_INDIRECT => {
                // double indirect
                let indirect_id = id - MAX_NBLOCK_INDIRECT;
                let mut indirect_block_id: u32 = 0;
                self.get_fs().device.read_block(
                    self.get_disk_inode().read().db_indirect as usize,
                    ENTRY_SIZE * (indirect_id / BLK_NENTRY),
                    indirect_block_id.as_buf_mut(),
                )?;
                assert!(indirect_block_id > 0);
                let disk_block_id = disk_block_id as u32;
                self.get_fs().device.write_block(
                    indirect_block_id as usize,
                    ENTRY_SIZE * (indirect_id as usize % BLK_NENTRY),
                    disk_block_id.as_buf(),
                )?;
                Ok(())
            }
            _ => unimplemented!("triple indirect blocks is not supported"),
        }
    }
    /// Only for Dir
    fn get_file_inode_and_entry_id(&self, name: &str) -> Option<(INodeId, usize)> {
        (0..self.get_disk_inode().read().blocks)
            .map(|i| {
                let mut entry: DiskEntry = unsafe { uninitialized() };
                self._read_at(i as usize * BLKSIZE, entry.as_buf_mut())
                    .unwrap();
                (entry, i)
            })
            .find(|(entry, _)| entry.name.as_ref() == name)
            .map(|(entry, id)| (entry.id as INodeId, id as usize))
    }
    fn get_file_inode_id(&self, name: &str) -> Option<INodeId> {
        self.get_file_inode_and_entry_id(name)
            .map(|(inode_id, _)| inode_id)
    }
    /// Init dir content. Insert 2 init entries.
    /// This do not init nlinks, please modify the nlinks in the invoker.
    fn init_dir_entry(&self, parent: INodeId) -> vfs::Result<()> {
        // Insert entries: '.' '..'
        self._resize(BLKSIZE * 2)?;
        self._write_at(
            BLKSIZE * 1,
            DiskEntry {
                id: parent as u32,
                name: Str256::from(".."),
            }
                .as_buf(),
        )
            .unwrap();
        self._write_at(
            BLKSIZE * 0,
            DiskEntry {
                id: self.get_id() as u32,
                name: Str256::from("."),
            }
                .as_buf(),
        )
            .unwrap();
        Ok(())
    }
    /// remove a page in middle of file and insert the last page here, useful for dirent remove
    /// should be only used in unlink
    fn remove_dirent_page(&self, id: usize) -> vfs::Result<()> {
        debug_assert!(id < self.get_disk_inode().read().blocks as usize);
        let to_remove = self.get_disk_block_id(id)?;
        let current_last = self.get_disk_block_id(self.get_disk_inode().read().blocks as usize - 1)?;
        self.set_disk_block_id(id, current_last)?;
        self.get_disk_inode().write().blocks -= 1;
        let new_size = self.get_disk_inode().read().blocks as usize * BLKSIZE;
        self._set_size(new_size);
        self.get_fs().free_block(to_remove);
        Ok(())
    }
    /// Resize content size, no matter what type it is.
    fn _resize(&self, len: usize) -> vfs::Result<()> {
        if len > MAX_FILE_SIZE {
            return Err(FsError::InvalidParam);
        }
        let blocks = ((len + BLKSIZE - 1) / BLKSIZE) as u32;
        if blocks > MAX_NBLOCK_DOUBLE_INDIRECT as u32 {
            return Err(FsError::InvalidParam);
        }
        use core::cmp::{Ord, Ordering};
        let old_blocks = self.get_disk_inode().read().blocks;
        match blocks.cmp(&old_blocks) {
            Ordering::Equal => {} // Do nothing
            Ordering::Greater => {
                {
                    let mut disk_inode = self.get_disk_inode().write();
                    disk_inode.blocks = blocks;
                    // allocate indirect block if needed
                    if old_blocks < MAX_NBLOCK_DIRECT as u32 && blocks >= MAX_NBLOCK_DIRECT as u32 {
                        disk_inode.indirect = self.get_fs().alloc_block().expect("no space") as u32;
                    }
                    // allocate double indirect block if needed
                    if blocks >= MAX_NBLOCK_INDIRECT as u32 {
                        if disk_inode.db_indirect == 0 {
                            disk_inode.db_indirect = self.get_fs().alloc_block().expect("no space") as u32;
                        }
                        let indirect_begin = {
                            if (old_blocks as usize) < MAX_NBLOCK_INDIRECT {
                                0
                            } else {
                                (old_blocks as usize - MAX_NBLOCK_INDIRECT) / BLK_NENTRY + 1
                            }
                        };
                        let indirect_end = (blocks as usize - MAX_NBLOCK_INDIRECT) / BLK_NENTRY + 1;
                        for i in indirect_begin..indirect_end {
                            let indirect = self.get_fs().alloc_block().expect("no space") as u32;
                            self.get_fs().device.write_block(
                                disk_inode.db_indirect as usize,
                                ENTRY_SIZE * i,
                                indirect.as_buf(),
                            )?;
                        }
                    }
                }
                // allocate extra blocks
                for i in old_blocks..blocks {
                    let disk_block_id = self.get_fs().alloc_block().expect("no space");
                    self.set_disk_block_id(i as usize, disk_block_id)?;
                }
                // clean up
                let old_size = self._size();
                self._set_size(len);
                self._clean_at(old_size, len).unwrap();
            }
            Ordering::Less => {
                // free extra blocks
                for i in blocks..old_blocks {
                    let disk_block_id = self.get_disk_block_id(i as usize)?;
                    self.get_fs().free_block(disk_block_id);
                }
                let mut disk_inode = self.get_disk_inode().write();
                // free indirect block if needed
                if blocks < MAX_NBLOCK_DIRECT as u32 && disk_inode.blocks >= MAX_NBLOCK_DIRECT as u32 {
                    self.get_fs().free_block(disk_inode.indirect as usize);
                    disk_inode.indirect = 0;
                }
                // free double indirect block if needed
                if disk_inode.blocks >= MAX_NBLOCK_INDIRECT as u32 {
                    let indirect_begin = {
                        if (blocks as usize) < MAX_NBLOCK_INDIRECT {
                            0
                        } else {
                            (blocks as usize - MAX_NBLOCK_INDIRECT) / BLK_NENTRY + 1
                        }
                    };
                    let indirect_end = (disk_inode.blocks as usize - MAX_NBLOCK_INDIRECT) / BLK_NENTRY + 1;
                    for i in indirect_begin..indirect_end {
                        let mut indirect: u32 = 0;
                        self.get_fs().device.read_block(
                            disk_inode.db_indirect as usize,
                            ENTRY_SIZE * i,
                            indirect.as_buf_mut(),
                        )?;
                        assert!(indirect > 0);
                        self.get_fs().free_block(indirect as usize);
                    }
                    if blocks < MAX_NBLOCK_INDIRECT as u32 {
                        assert!(disk_inode.db_indirect > 0);
                        self.get_fs().free_block(disk_inode.db_indirect as usize);
                        disk_inode.db_indirect = 0;
                    }
                }
                disk_inode.blocks = blocks;
            }
        }
        self._set_size(len);
        Ok(())
    }
    /// Get the actual size of this inode,
    /// since size in inode for dir is not real size
    fn _size(&self) -> usize {
        let disk_inode = self.get_disk_inode().read();
        match disk_inode.type_ {
            FileType::Dir => disk_inode.blocks as usize * BLKSIZE,
            FileType::File | FileType::SymLink => disk_inode.size as usize,
            _ => panic!("Unknown file type"),
        }
    }
    /// Set the ucore compat size of this inode,
    /// Size in inode for dir is size of entries
    fn _set_size(&self, len: usize) {
        let mut disk_inode = self.get_disk_inode().write();
        disk_inode.size = match disk_inode.type_ {
            FileType::Dir => disk_inode.blocks as usize * DIRENT_SIZE,
            FileType::File | FileType::SymLink => len,
            _ => panic!("Unknown file type"),
        } as u32
    }
    // Note: the _\w*_at method always return begin>size?0:begin<end?0:(min(size,end)-begin) when success
    /// Read/Write content, no matter what type it is
    /// Remove _io_at because it is not object-safe for using generic type
    /*
    fn _io_at<F>(&self, begin: usize, end: usize, mut f: F) -> vfs::Result<usize>
        where
            F: FnMut(&Arc<Device>, &BlockRange, usize) -> vfs::Result<()>,
    {
        let size = self._size();
        let iter = BlockIter {
            begin: size.min(begin),
            end: size.min(end),
            block_size_log2: BLKSIZE_LOG2,
        };

        // For each block
        let mut buf_offset = 0usize;
        for mut range in iter {
            range.block = self.get_disk_block_id(range.block)?;
            f(&self.get_fs().device, &range, buf_offset)?;
            buf_offset += range.len();
        }
        Ok(buf_offset)
    }
    */

    /// Read content, no matter what type it is
    fn _read_at(&self, offset: usize, buf: &mut [u8]) -> vfs::Result<usize> {
        let begin = offset;
        let end = offset + buf.len();

        let size = self._size();
        let iter = BlockIter {
            begin: size.min(begin),
            end: size.min(end),
            block_size_log2: BLKSIZE_LOG2,
        };

        // For each block
        let mut buf_offset = 0usize;
        for mut range in iter {
            range.block = self.get_disk_block_id(range.block)?;
            self.get_fs().device.read_block(range.block, range.begin, &mut buf[buf_offset..buf_offset + range.len()]);
            buf_offset += range.len();
        }
        Ok(buf_offset)
    }
    /// Write content, no matter what type it is
    fn _write_at(&self, offset: usize, buf: &[u8]) -> vfs::Result<usize> {
        let begin = offset;
        let end = offset + buf.len();

        let size = self._size();
        let iter = BlockIter {
            begin: size.min(begin),
            end: size.min(end),
            block_size_log2: BLKSIZE_LOG2,
        };

        // For each block
        let mut buf_offset = 0usize;
        for mut range in iter {
            range.block = self.get_disk_block_id(range.block)?;
            self.get_fs().device.write_block(range.block, range.begin, &buf[buf_offset..buf_offset + range.len()]);
            buf_offset += range.len();
        }
        Ok(buf_offset)
    }
    /// Clean content, no matter what type it is
    fn _clean_at(&self, begin: usize, end: usize) -> vfs::Result<usize> {
        static ZEROS: [u8; BLKSIZE] = [0; BLKSIZE];

        let size = self._size();
        let iter = BlockIter {
            begin: size.min(begin),
            end: size.min(end),
            block_size_log2: BLKSIZE_LOG2,
        };

        // For each block
        let mut buf_offset = 0usize;
        for mut range in iter {
            range.block = self.get_disk_block_id(range.block)?;
            self.get_fs().device.write_block(range.block, range.begin, &ZEROS[..range.len()]);
            buf_offset += range.len();
        }
        Ok(buf_offset)
    }
    fn nlinks_inc(&self) {
        self.get_disk_inode().write().nlinks += 1;
    }
    fn nlinks_dec(&self) {
        let mut disk_inode = self.get_disk_inode().write();
        assert!(disk_inode.nlinks > 0);
        disk_inode.nlinks -= 1;
    }
}

/// inode for sfs
pub struct SFSINodeImpl {
    /// inode number
    id: INodeId,
    /// on-disk inode
    disk_inode: RwLock<Dirty<DiskINode>>,
    /// Weak reference to SFS, used by almost all operations
    fs: Arc<SimpleFileSystem>,
}

impl Debug for SFSINodeImpl {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(
            f,
            "INode {{ id: {}, disk: {:?} }}",
            self.get_id(), self.get_disk_inode()
        )
    }
}

impl SFSINode for SFSINodeImpl {
    fn get_id(&self) -> INodeId {
        self.id
    }

    fn get_disk_inode(&self) -> &RwLock<Dirty<DiskINode>> {
        &self.disk_inode
    }

    fn get_fs(&self) -> &Arc<SimpleFileSystem> {
        &self.fs
    }
}

impl vfs::INode for SFSINodeImpl {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> vfs::Result<usize> {
        if self.get_disk_inode().read().type_ != FileType::File
            && self.get_disk_inode().read().type_ != FileType::SymLink
        {
            return Err(FsError::NotFile);
        }
        self._read_at(offset, buf)
    }
    fn write_at(&self, offset: usize, buf: &[u8]) -> vfs::Result<usize> {
        let DiskINode { type_, size, .. } = **self.get_disk_inode().read();
        if type_ != FileType::File && type_ != FileType::SymLink {
            return Err(FsError::NotFile);
        }
        // resize if not large enough
        let end_offset = offset + buf.len();
        if (size as usize) < end_offset {
            self._resize(end_offset)?;
        }
        self._write_at(offset, buf)
    }
    /// the size returned here is logical size(entry num for directory), not the disk space used.
    fn metadata(&self) -> vfs::Result<vfs::Metadata> {
        let disk_inode = self.get_disk_inode().read();
        Ok(vfs::Metadata {
            dev: 0,
            inode: self.get_id(),
            size: match disk_inode.type_ {
                FileType::File | FileType::SymLink => disk_inode.size as usize,
                FileType::Dir => disk_inode.blocks as usize,
                _ => panic!("Unknown file type"),
            },
            mode: 0o777,
            type_: vfs::FileType::from(disk_inode.type_.clone()),
            blocks: disk_inode.blocks as usize,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            nlinks: disk_inode.nlinks as usize,
            uid: 0,
            gid: 0,
            blk_size: BLKSIZE,
        })
    }

    fn chmod(&self, _mode: u16) -> vfs::Result<()> {
        // No-op for sfs
        Ok(())
    }

    fn sync_all(&self) -> vfs::Result<()> {
        let mut disk_inode = self.get_disk_inode().write();
        if disk_inode.dirty() {
            self.get_fs()
                .device
                .write_block(self.get_id(), 0, disk_inode.as_buf())?;
            disk_inode.sync();
        }
        Ok(())
    }
    fn sync_data(&self) -> vfs::Result<()> {
        self.sync_all()
    }
    fn resize(&self, len: usize) -> vfs::Result<()> {
        if self.get_disk_inode().read().type_ != FileType::File
            && self.get_disk_inode().read().type_ != FileType::SymLink
        {
            return Err(FsError::NotFile);
        }
        self._resize(len)
    }
    fn create(&self, name: &str, type_: vfs::FileType, _mode: u32) -> vfs::Result<Arc<SFSINode>> {
        let info = self.metadata()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }

        // Ensure the name is not exist
        if !self.get_file_inode_id(name).is_none() {
            return Err(FsError::EntryExist);
        }

        // Create new INode
        let inode = match type_ {
            vfs::FileType::File => self.get_fs().new_inode_file()?,
            vfs::FileType::SymLink => self.get_fs().new_inode_symlink()?,
            vfs::FileType::Dir => self.get_fs().new_inode_dir(self.get_id())?,
            _ => return Err(vfs::FsError::InvalidParam),
        };

        // Write new entry
        let entry = DiskEntry {
            id: inode.get_id() as u32,
            name: Str256::from(name),
        };
        let old_size = self._size();
        self._resize(old_size + BLKSIZE)?;
        self._write_at(old_size, entry.as_buf()).unwrap();
        inode.nlinks_inc();
        if type_ == vfs::FileType::Dir {
            inode.nlinks_inc(); //for .
            self.nlinks_inc(); //for ..
        }

        Ok(inode)
    }
    fn unlink(&self, name: &str) -> vfs::Result<()> {
        let info = self.metadata()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }
        if name == "." {
            return Err(FsError::IsDir);
        }
        if name == ".." {
            return Err(FsError::IsDir);
        }

        let (inode_id, entry_id) = self
            .get_file_inode_and_entry_id(name)
            .ok_or(FsError::EntryNotFound)?;
        let inode = self.get_fs().get_inode(inode_id);

        let type_ = inode.get_disk_inode().read().type_;
        if type_ == FileType::Dir {
            // only . and ..
            assert!(inode.get_disk_inode().read().blocks >= 2);
            if inode.get_disk_inode().read().blocks > 2 {
                return Err(FsError::DirNotEmpty);
            }
        }
        inode.nlinks_dec();
        if type_ == FileType::Dir {
            inode.nlinks_dec(); //for .
            self.nlinks_dec(); //for ..
        }
        self.remove_dirent_page(entry_id)?;

        Ok(())
    }
    fn link(&self, name: &str, other: &Arc<INode>) -> vfs::Result<()> {
        let info = self.metadata()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }
        if !self.get_file_inode_id(name).is_none() {
            return Err(FsError::EntryExist);
        }
        let child = other
            .downcast_ref::<SFSINodeImpl>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.get_fs(), &child.get_fs()) {
            return Err(FsError::NotSameFs);
        }
        if child.metadata()?.type_ == vfs::FileType::Dir {
            return Err(FsError::IsDir);
        }
        let entry = DiskEntry {
            id: child.get_id() as u32,
            name: Str256::from(name),
        };
        let old_size = self._size();
        self._resize(old_size + BLKSIZE)?;
        self._write_at(old_size, entry.as_buf()).unwrap();
        child.nlinks_inc();
        Ok(())
    }
    fn move_(&self, old_name: &str, target: &Arc<INode>, new_name: &str) -> vfs::Result<()> {
        let info = self.metadata()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }
        if old_name == "." {
            return Err(FsError::IsDir);
        }
        if old_name == ".." {
            return Err(FsError::IsDir);
        }

        let dest = target
            .downcast_ref::<SFSINodeImpl>()
            .ok_or(FsError::NotSameFs)?;
        let dest_info = dest.metadata()?;
        if !Arc::ptr_eq(&self.get_fs(), &dest.get_fs()) {
            return Err(FsError::NotSameFs);
        }
        if dest_info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if dest_info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }
        if dest.get_file_inode_id(new_name).is_some() {
            return Err(FsError::EntryExist);
        }

        let (inode_id, entry_id) = self
            .get_file_inode_and_entry_id(old_name)
            .ok_or(FsError::EntryNotFound)?;
        if info.inode == dest_info.inode {
            // rename: in place modify name
            let mut entry: DiskEntry = unsafe { uninitialized() };
            let entry_pos = entry_id as usize * BLKSIZE;
            self._read_at(entry_pos, entry.as_buf_mut()).unwrap();
            entry.name = Str256::from(new_name);
            self._write_at(entry_pos, entry.as_buf()).unwrap();
        } else {
            // move
            let inode = self.get_fs().get_inode(inode_id);

            let entry = DiskEntry {
                id: inode_id as u32,
                name: Str256::from(new_name),
            };
            let old_size = dest._size();
            dest._resize(old_size + BLKSIZE)?;
            dest._write_at(old_size, entry.as_buf()).unwrap();

            self.remove_dirent_page(entry_id)?;

            if inode.metadata()?.type_ == vfs::FileType::Dir {
                self.nlinks_dec();
                dest.nlinks_inc();
            }
        }

        Ok(())
    }
    fn find(&self, name: &str) -> vfs::Result<Arc<SFSINode>> {
        let info = self.metadata()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        let inode_id = self.get_file_inode_id(name).ok_or(FsError::EntryNotFound)?;
        Ok(self.get_fs().get_inode(inode_id))
    }
    fn get_entry(&self, id: usize) -> vfs::Result<String> {
        if self.get_disk_inode().read().type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        if id >= self.get_disk_inode().read().blocks as usize {
            return Err(FsError::EntryNotFound);
        };
        let mut entry: DiskEntry = unsafe { uninitialized() };
        self._read_at(id as usize * BLKSIZE, entry.as_buf_mut())
            .unwrap();
        Ok(String::from(entry.name.as_ref()))
    }
    fn fs(&self) -> Arc<vfs::FileSystem> {
        self.get_fs().clone()
    }
    fn as_any_ref(&self) -> &Any {
        self
    }
}

impl Drop for SFSINodeImpl {
    /// Auto sync when drop
    fn drop(&mut self) {
        self.sync_all()
            .expect("Failed to sync when dropping the SimpleFileSystem Inode");
        if self.get_disk_inode().read().nlinks <= 0 {
            self._resize(0).unwrap();
            self.get_disk_inode().write().sync();
            self.get_fs().free_block(self.get_id());
        }
    }
}

/// filesystem for sfs
///
/// ## 内部可变性
/// 为了方便协调外部及INode对SFS的访问，并为日后并行化做准备，
/// 将SFS设置为内部可变，即对外接口全部是&self，struct的全部field用RwLock包起来
/// 这样其内部各field均可独立访问
pub struct SimpleFileSystem {
    /// on-disk superblock
    super_block: RwLock<Dirty<SuperBlock>>,
    /// blocks in use are mared 0
    free_map: RwLock<Dirty<BitVec>>,
    /// inode list
    inodes: RwLock<BTreeMap<INodeId, Weak<SFSINode>>>,
    /// device
    device: Arc<Device>,
    /// Pointer to self, used by INodes
    self_ptr: Weak<SimpleFileSystem>,
}

impl SimpleFileSystem {
    /// Load SFS from device
    pub fn open(device: Arc<Device>) -> vfs::Result<Arc<Self>> {
        let super_block = device.load_struct::<SuperBlock>(BLKN_SUPER).unwrap();
        if !super_block.check() {
            return Err(FsError::WrongFs);
        }
        let mut freemap_disk = vec![0u8; BLKSIZE * super_block.freemap_blocks as usize];
        for i in 0..super_block.freemap_blocks as usize {
            device.read_block(BLKN_FREEMAP + i, 0, &mut freemap_disk[i * BLKSIZE..(i+1) *BLKSIZE])?;
        }

        Ok(SimpleFileSystem {
            super_block: RwLock::new(Dirty::new(super_block)),
            free_map: RwLock::new(Dirty::new(BitVec::from(freemap_disk.as_slice()))),
            inodes: RwLock::new(BTreeMap::new()),
            device,
            self_ptr: Weak::default(),
        }
        .wrap())
    }
    /// Create a new SFS on blank disk
    pub fn create(device: Arc<Device>, space: usize) -> Arc<Self> {
        let blocks = (space + BLKSIZE - 1) / BLKSIZE;
        let freemap_blocks = (space + BLKBITS * BLKSIZE - 1) / BLKBITS / BLKSIZE;
        assert!(blocks >= 16, "space too small");

        let super_block = SuperBlock {
            magic: MAGIC,
            blocks: blocks as u32,
            unused_blocks: (blocks - BLKN_FREEMAP - freemap_blocks) as u32,
            info: Str32::from(DEFAULT_INFO),
            freemap_blocks: freemap_blocks as u32,
        };
        let free_map = {
            let mut bitset = BitVec::with_capacity(freemap_blocks * BLKBITS);
            bitset.extend(core::iter::repeat(false).take(freemap_blocks * BLKBITS));
            for i in (BLKN_FREEMAP + freemap_blocks)..blocks {
                bitset.set(i, true);
            }
            bitset
        };

        let sfs = SimpleFileSystem {
            super_block: RwLock::new(Dirty::new_dirty(super_block)),
            free_map: RwLock::new(Dirty::new_dirty(free_map)),
            inodes: RwLock::new(BTreeMap::new()),
            device,
            self_ptr: Weak::default(),
        }
        .wrap();

        // Init root INode
        let root = sfs._new_inode(BLKN_ROOT, Dirty::new_dirty(DiskINode::new_dir()));
        root.init_dir_entry(BLKN_ROOT).unwrap();
        root.nlinks_inc(); //for .
        root.nlinks_inc(); //for ..(root's parent is itself)
        root.sync_all().unwrap();

        sfs
    }
    /// Wrap pure SimpleFileSystem with Arc
    /// Used in constructors
    fn wrap(self) -> Arc<Self> {
        // Create an Arc, make a Weak from it, then put it into the struct.
        // It's a little tricky.
        let fs = Arc::new(self);
        let weak = Arc::downgrade(&fs);
        let ptr = Arc::into_raw(fs) as *mut Self;
        unsafe {
            (*ptr).self_ptr = weak;
        }
        unsafe { Arc::from_raw(ptr) }
    }

    /// Allocate a block, return block id
    fn alloc_block(&self) -> Option<usize> {
        let mut free_map = self.free_map.write();
        let id = free_map.alloc();
        if let Some(block_id) = id {
            let mut super_block = self.super_block.write();
            if super_block.unused_blocks == 0 {
                free_map.set(block_id, true);
                return None;
            }
            super_block.unused_blocks -= 1; // will not underflow
            trace!("alloc block {:#x}", block_id);
        } else {
            let super_block = self.super_block.read();
            panic!("{:?}", super_block)
        }
        id
    }
    /// Free a block
    fn free_block(&self, block_id: usize) {
        let mut free_map = self.free_map.write();
        assert!(!free_map[block_id]);
        free_map.set(block_id, true);
        self.super_block.write().unused_blocks += 1;
        trace!("free block {:#x}", block_id);
    }

    /// Create a new INode struct, then insert it to self.inodes
    /// Private used for load or create INode
    fn _new_inode(&self, id: INodeId, disk_inode: Dirty<DiskINode>) -> Arc<SFSINode> {
        let inode : Arc<SFSINode> = Arc::new(SFSINodeImpl {
            id,
            disk_inode: RwLock::new(disk_inode),
            fs: self.self_ptr.upgrade().unwrap(),
        });
        self.inodes.write().insert(id, Arc::downgrade(&inode));
        inode
    }
    /// Get inode by id. Load if not in memory.
    /// ** Must ensure it's a valid INode **
    fn get_inode(&self, id: INodeId) -> Arc<SFSINode> {
        assert!(!self.free_map.read()[id]);

        // In the BTreeSet and not weak.
        if let Some(inode) = self.inodes.read().get(&id) {
            if let Some(inode) = inode.upgrade() {
                return inode;
            }
        }
        // Load if not in set, or is weak ref.
        let disk_inode = Dirty::new(self.device.load_struct::<DiskINode>(id).unwrap());
        self._new_inode(id, disk_inode)
    }
    /// Create a new INode file
    fn new_inode_file(&self) -> vfs::Result<Arc<SFSINode>> {
        let id = self.alloc_block().ok_or(FsError::NoDeviceSpace)?;
        let disk_inode = Dirty::new_dirty(DiskINode::new_file());
        Ok(self._new_inode(id, disk_inode))
    }
    /// Create a new INode symlink
    fn new_inode_symlink(&self) -> vfs::Result<Arc<SFSINode>> {
        let id = self.alloc_block().ok_or(FsError::NoDeviceSpace)?;
        let disk_inode = Dirty::new_dirty(DiskINode::new_symlink());
        Ok(self._new_inode(id, disk_inode))
    }
    /// Create a new INode dir
    fn new_inode_dir(&self, parent: INodeId) -> vfs::Result<Arc<SFSINode>> {
        let id = self.alloc_block().ok_or(FsError::NoDeviceSpace)?;
        let disk_inode = Dirty::new_dirty(DiskINode::new_dir());
        let inode = self._new_inode(id, disk_inode);
        inode.init_dir_entry(parent)?;
        Ok(inode)
    }
    fn flush_weak_inodes(&self) {
        let mut inodes = self.inodes.write();
        let remove_ids: Vec<_> = inodes
            .iter()
            .filter(|(_, inode)| inode.upgrade().is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in remove_ids.iter() {
            inodes.remove(&id);
        }
    }
}

impl vfs::FileSystem for SimpleFileSystem {
    /// Write back super block if dirty
    fn sync(&self) -> vfs::Result<()> {
        let mut super_block = self.super_block.write();
        if super_block.dirty() {
            self.device
                .write_at(BLKSIZE * BLKN_SUPER, super_block.as_buf())
                .unwrap();
            super_block.sync();
        }
        let mut free_map = self.free_map.write();
        if free_map.dirty() {
            let data = free_map.as_buf();
            for i in 0..super_block.freemap_blocks as usize {
                self.device
                    .write_at(BLKSIZE * (BLKN_FREEMAP + i), &data[i * BLKSIZE..(i+1) * BLKSIZE])
                    .unwrap();
            }
            free_map.sync();
        }
        self.flush_weak_inodes();
        for inode in self.inodes.read().values() {
            if let Some(inode) = inode.upgrade() {
                inode.sync_all()?;
            }
        }
        Ok(())
    }

    fn root_inode(&self) -> Arc<SFSINode> {
        self.get_inode(BLKN_ROOT)
        // let root = self.get_inode(BLKN_ROOT);
        // root.create("dev", vfs::FileType::Dir, 0).expect("fail to create dev"); // what's mode?
        // return root;
    }

    fn info(&self) -> vfs::FsInfo {
        let sb = self.super_block.read();
        vfs::FsInfo {
            bsize: BLKSIZE,
            frsize: BLKSIZE,
            blocks: sb.blocks as usize,
            bfree: sb.unused_blocks as usize,
            bavail: sb.unused_blocks as usize,
            files: sb.blocks as usize,        // inaccurate
            ffree: sb.unused_blocks as usize, // inaccurate
            namemax: MAX_FNAME_LEN,
        }
    }
}

impl Drop for SimpleFileSystem {
    /// Auto sync when drop
    fn drop(&mut self) {
        self.sync()
            .expect("Failed to sync when dropping the SimpleFileSystem");
    }
}

trait BitsetAlloc {
    fn alloc(&mut self) -> Option<usize>;
}

impl BitsetAlloc for BitVec {
    fn alloc(&mut self) -> Option<usize> {
        // TODO: more efficient
        let id = (0..self.len()).find(|&i| self[i]);
        if let Some(id) = id {
            self.set(id, false);
        }
        id
    }
}

impl AsBuf for BitVec {
    fn as_buf(&self) -> &[u8] {
        self.as_ref()
    }
    fn as_buf_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl AsBuf for [u8; BLKSIZE] {}

impl From<FileType> for vfs::FileType {
    fn from(t: FileType) -> Self {
        match t {
            FileType::File => vfs::FileType::File,
            FileType::SymLink => vfs::FileType::SymLink,
            FileType::Dir => vfs::FileType::Dir,
            _ => panic!("unknown file type"),
        }
    }
}
