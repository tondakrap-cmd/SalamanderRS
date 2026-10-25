//! SFTP version 3 client (draft-ietf-secsh-filexfer-02) over any byte stream, normally the
//! stdin/stdout of `ssh -s sftp`. Transfers pipeline many requests like OpenSSH's sftp.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_SETSTAT: u8 = 9;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_READLINK: u8 = 19;
const FXP_SYMLINK: u8 = 20;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;
const FXP_EXTENDED: u8 = 200;
const FXP_EXTENDED_REPLY: u8 = 201;

pub const OPEN_READ: u32 = 0x01;
pub const OPEN_WRITE: u32 = 0x02;
pub const OPEN_CREATE: u32 = 0x08;
pub const OPEN_TRUNCATE: u32 = 0x10;
pub const OPEN_EXCLUSIVE: u32 = 0x20;

const ATTR_SIZE: u32 = 0x1;
const ATTR_UIDGID: u32 = 0x2;
const ATTR_PERMISSIONS: u32 = 0x4;
const ATTR_ACMODTIME: u32 = 0x8;
const ATTR_EXTENDED: u32 = 0x8000_0000;

pub const STATUS_OK: u32 = 0;
pub const STATUS_EOF: u32 = 1;
pub const STATUS_NO_SUCH_FILE: u32 = 2;
pub const STATUS_PERMISSION_DENIED: u32 = 3;
pub const STATUS_FAILURE: u32 = 4;
const STATUS_NO_CONNECTION: u32 = 6;
const STATUS_CONNECTION_LOST: u32 = 7;

const CHUNK: u32 = 32 * 1024;
const MAX_OUTSTANDING: usize = 64;
const MAX_PACKET: usize = 1024 * 1024;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const S_IFREG: u32 = 0o100000;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub permissions: Option<u32>,
    pub atime: Option<u32>,
    pub mtime: Option<u32>,
}

impl Attrs {
    fn file_type(&self) -> u32 {
        self.permissions.unwrap_or(0) & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }

    pub fn is_file(&self) -> bool {
        self.file_type() == S_IFREG
    }
}

#[derive(Debug)]
pub enum SftpError {
    Status { code: u32, message: String },
    Io(String),
    Protocol(String),
    Cancelled,
}

impl SftpError {
    /// The session is unusable and must be re-established.
    pub fn is_connection(&self) -> bool {
        match self {
            SftpError::Io(_) | SftpError::Protocol(_) => true,
            SftpError::Status { code, .. } => *code == STATUS_NO_CONNECTION || *code == STATUS_CONNECTION_LOST,
            SftpError::Cancelled => false,
        }
    }

    pub fn code(&self) -> Option<u32> {
        match self {
            SftpError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl std::fmt::Display for SftpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SftpError::Status { code, message } => {
                let text = match *code {
                    STATUS_EOF => "konec souboru",
                    STATUS_NO_SUCH_FILE => "soubor neexistuje",
                    STATUS_PERMISSION_DENIED => "přístup odepřen",
                    STATUS_FAILURE => "operace selhala",
                    5 => "chybná zpráva",
                    STATUS_NO_CONNECTION => "bez spojení",
                    STATUS_CONNECTION_LOST => "spojení ztraceno",
                    8 => "operace není podporována",
                    _ => "chyba serveru",
                };
                if message.is_empty() || message.eq_ignore_ascii_case(text) {
                    write!(formatter, "{text}")
                } else {
                    write!(formatter, "{text} ({message})")
                }
            }
            SftpError::Io(message) => write!(formatter, "chyba spojení: {message}"),
            SftpError::Protocol(message) => write!(formatter, "chyba protokolu SFTP: {message}"),
            SftpError::Cancelled => write!(formatter, "zrušeno"),
        }
    }
}

