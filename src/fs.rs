use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    os::{unix::{fs::{MetadataExt, PermissionsExt}, ffi::OsStrExt}},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crossbeam_channel as mpmc;
use fuser::{FileAttr, Filesystem, Request as FuseRequest, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs};
use libc::c_int;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};

/// Time that we tell the kernel our data is good for. We cache for much longer
/// than this, but we want the kernel to come to us with questions...
const TIME_TO_LIVE: Duration = Duration::from_secs(10);
/// Time after which lookups will be rechecked. The lookups are NOT discarded
/// on recheck!
const LOOKUP_EXPIRE_TIME: Duration = Duration::from_secs(600);
/// Number of bytes we use when calculating "block sizes" in our reply records
const BLOCK_SIZE: u64 = 512;
/// Number of bytes to buffer ahead.
const BUFFER_AHEAD: usize = 512 * 1024 * 1024; // 512MiB
/// Number of bytes to buffer "behind".
const BUFFER_BEHIND: usize = 256 * 1024 * 1024; // 256MiB
/// Total number of bytes in the buffer at once
const BUFFER_TOTAL_SIZE: usize = BUFFER_AHEAD + BUFFER_BEHIND;
/// Amount to read at once. 32KiB takes less than 3ms to transfer on a 100Mb
/// link.
const READ_SIZE: usize = 32768;

struct RoraFile {
    path: PathBuf,
    ino: u64,
    /// The file handle we're using to read. Only used by the request handler
    /// thread.
    underlying_file: Option<(File,u64)>,
    /// Number of bytes behind `buffer_pos` currently still buffered
    buffered_behind: usize,
    /// Where, in the file, is the next read expected
    buffer_pos: u64,
    /// How big we think the file is
    file_size: u64,
    buffer: VecDeque<u8>,
}

#[derive(Debug)]
enum Request {
    /// Perform a metadata lookup on the given fake path
    Lookup(PathBuf),
    /// Perform a readdir on the given fake inode
    Readdir(u64),
    /// Sempai, notice that a file needs servicing
    Bump,
}

struct Dingus {
    inodes_for_paths: HashMap<PathBuf, (u64, u64)>,
    paths_for_inodes: HashMap<u64, PathBuf>,
    readdirs: HashMap<u64, (Instant, Vec<(u64, OsString)>)>,
    lookups: HashMap<PathBuf, (Instant, FileAttr, u64)>,
    open_files: Vec<Option<RoraFile>>,
}

pub(crate) struct FS {
    atty: bool,
    dingus: Arc<RwLock<Dingus>>,
    readlinks: HashMap<u64, Vec<u8>>,
    srcpath: PathBuf,
    /// maps a full path to an (inode, generation) pair
    request_tx: mpmc::Sender<(Request, Option<mpmc::Sender<Result<(), c_int>>>)>,
}

