use ::base64::prelude::BASE64_STANDARD as base64;
use ::base64::Engine as _;
use cached::{Cached, TimedCache};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request as FuserRequest,
};
use fuser::{MountOption, ReplyEmpty, ReplyOpen};
use indexmap::IndexMap;
use libc::{EBADFD, EIO, ENOENT};
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::UNIX_EPOCH;
use ureq::{Agent, MiddlewareNext, Request, Response};
use url::Url;

fn agent(auth: Option<String>) -> Agent {
    ureq::builder()
        .no_delay(false)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(15))
        .try_proxy_from_env(true)
        .middleware(
            move |req: Request, next: MiddlewareNext| -> Result<Response, ureq::Error> {
                next.handle(match &auth {
                    Some(auth) => req.set("Authorization", &auth),
                    None => req,
                })
            },
        )
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
}

#[derive(Serialize)]
struct AuthReq<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct AuthResp {
    username: String,
    token: String,
    #[allow(unused)]
    is_admin: bool,
}

#[derive(Deserialize, Debug)]
#[allow(unused)]
struct EntryRespCommon {
    path: Utf8PathBuf,
    artist: Option<String>,
    album: Option<String>,
    artwork: Option<String>,
    year: Option<i32>,
}
#[derive(Deserialize, Debug)]
#[allow(unused)]
enum EntryResp {
    Directory {
        #[serde(flatten)]
        common: EntryRespCommon,
        date_added: i64,
    },
    Song {
        #[serde(flatten)]
        common: EntryRespCommon,
        track_number: Option<u32>,
        disc_number: Option<u32>,
        title: Option<String>,
        album_artist: Option<String>,
        duration: Option<u64>,
        lyricist: Option<String>,
        composer: Option<String>,
        genre: Option<String>,
        label: Option<String>,
    },
}

impl EntryResp {
    fn common(&self) -> &EntryRespCommon {
        match self {
            EntryResp::Directory { common, .. } => common,
            EntryResp::Song { common, .. } => common,
        }
    }
}

type ListResp = Arc<IndexMap<String, EntryResp>>;

type Inode = u64;
type FileHandle = u64;

#[derive(Debug)]
struct Node {
    id: Inode,
    path: Arc<Utf8PathBuf>,
    kind: FileType,
    lookups: u64,
    size: u64,
}

struct OpenFile {
    reader: Box<dyn Read + Send + Sync + 'static>,
    buffer: Vec<u8>, // TODO: VecDeque, discards, seeks
    size: u64,
}

impl Node {
    fn path_stem(&self) -> &str {
        self.path
            .components()
            .next()
            .map(|c| c.as_str())
            .unwrap_or("")
    }
}

struct Client {
    agent: Agent,
    base: Url,
    list_results: TimedCache<Utf8PathBuf, ListResp>,
    open_results: TimedCache<String, Arc<Mutex<OpenFile>>>,
}
struct Tables {
    inodes: BTreeMap<Inode, Node>,
    path2inode: BTreeMap<Arc<Utf8PathBuf>, Inode>,
    open_dirs: BTreeMap<FileHandle, ListResp>,
    open_files: BTreeMap<FileHandle, Arc<Mutex<OpenFile>>>,
}

struct Fs {
    client: Client,
    tables: Tables,
}

