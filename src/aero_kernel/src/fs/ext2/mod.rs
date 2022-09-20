/*
 * Copyright (C) 2021-2022 The Aero Project Developers.
 *
 * This file is part of The Aero Project.
 *
 * Aero is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * Aero is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with Aero. If not, see <https://www.gnu.org/licenses/>.
 */

use core::mem::MaybeUninit;

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};

use crate::fs::cache::CachedINode;
use crate::utils::CeilDiv;

use super::block::BlockDevice;

use super::cache::{DirCacheItem, INodeCacheItem};
use super::{cache, FileSystemError};

use super::inode::{DirEntry, INodeInterface, Metadata};
use super::FileSystem;

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct SuperBlock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub r_blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_frag_size: u32,
    pub blocks_per_group: u32,
    pub frags_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: u32,
    pub wtime: u32,
    pub mnt_count: u16,
    pub max_mnt_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_rev_level: u16,
    pub lastcheck: u32,
    pub checkinterval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub def_resuid: u16,
    pub def_gid: u16,

    // Extended Superblock fields
    //
    // XXX: If version number >= 1, we have to use the ext2 extended superblock as well :)
    pub first_ino: u32,
    pub inode_size: u16,
    pub block_group_nr: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u64; 2usize],
    pub volume_name: [u8; 16usize],
    pub last_mounted: [u64; 8usize],
    pub compression_info: u32,
    pub prealloc_blocks: u8,
    pub prealloc_dir_blocks: u8,
    pub reserved_gdt_blocks: u16,
    pub journal_uuid: [u8; 16usize],
    pub journal_inum: u32,
    pub journal_dev: u32,
    pub last_orphan: u32,
    pub hash_seed: [u32; 4usize],
    pub def_hash_version: u8,
    pub jnl_backup_type: u8,
    pub group_desc_size: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    pub mkfs_time: u32,
    pub jnl_blocks: [u32; 17usize],
}

impl SuperBlock {
    const MAGIC: u16 = 0xef53;

    /// Returns the size of a block in bytes.
    pub fn block_size(&self) -> usize {
        1024usize << self.log_block_size
    }

    /// Returns the length of the BGDT.
    pub fn bgdt_len(&self) -> usize {
        self.blocks_count.ceil_div(self.blocks_per_group) as usize
    }