impl Filesystem for FS {
    fn init(&mut self, _req: &FuseRequest<'_>, config: &mut fuser::KernelConfig) -> Result<(), libc::c_int> {
        const FLAGS: u32 = 1 << 13; // FUSE_DO_READDIRPLUS
        let _ = config.add_capabilities(FLAGS);
        Ok(())
    }
    fn lookup(&mut self, _req: &FuseRequest<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let lock = self.dingus.read();
        let path = match Self::path_from_inoderef(&*lock, parent, name) {
            Err(_) => {
                reply.error(libc::ENOENT);
                return
            },
            Ok(x) => x,
        };
        match lock.lookups.get(&path) {
            None => {
                drop(lock);
                if let Err(x) = Request::Lookup(path.to_owned()).send_sync(&mut self.request_tx) {
                    reply.error(x);
                    return
                }
                match self.dingus.read().lookups.get(&path) {
                    None => {
                        reply.error(libc::ENOENT);
                        return
                    },
                    Some((_when_expire, what, genny)) => {
                        reply.entry(&TIME_TO_LIVE, what, *genny);
                        return
                    },
                }
            },
            Some((when_expire, what, genny)) => {
                // send the cached response, but...
                reply.entry(&TIME_TO_LIVE, what, *genny);
                if when_expire < &Instant::now() {
                    // send a request to recheck this, but don't wait for it
                    Request::Lookup(path.to_owned()).send_async(&mut self.request_tx);
                }
                return;
            },
        }
    }
    fn getattr(&mut self, _req: &FuseRequest<'_>, ino: u64, reply: ReplyAttr) {
        let dingus = self.dingus.read();
        match dingus.paths_for_inodes.get(&ino) {
            None => reply.error(libc::ENOENT),
            Some(path) => {
                match dingus.lookups.get(path) {
                    Some((_, what, _)) => reply.attr(&TIME_TO_LIVE, &what),
                    _ => reply.error(libc::ENOENT),
                }
            },
        }
    }
    fn readlink(&mut self, _req: &FuseRequest<'_>, ino: u64, reply: ReplyData) {
        // we don't use the alternate thread for readlink. it's going to be
        // very rare and we cache it forever.
        if let Some(data) = self.readlinks.get(&ino) {
            reply.data(data);
            return;
        }
        let dingus = self.dingus.read();
        match dingus.paths_for_inodes.get(&ino) {
            None => reply.error(libc::ENOENT),
            Some(path) => {
                match nix::fcntl::readlink(&self.srcpath.join(path)) {
                    Err(x) => {
                        reply.error(x as i32);
                        return
                    },
                    Ok(x) => {
                        reply.data(x.as_bytes());
                        self.readlinks.insert(ino, x.as_bytes().to_vec());
                        return
                    },
                }
            },
        }
    }
    fn open(&mut self, _req: &FuseRequest<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let lock = self.dingus.read();
        let path = match lock.paths_for_inodes.get(&ino) {
            Some(x) => x,
            None => {
                reply.error(libc::ENOENT);
                return
            },
        };
        let attrs = match lock.lookups.get(path) {
            Some(x) if x.1.kind == fuser::FileType::Directory => {
                reply.error(libc::EISDIR);
                return
            },
            Some(x) if x.1.kind != fuser::FileType::RegularFile => {
                reply.error(libc::EINVAL);
                return
            },
            Some(x) => x,
            None => {
                reply.error(libc::EIO);
                return
            },
        };
        let path = path.to_owned();
        let file_size = attrs.1.size;
        drop(lock);
        let underlying_file = match File::open(self.srcpath.join(&path)) {
            Ok(x) => x,
            Err(x) => {
                reply.error(x.raw_os_error().unwrap_or(libc::EIO));
                return
            },
        };
        let file = RoraFile {
            path: path,
            ino,
            underlying_file: Some((underlying_file, 0)),
            buffered_behind: 0,
            buffer_pos: 0,
            file_size,
            buffer: VecDeque::with_capacity((file_size.min(usize::MAX as u64) as usize).min(BUFFER_TOTAL_SIZE)),
        };
        let mut lock = self.dingus.write();
        let fd = lock.open_files.iter().position(Option::is_none).unwrap_or(lock.open_files.len());
        if fd >= lock.open_files.len() {
            assert_eq!(fd, lock.open_files.len());
            lock.open_files.push(Some(file));
        }
        else {
            assert!(lock.open_files[fd].is_none());
            lock.open_files[fd] = Some(file);
        }
        reply.opened(fd as u64, 0);
    }
    fn read(
        &mut self,
        _req: &FuseRequest<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return
        }
        let offset = offset as u64;
        let fh: usize = match fh.try_into() {
            Ok(x) => x,
            Err(_) => {
                reply.error(libc::EBADFD);
                return
            },
        };
        let mut reply = Some(reply);
        // Annoyingly, FUSE interprets any short read as indicative of EOF.
        let mut buf = None;
        if let Err(x) = self.inner_read(fh, offset, size, |x| {
            debug_assert!(buf.is_none());
            debug_assert!(x.len() <= size as usize);
            if x.len() == size as usize {
                reply.take().unwrap().data(x);
                buf = None;
            }
            else {
                let mut vec: Vec<u8> = Vec::with_capacity(size as usize);
                vec.extend(x);
                buf = Some(vec);
            }
        }) {
            reply.take().unwrap().error(x);
        }
        else if let Some(mut vec) = buf {
            let total_size = size;
            let mut offset = offset + vec.len() as u64;
            let mut size = size - vec.len() as u32;
            let mut eof = false;
            while vec.len() < total_size as usize && !eof {
                if let Err(x) = self.inner_read(fh, offset, size, |x| {
                    if x.len() == 0 {
                        eof = true
                    }
                    else {
                        debug_assert!(x.len() <= size as usize);
                        vec.extend(x);
                        offset += x.len() as u64;
                        size -= x.len() as u32;
                    }
                }) {
                    reply.take().unwrap().error(x);
                    return
                }
            }
            reply.take().unwrap().data(&vec[..]);
        }
        assert!(reply.is_none());
    }
    fn release(
        &mut self,
        _req: &FuseRequest<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty
    ) {
        let fh: usize = match fh.try_into() {
            Ok(x) => x,
            Err(_) => {
                reply.error(libc::EBADFD);
                return
            },
        };
        let mut lock = self.dingus.write();
        if fh >= lock.open_files.len() || lock.open_files[fh].is_none() {
            reply.error(libc::EBADFD);
            return
        }
        lock.open_files[fh] = None;
        // make sure the status line can update
        Request::Bump.send_async(&mut self.request_tx);
        reply.ok()
    }
    fn opendir(&mut self, _req: &FuseRequest<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let lock = self.dingus.read();
        if let Some((when, _)) = lock.readdirs.get(&ino) {
            if when < &Instant::now() {
                // send another request, just in case
                Request::Readdir(ino).send_async(&mut self.request_tx);
                // (but don't wait)
            }
            reply.opened(ino, 0);
            return
        }
        drop(lock);
        // send a request, and see if it worked
        match Request::Readdir(ino).send_sync(&mut self.request_tx) {
            Err(x) => reply.error(x),
            Ok(_) => reply.opened(ino, 0),
        }
    }
    fn readdir(
        &mut self,
        _req: &FuseRequest<'_>,
        ino: u64,
        _fh: u64,
        mut offset: i64,
        mut reply: ReplyDirectory
    ) {
        let lock = self.dingus.read();
        match lock.readdirs.get(&ino) {
            None => reply.error(libc::EIO),
            Some((_, inos)) => {
                while (offset as usize) < inos.len() {
                    let o = offset as usize;
                    let path = match lock.paths_for_inodes.get(&inos[o].0) {
                        Some(x) => x,
                        None => {
                            reply.error(libc::EIO);
                            return
                        },
                    };
                    let lookup = lock.lookups.get(path).unwrap();
                    offset += 1;
                    if reply.add(inos[o].0, offset, lookup.1.kind, &inos[o].1) { break }
                }
                reply.ok();
                return
            }
        }
    }
    fn readdirplus(
        &mut self,
        _req: &FuseRequest<'_>,
        ino: u64,
        _fh: u64,
        mut offset: i64,
        mut reply: ReplyDirectoryPlus
    ) {
        let lock = self.dingus.read();
        match lock.readdirs.get(&ino) {
            None => reply.error(libc::EIO),
            Some((_, inos)) => {
                while (offset as usize) < inos.len() {
                    let o = offset as usize;
                    let path = match lock.paths_for_inodes.get(&inos[o].0) {
                        Some(x) => x,
                        None => {
                            reply.error(libc::EIO);
                            return
                        },
                    };
                    let lookup = lock.lookups.get(path).unwrap();
                    offset += 1;
                    if reply.add(inos[o].0, offset, &inos[o].1, &TIME_TO_LIVE, &lookup.1, lookup.2) { break }
                }
                reply.ok();
                return
            }
        }
    }
    fn releasedir(
        &mut self,
        _req: &FuseRequest<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty
    ) {
        reply.ok()
    }
    fn statfs(&mut self, _req: &FuseRequest<'_>, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 0, u32::MAX, 0)
    }
}

