//! FUSE filesystem: the Drive tree served from SQLite with local on-demand caching
//! and bidirectional read/write operations (local edits, creation, trash, renames).
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
};
use gdsync_core::cache::Cache;
use gdsync_core::db::{Db, Item, ROOT_INO};
use gdsync_core::drive::FOLDER_MIME;
use gdsync_core::names::{desktop_link, local_entries};
use libc::{EACCES, EBADF, EEXIST, EIO, EISDIR, ENODATA, ENOENT, ENOTDIR, ENOTEMPTY, ERANGE};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime::Handle;
use tracing::{debug, warn};

const TTL: Duration = Duration::from_secs(1);

enum OpenFile {
    Disk(File),
    DiskRw { file: File, item_id: String, dirty: bool },
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
    total_space: Arc<AtomicU64>,
    used_space: Arc<AtomicU64>,
    notify_upload: Arc<tokio::sync::Notify>,
}

impl DriveFs {
    pub fn new(
        db: Arc<Db>,
        cache: Arc<Cache>,
        rt: Handle,
        total_space: Arc<AtomicU64>,
        used_space: Arc<AtomicU64>,
        notify_upload: Arc<tokio::sync::Notify>,
    ) -> Self {
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
            total_space,
            used_space,
            notify_upload,
        }
    }

    fn enqueue(&self, file_id: &str, op: &str, payload: Option<&str>) {
        if let Ok(id) = self.db.enqueue_upload(file_id, op, payload) {
            if id > 0 {
                self.notify_upload.notify_one();
            }
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

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(item) = self.item(ino) else { return reply.error(ENOENT) };
        if let Some(new_size) = size {
            let path = self.cache.path(&item.id);
            if let Some(h) = fh {
                let handles = self.handles.lock().unwrap();
                if let Some(OpenFile::DiskRw { file, .. }) = handles.get(&h) {
                    let _ = file.set_len(new_size);
                }
            } else if path.exists() {
                let _ = OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(new_size));
            }
            let _ = self.db.update_item_size_mtime(&item.id, new_size, gdsync_core::now());
            self.enqueue(&item.id, "update", None);
        }
        let updated = self.item(ino).unwrap_or(item);
        reply.attr(&TTL, &self.attr(&updated));
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
        let mut list = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];
        list.extend(self.entries(&dir).into_iter().map(|(n, i)| (i.ino, kind(&i), n)));
        for (i, (ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(item) = self.item(ino) else { return reply.error(ENOENT) };
        if item.is_dir() {
            return reply.error(EISDIR);
        }
        if item.is_google_doc() {
            let fh = self.new_handle(OpenFile::Mem(desktop_link(&item).into_bytes()));
            return reply.opened(fh, 0);
        }

        let write_mode = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        if !write_mode && !self.cache.is_fresh(&item) && is_background_reader(req.pid()) {
            debug!("bloqueada lectura en segundo plano de {} (pid {})", item.name, req.pid());
            return reply.error(EACCES);
        }

        let cache = self.cache.clone();
        let handles = self.handles.clone();
        let next_fh = self.next_fh.clone();
        let item_id = item.id.clone();

        self.rt.spawn(async move {
            let path = match cache.ensure(&item).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("no se pudo asegurar {} en caché: {e:#}", item.name);
                    return reply.error(EIO);
                }
            };

            let file_res = if write_mode {
                OpenOptions::new().read(true).write(true).open(&path)
            } else {
                File::open(&path)
            };

            match file_res {
                Ok(f) => {
                    let fh = next_fh.fetch_add(1, Ordering::Relaxed);
                    let handle = if write_mode {
                        OpenFile::DiskRw { file: f, item_id, dirty: false }
                    } else {
                        OpenFile::Disk(f)
                    };
                    handles.lock().unwrap().insert(fh, handle);
                    reply.opened(fh, 0);
                }
                Err(e) => {
                    warn!("error abriendo archivo en disco: {e:#}");
                    reply.error(e.raw_os_error().unwrap_or(EIO));
                }
            }
        });
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_item) = self.item(parent) else { return reply.error(ENOENT) };
        if !parent_item.is_dir() {
            return reply.error(ENOTDIR);
        }
        let name_str = name.to_string_lossy().to_string();
        let rand_id: u64 = rand::random();
        let local_id = format!("local_{rand_id:016x}");
        let path = self.cache.path(&local_id);

        let file = match OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path) {
            Ok(f) => f,
            Err(e) => return reply.error(e.raw_os_error().unwrap_or(EIO)),
        };

        let now = gdsync_core::now();
        let ino = match self.db.insert_local_item(&local_id, Some(&parent_item.id), &name_str, "application/octet-stream", 0, now) {
            Ok(i) => i,
            Err(_) => return reply.error(EIO),
        };
        let Some(created_item) = self.item(ino) else { return reply.error(EIO) };
        let _ = self.db.cache_put(&local_id, &created_item.version(), 0);
        self.enqueue(&local_id, "create", None);

        let fh = self.new_handle(OpenFile::DiskRw { file, item_id: local_id.clone(), dirty: true });
        reply.created(&TTL, &self.attr(&created_item), 0, fh, 0);
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(parent_item) = self.item(parent) else { return reply.error(ENOENT) };
        if !parent_item.is_dir() {
            return reply.error(ENOTDIR);
        }
        let name_str = name.to_string_lossy().to_string();
        let rand_id: u64 = rand::random();
        let local_id = format!("local_folder_{rand_id:016x}");

        let now = gdsync_core::now();
        let ino = match self.db.insert_local_item(&local_id, Some(&parent_item.id), &name_str, FOLDER_MIME, 0, now) {
            Ok(i) => i,
            Err(_) => return reply.error(EIO),
        };
        self.enqueue(&local_id, "mkdir", None);

        let Some(created_item) = self.item(ino) else { return reply.error(EIO) };
        reply.entry(&TTL, &self.attr(&created_item), 0);
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
            Some(OpenFile::Disk(f)) | Some(OpenFile::DiskRw { file: f, .. }) => {
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

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let mut handles = self.handles.lock().unwrap();
        match handles.get_mut(&fh) {
            Some(OpenFile::DiskRw { file, item_id, dirty }) => {
                if let Err(e) = file.write_all_at(data, offset as u64) {
                    return reply.error(e.raw_os_error().unwrap_or(EIO));
                }
                *dirty = true;
                let new_size = file.metadata().map(|m| m.len()).unwrap_or(offset as u64 + data.len() as u64);
                let _ = self.db.update_item_size_mtime(item_id, new_size, gdsync_core::now());
                reply.written(data.len() as u32);
            }
            Some(OpenFile::Disk(_)) => reply.error(EACCES),
            Some(OpenFile::Mem(_)) => reply.error(EACCES),
            None => reply.error(EBADF),
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let mut handles = self.handles.lock().unwrap();
        if let Some(OpenFile::DiskRw { item_id, dirty, .. }) = handles.get_mut(&fh) {
            if *dirty {
                self.enqueue(item_id, "update", None);
                *dirty = false;
            }
        }
        reply.ok();
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
        let mut handles = self.handles.lock().unwrap();
        if let Some(OpenFile::DiskRw { item_id, dirty, .. }) = handles.remove(&fh) {
            if dirty {
                self.enqueue(&item_id, "update", None);
            }
        }
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_item) = self.item(parent) else { return reply.error(ENOENT) };
        let name_str = name.to_string_lossy();
        let target = self
            .entries(&parent_item)
            .into_iter()
            .find(|(n, _)| n == &name_str)
            .map(|(_, item)| item);

        let Some(item) = target else { return reply.error(ENOENT) };
        if item.is_dir() {
            return reply.error(EISDIR);
        }

        let payload = serde_json::json!({ "name": item.name }).to_string();
        let _ = self.db.remove(&item.id);
        self.cache.forget(&item.id);
        if item.id.starts_with("local_") {
            let _ = self.db.cancel_uploads(&item.id);
        } else {
            self.enqueue(&item.id, "trash", Some(&payload));
        }
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_item) = self.item(parent) else { return reply.error(ENOENT) };
        let name_str = name.to_string_lossy();
        let target = self
            .entries(&parent_item)
            .into_iter()
            .find(|(n, _)| n == &name_str)
            .map(|(_, item)| item);

        let Some(item) = target else { return reply.error(ENOENT) };
        if !item.is_dir() {
            return reply.error(ENOTDIR);
        }

        let children = self.db.children(&item.id).unwrap_or_default();
        if !children.is_empty() {
            return reply.error(ENOTEMPTY);
        }

        let payload = serde_json::json!({ "name": item.name }).to_string();
        let _ = self.db.remove(&item.id);
        if item.id.starts_with("local_") {
            let _ = self.db.cancel_uploads(&item.id);
        } else {
            self.enqueue(&item.id, "trash", Some(&payload));
        }
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(parent_item) = self.item(parent) else { return reply.error(ENOENT) };
        let Some(newparent_item) = self.item(newparent) else { return reply.error(ENOENT) };
        if !newparent_item.is_dir() {
            return reply.error(ENOTDIR);
        }

        let name_str = name.to_string_lossy();
        let newname_str = newname.to_string_lossy().to_string();

        let target = self
            .entries(&parent_item)
            .into_iter()
            .find(|(n, _)| n == &name_str)
            .map(|(_, item)| item);

        let Some(item) = target else { return reply.error(ENOENT) };

        // Check if destination already exists
        if let Ok(Some(existing)) = self.db.lookup_child(&newparent_item.id, &newname_str) {
            if existing.id == item.id {
                return reply.ok();
            }

            let flags_noreplace = (flags & 1) != 0; // libc::RENAME_NOREPLACE
            if flags_noreplace {
                return reply.error(EEXIST);
            }

            if item.is_dir() {
                if !existing.is_dir() {
                    return reply.error(ENOTDIR);
                }
                let children = self.db.children(&existing.id).unwrap_or_default();
                if !children.is_empty() {
                    return reply.error(ENOTEMPTY);
                }
            } else if existing.is_dir() {
                return reply.error(EISDIR);
            }

            // Editor atomic save replacing an existing remote file:
            // Adopt the existing remote ID so Google Drive updates the file in place
            // rather than trashing it and creating an untracked new file ID.
            if !existing.is_dir() && !item.is_dir() && !existing.id.starts_with("local_") && item.id.starts_with("local_") {
                let now = gdsync_core::now();
                let file_size = std::fs::metadata(self.cache.path(&item.id)).map(|m| m.len()).unwrap_or(item.size);
                if let Err(e) = self.db.replace_existing_with_local(&existing.id, item.ino, &newname_str, &newparent_item.id, file_size, now) {
                    warn!("error reemplazando archivo existente en db: {e:#}");
                    return reply.error(EIO);
                }
                let _ = std::fs::rename(self.cache.path(&item.id), self.cache.path(&existing.id));
                let _ = self.db.cache_put(&existing.id, &format!("mtime:{now}"), file_size);
                let _ = self.db.cancel_uploads(&item.id);
                self.enqueue(&existing.id, "update", None);
                return reply.ok();
            }

            // General replacement: delete existing destination
            let _ = self.db.remove(&existing.id);
            self.cache.forget(&existing.id);
            if existing.id.starts_with("local_") {
                let _ = self.db.cancel_uploads(&existing.id);
            } else {
                self.enqueue(&existing.id, "trash", None);
            }
        }

        let moved_parent = if parent != newparent { Some(newparent_item.id.as_str()) } else { None };
        if let Err(_) = self.db.rename_item(&item.id, &newname_str, moved_parent) {
            return reply.error(EIO);
        }

        if !item.id.starts_with("local_") {
            if item.name != newname_str {
                let payload = serde_json::json!({ "name": newname_str }).to_string();
                self.enqueue(&item.id, "rename", Some(&payload));
            }
            if parent != newparent {
                let payload = serde_json::json!({
                    "new_parent": newparent_item.id,
                    "old_parent": parent_item.id,
                }).to_string();
                self.enqueue(&item.id, "move", Some(&payload));
            }
        }

        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let total = self.total_space.load(Ordering::Relaxed);
        let used = self.used_space.load(Ordering::Relaxed);
        let free = total.saturating_sub(used);
        let bsize = 4096u32;
        let blocks = total / bsize as u64;
        let bfree = free / bsize as u64;
        let files = self.db.count_items().unwrap_or(1000);
        reply.statfs(blocks, bfree, bfree, files, 1_000_000, bsize, 255, bsize);
    }

    fn getxattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(item) = self.item(ino) else { return reply.error(ENOENT) };
        let name_str = name.to_string_lossy();
        let value = match name_str.as_ref() {
            "user.gdsync.pinned" => (if item.pinned { "1" } else { "0" }).as_bytes().to_vec(),
            "user.gdsync.status" => {
                let status = if item.pinned {
                    "pinned"
                } else if self.cache.is_fresh(&item) {
                    "synced"
                } else {
                    "cloud_only"
                };
                status.as_bytes().to_vec()
            }
            _ => return reply.error(ENODATA),
        };

        if size == 0 {
            reply.size(value.len() as u32);
        } else if size >= value.len() as u32 {
            reply.data(&value);
        } else {
            reply.error(ERANGE);
        }
    }

    fn setxattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        let Some(item) = self.item(ino) else { return reply.error(ENOENT) };
        let name_str = name.to_string_lossy();
        match name_str.as_ref() {
            "user.gdsync.pinned" => {
                let pin = value == b"1" || value == b"true";
                let _ = self.db.set_pinned(&item.id, pin);
                reply.ok();
            }
            _ => reply.error(ENODATA),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, _ino: u64, size: u32, reply: ReplyXattr) {
        let attrs = b"user.gdsync.pinned\0user.gdsync.status\0";
        if size == 0 {
            reply.size(attrs.len() as u32);
        } else if size >= attrs.len() as u32 {
            reply.data(attrs);
        } else {
            reply.error(ERANGE);
        }
    }
}
