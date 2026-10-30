//! FUSE filesystem: the Drive tree served from SQLite, content from the cache.
//! Phase 1: read-only.
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, Request,
};
use gdsync_core::cache::Cache;
use gdsync_core::db::{Db, Item, ROOT_INO};
use gdsync_core::names::{desktop_link, local_entries};
use libc::{EACCES, EBADF, EIO, EISDIR, ENOENT, ENOTDIR, EROFS};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime::Handle;
use tracing::{debug, warn};

const TTL: Duration = Duration::from_secs(1);

enum OpenFile {
    Disk(File),
    Mem(Vec<u8>),
}

pub struct DriveFs {
    db: Arc<Db>,
    cache: Arc<Cache>,
    rt: Handle,
    handles: Arc<Mutex<HashMap<u64, OpenFile>>>,
    next_fh: Arc<AtomicU64>,
    uid: u32,
    gid: u32,
}

impl DriveFs {
    pub fn new(db: Arc<Db>, cache: Arc<Cache>, rt: Handle) -> Self {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            db,
            cache,
            rt,
            handles: Arc::default(),
            next_fh: Arc::new(AtomicU64::new(1)),
            uid,
            gid,
        }
    }

    fn attr(&self, item: &Item) -> FileAttr {
        let (kind, perm, size) = if item.is_dir() {
            (FileType::Directory, 0o755, 0)
        } else if item.is_google_doc() {
            (FileType::RegularFile, 0o644, desktop_link(item).len() as u64)
        } else {
            (FileType::RegularFile, 0o644, item.size)
        };
        let t = UNIX_EPOCH + Duration::from_secs(item.mtime.max(0) as u64);
        FileAttr {
            ino: item.ino,
            size,
            blocks: size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind,
            perm,
            nlink: if item.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn item(&self, ino: u64) -> Option<Item> {
        self.db.by_ino(ino).unwrap_or_else(|e| {
            warn!("db: {e:#}");
            None
        })
    }

    fn entries(&self, dir: &Item) -> Vec<(String, Item)> {
        match self.db.children(&dir.id) {
            Ok(c) => local_entries(c),
            Err(e) => {
                warn!("db: {e:#}");
                Vec::new()
            }
        }
    }

    fn new_handle(&self, f: OpenFile) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, f);
        fh
    }
}

fn kind(item: &Item) -> FileType {
    if item.is_dir() {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

/// Indexers and thumbnailers read every file; letting them would download the
/// whole Drive. They only get files that are already cached.
fn is_background_reader(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let comm = comm.trim();
    if comm.starts_with("baloo") || comm.starts_with("tracker") || comm.starts_with("localsearch") {
        return true;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    String::from_utf8_lossy(&cmdline).contains("thumbnail")
}

/// pread that retries short reads (a short reply means EOF to the kernel).
fn read_full_at(f: &File, buf: &mut [u8], mut offset: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], offset)? {
            0 => break,
            n => {
                done += n;
                offset += n as u64;
            }
        }
    }
    Ok(done)
}

impl Filesystem for DriveFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = self.item(parent) else { return reply.error(ENOENT) };
        match self.entries(&dir).into_iter().find(|(n, _)| OsStr::new(n) == name) {
            Some((_, item)) => reply.entry(&TTL, &self.attr(&item), 0),
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.item(ino) {
            Some(item) => reply.attr(&TTL, &self.attr(&item)),
            None => reply.error(ENOENT),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(dir) = self.item(ino) else { return reply.error(ENOENT) };
        if !dir.is_dir() {
            return reply.error(ENOTDIR);
        }
        let parent_ino = dir
            .parent_id
            .as_deref()
            .and_then(|p| self.db.by_id(p).ok().flatten())
            .map_or(ROOT_INO, |p| p.ino);
        let mut list = vec![(ino, FileType::Directory, ".".to_string()), (parent_ino, FileType::Directory, "..".to_string())];
        list.extend(self.entries(&dir).into_iter().map(|(n, i)| (i.ino, kind(&i), n)));
        for (i, (ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(EROFS);
        }
        let Some(item) = self.item(ino) else { return reply.error(ENOENT) };
        if item.is_dir() {
            return reply.error(EISDIR);
        }
        if item.is_google_doc() {
            let fh = self.new_handle(OpenFile::Mem(desktop_link(&item).into_bytes()));
            return reply.opened(fh, 0);
        }
        if !self.cache.is_fresh(&item) && is_background_reader(req.pid()) {
            debug!("bloqueada lectura en segundo plano de {} (pid {})", item.name, req.pid());
            return reply.error(EACCES);
        }
        // Download off the FUSE thread so the rest of the filesystem stays responsive.
        let cache = self.cache.clone();
        let handles = self.handles.clone();
        let next_fh = self.next_fh.clone();
        self.rt.spawn(async move {
            let file = match cache.ensure(&item).await {
                Ok(path) => File::open(path).map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            match file {
                Ok(f) => {
                    let fh = next_fh.fetch_add(1, Ordering::Relaxed);
                    handles.lock().unwrap().insert(fh, OpenFile::Disk(f));
                    reply.opened(fh, 0);
                }
                Err(e) => {
                    warn!("no se pudo abrir {}: {e:#}", item.name);
                    reply.error(EIO);
                }
            }
        });
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let handles = self.handles.lock().unwrap();
        match handles.get(&fh) {
            Some(OpenFile::Disk(f)) => {
                let mut buf = vec![0u8; size as usize];
                match read_full_at(f, &mut buf, offset as u64) {
                    Ok(n) => reply.data(&buf[..n]),
                    Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
                }
            }
            Some(OpenFile::Mem(b)) => {
                let start = (offset as usize).min(b.len());
                let end = (start + size as usize).min(b.len());
                reply.data(&b[start..end]);
            }
            None => reply.error(EBADF),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.lock().unwrap().remove(&fh);
        reply.ok();
    }
}