impl FS {
    pub fn new(srcpath: PathBuf) -> FS {
        let atty = atty::is(atty::Stream::Stdout);
        let dingus = Arc::new(RwLock::new(Dingus {
            inodes_for_paths: HashMap::new(),
            paths_for_inodes: [(1, PathBuf::from("."))].into_iter().collect(),
            readdirs: HashMap::new(),
            lookups: HashMap::new(),
            open_files: vec![],
        }));
        let (tx, rx) = mpmc::unbounded();
        let _ = tx.send((Request::Lookup(PathBuf::from(".")), None));
        let srcpath_clone = srcpath.clone();
        let dingus_clone = dingus.clone();
        let _fsthread = std::thread::spawn(move || {
            fsthread(srcpath_clone, rx, dingus_clone)
        });
        FS {
            dingus,
            atty,
            request_tx: tx,
            srcpath,
            readlinks: HashMap::new(),
        }
    }
    fn path_from_inoderef(dingus: &Dingus, parent: u64, filename: &OsStr) -> Result<PathBuf, ()> {
        dingus.paths_for_inodes.get(&parent).map(|x| x.join(filename)).ok_or(())
    }
    fn inner_read<H: FnMut(&[u8])>(&mut self, fh: usize, offset: u64, size: u32, mut handler: H) -> Result<(), i32> {
        let mut lock = self.dingus.write();
        if fh >= lock.open_files.len() || lock.open_files[fh].is_none() {
            return Err(libc::EBADFD)
        }
        let file = lock.open_files[fh].as_ref().unwrap();
        let start_offset = offset.min(file.file_size);
        if start_offset == file.file_size {
            handler(&[]);
            return Ok(())
        }
        drop(file);
        let end_offset = (start_offset + size as u64).min(file.file_size);
        let size = (end_offset - start_offset).min(usize::MAX as u64) as usize;
        while lock.open_files[fh].as_mut().unwrap().seek(start_offset) {
            drop(lock);
            Request::Bump.send_sync(&mut self.request_tx).unwrap();
            lock = self.dingus.write();
        }
        let file = lock.open_files[fh].as_mut().unwrap();
        debug_assert_eq!(file.buffer_pos, start_offset);
        let buffer_start_pos = file.buffer_pos - file.buffered_behind as u64;
        let offset_in_buffer = (start_offset - buffer_start_pos) as usize;
        let (first_slice, second_slice) = file.buffer.as_slices();
        debug_assert_eq!(first_slice.len() + second_slice.len(), file.buffer.len());
        let ret = if offset_in_buffer < first_slice.len() {
            let size = size.min(first_slice.len() - offset_in_buffer);
            &first_slice[offset_in_buffer .. offset_in_buffer + size]
        }
        else {
            let size = size.min(second_slice.len() + first_slice.len() - offset_in_buffer);
            &second_slice[offset_in_buffer - first_slice.len() .. offset_in_buffer - first_slice.len() + size]
        };
        let amount_read = ret.len();
        file.buffer_pos += amount_read as u64;
        file.buffered_behind += amount_read;
        handler(ret);
        Ok(())
    }
}

