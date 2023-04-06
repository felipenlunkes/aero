/*
 * Copyright (C) 2021-2023 The Aero Project Developers.
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

use core::sync::atomic::{AtomicU32, Ordering};

use aero_syscall::Termios;
use aero_syscall::WinSize;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use spin::{Once, RwLock};

use uapi::pty::*;

use crate::fs::cache;
use crate::fs::cache::*;
use crate::fs::devfs;
use crate::fs::devfs::DEV_FILESYSTEM;
use crate::fs::inode::FileType;
use crate::fs::inode::PollFlags;
use crate::fs::inode::{DirEntry, INodeInterface};
use crate::fs::FileSystem;
use crate::fs::Path;
use crate::fs::MOUNT_MANAGER;
use crate::fs::{self, FileSystemError};

use crate::mem::paging::VirtAddr;
use crate::userland::scheduler;
use crate::userland::task::Task;
use crate::userland::terminal::LineDiscipline;
use crate::userland::terminal::TerminalDevice;
use crate::utils::sync::BlockQueue;
use crate::utils::sync::Mutex;

lazy_static::lazy_static! {
    static ref PTMX: Arc<Ptmx> = Arc::new(Ptmx::new());
}

static PTS_FS: Once<Arc<PtsFs>> = Once::new();
static PTY_ID: AtomicU32 = AtomicU32::new(0);

struct Master {
    id: u32,
    wq: BlockQueue,
    window_size: Mutex<WinSize>,
    buffer: Mutex<Vec<u8>>,

    discipline: LineDiscipline,
}

impl Master {
    pub fn new() -> Self {
        Self {
            id: PTY_ID.fetch_add(1, Ordering::SeqCst),
            wq: BlockQueue::new(),
            window_size: Mutex::new(WinSize::default()),
            buffer: Mutex::new(Vec::new()),

            discipline: LineDiscipline::new(),
        }
    }
}

impl INodeInterface for Master {
    fn read_at(&self, _offset: usize, buffer: &mut [u8]) -> fs::Result<usize> {
        let mut pty_buffer = self.buffer.lock_irq();

        if pty_buffer.is_empty() {
            return Err(FileSystemError::WouldBlock);
        }

        let size = core::cmp::min(pty_buffer.len(), buffer.len());
        buffer[..size].copy_from_slice(&pty_buffer.drain(..size).collect::<Vec<_>>());
        Ok(size)
    }

    fn write_at(&self, _offset: usize, buffer: &[u8]) -> fs::Result<usize> {
        self.discipline.write(buffer);
        Ok(buffer.len())
    }

    fn poll(&self, table: Option<&mut fs::inode::PollTable>) -> fs::Result<fs::inode::PollFlags> {
        table.map(|e| e.insert(&self.wq));
        let mut flags = fs::inode::PollFlags::OUT;

        if !self.buffer.lock_irq().is_empty() {
            flags |= fs::inode::PollFlags::IN;
        }

        Ok(flags)
    }

    fn ioctl(&self, command: usize, arg: usize) -> fs::Result<usize> {
        match command {
            TIOCGPTN => {
                let id = VirtAddr::new(arg as u64).read_mut::<u32>()?;
                *id = self.id;
            }

            aero_syscall::TIOCSWINSZ => {
                let winsize = VirtAddr::new(arg as u64).read_mut::<WinSize>()?;
                *self.window_size.lock_irq() = *winsize;
            }

            _ => {
                log::warn!("ptmx: unknown ioctl (command={command:#x})")
            }
        }

        Ok(0)
    }
}

impl TerminalDevice for Master {
    fn attach(&self, task: Arc<Task>) {
        assert!(task.is_session_leader());
        self.discipline.set_foreground(task);
    }
}

struct SlaveInner {
    termios: Termios,
}

struct Slave {
    master: Arc<Master>,
    inner: Mutex<SlaveInner>,
}

impl Slave {
    pub fn new(master: Arc<Master>) -> Self {
        Self {
            master,
            inner: Mutex::new(SlaveInner {
                termios: Termios {
                    c_iflag: aero_syscall::TermiosIFlag::empty(),
                    c_oflag: aero_syscall::TermiosOFlag::ONLCR,
                    c_cflag: aero_syscall::TermiosCFlag::empty(),
                    c_lflag: aero_syscall::TermiosLFlag::ECHO | aero_syscall::TermiosLFlag::ICANON,
                    c_line: 0,
                    c_cc: [0; 32],
                    c_ispeed: 0,
                    c_ospeed: 0,
                },
            }),
        }
    }
}

impl INodeInterface for Slave {
    fn metadata(&self) -> fs::Result<fs::inode::Metadata> {
        Ok(fs::inode::Metadata {
            id: 0,
            file_type: FileType::Device,
            children_len: 0,
            size: 0,
        })
    }

    fn stat(&self) -> fs::Result<aero_syscall::Stat> {
        Ok(aero_syscall::Stat::default())
    }

    fn ioctl(&self, command: usize, arg: usize) -> fs::Result<usize> {
        let mut inner = self.inner.lock_irq();

        match command {
            aero_syscall::TIOCGWINSZ => {
                let winsize = VirtAddr::new(arg as u64).read_mut::<WinSize>()?;
                *winsize = *self.master.window_size.lock_irq();

                Ok(0)
            }

            aero_syscall::TCGETS => {
                let termios = VirtAddr::new(arg as u64).read_mut::<Termios>()?;
                *termios = inner.termios;

                Ok(0)
            }

            aero_syscall::TCSETSF => {
                let termios = VirtAddr::new(arg as u64).read_mut::<Termios>()?;
                inner.termios = *termios;

                Ok(0)
            }

            aero_syscall::TIOCSCTTY => {
                let current_task = scheduler::get_scheduler().current_task();
                assert!(current_task.is_session_leader());

                current_task.attach(self.master.clone());
                Ok(0)
            }

            _ => Err(FileSystemError::NotSupported),
        }
    }

    fn poll(&self, table: Option<&mut fs::inode::PollTable>) -> fs::Result<PollFlags> {
        if let Some(table) = table {
            table.insert(&self.master.wq);
            table.insert(self.master.discipline.wait_queue());
        }

        let mut flags = PollFlags::OUT;

        if !self.master.discipline.is_empty() {
            flags |= PollFlags::IN;
        }

        Ok(flags)
    }

    fn read_at(&self, _offset: usize, buffer: &mut [u8]) -> fs::Result<usize> {
        Ok(self.master.discipline.read(buffer)?)
    }

    fn write_at(&self, _offset: usize, buffer: &[u8]) -> fs::Result<usize> {
        if self
            .inner
            .lock_irq()
            .termios
            .c_oflag
            .contains(aero_syscall::TermiosOFlag::ONLCR)
        {
            let mut master = self.master.buffer.lock_irq();

            for b in buffer.iter() {
                if *b == b'\n' {
                    // ONLCR: Convert NL to CR + NL
                    master.extend_from_slice(&[b'\r', b'\n']);
                    continue;
                }

                master.push(*b);
            }
        } else {
            let mut pty_buffer = self.master.buffer.lock_irq();
            pty_buffer.extend_from_slice(buffer);
        }

        self.master.wq.notify_complete();
        Ok(buffer.len())
    }
}

struct Ptmx {
    device_id: usize,
}

impl Ptmx {
    fn new() -> Self {
        Self {
            device_id: devfs::alloc_device_marker(),
        }
    }
}

impl devfs::Device for Ptmx {
    fn device_marker(&self) -> usize {
        self.device_id
    }

    fn device_name(&self) -> String {
        String::from("ptmx")
    }

    fn inode(&self) -> Arc<dyn INodeInterface> {
        PTMX.clone()
    }
}

impl INodeInterface for Ptmx {
    fn open(
        &self,
        _flags: aero_syscall::OpenFlags,
        _handle: Arc<fs::file_table::FileHandle>,
    ) -> fs::Result<Option<DirCacheItem>> {
        let master = Arc::new(Master::new());
        let slave = Arc::new(Slave::new(master.clone()));
        let inode = DirEntry::from_inode(master, String::from("<pty>"));

        PTS_FS.get().unwrap().insert_slave(slave);
        Ok(Some(inode))
    }
}

#[derive(Default)]
struct PtsINode {
    inode: Once<INodeCacheItem>,
    fs: Once<Weak<PtsFs>>,
    slaves: RwLock<BTreeMap<u32, INodeCacheItem>>,
}

impl INodeInterface for PtsINode {
    fn metadata(&self) -> fs::Result<fs::inode::Metadata> {
        Ok(fs::inode::Metadata {
            id: 0,
            file_type: FileType::Directory,
            children_len: self.slaves.read().len(),
            size: 0,
        })
    }

    fn stat(&self) -> fs::Result<aero_syscall::Stat> {
        Ok(aero_syscall::Stat::default())
    }

    fn dirent(&self, parent: DirCacheItem, index: usize) -> fs::Result<Option<DirCacheItem>> {
        Ok(match index {
            0x00 => Some(DirEntry::new(
                parent,
                self.inode.get().unwrap().clone(),
                String::from("."),
            )),

            0x01 => Some(DirEntry::new(
                parent,
                self.inode.get().unwrap().clone(),
                String::from(".."),
            )),

            _ => {
                let a = self
                    .slaves
                    .read()
                    .iter()
                    .nth(index - 2)
                    .map(|(id, inode)| DirEntry::new(parent, inode.clone(), id.to_string()));
                log::debug!("{}", a.is_some());
                a
            }
        })
    }

    fn lookup(&self, dir: DirCacheItem, name: &str) -> fs::Result<DirCacheItem> {
        let id = name.parse::<u32>().unwrap();
        let slaves = self.slaves.read();

        let (_, inode) = slaves
            .iter()
            .find(|(&e, _)| e == id)
            .ok_or(FileSystemError::EntryNotFound)?;

        Ok(DirEntry::new(
            dir.clone(),
            inode.clone(),
            String::from(name),
        ))
    }

    fn weak_filesystem(&self) -> Option<Weak<dyn FileSystem>> {
        Some(self.fs.get()?.clone())
    }
}

struct PtsFs {
    root_dir: DirCacheItem,
}

impl PtsFs {
    fn new() -> Arc<Self> {
        let icache = cache::icache();
        let root_inode = icache.make_item_no_cache(CachedINode::new(Arc::new(PtsINode::default())));

        let root_dir = DirEntry::new_root(root_inode.clone(), String::from("/"));
        let pts_root = root_dir.inode().downcast_arc::<PtsINode>().unwrap();

        let this = Arc::new(Self { root_dir });

        // Initialize the PTS root inode.
        pts_root.fs.call_once(|| Arc::downgrade(&this));
        pts_root.inode.call_once(|| root_inode.clone());

        this
    }

    fn insert_slave(&self, slave: Arc<Slave>) {
        let icache = cache::icache();

        let pts_root = self.root_dir.inode().downcast_arc::<PtsINode>().unwrap();
        pts_root.slaves.write().insert(
            slave.master.id,
            icache.make_item_no_cache(CachedINode::new(slave)),
        );
    }
}

impl fs::FileSystem for PtsFs {
    fn root_dir(&self) -> DirCacheItem {
        self.root_dir.clone()
    }
}

fn pty_init() {
    devfs::install_device(PTMX.clone()).unwrap();

    let fs = PTS_FS.call_once(|| PtsFs::new());

    let root = DEV_FILESYSTEM.root_dir().inode();
    root.mkdir("pts").unwrap();

    let pts_dir = fs::lookup_path(Path::new("/dev/pts")).unwrap();
    MOUNT_MANAGER.mount(pts_dir, fs.clone()).unwrap();
}

crate::module_init!(pty_init, ModuleType::Other);