impl From<io::Error> for SftpError {
    fn from(error: io::Error) -> Self {
        SftpError::Io(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SftpError>;

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: OsString,
    /// `ls -l` style line from the server, used for owner and group names.
    pub longname: String,
    pub attrs: Attrs,
}

impl DirEntry {
    /// A plain file name; names with `/`, `.` or `..` from a hostile server must never be
    /// joined to local paths.
    pub fn is_safe_name(&self) -> bool {
        let bytes = self.name.as_bytes();
        !bytes.is_empty() && bytes != b"." && bytes != b".." && !bytes.contains(&b'/') && !bytes.contains(&0)
    }

    /// Owner and group names parsed from the long name, if present.
    pub fn owner_group(&self) -> Option<(String, String)> {
        let mut fields = self.longname.split_whitespace();
        let _permissions = fields.next()?;
        let _links = fields.next()?;
        Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
    }
}

struct Packet(Vec<u8>);

impl Packet {
    fn new(kind: u8) -> Packet {
        Packet(vec![kind])
    }

    fn u32(mut self, value: u32) -> Packet {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Packet {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn bytes(mut self, value: &[u8]) -> Packet {
        self.0.extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.0.extend_from_slice(value);
        self
    }

    fn path(self, path: &Path) -> Packet {
        self.bytes(path.as_os_str().as_bytes())
    }

    fn attrs(mut self, attrs: &Attrs) -> Packet {
        let mut flags = 0;
        if attrs.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if attrs.uid.is_some() && attrs.gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if attrs.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if attrs.atime.is_some() && attrs.mtime.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        self = self.u32(flags);
        if let Some(size) = attrs.size {
            self = self.u64(size);
        }
        if flags & ATTR_UIDGID != 0 {
            self = self.u32(attrs.uid.unwrap()).u32(attrs.gid.unwrap());
        }
        if let Some(permissions) = attrs.permissions {
            self = self.u32(permissions);
        }
        if flags & ATTR_ACMODTIME != 0 {
            self = self.u32(attrs.atime.unwrap()).u32(attrs.mtime.unwrap());
        }
        self
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Cursor<'a> {
        Cursor { data, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| SftpError::Protocol("zkrácený paket".to_owned()))?;
        let slice = &self.data[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    fn attrs(&mut self) -> Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            attrs.uid = Some(self.u32()?);
            attrs.gid = Some(self.u32()?);
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.atime = Some(self.u32()?);
            attrs.mtime = Some(self.u32()?);
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }

    fn done(&self) -> bool {
        self.position >= self.data.len()
    }
}

fn status_of(body: &[u8]) -> Result<(u32, String)> {
    let mut cursor = Cursor::new(body);
    let code = cursor.u32()?;
    // Some old servers omit the message and language fields.
    let message = if cursor.done() { String::new() } else { cursor.string()? };
    Ok((code, message))
}

fn status_result(body: &[u8]) -> Result<()> {
    match status_of(body)? {
        (STATUS_OK, _) => Ok(()),
        (code, message) => Err(SftpError::Status { code, message }),
    }
}

fn unexpected(kind: u8) -> SftpError {
    SftpError::Protocol(format!("neočekávaná odpověď typu {kind}"))
}

/// Free and total bytes from `statvfs@openssh.com`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Space {
    pub available: u64,
    pub total: u64,
}

pub struct Sftp {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: BufWriter<Box<dyn Write + Send>>,
    next_id: u32,
    pub version: u32,
    extensions: HashMap<String, Vec<u8>>,
    child: Option<Child>,
    stderr: Option<Arc<Mutex<String>>>,
}

impl Sftp {
    /// Performs the INIT/VERSION handshake.
    pub fn new(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> Result<Sftp> {
        let mut sftp = Sftp {
            reader: BufReader::with_capacity(256 * 1024, reader),
            writer: BufWriter::with_capacity(256 * 1024, writer),
            next_id: 1,
            version: 0,
            extensions: HashMap::new(),
            child: None,
            stderr: None,
        };
        sftp.send(Packet::new(FXP_INIT).u32(3))?;
        let (kind, body) = sftp.receive()?;
        if kind != FXP_VERSION {
            return Err(unexpected(kind));
        }
        let mut cursor = Cursor::new(&body);
        sftp.version = cursor.u32()?;
        while !cursor.done() {
            let name = cursor.string()?;
            let data = cursor.bytes()?.to_vec();
            sftp.extensions.insert(name, data);
        }
        Ok(sftp)
    }

    /// Runs over a child process (ssh or sftp-server) whose stdin/stdout are piped.
    pub fn from_child(mut child: Child, stderr: Option<Arc<Mutex<String>>>) -> Result<Sftp> {
        let stdin = child.stdin.take().ok_or_else(|| SftpError::Io("chybí stdin".to_owned()))?;
        let stdout = child.stdout.take().ok_or_else(|| SftpError::Io("chybí stdout".to_owned()))?;
        match Sftp::new(Box::new(stdout), Box::new(stdin)) {
            Ok(mut sftp) => {
                sftp.child = Some(child);
                sftp.stderr = stderr;
                Ok(sftp)
            }
            Err(error) => {
                // The process usually exits on its own (e.g. authentication failed); reap it.
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.contains_key(name)
    }

    fn send(&mut self, packet: Packet) -> Result<()> {
        self.writer.write_all(&(packet.0.len() as u32).to_be_bytes())?;
        self.writer.write_all(&packet.0)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Queues a packet without flushing (pipelining).
    fn queue(&mut self, packet: Packet) -> Result<()> {
        self.writer.write_all(&(packet.0.len() as u32).to_be_bytes())?;
        self.writer.write_all(&packet.0)?;
        Ok(())
    }

    fn receive(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut length = [0u8; 4];
        self.reader.read_exact(&mut length).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                SftpError::Io("server ukončil spojení".to_owned())
            } else {
                error.into()
            }
        })?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_PACKET {
            return Err(SftpError::Protocol(format!("neplatná délka paketu {length}")));
        }
        let mut body = vec![0u8; length];
        self.reader.read_exact(&mut body)?;
        let kind = body.remove(0);
        Ok((kind, body))
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Receives a response and checks that it answers request `id`.
    fn response(&mut self, id: u32) -> Result<(u8, Vec<u8>)> {
        let (kind, mut body) = self.receive()?;
        if body.len() < 4 {
            return Err(SftpError::Protocol("odpověď bez identifikátoru".to_owned()));
        }
        let got = u32::from_be_bytes(body[..4].try_into().unwrap());
        if got != id {
            return Err(SftpError::Protocol(format!("odpověď na požadavek {got}, čekán {id}")));
        }
        body.drain(..4);
        Ok((kind, body))
    }

    fn request(&mut self, build: impl FnOnce(u32) -> Packet) -> Result<(u8, Vec<u8>)> {
        let id = self.id();
        self.send(build(id))?;
        self.response(id)
    }

    fn expect_status(&mut self, build: impl FnOnce(u32) -> Packet) -> Result<()> {
        match self.request(build)? {
            (FXP_STATUS, body) => status_result(&body),
            (kind, _) => Err(unexpected(kind)),
        }
    }

    fn expect_handle(&mut self, build: impl FnOnce(u32) -> Packet) -> Result<Vec<u8>> {
        match self.request(build)? {
            (FXP_HANDLE, body) => Ok(Cursor::new(&body).bytes()?.to_vec()),
            (FXP_STATUS, body) => Err(status_result(&body).err().unwrap_or_else(|| unexpected(FXP_STATUS))),
            (kind, _) => Err(unexpected(kind)),
        }
    }

    fn expect_attrs(&mut self, build: impl FnOnce(u32) -> Packet) -> Result<Attrs> {
        match self.request(build)? {
            (FXP_ATTRS, body) => Cursor::new(&body).attrs(),
            (FXP_STATUS, body) => Err(status_result(&body).err().unwrap_or_else(|| unexpected(FXP_STATUS))),
            (kind, _) => Err(unexpected(kind)),
        }
    }

    fn expect_single_name(&mut self, build: impl FnOnce(u32) -> Packet) -> Result<OsString> {
        match self.request(build)? {
            (FXP_NAME, body) => {
                let mut cursor = Cursor::new(&body);
                if cursor.u32()? < 1 {
                    return Err(SftpError::Protocol("prázdná odpověď NAME".to_owned()));
                }
                Ok(OsString::from_vec(cursor.bytes()?.to_vec()))
            }
            (FXP_STATUS, body) => Err(status_result(&body).err().unwrap_or_else(|| unexpected(FXP_STATUS))),
            (kind, _) => Err(unexpected(kind)),
        }
    }

    pub fn realpath(&mut self, path: &Path) -> Result<OsString> {
        self.expect_single_name(|id| Packet::new(FXP_REALPATH).u32(id).path(path))
    }

    pub fn stat(&mut self, path: &Path) -> Result<Attrs> {
        self.expect_attrs(|id| Packet::new(FXP_STAT).u32(id).path(path))
    }

    pub fn lstat(&mut self, path: &Path) -> Result<Attrs> {
        self.expect_attrs(|id| Packet::new(FXP_LSTAT).u32(id).path(path))
    }

    /// `Ok(None)` when the path does not exist.
    pub fn try_lstat(&mut self, path: &Path) -> Result<Option<Attrs>> {
        match self.lstat(path) {
            Ok(attrs) => Ok(Some(attrs)),
            Err(error) if error.code() == Some(STATUS_NO_SUCH_FILE) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn setstat(&mut self, path: &Path, attrs: &Attrs) -> Result<()> {
        self.expect_status(|id| Packet::new(FXP_SETSTAT).u32(id).path(path).attrs(attrs))
    }

    pub fn mkdir(&mut self, path: &Path, permissions: u32) -> Result<()> {
        let attrs = Attrs {
            permissions: Some(permissions & 0o7777),
            ..Attrs::default()
        };
        self.expect_status(|id| Packet::new(FXP_MKDIR).u32(id).path(path).attrs(&attrs))
    }

    pub fn rmdir(&mut self, path: &Path) -> Result<()> {
        self.expect_status(|id| Packet::new(FXP_RMDIR).u32(id).path(path))
    }

    pub fn remove(&mut self, path: &Path) -> Result<()> {
        self.expect_status(|id| Packet::new(FXP_REMOVE).u32(id).path(path))
    }

    /// Renames; with OpenSSH's posix-rename extension an existing target is replaced
    /// atomically, otherwise the server refuses to overwrite.
    pub fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        if self.has_extension("posix-rename@openssh.com") {
            self.expect_status(|id| {
                Packet::new(FXP_EXTENDED)
                    .u32(id)
                    .bytes(b"posix-rename@openssh.com")
                    .path(from)
                    .path(to)
            })
        } else {
            self.expect_status(|id| Packet::new(FXP_RENAME).u32(id).path(from).path(to))
        }
    }

    pub fn readlink(&mut self, path: &Path) -> Result<OsString> {
        self.expect_single_name(|id| Packet::new(FXP_READLINK).u32(id).path(path))
    }

    /// Creates `link` pointing to `target`. OpenSSH (and most servers) take the arguments in
    /// the reverse order of the draft, target first.
    pub fn symlink(&mut self, target: &Path, link: &Path) -> Result<()> {
        self.expect_status(|id| Packet::new(FXP_SYMLINK).u32(id).path(target).path(link))
    }

    pub fn statvfs(&mut self, path: &Path) -> Result<Option<Space>> {
        if !self.has_extension("statvfs@openssh.com") {
            return Ok(None);
        }
        match self.request(|id| {
            Packet::new(FXP_EXTENDED)
                .u32(id)
                .bytes(b"statvfs@openssh.com")
                .path(path)
        })? {
            (FXP_EXTENDED_REPLY, body) => {
                let mut cursor = Cursor::new(&body);
                let _bsize = cursor.u64()?;
                let frsize = cursor.u64()?;
                let blocks = cursor.u64()?;
                let _bfree = cursor.u64()?;
                let bavail = cursor.u64()?;
                Ok(Some(Space {
                    available: bavail.saturating_mul(frsize),
                    total: blocks.saturating_mul(frsize),
                }))
            }
            (FXP_STATUS, _) => Ok(None),
            (kind, _) => Err(unexpected(kind)),
        }
    }

    pub fn read_dir(&mut self, path: &Path) -> Result<Vec<DirEntry>> {
        let handle = self.expect_handle(|id| Packet::new(FXP_OPENDIR).u32(id).path(path))?;
        let mut entries = Vec::new();
        let result = loop {
            match self.request(|id| Packet::new(FXP_READDIR).u32(id).bytes(&handle)) {
                Ok((FXP_NAME, body)) => {
                    let mut cursor = Cursor::new(&body);
                    let parsed: Result<()> = (|| {
                        for _ in 0..cursor.u32()? {
                            let name = OsString::from_vec(cursor.bytes()?.to_vec());
                            let longname = cursor.string()?;
                            let attrs = cursor.attrs()?;
                            if name != "." && name != ".." {
                                entries.push(DirEntry { name, longname, attrs });
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = parsed {
                        break Err(error);
                    }
                }
                Ok((FXP_STATUS, body)) => match status_of(&body) {
                    Ok((STATUS_EOF, _)) => break Ok(()),
                    Ok((code, message)) => break Err(SftpError::Status { code, message }),
                    Err(error) => break Err(error),
                },
                Ok((kind, _)) => break Err(unexpected(kind)),
                Err(error) => break Err(error),
            }
        };
        let closed = self.close(&handle);
        result?;
        closed?;
        Ok(entries)
    }

    pub fn open(&mut self, path: &Path, flags: u32, attrs: &Attrs) -> Result<Vec<u8>> {
        self.expect_handle(|id| Packet::new(FXP_OPEN).u32(id).path(path).u32(flags).attrs(attrs))
    }

    pub fn close(&mut self, handle: &[u8]) -> Result<()> {
        self.expect_status(|id| Packet::new(FXP_CLOSE).u32(id).bytes(handle))
    }

    /// Downloads `path` into `file` with many outstanding READ requests; `progress` receives
    /// the number of bytes received in each step.
    pub fn download(
        &mut self,
        path: &Path,
        file: &File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let handle = self.open(path, OPEN_READ, &Attrs::default())?;
        let result = self.download_handle(&handle, file, cancel, progress);
        let closed = self.close(&handle);
        let total = result?;
        closed?;
        Ok(total)
    }

    fn download_handle(
        &mut self,
        handle: &[u8],
        file: &File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let mut outstanding: HashMap<u32, (u64, u32)> = HashMap::new();
        let mut next_offset = 0u64;
        let mut eof = false;
        let mut total = 0u64;
        let mut failure: Option<SftpError> = None;
        loop {
            let stop = eof || failure.is_some() || cancel.load(Ordering::Relaxed);
            if !stop && outstanding.len() < MAX_OUTSTANDING {
                while outstanding.len() < MAX_OUTSTANDING {
                    let id = self.id();
                    self.queue(Packet::new(FXP_READ).u32(id).bytes(handle).u64(next_offset).u32(CHUNK))?;
                    outstanding.insert(id, (next_offset, CHUNK));
                    next_offset += u64::from(CHUNK);
                }
                self.writer.flush()?;
            }
            if outstanding.is_empty() {
                break;
            }
            // All outstanding replies must be drained even after an error or cancel.
            let (kind, mut body) = self.receive()?;
            if body.len() < 4 {
                return Err(SftpError::Protocol("odpověď bez identifikátoru".to_owned()));
            }
            let id = u32::from_be_bytes(body[..4].try_into().unwrap());
            body.drain(..4);
            let Some((offset, length)) = outstanding.remove(&id) else {
                return Err(SftpError::Protocol(format!("neznámá odpověď {id}")));
            };
            match kind {
                FXP_DATA => {
                    let data = Cursor::new(&body).bytes()?;
                    if failure.is_none() && !cancel.load(Ordering::Relaxed) {
                        if let Err(error) = file.write_all_at(data, offset) {
                            failure = Some(error.into());
                            continue;
                        }
                        total += data.len() as u64;
                        progress(data.len() as u64);
                        // A short read in the middle: ask for the rest of that block.
                        if (data.len() as u32) < length && !data.is_empty() && !eof {
                            let rest_offset = offset + data.len() as u64;
                            let rest = length - data.len() as u32;
                            let id = self.id();
                            self.send(Packet::new(FXP_READ).u32(id).bytes(handle).u64(rest_offset).u32(rest))?;
                            outstanding.insert(id, (rest_offset, rest));
                        }
                    }
                }
                FXP_STATUS => match status_of(&body)? {
                    (STATUS_EOF, _) => eof = true,
                    (code, message) => {
                        failure.get_or_insert(SftpError::Status { code, message });
                    }
                },
                other => {
                    failure.get_or_insert(unexpected(other));
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(SftpError::Cancelled);
        }
        Ok(total)
    }

    /// Uploads `file` to `path`, pipelining WRITE requests.
    pub fn upload(
        &mut self,
        file: &mut File,
        path: &Path,
        flags: u32,
        attrs: &Attrs,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let handle = self.open(path, flags, attrs)?;
        let result = self.upload_handle(&handle, file, cancel, progress);
        let closed = self.close(&handle);
        let total = result?;
        closed?;
        Ok(total)
    }

    fn upload_handle(
        &mut self,
        handle: &[u8],
        file: &mut File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let mut outstanding: HashMap<u32, u64> = HashMap::new();
        let mut offset = 0u64;
        let mut finished_reading = false;
        let mut failure: Option<SftpError> = None;
        let mut buffer = vec![0u8; CHUNK as usize];
        loop {
            let stop = finished_reading || failure.is_some() || cancel.load(Ordering::Relaxed);
            if !stop {
                while outstanding.len() < MAX_OUTSTANDING {
                    let read = match file.read(&mut buffer) {
                        Ok(read) => read,
                        Err(error) => {
                            failure = Some(error.into());
                            break;
                        }
                    };
                    if read == 0 {
                        finished_reading = true;
                        break;
                    }
                    let id = self.id();
                    self.queue(
                        Packet::new(FXP_WRITE)
                            .u32(id)
                            .bytes(handle)
                            .u64(offset)
                            .bytes(&buffer[..read]),
                    )?;
                    outstanding.insert(id, read as u64);
                    offset += read as u64;
                }
                self.writer.flush()?;
            }
            if outstanding.is_empty() {
                break;
            }
            let (kind, mut body) = self.receive()?;
            if body.len() < 4 {
                return Err(SftpError::Protocol("odpověď bez identifikátoru".to_owned()));
            }
            let id = u32::from_be_bytes(body[..4].try_into().unwrap());
            body.drain(..4);
            let Some(length) = outstanding.remove(&id) else {
                return Err(SftpError::Protocol(format!("neznámá odpověď {id}")));
            };
            match kind {
                FXP_STATUS => match status_result(&body) {
                    Ok(()) => progress(length),
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                },
                other => {
                    failure.get_or_insert(unexpected(other));
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(SftpError::Cancelled);
        }
        Ok(offset)
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // stdin closes when the fields drop right after this; the transport then exits.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    /// Path of an OpenSSH `sftp-server` binary for tests, if available.
    pub fn sftp_server() -> Option<PathBuf> {
        let candidates = [
            std::env::var_os("SALAMANDER_TEST_SFTP_SERVER").map(PathBuf::from),
            Some(PathBuf::from("/usr/lib/openssh/sftp-server")),
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("srs-sshtest/root/usr/lib/openssh/sftp-server")),
        ];
        candidates.into_iter().flatten().find(|path| path.is_file())
    }

    pub fn local_session() -> Option<Sftp> {
        let Some(server) = sftp_server() else {
            eprintln!("sftp-server not found, skipping");
            return None;
        };
        let child = Command::new(server)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Some(Sftp::from_child(child, None).unwrap())
    }

    #[test]
    fn rejects_hostile_entry_names() {
        let entry = |name: &str| DirEntry { name: name.into(), longname: String::new(), attrs: Attrs::default() };
        assert!(entry("file.txt").is_safe_name());
        assert!(entry(".hidden").is_safe_name());
        for bad in ["", ".", "..", "../../.bashrc", "a/b", "/etc/passwd"] {
            assert!(!entry(bad).is_safe_name(), "{bad}");
        }
    }

    #[test]
    fn handshake_and_metadata_operations() {
        let Some(mut sftp) = local_session() else { return };
        assert_eq!(sftp.version, 3);
        assert!(sftp.has_extension("posix-rename@openssh.com"));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let real = sftp.realpath(root).unwrap();
        assert_eq!(Path::new(&real), fs::canonicalize(root).unwrap());

        sftp.mkdir(&root.join("dir"), 0o750).unwrap();
        assert!(sftp.stat(&root.join("dir")).unwrap().is_dir());
        assert_eq!(fs::metadata(root.join("dir")).unwrap().permissions().mode() & 0o777, 0o750);
        fs::write(root.join("dir/a.txt"), "hello").unwrap();
        sftp.symlink(Path::new("a.txt"), &root.join("dir/link")).unwrap();
        assert_eq!(fs::read_link(root.join("dir/link")).unwrap(), Path::new("a.txt"));
        assert_eq!(sftp.readlink(&root.join("dir/link")).unwrap(), "a.txt");
        assert!(sftp.lstat(&root.join("dir/link")).unwrap().is_symlink());

        let mut names: Vec<_> = sftp
            .read_dir(&root.join("dir"))
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name.to_string_lossy().into_owned(), entry.attrs.size, entry.owner_group().is_some()))
            .collect();
        names.sort();
        assert_eq!(names[0], ("a.txt".to_owned(), Some(5), true));
        assert_eq!(names[1].0, "link");

        sftp.setstat(&root.join("dir/a.txt"), &Attrs { permissions: Some(0o600), atime: Some(1_000_000_000), mtime: Some(1_000_000_000), ..Attrs::default() }).unwrap();
        let attrs = sftp.stat(&root.join("dir/a.txt")).unwrap();
        assert_eq!(attrs.permissions.unwrap() & 0o777, 0o600);
        assert_eq!(attrs.mtime, Some(1_000_000_000));

        fs::write(root.join("dir/b.txt"), "other").unwrap();
        sftp.rename(&root.join("dir/a.txt"), &root.join("dir/b.txt")).unwrap();
        assert_eq!(fs::read_to_string(root.join("dir/b.txt")).unwrap(), "hello");
        assert!(sftp.try_lstat(&root.join("dir/a.txt")).unwrap().is_none());

        let missing = sftp.stat(&root.join("missing")).unwrap_err();
        assert_eq!(missing.code(), Some(STATUS_NO_SUCH_FILE));
        assert!(sftp.rmdir(&root.join("dir")).is_err());
        sftp.remove(&root.join("dir/b.txt")).unwrap();
        sftp.remove(&root.join("dir/link")).unwrap();
        sftp.rmdir(&root.join("dir")).unwrap();
        assert!(!root.join("dir").exists());
        assert!(sftp.statvfs(root).unwrap().is_some_and(|space| space.total > 0));
    }

    #[test]
    fn pipelined_transfers_round_trip_large_files() {
        let Some(mut sftp) = local_session() else { return };
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.bin");
        // Not a multiple of the chunk size, and larger than the pipeline window.
        let data: Vec<u8> = (0..(5 * 1024 * 1024 + 12345)).map(|index| (index * 7 % 251) as u8).collect();
        fs::write(&source, &data).unwrap();

        let downloaded = temp.path().join("downloaded.bin");
        let file = File::create(&downloaded).unwrap();
        let mut seen = 0;
        let total = sftp
            .download(&source, &file, &AtomicBool::new(false), &mut |bytes| seen += bytes)
            .unwrap();
        assert_eq!(total, data.len() as u64);
        assert_eq!(seen, data.len() as u64);
        assert!(fs::read(&downloaded).unwrap() == data);

        let uploaded = temp.path().join("uploaded.bin");
        let mut input = File::open(&source).unwrap();
        let attrs = Attrs { permissions: Some(0o640), ..Attrs::default() };
        sftp.upload(&mut input, &uploaded, OPEN_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE, &attrs, &AtomicBool::new(false), &mut |_| {})
            .unwrap();
        assert!(fs::read(&uploaded).unwrap() == data);
        // Exclusive create refuses an existing file.
        let mut input = File::open(&source).unwrap();
        assert!(sftp
            .upload(&mut input, &uploaded, OPEN_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE, &attrs, &AtomicBool::new(false), &mut |_| {})
            .is_err());
        // The session stays usable after errors.
        assert!(sftp.stat(&uploaded).unwrap().is_file());

        let empty = temp.path().join("empty");
        fs::write(&empty, "").unwrap();
        let out = File::create(temp.path().join("empty.out")).unwrap();
        assert_eq!(sftp.download(&empty, &out, &AtomicBool::new(false), &mut |_| {}).unwrap(), 0);
    }

    #[test]
    fn cancelled_download_drains_and_keeps_session() {
        let Some(mut sftp) = local_session() else { return };
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("big");
        fs::write(&source, vec![1u8; 4 * 1024 * 1024]).unwrap();
        let cancel = AtomicBool::new(false);
        let file = File::create(temp.path().join("out")).unwrap();
        let result = sftp.download(&source, &file, &cancel, &mut |_| cancel.store(true, Ordering::Relaxed));
        assert!(matches!(result, Err(SftpError::Cancelled)));
        assert!(sftp.stat(&source).unwrap().is_file());
    }
}