impl Drop for FS {
    fn drop(&mut self) {
        if self.atty {

        }
    }
}

fn fsthread(
    srcpath: PathBuf,
    rx: mpmc::Receiver<(Request, Option<mpmc::Sender<Result<(), c_int>>>)>,
    dingus: Arc<RwLock<Dingus>>
) {
    let mut o = if atty::is(atty::Stream::Stdout) {
        let mut io = liso::InputOutput::new();
        io.prompt("", false, true);
        let o = io.clone_output();
        std::thread::Builder::new().name(format!("quit listener")).spawn(move ||{
            loop {
                let x = io.read_blocking();
                match x {
                    liso::Response::Dead | liso::Response::Quit => break,
                    _ => (),
                }
            }
            drop(io);
            std::process::exit(0);
        }).unwrap();
        Some(o)
    } else { None };
    let mut buf = [0u8; READ_SIZE];
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let mut next_inode = 2;
    'outer: loop {
        // TODO: files
        let (req, notifier) = {
            let req = match rx.try_recv() {
                Err(mpmc::TryRecvError::Empty) => None,
                Err(mpmc::TryRecvError::Disconnected) => break,
                Ok(x) => Some(x),
            };
            if req.is_none() {
                if let Some(o) = o.as_mut() {
                    update_status(o, &dingus);
                }
            }
            match req {
                None | Some((Request::Bump, _)) => {
                    let lock = dingus.upgradable_read();
                    let mut file_needs_service = None;
                    for (i, f) in lock.open_files.iter().enumerate().filter_map(|(i,x)| {
                        if let Some(x) = x { Some((i,x)) } else { None }
                    }) {
                        let needs_service = f.needs_service();
                        if needs_service > file_needs_service.map(|(_,x)| x).unwrap_or(0) {
                            file_needs_service = Some((i, needs_service));
                        }
                    }
                    if let Some((i, _)) = file_needs_service {
                        let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
                        let f = lock.open_files[i].as_mut().unwrap();
                        let (mut file, pos) = f.underlying_file.take().unwrap();
                        let ino = f.ino;
                        let cur_end_pos = f.buffer_pos - f.buffered_behind as u64 + f.buffer.len() as u64;
                        let size = f.file_size;
                        drop(f);
                        drop(lock);
                        debug_assert_eq!(file.stream_position().unwrap(), pos);
                        if pos != cur_end_pos {
                            file.seek(SeekFrom::Start(cur_end_pos)).expect("TODO: handle IO errors on seek");
                        }
                        let amount_to_read = (size - cur_end_pos).min(READ_SIZE as u64) as usize;
                        let amount_read = file.read(&mut buf[..amount_to_read]).expect("TODO: handle IO errors on read");
                        let mut lock = dingus.write();
                        let fh = &mut lock.open_files[i];
                        match fh {
                            None => (), // Oh well, file got closed
                            Some(f) => {
                                // File may have been closed, and handle reused
                                if f.ino == ino && f.underlying_file.is_none() {
                                    let pos = cur_end_pos + amount_read as u64;
                                    debug_assert_eq!(file.stream_position().unwrap(), pos);
                                    f.underlying_file = Some((file, pos));
                                    f.maybe_accept_moar(cur_end_pos, &buf[..amount_read]);
                                }
                            },
                        }
                        if let Some((Request::Bump, Some(notifier))) = req {
                            let _ = notifier.send(Ok(()));
                        }
                        continue
                    }
                    else {
                        drop(lock);
                    }
                    match req {
                        None => {
                            match rx.recv() {
                                Ok(x) => x,
                                Err(_) => break,
                            }
                        },
                        Some(x) => x,
                    }
                },
                Some(x) => x,
            }
        };
        match req {
            Request::Lookup(rawpath) => {
                assert!(!rawpath.has_root());
                let path = srcpath.join(&rawpath);
                let mut dingus = dingus.write();
                let metadata = match path.symlink_metadata() {
                    Ok(x) => x,
                    Err(x) => {
                        if let Some(notifier) = notifier {
                            let _ = notifier.send(Err(x.raw_os_error().unwrap_or(libc::EIO)));
                        }
                        continue;
                    },
                };
                add_lookup(metadata, rawpath, &mut *dingus, uid, gid, &mut next_inode);
                if let Some(notifier) = notifier {
                    let _ = notifier.send(Ok(()));
                }
                continue;
            },
            Request::Readdir(ino) => {
                let mut dingus = dingus.write();
                let rawpath = match dingus.paths_for_inodes.get(&ino) {
                    None => {
                        if let Some(notifier) = notifier {
                            let _ = notifier.send(Err(libc::ENOENT));
                        }
                        continue;
                    },
                    Some(x) if dingus.lookups.get(x).map(|x| x.1.kind != fuser::FileType::Directory).unwrap_or(true) => {
                        if let Some(notifier) = notifier {
                            let _ = notifier.send(Err(libc::ENOTDIR));
                        }
                        continue;
                    },
                    Some(x) => x,
                }.clone();
                let path = srcpath.join(&rawpath);
                let dir = match std::fs::read_dir(&path) {
                    Ok(x) => x,
                    Err(x) => {
                        if let Some(notifier) = notifier {
                            let _ = notifier.send(Err(x.raw_os_error().unwrap_or(libc::EIO)));
                        }
                        continue;
                    },
                };
                let mut red = Vec::new();
                for ent in dir {
                    let ent = match ent {
                        Ok(x) => x,
                        Err(x) => {
                            if let Some(notifier) = notifier {
                                let _ = notifier.send(Err(x.raw_os_error().unwrap_or(libc::EIO)));
                            }
                            continue 'outer;
                        }
                    };
                    let file_name = ent.file_name();
                    if file_name == "." || file_name == ".." {
                        continue
                    }
                    let metadata = match ent.metadata() {
                        Ok(x) => x,
                        Err(x) => {
                            if let Some(notifier) = notifier {
                                let _ = notifier.send(Err(x.raw_os_error().unwrap_or(libc::EIO)));
                            }
                            continue 'outer;
                        }
                    };
                    let truepath = rawpath.join(ent.file_name());
                    let ino = add_lookup(metadata, truepath, &mut *dingus, uid, gid, &mut next_inode);
                    red.push((ino, file_name));
                }
                dingus.readdirs.insert(ino, (Instant::now() + LOOKUP_EXPIRE_TIME, red));
                if let Some(notifier) = notifier {
                    let _ = notifier.send(Ok(()));
                    continue
                }
            },
            Request::Bump => {
                if let Some(notifier) = notifier {
                    let _ = notifier.send(Ok(()));
                    continue
                }
            },
        }
        #[allow(unreachable_code)]
        if let Some(notifier) = notifier {
            let _ = notifier.send(Err(libc::EIO));
        }
    }
}