    /// Returns the sector where the BGDT starts.
    pub fn bgdt_sector(&self) -> usize {
        // XXX: The block group descriptors are always located in the block immediately
        // following the superblock.
        match self.block_size() {
            1024 => 4,
            x if x > 1024 => x / 512,
            _ => unreachable!(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GroupDescriptor {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
    pub pad: u16,
    pub reserved: [u8; 12usize],
}

const_assert_eq!(core::mem::size_of::<GroupDescriptor>(), 32);

pub enum FileType {
    Fifo,
    CharDev,
    Directory,
    BlockDev,
    File,
    Symlink,
    Socket,
    Unknown,
}

impl From<FileType> for super::inode::FileType {
    fn from(ty: FileType) -> Self {
        match ty {
            FileType::Symlink => Self::Symlink,
            FileType::Directory => Self::Directory,
            FileType::BlockDev | FileType::CharDev => Self::Device,

            _ => Self::File,
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct DiskINode {
    type_and_perm: u16,
    pub user_id: u16,
    pub size_lower: u32,
    pub last_access: u32,
    pub creation_time: u32,
    pub last_modification: u32,
    pub deletion_time: u32,
    pub group_id: u16,
    pub hl_count: u16,
    pub sector_count: u32,
    pub flags: u32,
    pub os_specific: u32,
    pub data_ptr: [u32; 15],
    pub gen_number: u32,
    pub ext_attr_block: u32,
    pub size_or_acl: u32,
    pub fragment_address: u32,
    pub os_specific2: [u8; 12],
}

impl DiskINode {
    pub fn file_type(&self) -> FileType {
        let ty = self.type_and_perm >> 12;

        match ty {
            0x1 => FileType::Fifo,
            0x2 => FileType::CharDev,
            0x4 => FileType::Directory,
            0x6 => FileType::BlockDev,
            0x8 => FileType::File,
            0xa => FileType::Symlink,
            0xc => FileType::Socket,
            _ => FileType::Unknown,
        }
    }
}

const_assert_eq!(core::mem::size_of::<DiskINode>(), 128);

pub struct INode {
    id: usize,
    fs: Weak<Ext2>,
    inode: Box<DiskINode>,

    sref: Weak<INode>,
}

impl INode {
    pub fn new(ext2: Weak<Ext2>, id: usize) -> Option<INodeCacheItem> {
        debug_assert!(id != 0);

        let icache = cache::icache();

        // Check if the inode is in the cache.
        if let Some(inode) = icache.get(INodeCacheItem::make_key(ext2.clone(), id)) {
            Some(inode)
        } else {
            let fs = ext2.upgrade()?;
            let superblock = &fs.superblock;

            // There is one inode table per block group and can be located by
            // the `inode_table` offset in the group descriptor. Also there are
            // `inodes_per_group` inodes per table.
            let ino_per_group = superblock.inodes_per_group as usize;

            let ino_block_group = (id - 1) / ino_per_group;
            let ino_table_index = (id - 1) % ino_per_group;

            let group_descriptor = &fs.bgdt[ino_block_group];

            let table_offset = group_descriptor.inode_table as usize * superblock.block_size();

            let mut inode = Box::<DiskINode>::new_uninit();

            fs.block.device().read(
                table_offset + (ino_table_index * core::mem::size_of::<DiskINode>()),
                inode.as_bytes_mut(),
            )?;

            // SAFETY: We have initialized the inode above.
            let inode = unsafe { inode.assume_init() };

            Some(
                icache.make_item_cached(CachedINode::new(Arc::new_cyclic(|sref| Self {
                    inode,
                    id,
                    fs: ext2,

                    sref: sref.clone(),
                }))),
            )
        }
    }

    pub fn sref(&self) -> Arc<INode> {
        self.sref.upgrade().unwrap()
    }

    pub fn make_dir_entry(
        &self,
        parent: DirCacheItem,
        name: &str,
        entry: &DiskDirEntry,
    ) -> Option<DirCacheItem> {
        let inode = self.fs.upgrade()?.find_inode(entry.inode as usize)?;
        Some(DirEntry::new(parent, inode, name.to_string()))
    }
}

impl INodeInterface for INode {
    fn weak_filesystem(&self) -> Option<Weak<dyn FileSystem>> {
        Some(self.fs.clone())
    }

    fn metadata(&self) -> super::Result<Metadata> {
        Ok(Metadata {
            id: self.id,
            file_type: self.inode.file_type().into(),
            size: self.inode.size_lower as _,
            children_len: 0,
        })
    }

    fn stat(&self) -> super::Result<aero_syscall::Stat> {
        use super::inode::FileType;
        use aero_syscall::{Mode, Stat};

        let filesystem = self.fs.upgrade().unwrap();
        let filetype = self.metadata()?.file_type();

        let mut mode = Mode::empty();

        match filetype {
            FileType::File => mode.insert(Mode::S_IFREG),
            FileType::Directory => mode.insert(Mode::S_IFDIR),
            FileType::Device => mode.insert(Mode::S_IFCHR),
            FileType::Socket => mode.insert(Mode::S_IFSOCK),
            FileType::Symlink => mode.insert(Mode::S_IFLNK),
        }

        // FIXME: read permission bits from the inode.
        mode.insert(Mode::S_IRWXU | Mode::S_IRWXG | Mode::S_IRWXO);

        Ok(Stat {
            st_ino: self.id as _,
            st_blksize: filesystem.superblock.block_size() as _,
            st_size: self.inode.size_lower as _,
            st_mode: mode,

            ..Default::default()
        })
    }

    fn dirent(&self, parent: DirCacheItem, index: usize) -> super::Result<Option<DirCacheItem>> {
        Ok(DirEntryIter::new(parent, self.sref()).nth(index))
    }

    fn lookup(&self, dir: DirCacheItem, name: &str) -> super::Result<DirCacheItem> {
        DirEntryIter::new(dir, self.sref())
            .find(|e| &e.name() == name)
            .ok_or(FileSystemError::EntryNotFound)
    }

    fn read_at(&self, offset: usize, buffer: &mut [u8]) -> super::Result<usize> {
        let filesystem = self.fs.upgrade().unwrap();
        let block_size = filesystem.superblock.block_size();

        let mut progress = 0;

        let count = core::cmp::min(self.inode.size_lower as usize - offset, buffer.len());

        while progress < count {
            let block = (offset + progress) / block_size;
            let loc = (offset + progress) % block_size;

            let mut chunk = count - progress;

            if chunk > block_size - loc {
                chunk = block_size - loc;
            }

            let block_index = self.inode.data_ptr[block];

            // TODO: We really should not allocate another buffer here.
            let mut data = Box::<[u8]>::new_uninit_slice(chunk);

            filesystem.block.device().read(
                (block_index as usize * block_size) + loc,
                MaybeUninit::slice_as_bytes_mut(&mut data),
            );

            // SAFETY: We have initialized the data buffer above.
            let data = unsafe { data.assume_init() };

            buffer[progress..progress + data.len()].copy_from_slice(&*data);
            progress += chunk;
        }

        Ok(count)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct DiskDirEntry {
    inode: u32,
    entry_size: u16,
    name_size: u8,
    file_type: u8,
}

pub struct DirEntryIter {
    parent: DirCacheItem,
    inode: Arc<INode>,
    offset: usize,
}

impl DirEntryIter {
    pub fn new(parent: DirCacheItem, inode: Arc<INode>) -> Self {
        Self {
            parent,
            inode,

            offset: 0,
        }
    }
}

impl Iterator for DirEntryIter {
    type Item = DirCacheItem;

    fn next(&mut self) -> Option<Self::Item> {
        let filesystem = self.inode.fs.upgrade()?;
        let file_size = self.inode.inode.size_lower as usize;

        if self.offset + core::mem::size_of::<DiskDirEntry>() > file_size {
            return None;
        }

        let mut entry = Box::<DiskDirEntry>::new_uninit();

        let offset = (self.inode.inode.data_ptr[0] as usize * filesystem.superblock.block_size())
            + self.offset;

        filesystem.block.device().read(offset, entry.as_bytes_mut());

        // SAFETY: We have initialized the entry above.
        let entry = unsafe { entry.assume_init() };

        let mut name = Box::<[u8]>::new_uninit_slice(entry.name_size as usize);
        filesystem.block.device().read(
            offset + core::mem::size_of::<DiskDirEntry>(),
            MaybeUninit::slice_as_bytes_mut(&mut name),
        );

        // SAFETY: We have initialized the name above.
        let name = unsafe { name.assume_init() };
        let name = unsafe { core::str::from_utf8_unchecked(&*name) };

        self.offset += entry.entry_size as usize;
        self.inode.make_dir_entry(self.parent.clone(), name, &entry)
    }
}

pub struct Ext2 {
    superblock: Box<SuperBlock>,
    bgdt: Box<[GroupDescriptor]>,
    block: Arc<BlockDevice>,

    sref: Weak<Self>,
}

impl Ext2 {
    const ROOT_INODE_ID: usize = 2;

    pub fn new(block: Arc<BlockDevice>) -> Option<Arc<Self>> {
        let mut superblock = Box::<SuperBlock>::new_uninit();
        block.device().read_block(2, superblock.as_bytes_mut())?;

        // SAFETY: We have initialized the superblock above.
        let superblock = unsafe { superblock.assume_init() };

        if superblock.magic != SuperBlock::MAGIC {
            return None;
        }

        assert_eq!(
            superblock.inode_size as usize,
            core::mem::size_of::<DiskINode>()
        );

        let bgdt_len = superblock.bgdt_len();
        let mut bgdt = Box::<[GroupDescriptor]>::new_uninit_slice(bgdt_len);

        block.device().read_block(
            superblock.bgdt_sector(),
            MaybeUninit::slice_as_bytes_mut(&mut bgdt),
        )?;

        // SAFETY: We have initialized the BGD (Block Group Descriptor Table) above.
        let bgdt = unsafe { bgdt.assume_init() };

        Some(Arc::new_cyclic(|sref| Self {
            bgdt,
            superblock,
            block,

            sref: sref.clone(),
        }))
    }

    pub fn find_inode(&self, id: usize) -> Option<INodeCacheItem> {
        INode::new(self.sref.clone(), id)
    }
}

impl FileSystem for Ext2 {
    fn root_dir(&self) -> DirCacheItem {
        let inode = self
            .find_inode(Ext2::ROOT_INODE_ID)
            .expect("ext2: invalid filesystem (root inode not found)");

        DirEntry::new_root(inode, String::from("/"))
    }
}