impl Fs {
    fn new(base: &str, username: &str, password: &str) -> Self {
        let base = Url::parse(base)
            .expect("Base url is invalid")
            .join("/api/")
            .expect("Base url is not a base");
        debug!("base: {base}, username: {username}");
        let authurl = base.join("./auth").unwrap();
        trace!("{authurl}");
        let auth = agent(None)
            .request_url("POST", &authurl)
            .send_json(AuthReq { username, password })
            .expect("Failed to connect to authenticate")
            .into_json::<AuthResp>()
            .expect("Failed to authenticate");
        info!("Authenticated as {}", auth.username);
        let node = |id, path: &str, size| {
            (
                id,
                Node {
                    id,
                    path: Arc::new(path.into()),
                    lookups: 1, /* shouldn't be forgotten */
                    kind: FileType::Directory,
                    size,
                },
            )
        };
        let inodes = BTreeMap::from([node(1, "", 2)]);
        let path2inode = inodes
            .iter()
            .map(|(a, b)| (b.path.clone(), a.clone()))
            .collect();
        Self {
            client: Client {
                agent: agent(Some(format!("Bearer {}", auth.token))),
                base,
                list_results: TimedCache::with_lifespan_and_refresh(10, false),
                open_results: TimedCache::with_lifespan_and_refresh(20, false),
            },
            tables: Tables {
                inodes,
                path2inode,
                open_dirs: Default::default(),
                open_files: Default::default(),
            },
        }
    }
}
impl Client {
    fn list(&mut self, path: &Utf8Path) -> Option<ListResp> {
        match self.list_results.cache_get(path) {
            Some(x) => return Some(x.clone()),
            None => (),
        };
        let url = match self.base.join(path.as_str()) {
            Ok(x) => x,
            Err(e) => {
                error!("path {path} is cannot be used: {e:?}");
                return None;
            }
        };
        let req = match self.agent.request_url("GET", &url).call() {
            Ok(x) => x,
            Err(e) => {
                // TODO: retry
                error!("querying {path} failed: {e:?}");
                return None;
            }
        };
        let res = match req.into_json::<Vec<EntryResp>>() {
            Ok(x) => x,
            Err(e) => {
                error!("querying {path} returned invalid result: {e:?}");
                return None;
            }
        };
        let mut store = IndexMap::with_capacity(res.len());
        for e in res {
            let path = &e
                .common()
                .path
                .file_name()
                .expect("All list entries have names");
            for suff in 1.. {
                let name = match suff {
                    1 => path.to_string(),
                    n => format!("{path} (#{n})"),
                };
                if let indexmap::map::Entry::Vacant(vac) = store.entry(name) {
                    vac.insert(e);
                    break;
                }
            }
        }
        let store: ListResp = Arc::new(store);
        self.list_results.cache_set(path.into(), store.clone());
        Some(store)
    }
    fn open_file(&mut self, name: &str) -> Option<Arc<Mutex<OpenFile>>> {
        match self.open_results.cache_get(name) {
            Some(x) => return Some(x.clone()),
            None => (),
        };
        let mut url = self.base.clone();
        let mut segments = url.path_segments_mut().expect("Checked that it's a base");
        segments.push("audio");
        segments.push(name);
        std::mem::drop(segments);
        let req = match self.agent.request_url("GET", &url).call() {
            Ok(x) => x,
            Err(e) => {
                // TODO: retry
                error!("getting audio {url} failed: {e:?}");
                return None;
            }
        };
        let Some(size) = req.header("content-length").and_then(|l| l.parse().ok()) else {
            error!("no length received for {url}, can't use");
            return None;
        };
        let ret = Arc::new(Mutex::new(OpenFile {
            size,
            reader: req.into_reader(),
            buffer: Vec::new(),
        }));
        self.open_results.cache_set(name.into(), ret.clone());
        Some(ret)
    }
}
impl Tables {
    fn unused_id(&self) -> u64 {
        fn max_id<V>(m: &BTreeMap<u64, V>) -> u64 {
            m.last_key_value().map(|(&k, _)| k).unwrap_or(0)
        }
        max(max_id(&self.inodes), max_id(&self.open_dirs)) + 1
    }
}

const TTL: Duration = Duration::from_secs(60); // 1 second