fn add_lookup(metadata: std::fs::Metadata, path: PathBuf, dingus: &mut Dingus, uid: u32, gid: u32, next_inode: &mut u64) -> u64 {
    let (ino, old_generation) = match dingus.inodes_for_paths.get(&path) {
        None => {
            let ino = *next_inode;
            dingus.inodes_for_paths.insert(path.clone(), (ino, 1));
            dingus.paths_for_inodes.insert(ino, path.clone());
            *next_inode += 1;
            (ino, 1)
        },
        Some(x) => *x,
    };
    let new_attrs = FileAttr {
        ino,
        size: metadata.size(),
        blocks: (metadata.size() + (BLOCK_SIZE-1)) / BLOCK_SIZE,
        atime: SystemTime::UNIX_EPOCH + Duration::new(metadata.atime() as u64, metadata.atime_nsec() as u32),
        mtime: SystemTime::UNIX_EPOCH + Duration::new(metadata.mtime() as u64, metadata.mtime_nsec() as u32),
        ctime: SystemTime::UNIX_EPOCH + Duration::new(metadata.ctime() as u64, metadata.ctime_nsec() as u32),
        crtime: metadata.created().unwrap_or_else(|_| {
            SystemTime::UNIX_EPOCH + Duration::new(metadata.mtime() as u64, metadata.mtime_nsec() as u32)
        }),
        kind: match metadata.file_type() {
            x if x.is_symlink() => fuser::FileType::Symlink,
            x if x.is_dir() => fuser::FileType::Directory,
            _ => fuser::FileType::RegularFile,
        },
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: BLOCK_SIZE as u32,
        perm: if metadata.permissions().mode() & 0o111 != 0 {
            0o555
        } else { 0o444 },
        flags: 0, // macOS cruft?
    };
    let new_generation = match dingus.lookups.get(&path) {
        Some(x) if x.1 != new_attrs => {
            old_generation + 1
        },
        _ => old_generation,
    };
    dingus.lookups.insert(path, (Instant::now() + LOOKUP_EXPIRE_TIME, new_attrs, new_generation));
    ino
}

impl RoraFile {
    /// Makes it so that `buffer_pos` is equal to the given offset or EOF.
    /// Returns true if you need to bump and relock.
    fn seek(&mut self, offset: u64) -> bool {
        assert!(offset <= self.file_size);
        if offset == self.file_size { return false }
        let buffer_start_pos = self.buffer_pos - self.buffered_behind as u64;
        let buffer_end_pos = buffer_start_pos + self.buffer.len() as u64;
        if offset > buffer_end_pos || offset < buffer_start_pos {
            // discard the whole buffer :(
            self.buffer_pos = offset;
            self.buffered_behind = 0;
            self.buffer.clear();
            true
        }
        else if offset == buffer_end_pos {
            // no need to discard anything! We just have to wait a bit.
            self.buffered_behind = self.buffer.len();
            self.buffer_pos = buffer_end_pos;
            true
        }
        else {
            assert!(offset >= buffer_start_pos);
            self.buffer_pos = offset;
            self.buffered_behind = (offset - buffer_start_pos) as usize;
            false
        }
    }
    fn needs_service(&self) -> usize {
        let amount_buffered_ahead = self.buffer.len() - self.buffered_behind;
        if amount_buffered_ahead < BUFFER_AHEAD {
            let buffer_start_pos = self.buffer_pos - self.buffered_behind as u64;
            let buffer_end_pos = buffer_start_pos + self.buffer.len() as u64;
            if buffer_end_pos < self.file_size {
                return BUFFER_AHEAD as usize - amount_buffered_ahead
            }
        }
        0
    }
    fn maybe_accept_moar(&mut self, offset: u64, moar: &[u8]) {
        let buffer_start_pos = self.buffer_pos - self.buffered_behind as u64;
        let buffer_end_pos = buffer_start_pos + self.buffer.len() as u64;
        if offset == buffer_end_pos {
            let new_size = self.buffer.len() + moar.len();
            if new_size > BUFFER_TOTAL_SIZE {
                let to_discard = new_size - BUFFER_TOTAL_SIZE;
                self.buffer.drain(..to_discard);
            }
            self.buffer.extend(moar);
        }
        // There are probably more circumstances where we could incorporate
        // some data, but... we should be extremely careful about that.
    }
}