const FILE_ATTR: FileAttr = FileAttr {
    ino: 0,
    size: 4096,
    blocks: 0,
    atime: UNIX_EPOCH, // 1970-01-01 00:00:00
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::RegularFile,
    perm: 0o755,
    nlink: 2,
    uid: 501,
    gid: 20,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

const PATH_ATTR: FileAttr = FileAttr {
    kind: FileType::Directory,
    perm: 0o755,
    ..FILE_ATTR
};

const SYM_ATTR: FileAttr = FileAttr {
    kind: FileType::Symlink,
    perm: 0o777,
    ..FILE_ATTR
};

impl Filesystem for Fs {
    fn lookup(&mut self, _req: &FuserRequest, parent: Inode, name: &OsStr, reply: ReplyEntry) {
        let Some(parent) = self.tables.inodes.get(&parent) else {
            warn!("Inode 0x{parent:x} not found, cannot lookup {name:?} (internal error?)");
            return reply.error(ENOENT);
        };
        let Some(name) = name.to_str() else {
            debug!("Rejecting non-utf8 {name:?} in {}", parent.path);
            return reply.error(ENOENT);
        };
        let path = parent.path.join(name);
        let parent_path = parent.path.clone();
        let Fs {
            client,
            tables: inodes,
        } = self;
        let mut newnode = |path: Utf8PathBuf, kind, size| {
            let ino = match inodes.path2inode.get(&path) {
                Some(&ino) => {
                    /*TODO increase lookup count*/
                    ino
                }
                None => {
                    let path = Arc::new(path.clone());
                    let id = inodes.unused_id();
                    inodes.path2inode.insert(path.clone(), id);
                    inodes.inodes.insert(
                        id,
                        Node {
                            id,
                            path,
                            kind,
                            lookups: 1,
                            size,
                        },
                    );
                    id
                }
            };
            debug!("LOOKUP {path} -> {ino}, {kind:?}, {size} B");
            attr_for(ino, kind, size)
        };

        if matches!(path.as_str(), "audio" | "browse" | "search") {
            return reply.entry(&TTL, &newnode(path, FileType::Directory, 1), 0);
        }

        let stem = path
            .components()
            .next()
            .expect("Path can't be empty after join");
        if stem == Utf8Component::Normal("browse") {
            match client.list(&parent_path) {
                Some(list) => match list.get(name) {
                    Some(e) => reply.entry(
                        &TTL,
                        &newnode(
                            path.into(),
                            match e {
                                EntryResp::Directory { .. } => FileType::Directory,
                                EntryResp::Song { .. } => FileType::Symlink,
                            },
                            1,
                        ),
                        0,
                    ),
                    None => {
                        debug!("{path} not found in {}", parent_path);
                        reply.error(ENOENT)
                    }
                },
                None => reply.error(EIO),
            }
        } else if stem == Utf8Component::Normal("audio".as_ref()) {
            match path.components().count() {
                1 => unreachable!(),
                2 => {
                    let Some(name) = decode_audio_name(&path) else {
                        return reply.error(ENOENT);
                    };
                    let Some(file) = client.open_file(&name) else {
                        return reply.error(ENOENT);
                    };
                    let size = file.lock().unwrap().size;
                    reply.entry(&TTL, &newnode(path, FileType::RegularFile, size), 0)
                }
                _ => reply.error(ENOENT),
            }
        } else if stem == Utf8Component::Normal("search".as_ref()) {
            match path.components().count() {
                1 => unreachable!(),
                2 => reply.entry(&TTL, &newnode(path, FileType::Directory, 1), 0),
                3 => reply.entry(&TTL, &newnode(path, FileType::Symlink, 1), 0),
                _ => reply.error(ENOENT),
            }
        } else {
            reply.error(ENOENT)
        }
    }
    fn forget(&mut self, _req: &FuserRequest<'_>, ino: u64, _nlookup: u64) {
        /* TODO */
    }

    fn getattr(&mut self, _req: &FuserRequest, ino: u64, reply: ReplyAttr) {
        let Some(node) = self.tables.inodes.get(&ino) else {
            warn!("Inode 0x{ino:x} not found, cannot get attribues (internal error?)");
            return reply.error(ENOENT);
        };
        reply.attr(&TTL, &attr_for(node.id, node.kind, node.size));
    }

    fn open(&mut self, _req: &FuserRequest<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let Some(node) = self.tables.inodes.get(&ino) else {
            warn!("Inode 0x{ino:x} not found, cannot open directory (internal error?)");
            return reply.error(ENOENT);
        };
        if !matches!(node.path_stem(), "audio") || node.path.components().count() != 2 {
            return reply.error(ENOENT);
        }
        let Some(name) = decode_audio_name(&node.path) else {
            warn!("Weird audio path access {}", node.path);
            return reply.error(ENOENT);
        };
        let fh = self.tables.unused_id();

        let Some(file) = self.client.open_file(&name) else {
            return reply.error(ENOENT);
        };
        self.tables.open_files.insert(fh, file);
        reply.opened(fh, 0);
        debug!("OPEN {} -> {fh}", node.path);
    }
    fn read(
        &mut self,
        _req: &FuserRequest,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let path = self.tables.inodes.get(&ino).map(|node| node.path.clone());
        let Some(open) = self.tables.open_files.get_mut(&fh) else {
            warn!("Trying to read unkown file node {ino} fh {fh}");
            return reply.error(ENOENT);
        };
        let open = open.clone();
        std::thread::spawn(move || {
            let Ok(offset) = usize::try_from(offset) else {
                return reply.error(EIO);
            };
            let size = usize::try_from(size).unwrap();
            let mut open = open.lock().unwrap();
            let OpenFile { reader, buffer, .. } = &mut *open;
            if let Some(missing) = (offset + size).checked_sub(buffer.len()) {
                let res = reader.take(missing as u64).read_to_end(buffer);
                match res {
                    Ok(res) => trace!(
                        "Fh {fh} requested {missing} read {res} now {}",
                        buffer.len()
                    ),
                    Err(e) => {
                        let unk;
                        let name = match &path {
                            Some(path) => path.as_str(),
                            None => {
                                unk = format!("[Unknown node, handle {fh}]");
                                unk.as_str()
                            }
                        };
                        error!("Error reading {name}: {e:?}");
                        return reply.error(EIO);
                    }
                }
            }
            let end = min(offset + size, buffer.len());
            return reply.data(&buffer[offset..end]);
        });
    }

    fn readlink(&mut self, _req: &FuserRequest<'_>, ino: u64, reply: ReplyData) {
        let Some(node) = self.tables.inodes.get(&ino) else {
            warn!("Inode 0x{ino:x} not found, cannot read link (internal error?)");
            return reply.error(ENOENT);
        };
        if node.path.components().count() < 2 {
            warn!("no symlinks in root dir");
            return reply.error(ENOENT);
        }
        let target = match node.path_stem() {
            "browse" => {
                let mut target = Utf8PathBuf::new();
                let media_path = node.path.strip_prefix("browse").unwrap();
                for _ in 1..node.path.components().count() {
                    target.push("..");
                }
                target.push("audio");
                target.push(base64.encode(media_path.as_str().as_bytes()));
                target
            }
            "search" => {
                let search = node
                    .path
                    .parent()
                    .expect("In subpath of serach/, should have a parent");
                let entry = node
                    .path
                    .file_name()
                    .expect("Readlink paths must have names");
                let Some(list) = self.client.list(search) else {
                    return reply.error(ENOENT);
                };
                let Some(entry) = list.get(entry) else {
                    return reply.error(ENOENT);
                };
                let unsearch = Utf8Path::new("../..");
                match entry {
                    EntryResp::Song { .. } => unsearch
                        .join("audio")
                        .join(base64.encode(entry.common().path.as_str().as_bytes())),
                    EntryResp::Directory { .. } => {
                        unsearch.join("browse").join(&entry.common().path)
                    }
                }
            }
            _ => {
                return reply.error(ENOENT);
            }
        };
        reply.data(target.as_str().as_bytes());
        debug!("READLINK {} -> {target}", node.path);
    }

    fn opendir(&mut self, _req: &FuserRequest<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let Some(node) = self.tables.inodes.get(&ino) else {
            warn!("Inode 0x{ino:x} not found, cannot open directory (internal error?)");
            return reply.error(ENOENT);
        };
        if node.path.as_ref() == "" {
            return reply.opened(0, 0);
        }
        let stem = node.path_stem();
        if !matches!(stem, "browse" | "search") {
            return reply.error(ENOENT);
        }
        let Some(list) = self.client.list(node.path.as_ref()) else {
            return reply.error(EIO);
        };
        let fh = self.tables.unused_id();
        self.tables.open_dirs.insert(fh, list);
        reply.opened(fh, 0);
        debug!("OPEN DIR {} -> {fh}", node.path);
    }
    fn releasedir(
        &mut self,
        _req: &FuserRequest<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        if fh == 0 {
            return reply.ok();
        }
        match self.tables.open_dirs.remove(&fh) {
            Some(_) => reply.ok(),
            None => {
                warn!("Unknown file handle {fh} used in directory close");
                reply.error(EBADFD);
            }
        }
    }
    fn release(
        &mut self,
        _req: &FuserRequest<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.tables.open_files.remove(&fh) {
            Some(_) => reply.ok(),
            None => {
                warn!("Unknown file handle {fh} used in file close");
                reply.error(EBADFD);
            }
        }
    }
    fn flush(
        &mut self,
        _req: &FuserRequest<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.tables.open_files.contains_key(&fh) {
            true => reply.ok(),
            false => {
                warn!("Unknown file handle {fh} used in file flush");
                reply.error(EBADFD);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &FuserRequest,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.tables.inodes.get(&ino) else {
            warn!("Inode 0x{ino:x} not found, cannot list directory (internal error?)");
            return reply.error(ENOENT);
        };
        let mut entries: Vec<(_, _, &str)> = vec![
            (1, FileType::Directory, "."),
            (0, FileType::Directory, ".."),
        ];
        if ino == 1 {
            entries.push((2, FileType::Directory, "browse"));
            entries.push((2, FileType::Directory, "search"));
            // Guess I'll keep "audio" hidden
        } else if let Some(open_dir) = self.tables.open_dirs.get(&fh) {
            let stem = node.path_stem();
            for (name, x) in open_dir.iter() {
                let typ = match stem {
                    "browse" => match x {
                        EntryResp::Directory { .. } => FileType::Directory,
                        EntryResp::Song { .. } => FileType::Symlink,
                    },
                    "search" => FileType::Symlink,
                    _ => unreachable!("Somehow got an open dir other than browse/search: {stem}"),
                };
                entries.push((0, typ, name.as_str()));
            }
        } else {
            warn!("Trying to read unknown dir node {ino} handle {fh}");
            return reply.error(ENOENT);
        };

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }
}

fn decode_audio_name(path: &Utf8Path) -> Option<String> {
    let name = path
        .strip_prefix("audio")
        .expect("Only used on audio opaths");
    let name = base64
        .decode(name.file_name().expect("Two components"))
        .ok();
    let name = name.and_then(|name| String::from_utf8(name).ok());
    name
}

fn attr_for(ino: u64, kind: FileType, size: u64) -> FileAttr {
    let res = FileAttr {
        ino,
        size,
        ..match kind {
            FileType::Directory => PATH_ATTR,
            FileType::RegularFile => FILE_ATTR,
            FileType::Symlink => SYM_ATTR,
            _ => unreachable!("I have no dealings with this man"),
        }
    };
    assert_eq!(res.kind, kind);
    res
}

fn main() {
    femtofemme::init(env!("CARGO_CRATE_NAME"));
    let args = std::env::args().collect::<Vec<_>>();
    let [_, mount, base, user, secretfile] = &args[..] else {
        let basename = args.get(0).map(|b| b.as_str());
        let basename = basename.unwrap_or(env!("CARGO_CRATE_NAME"));
        eprintln!("Usage: {basename} <mountpoint> <polaris-url> <username> <password-file>");
        std::process::exit(1);
    };
    let pw = String::from_utf8(std::fs::read(secretfile).expect("Could not read password file"))
        .expect("Non-utf8 password");
    let client = Fs::new(base, user, &pw.strip_suffix("\n").unwrap_or(&pw));
    std::mem::drop(pw);

    let opts = &[
        MountOption::RO,
        MountOption::DefaultPermissions,
        MountOption::FSName(concat!("fsname=", env!("CARGO_CRATE_NAME")).into()),
    ];
    fuser::mount2(client, mount, opts).unwrap();
}