impl Request {
    pub fn send_async(self, request_tx: &mut mpmc::Sender<(Request, Option<mpmc::Sender<Result<(), c_int>>>)>) {
        request_tx.send((self, None)).unwrap();
    }
    pub fn send_sync(self, request_tx: &mut mpmc::Sender<(Request, Option<mpmc::Sender<Result<(), c_int>>>)>) -> Result<(), c_int> {
        let (tx, rx) = mpmc::bounded(0);
        request_tx.send((self, Some(tx))).unwrap();
        rx.recv().expect("Request didn't get a response!")
    }
}

fn update_status(o: &mut liso::OutputOnly,
                 dingus: &Arc<RwLock<Dingus>>) {
    let lock = dingus.read();
    let mut line = liso::Line::new();
    let w = crossterm::terminal::size().unwrap().0 as usize;
    let count = lock.open_files.iter().fold(0, |a,x| if x.is_some() { a+1 } else { a });
    line.add_text(format!("{} file{} open", count, if count == 1 { "" } else { "s" }));
    for open_file in lock.open_files.iter().filter_map(Option::as_ref) {
        let name = open_file.path.file_name().unwrap().to_string_lossy();
        let buffer_amount = open_file.buffer.len() - open_file.buffered_behind;
        let target_size = open_file.file_size - open_file.buffer_pos + open_file.buffered_behind as u64;
        let target_size = target_size.min(BUFFER_AHEAD as u64) as usize;
        let frac = if target_size == 0 { w } else { buffer_amount * w as usize / target_size };
        let color = if buffer_amount >= target_size / 2 { liso::Color::Green }
        else if buffer_amount >= target_size / 4 { liso::Color::Yellow }
        else { liso::Color::Red };
        let mut cutoff = None;
        let mut x = 0;
        for (i, c) in name.char_indices() {
            if x == frac {
                cutoff = Some(i);
                break;
            }
            else {
                // note: liso will display control characters in two cells
                // ...unless it's ^J I guess >_>
                x += unicode_width::UnicodeWidthChar::width(c).unwrap_or(2);
            }
        }
        line.add_text("\n");
        line.set_fg_color(Some(color));
        line.set_style(liso::Style::INVERSE);
        if let Some(cutoff) = cutoff {
            line.add_text(&name[..cutoff]);
            line.set_fg_color(None);
            line.set_style(liso::Style::empty());
            line.add_text(&name[cutoff..]);
        }
        else {
            line.add_text(&name[..]);
            while x < frac {
                line.add_text(" ");
                x += 1;
            }
            line.set_fg_color(None);
            line.set_style(liso::Style::empty());
        }
    }
    o.status(Some(line));
}