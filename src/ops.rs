use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::model::OpError;

const BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpKind {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Completed,
    Cancelled,
}

#[derive(Debug)]
pub struct Op {
    pub kind: OpKind,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
}

#[derive(Debug)]
pub struct Conflict {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictAction {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
    Abort,
}

#[derive(Debug)]
pub struct OpProgress {
    pub done_files: u64,
    pub done_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
    pub current: PathBuf,
}

#[derive(Debug)]
pub struct OpResult {
    pub outcome: Outcome,
    pub errors: Vec<OpError>,
}

#[derive(Debug)]
pub enum OpEvent {
    Preparing { current: PathBuf },
    Progress(OpProgress),
    Conflict(Conflict),
    Finished(OpResult),
}

pub fn start(
    op: Op,
    events: async_channel::Sender<OpEvent>,
    replies: mpsc::Receiver<ConflictAction>,
    cancel: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = execute(
            op,
            |event| events.send_blocking(event).is_ok(),
            |conflict| {
                if events.send_blocking(OpEvent::Conflict(conflict)).is_err() {
                    return ConflictAction::Abort;
                }
                loop {
                    if events.is_closed() || cancel.load(Ordering::Relaxed) {
                        return ConflictAction::Abort;
                    }
                    match replies.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(action) => return action,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return ConflictAction::Abort;
                        }
                    }
                }
            },
            &cancel,
        );
        let _ = events.send_blocking(OpEvent::Finished(result));
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct Scan {
    files: u64,
    bytes: u64,
}

struct Prepared {
    source: PathBuf,
    target: PathBuf,
    scan: Scan,
}

#[derive(Default)]
struct TopState {
    had_skip: bool,
    had_error: bool,
    cancelled: bool,
}

struct Engine<'a, E, R> {
    kind: OpKind,
    events: E,
    resolver: R,
    cancel: &'a AtomicBool,
    errors: Vec<OpError>,
    done_files: u64,
    done_bytes: u64,
    total_files: u64,
    total_bytes: u64,
    buffer: Vec<u8>,
    counter: u64,
    overwrite_all: bool,
    skip_all: bool,
    aborted: bool,
    disconnected: bool,
}

pub(crate) fn execute<E, R>(op: Op, events: E, resolver: R, cancel: &AtomicBool) -> OpResult
where
    E: FnMut(OpEvent) -> bool,
    R: FnMut(Conflict) -> ConflictAction,
{
    let mut engine = Engine {
        kind: op.kind,
        events,
        resolver,
        cancel,
        errors: Vec::new(),
        done_files: 0,
        done_bytes: 0,
        total_files: 0,
        total_bytes: 0,
        buffer: vec![0; BUFFER_SIZE],
        counter: 0,
        overwrite_all: false,
        skip_all: false,
        aborted: false,
        disconnected: false,
    };

    engine.run_inner(op.sources, op.dest_dir)
}

impl<E, R> Engine<'_, E, R>
where
    E: FnMut(OpEvent) -> bool,
    R: FnMut(Conflict) -> ConflictAction,
{
    fn run_inner(&mut self, sources: Vec<PathBuf>, dest_dir: PathBuf) -> OpResult {
        let dest_dir = match fs::canonicalize(&dest_dir) {
            Ok(path) if path.is_dir() => path,
            Ok(path) => {
                self.errors.push(OpError {
                    source: path,
                    target: None,
                    error: "Cíl není adresář".to_string(),
                });
                return self.result();
            }
            Err(error) => {
                self.errors.push(OpError {
                    source: dest_dir,
                    target: None,
                    error: error.to_string(),
                });
                return self.result();
            }
        };

        let mut prepared = Vec::new();
        for source in sources {
            if self.is_cancelled() {
                break;
            }
            let Some(name) = source.file_name() else {
                self.errors.push(OpError {
                    source,
                    target: None,
                    error: "Zdroj nemá název souboru".to_string(),
                });
                continue;
            };
            let target = dest_dir.join(name);
            let metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.errors.push(OpError {
                        source,
                        target: Some(target),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            if let Err(error) = validate_source(&source, &target, &dest_dir, &metadata) {
                self.errors.push(OpError {
                    source,
                    target: Some(target),
                    error,
                });
                continue;
            }
            match self.scan(&source) {
                Ok(scan) => prepared.push(Prepared {
                    source,
                    target,
                    scan,
                }),
                Err(error) => {
                    if self.is_cancelled() {
                        break;
                    }
                    self.errors.push(error);
                }
            }
        }

        if self.is_cancelled() {
            return self.result();
        }

        self.total_files = prepared.iter().map(|item| item.scan.files).sum();
        self.total_bytes = prepared.iter().map(|item| item.scan.bytes).sum();

        for item in prepared {
            if self.is_cancelled() {
                break;
            }
            let mut top = TopState::default();
            match self.kind {
                OpKind::Copy => {
                    self.copy_item(&item.source, &item.target, None, &mut top);
                }
                OpKind::Move => self.move_top(&item, &mut top),
            }
        }
        self.result()
    }

    fn result(&mut self) -> OpResult {
        OpResult {
            outcome: if self.aborted || self.disconnected || self.is_cancelled() {
                Outcome::Cancelled
            } else {
                Outcome::Completed
            },
            errors: std::mem::take(&mut self.errors),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.aborted || self.disconnected || self.cancel.load(Ordering::Relaxed)
    }

    fn emit(&mut self, event: OpEvent) -> bool {
        if self.disconnected {
            return false;
        }
        if !(self.events)(event) {
            self.disconnected = true;
            false
        } else {
            true
        }
    }

    fn preparing(&mut self, current: &Path) -> bool {
        !self.is_cancelled()
            && self.emit(OpEvent::Preparing {
                current: current.to_path_buf(),
            })
    }

    fn progress(&mut self, current: &Path) -> bool {
        self.emit(OpEvent::Progress(OpProgress {
            done_files: self.done_files,
            done_bytes: self.done_bytes,
            total_files: self.total_files,
            total_bytes: self.total_bytes,
            current: current.to_path_buf(),
        }))
    }

    fn scan(&mut self, source: &Path) -> Result<Scan, OpError> {
        if !self.preparing(source) {
            return Err(OpError {
                source: source.to_path_buf(),
                target: None,
                error: "Operace zrušena".to_string(),
            });
        }
        let metadata = fs::symlink_metadata(source).map_err(|error| OpError {
            source: source.to_path_buf(),
            target: None,
            error: error.to_string(),
        })?;
        if metadata.file_type().is_symlink() {
            return Ok(Scan { files: 1, bytes: 0 });
        }
        if metadata.is_file() {
            return Ok(Scan {
                files: 1,
                bytes: metadata.len(),
            });
        }
        if !metadata.is_dir() {
            return Ok(Scan::default());
        }

        let mut scan = Scan::default();
        let entries = fs::read_dir(source).map_err(|error| OpError {
            source: source.to_path_buf(),
            target: None,
            error: error.to_string(),
        })?;
        for entry in entries {
            if self.is_cancelled() {
                return Err(OpError {
                    source: source.to_path_buf(),
                    target: None,
                    error: "Operace zrušena".to_string(),
                });
            }
            let entry = entry.map_err(|error| OpError {
                source: source.to_path_buf(),
                target: None,
                error: error.to_string(),
            })?;
            let child = self.scan(&entry.path())?;
            scan.files = scan.files.saturating_add(child.files);
            scan.bytes = scan.bytes.saturating_add(child.bytes);
        }
        Ok(scan)
    }

    fn fail(&mut self, top: &mut TopState, source: &Path, target: Option<&Path>, error: String) {
        top.had_error = true;
        self.errors.push(OpError {
            source: source.to_path_buf(),
            target: target.map(Path::to_path_buf),
            error,
        });
    }

    fn resolve(&mut self, source: &Path, target: &Path, top: &mut TopState) -> ConflictAction {
        let action = if self.skip_all {
            ConflictAction::Skip
        } else if self.overwrite_all {
            ConflictAction::Overwrite
        } else {
            (self.resolver)(Conflict {
                source: source.to_path_buf(),
                target: target.to_path_buf(),
            })
        };
        match action {
            ConflictAction::OverwriteAll => {
                self.overwrite_all = true;
                self.skip_all = false;
                ConflictAction::Overwrite
            }
            ConflictAction::SkipAll => {
                self.skip_all = true;
                self.overwrite_all = false;
                top.had_skip = true;
                ConflictAction::Skip
            }
            ConflictAction::Skip => {
                top.had_skip = true;
                action
            }
            ConflictAction::Abort => {
                self.aborted = true;
                top.cancelled = true;
                action
            }
            ConflictAction::Overwrite => action,
        }
    }

    fn move_top(&mut self, item: &Prepared, top: &mut TopState) {
        let source = &item.source;
        let target = &item.target;
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return;
            }
        };
        if unsupported(&metadata) {
            self.fail(
                top,
                source,
                Some(target),
                "Nepodporovaný typ souboru".to_string(),
            );
            return;
        }

        let target_metadata = match target_metadata(target) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.fail(top, source, Some(target), error);
                return;
            }
        };
        let merge = metadata.is_dir()
            && target_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_dir());
        let mut overwrite = false;
        if target_metadata.is_some() {
            match self.resolve(source, target, top) {
                ConflictAction::Overwrite => overwrite = true,
                ConflictAction::Skip | ConflictAction::Abort => return,
                ConflictAction::OverwriteAll | ConflictAction::SkipAll => unreachable!(),
            }
        }

        let rename_overwrite = overwrite
            && !metadata.is_dir()
            && target_metadata
                .as_ref()
                .is_some_and(|metadata| !metadata.is_dir());
        if !merge && (target_metadata.is_none() || rename_overwrite) {
            if rename_overwrite
                && same_file(
                    &metadata,
                    target_metadata.as_ref().expect("overwrite target"),
                )
            {
                // POSIX rename is a successful no-op for two hard links to the
                // same inode. Removing only the source name completes the Move.
                if let Err(error) = fs::remove_file(source) {
                    self.fail(top, source, Some(target), error.to_string());
                    return;
                }
                self.done_files = self.done_files.saturating_add(item.scan.files);
                self.done_bytes = self.done_bytes.saturating_add(item.scan.bytes);
                self.progress(source);
                return;
            }
            match fs::rename(source, target) {
                Ok(()) => {
                    self.done_files = self.done_files.saturating_add(item.scan.files);
                    self.done_bytes = self.done_bytes.saturating_add(item.scan.bytes);
                    self.progress(source);
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                    // The source is intact; use the same safe copy path as other copies.
                }
                Err(error) => {
                    self.fail(top, source, Some(target), error.to_string());
                    return;
                }
            }
        }

        if merge {
            self.copy_item(source, target, Some(ConflictAction::Overwrite), top);
        } else {
            self.copy_item(
                source,
                target,
                Some(if overwrite {
                    ConflictAction::Overwrite
                } else {
                    // No conflict was present before the EXDEV rename attempt.
                    ConflictAction::Skip
                }),
                top,
            );
        }

        if !top.had_skip && !top.had_error && !top.cancelled && !self.is_cancelled() {
            if let Err(error) = remove_source(source) {
                self.fail(top, source, Some(target), error);
            }
        }
    }

    /// `resolved` is used by Move after it has already handled the top-level conflict.
    fn copy_item(
        &mut self,
        source: &Path,
        target: &Path,
        resolved: Option<ConflictAction>,
        top: &mut TopState,
    ) -> bool {
        if self.is_cancelled() {
            top.cancelled = true;
            return false;
        }
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        if unsupported(&metadata) {
            self.fail(
                top,
                source,
                Some(target),
                "Nepodporovaný typ souboru".to_string(),
            );
            return false;
        }

        let existing = match target_metadata(target) {
            Ok(existing) => existing,
            Err(error) => {
                self.fail(top, source, Some(target), error);
                return false;
            }
        };
        let merge = metadata.is_dir() && existing.as_ref().is_some_and(|item| item.is_dir());
        let overwrite = if existing.is_some() {
            match resolved {
                Some(ConflictAction::Overwrite) => true,
                // Move uses Skip as an internal marker for an originally absent target.
                // If a target appeared meanwhile, ask rather than silently replacing it.
                _ => match self.resolve(source, target, top) {
                    ConflictAction::Overwrite => true,
                    ConflictAction::Skip | ConflictAction::Abort => return false,
                    ConflictAction::OverwriteAll | ConflictAction::SkipAll => unreachable!(),
                },
            }
        } else {
            false
        };

        if merge && overwrite {
            self.merge_directory(source, target, top)
        } else if overwrite {
            self.copy_overwrite(source, target, &metadata, top)
        } else {
            self.copy_new(source, target, &metadata, top)
        }
    }

    fn copy_new(
        &mut self,
        source: &Path,
        target: &Path,
        metadata: &fs::Metadata,
        top: &mut TopState,
    ) -> bool {
        if metadata.is_file() {
            return self.copy_file_new(source, target, top);
        }
        if metadata.file_type().is_symlink() {
            return self.copy_symlink_new(source, target, top);
        }
        if !metadata.is_dir() {
            self.fail(
                top,
                source,
                Some(target),
                "Nepodporovaný typ souboru".to_string(),
            );
            return false;
        }

        if let Err(error) = fs::create_dir(target) {
            self.fail(top, source, Some(target), error.to_string());
            return false;
        }
        let mut ok = self.copy_directory_children(source, target, top);
        if !self.is_cancelled() {
            if let Err(error) = fs::set_permissions(target, metadata.permissions()) {
                self.fail(top, source, Some(target), error.to_string());
                ok = false;
            } else if let Err(error) = preserve_times(metadata, target) {
                self.fail(top, source, Some(target), error);
                ok = false;
            } else {
                let _ = crate::xattr::copy_all(source, target);
            }
        }
        ok
    }

    fn merge_directory(&mut self, source: &Path, target: &Path, top: &mut TopState) -> bool {
        self.copy_directory_children(source, target, top)
    }

    fn copy_directory_children(
        &mut self,
        source: &Path,
        target: &Path,
        top: &mut TopState,
    ) -> bool {
        let entries = match fs::read_dir(source) {
            Ok(entries) => entries,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        let mut ok = true;
        for entry in entries {
            if self.is_cancelled() {
                top.cancelled = true;
                return false;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.fail(top, source, Some(target), error.to_string());
                    ok = false;
                    continue;
                }
            };
            let child_source = entry.path();
            let child_target = target.join(entry.file_name());
            if !self.copy_item(&child_source, &child_target, None, top) {
                ok = false;
            }
        }
        ok
    }

    fn copy_file_new(&mut self, source: &Path, target: &Path, top: &mut TopState) -> bool {
        let mut input = match File::open(source) {
            Ok(file) => file,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        let output = match OpenOptions::new().write(true).create_new(true).open(target) {
            Ok(file) => file,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        if !self.copy_file_contents(source, target, &mut input, output, top) {
            // This path was created by us with create_new, so cleanup cannot remove
            // a pre-existing target.
            if let Err(error) = remove_if_exists(target) {
                self.fail(top, source, Some(target), error);
            }
            return false;
        }
        self.finish_file(source, target, top)
    }

    fn copy_symlink_new(&mut self, source: &Path, target: &Path, top: &mut TopState) -> bool {
        let link = match fs::read_link(source) {
            Ok(link) => link,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        if let Err(error) = std::os::unix::fs::symlink(link, target) {
            self.fail(top, source, Some(target), error.to_string());
            return false;
        }
        if let Ok(metadata) = fs::symlink_metadata(source) {
            let _ = preserve_times(&metadata, target);
        }
        self.done_files = self.done_files.saturating_add(1);
        self.progress(source);
        true
    }

    fn copy_overwrite(
        &mut self,
        source: &Path,
        target: &Path,
        metadata: &fs::Metadata,
        top: &mut TopState,
    ) -> bool {
        let staging = match self.create_staging(source, target, metadata, top) {
            Some(path) => path,
            None => return false,
        };
        let mut ok = true;
        if metadata.is_dir() {
            ok = self.copy_directory_children(source, &staging, top);
            if ok && !self.is_cancelled() {
                if let Err(error) = fs::set_permissions(&staging, metadata.permissions()) {
                    self.fail(top, source, Some(target), error.to_string());
                    ok = false;
                } else if let Err(error) = preserve_times(metadata, &staging) {
                    self.fail(top, source, Some(target), error);
                    ok = false;
                } else {
                    let _ = crate::xattr::copy_all(source, &staging);
                }
            }
        }

        if !ok || self.is_cancelled() {
            top.cancelled |= self.is_cancelled();
            if let Err(error) = remove_if_exists(&staging) {
                self.fail(top, source, Some(target), error);
            }
            return false;
        }

        let backup = match self.unused_backup(target) {
            Ok(path) => path,
            Err(error) => {
                self.fail(top, source, Some(target), error);
                if let Err(cleanup) = remove_if_exists(&staging) {
                    self.fail(top, source, Some(target), cleanup);
                }
                return false;
            }
        };
        if let Err(error) = commit_staging(&staging, target, &backup) {
            self.fail(top, source, Some(target), error);
            return false;
        }
        true
    }

    fn create_staging(
        &mut self,
        source: &Path,
        target: &Path,
        metadata: &fs::Metadata,
        top: &mut TopState,
    ) -> Option<PathBuf> {
        let parent = match target.parent() {
            Some(parent) => parent,
            None => {
                self.fail(
                    top,
                    source,
                    Some(target),
                    "Cíl nemá rodičovský adresář".to_string(),
                );
                return None;
            }
        };
        loop {
            if self.is_cancelled() {
                top.cancelled = true;
                return None;
            }
            self.counter = self.counter.wrapping_add(1);
            let staging = parent.join(format!(
                ".salamander-rs-{}-{}.tmp",
                std::process::id(),
                self.counter
            ));
            if metadata.is_file() {
                let mut input = match File::open(source) {
                    Ok(file) => file,
                    Err(error) => {
                        self.fail(top, source, Some(target), error.to_string());
                        return None;
                    }
                };
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staging)
                {
                    Ok(output) => {
                        if self.copy_file_contents(source, target, &mut input, output, top) {
                            if let Err(error) =
                                fs::set_permissions(&staging, metadata.permissions())
                                    .map_err(|error| error.to_string())
                                    .and_then(|()| preserve_times(metadata, &staging))
                                    .map(|()| {
                                        let _ = crate::xattr::copy_all(source, &staging);
                                    })
                            {
                                self.fail(top, source, Some(target), error);
                                if let Err(cleanup) = remove_if_exists(&staging) {
                                    self.fail(top, source, Some(target), cleanup);
                                }
                                return None;
                            }
                            self.done_files = self.done_files.saturating_add(1);
                            if !self.progress(source) {
                                top.cancelled = true;
                                if let Err(cleanup) = remove_if_exists(&staging) {
                                    self.fail(top, source, Some(target), cleanup);
                                }
                                return None;
                            }
                            return Some(staging);
                        }
                        if let Err(error) = remove_if_exists(&staging) {
                            self.fail(top, source, Some(target), error);
                        }
                        return None;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        self.fail(top, source, Some(target), error.to_string());
                        return None;
                    }
                }
            }
            if metadata.is_dir() {
                match fs::create_dir(&staging) {
                    Ok(()) => return Some(staging),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        self.fail(top, source, Some(target), error.to_string());
                        return None;
                    }
                }
            }
            if metadata.file_type().is_symlink() {
                let link = match fs::read_link(source) {
                    Ok(link) => link,
                    Err(error) => {
                        self.fail(top, source, Some(target), error.to_string());
                        return None;
                    }
                };
                match std::os::unix::fs::symlink(link, &staging) {
                    Ok(()) => {
                        self.done_files = self.done_files.saturating_add(1);
                        if !self.progress(source) {
                            top.cancelled = true;
                            if let Err(cleanup) = remove_if_exists(&staging) {
                                self.fail(top, source, Some(target), cleanup);
                            }
                            return None;
                        }
                        return Some(staging);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        self.fail(top, source, Some(target), error.to_string());
                        return None;
                    }
                }
            }
            self.fail(
                top,
                source,
                Some(target),
                "Nepodporovaný typ souboru".to_string(),
            );
            return None;
        }
    }

    fn copy_file_contents(
        &mut self,
        source: &Path,
        target: &Path,
        input: &mut File,
        mut output: File,
        top: &mut TopState,
    ) -> bool {
        loop {
            if self.is_cancelled() {
                top.cancelled = true;
                return false;
            }
            let read = match input.read(&mut self.buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    self.fail(top, source, Some(target), error.to_string());
                    return false;
                }
            };
            if let Err(error) = output.write_all(&self.buffer[..read]) {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
            self.done_bytes = self.done_bytes.saturating_add(read as u64);
            if !self.progress(source) || self.is_cancelled() {
                top.cancelled = true;
                return false;
            }
        }
        true
    }

    fn finish_file(&mut self, source: &Path, target: &Path, top: &mut TopState) -> bool {
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.fail(top, source, Some(target), error.to_string());
                return false;
            }
        };
        if let Err(error) = fs::set_permissions(target, metadata.permissions()) {
            self.fail(top, source, Some(target), error.to_string());
            return false;
        }
        if let Err(error) = preserve_times(&metadata, target) {
            self.fail(top, source, Some(target), error);
            return false;
        }
        // Extended attributes and ACLs are copied best effort, like `cp -a` on filesystems
        // that may not support them.
        let _ = crate::xattr::copy_all(source, target);
        self.done_files = self.done_files.saturating_add(1);
        self.progress(source)
    }

    fn unused_backup(&mut self, target: &Path) -> Result<PathBuf, String> {
        let parent = target
            .parent()
            .ok_or_else(|| "Cíl nemá rodičovský adresář".to_string())?;
        loop {
            self.counter = self.counter.wrapping_add(1);
            let backup = parent.join(format!(
                ".salamander-rs-{}-{}.bak",
                std::process::id(),
                self.counter
            ));
            match fs::symlink_metadata(&backup) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(backup),
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

fn validate_source(
    source: &Path,
    target: &Path,
    canonical_dest: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    let name = source
        .file_name()
        .ok_or_else(|| "Zdroj nemá název souboru".to_string())?;
    let parent = source
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let lexical_source = fs::canonicalize(parent)
        .map_err(|error| error.to_string())?
        .join(name);
    if lexical_source == target {
        return Err("Zdroj a cíl jsou stejná cesta".to_string());
    }
    let canonical_target = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    if !metadata.file_type().is_symlink() {
        let canonical_source = fs::canonicalize(source).map_err(|error| error.to_string())?;
        if canonical_source == canonical_target {
            return Err("Zdroj a cíl jsou stejná cesta".to_string());
        }
        if metadata.is_dir() && canonical_dest.starts_with(&canonical_source) {
            return Err(
                "Adresář nelze kopírovat ani přesouvat do sebe nebo vlastního podstromu"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn target_metadata(target: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(target) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn unsupported(metadata: &fs::Metadata) -> bool {
    !metadata.is_file() && !metadata.is_dir() && !metadata.file_type().is_symlink()
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn preserve_times(source: &fs::Metadata, target: &Path) -> Result<(), String> {
    crate::sys::set_times(target, source.accessed().ok(), source.modified().ok())
        .map_err(|error| format!("Nelze nastavit časy: {error}"))
}

/// Recursive size of a file or directory tree; symlinks are counted but not followed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Measure {
    pub files: u64,
    /// Directories below the measured roots (the roots themselves are not counted).
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
}

impl Measure {
    pub fn add(&mut self, other: Measure) {
        self.files += other.files;
        self.dirs += other.dirs;
        self.bytes += other.bytes;
        self.errors += other.errors;
    }
}

pub fn measure(path: &Path, cancel: &AtomicBool) -> Measure {
    let mut total = Measure::default();
    let mut stack = vec![(path.to_path_buf(), true)];
    while let Some((current, is_root)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(_) => {
                total.errors += 1;
                continue;
            }
        };
        if !metadata.is_dir() {
            total.files += 1;
            if metadata.is_file() {
                total.bytes += metadata.len();
            }
            continue;
        }
        if !is_root {
            total.dirs += 1;
        }
        match fs::read_dir(&current) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => stack.push((entry.path(), false)),
                        Err(_) => total.errors += 1,
                    }
                }
            }
            Err(_) => total.errors += 1,
        }
    }
    total
}

fn remove_source(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    } else {
        fs::remove_file(path).map_err(|error| error.to_string())
    }
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(path).map_err(|error| error.to_string())
            } else {
                fs::remove_file(path).map_err(|error| error.to_string())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn commit_staging(staging: &Path, target: &Path, backup: &Path) -> Result<(), String> {
    if let Err(error) = fs::rename(target, backup) {
        let mut message = format!("Nelze zálohovat cíl {}: {error}", target.to_string_lossy());
        if let Err(cleanup) = remove_if_exists(staging) {
            message.push_str(&format!(
                "; navíc nelze odstranit staging {}: {cleanup}",
                staging.to_string_lossy()
            ));
        }
        return Err(message);
    }

    if let Err(error) = fs::rename(staging, target) {
        let main = format!(
            "Nelze dokončit přepsání cíle {}: {error}",
            target.to_string_lossy()
        );
        if let Err(rollback) = fs::rename(backup, target) {
            return Err(format!(
                "{main}; rollback selhal: {rollback}; zachovaný backup {} a staging {}",
                backup.to_string_lossy(),
                staging.to_string_lossy()
            ));
        }
        let mut message = main;
        if let Err(cleanup) = remove_if_exists(staging) {
            message.push_str(&format!(
                "; navíc nelze odstranit staging {}: {cleanup}",
                staging.to_string_lossy()
            ));
        }
        return Err(message);
    }

    if let Err(error) = remove_if_exists(backup) {
        return Err(format!(
            "Cíl byl přepsán, ale backup {} nelze odstranit: {error}",
            backup.to_string_lossy()
        ));
    }
    Ok(())
}

pub fn trash_paths(paths: Vec<PathBuf>) -> Vec<OpError> {
    let mut errors = Vec::new();
    for path in paths {
        if let Err(error) = trash::delete(&path) {
            errors.push(OpError {
                source: path,
                target: None,
                error: error.to_string(),
            });
        }
    }
    errors
}

pub fn delete_permanently(paths: Vec<PathBuf>) -> Vec<OpError> {
    let mut errors = Vec::new();
    for path in paths {
        let result = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&path)
            }
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            errors.push(OpError {
                source: path,
                target: None,
                error: error.to_string(),
            });
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn no_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn run(op: Op, resolver: impl FnMut(Conflict) -> ConflictAction) -> OpResult {
        let cancel = no_cancel();
        execute(op, |_| true, resolver, &cancel)
    }

    fn copy(sources: Vec<PathBuf>, dest: &Path) -> Op {
        Op {
            kind: OpKind::Copy,
            sources,
            dest_dir: dest.to_path_buf(),
        }
    }

    #[test]
    fn recursive_copy_preserves_tree_and_relative_symlink() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("a");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("b")).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("x.txt"), "x").unwrap();
        fs::write(source.join("b/y.txt"), "y").unwrap();
        std::os::unix::fs::symlink("b", source.join("link")).unwrap();
        std::os::unix::fs::symlink("missing-target", source.join("broken")).unwrap();

        let result = run(copy(vec![source], &dest), |_| ConflictAction::Abort);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.outcome, Outcome::Completed);
        assert_eq!(fs::read(dest.join("a/x.txt")).unwrap(), b"x");
        assert_eq!(fs::read(dest.join("a/b/y.txt")).unwrap(), b"y");
        assert_eq!(fs::read_link(dest.join("a/link")).unwrap(), Path::new("b"));
        assert!(fs::symlink_metadata(dest.join("a/link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(dest.join("a/broken")).unwrap(),
            Path::new("missing-target")
        );
    }

    #[test]
    fn conflict_skip_preserves_both_files() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "new").unwrap();
        fs::write(dest.join("source.txt"), "old").unwrap();

        let result = run(copy(vec![source.clone()], &dest), |_| ConflictAction::Skip);
        assert!(result.errors.is_empty());
        assert_eq!(fs::read_to_string(source).unwrap(), "new");
        assert_eq!(fs::read_to_string(dest.join("source.txt")).unwrap(), "old");
    }

    #[test]
    fn conflict_overwrite_replaces_file() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "new").unwrap();
        fs::write(dest.join("source.txt"), "old").unwrap();

        let result = run(copy(vec![source], &dest), |_| ConflictAction::Overwrite);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(fs::read_to_string(dest.join("source.txt")).unwrap(), "new");
    }

    #[test]
    fn move_overwrite_between_hard_links_removes_source_name() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "contents").unwrap();
        fs::hard_link(&source, dest.join("source.txt")).unwrap();

        let result = run(
            Op {
                kind: OpKind::Move,
                sources: vec![source.clone()],
                dest_dir: dest.clone(),
            },
            |_| ConflictAction::Overwrite,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(dest.join("source.txt")).unwrap(),
            "contents"
        );
    }

    #[test]
    fn directory_merge_preserves_unrelated_target() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir_all(dest.join("tree")).unwrap();
        fs::write(source.join("sub/new.txt"), "new").unwrap();
        fs::write(dest.join("tree/unrelated.txt"), "keep").unwrap();

        let result = run(copy(vec![source], &dest), |_| ConflictAction::Overwrite);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            fs::read_to_string(dest.join("tree/sub/new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(dest.join("tree/unrelated.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn skip_all_does_not_call_resolver_again() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(dest.join("tree")).unwrap();
        for name in ["one", "two"] {
            fs::write(source.join(name), "new").unwrap();
            fs::write(dest.join("tree").join(name), "old").unwrap();
        }
        let mut calls = 0;
        let result = run(copy(vec![source], &dest), |_| {
            calls += 1;
            if calls == 1 {
                ConflictAction::Overwrite
            } else {
                ConflictAction::SkipAll
            }
        });
        assert!(result.errors.is_empty());
        assert_eq!(calls, 2);
        assert_eq!(fs::read_to_string(dest.join("tree/one")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("tree/two")).unwrap(), "old");
    }

    #[test]
    fn move_merge_nested_skip_preserves_entire_top_level_source() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir_all(dest.join("tree/sub")).unwrap();
        fs::write(source.join("sub/skip.txt"), "source skip").unwrap();
        fs::write(source.join("sub/copied.txt"), "copied").unwrap();
        fs::write(dest.join("tree/sub/skip.txt"), "target keep").unwrap();
        fs::write(dest.join("tree/unrelated.txt"), "unrelated").unwrap();

        let result = run(
            Op {
                kind: OpKind::Move,
                sources: vec![source.clone()],
                dest_dir: dest.clone(),
            },
            |conflict| {
                if fs::symlink_metadata(&conflict.source).unwrap().is_dir() {
                    ConflictAction::Overwrite
                } else {
                    ConflictAction::Skip
                }
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(source.join("sub/skip.txt").exists());
        assert!(source.join("sub/copied.txt").exists());
        assert_eq!(
            fs::read_to_string(dest.join("tree/sub/skip.txt")).unwrap(),
            "target keep"
        );
        assert_eq!(
            fs::read_to_string(dest.join("tree/sub/copied.txt")).unwrap(),
            "copied"
        );
        assert_eq!(
            fs::read_to_string(dest.join("tree/unrelated.txt")).unwrap(),
            "unrelated"
        );
    }

    #[test]
    fn copy_and_move_reject_self_and_directory_subtree() {
        for kind in [OpKind::Copy, OpKind::Move] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("tree");
            fs::create_dir_all(source.join("sub")).unwrap();
            fs::write(source.join("file.txt"), "keep").unwrap();

            let same = run(
                Op {
                    kind,
                    sources: vec![source.join("file.txt")],
                    dest_dir: source.clone(),
                },
                |_| ConflictAction::Overwrite,
            );
            assert!(!same.errors.is_empty());
            assert_eq!(fs::read_to_string(source.join("file.txt")).unwrap(), "keep");

            let nested = run(
                Op {
                    kind,
                    sources: vec![source.clone()],
                    dest_dir: source.join("sub"),
                },
                |_| ConflictAction::Overwrite,
            );
            assert!(!nested.errors.is_empty());
            assert!(source.join("file.txt").exists());
            assert!(!source.join("sub/tree").exists());
        }
    }

    #[test]
    fn fifo_is_rejected_without_opening_it() {
        let temp = TempDir::new().unwrap();
        let fifo = temp.path().join("fifo");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());

        let result = run(copy(vec![fifo.clone()], &dest), |_| ConflictAction::Abort);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].error, "Nepodporovaný typ souboru");
        assert!(fifo.exists());
        assert!(target_metadata(&dest.join("fifo")).unwrap().is_none());
    }

    #[test]
    fn failed_staging_preserves_move_source_and_original_target() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("good.txt"), "source").unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(source.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(dest.join("tree"), "original target").unwrap();

        let result = run(
            Op {
                kind: OpKind::Move,
                sources: vec![source.clone()],
                dest_dir: dest.clone(),
            },
            |_| ConflictAction::Overwrite,
        );
        assert!(result
            .errors
            .iter()
            .any(|error| error.error == "Nepodporovaný typ souboru"));
        assert_eq!(
            fs::read_to_string(dest.join("tree")).unwrap(),
            "original target"
        );
        assert_eq!(
            fs::read_to_string(source.join("good.txt")).unwrap(),
            "source"
        );
        assert!(source.join("fifo").exists());
    }

    #[test]
    fn preset_cancel_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "source").unwrap();
        fs::write(dest.join("unrelated"), "keep").unwrap();
        let cancel = AtomicBool::new(true);

        let result = execute(
            copy(vec![source.clone()], &dest),
            |_| true,
            |_| ConflictAction::Abort,
            &cancel,
        );
        assert_eq!(result.outcome, Outcome::Cancelled);
        assert_eq!(fs::read_to_string(source).unwrap(), "source");
        assert_eq!(fs::read_to_string(dest.join("unrelated")).unwrap(), "keep");
        assert!(target_metadata(&dest.join("source.txt")).unwrap().is_none());
    }

    #[test]
    fn start_delivers_exactly_one_finished_to_live_receiver() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "contents").unwrap();
        let (events_tx, events_rx) = async_channel::bounded(16);
        let (_replies_tx, replies_rx) = mpsc::channel();

        let handle = start(
            copy(vec![source], &dest),
            events_tx,
            replies_rx,
            no_cancel(),
        );
        let mut finished = 0;
        while let Ok(event) = events_rx.recv_blocking() {
            if matches!(event, OpEvent::Finished(_)) {
                finished += 1;
            }
        }
        handle.join().unwrap();
        assert_eq!(finished, 1);
        assert_eq!(
            fs::read_to_string(dest.join("source.txt")).unwrap(),
            "contents"
        );
    }

    #[test]
    fn disconnected_event_receiver_exits_without_waiting_for_reply() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "new").unwrap();
        fs::write(dest.join("source.txt"), "old").unwrap();
        let (events_tx, events_rx) = async_channel::bounded(1);
        let (_replies_tx, replies_rx) = mpsc::channel();
        drop(events_rx);

        let handle = start(
            copy(vec![source], &dest),
            events_tx,
            replies_rx,
            no_cancel(),
        );
        handle.join().unwrap();
        assert_eq!(fs::read_to_string(dest.join("source.txt")).unwrap(), "old");
    }

    #[test]
    fn disconnect_after_conflict_event_interrupts_reply_wait() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, "new").unwrap();
        fs::write(dest.join("source.txt"), "old").unwrap();
        let (events_tx, events_rx) = async_channel::bounded(8);
        let (_replies_tx, replies_rx) = mpsc::channel();

        let handle = start(
            copy(vec![source], &dest),
            events_tx,
            replies_rx,
            no_cancel(),
        );
        loop {
            match events_rx.recv_blocking() {
                Ok(OpEvent::Conflict(_)) => break,
                Ok(_) => {}
                Err(error) => panic!("operation ended before conflict: {error}"),
            }
        }
        drop(events_rx);
        handle.join().unwrap();
        assert_eq!(fs::read_to_string(dest.join("source.txt")).unwrap(), "old");
    }

    #[test]
    fn copy_preserves_modification_time() {
        use std::os::unix::fs::MetadataExt;
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("tree");
        let dest = temp.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("file"), "x").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567_890);
        crate::sys::set_times(&source.join("file"), None, Some(old)).unwrap();
        crate::sys::set_times(&source, None, Some(old)).unwrap();

        let result = run(copy(vec![source], &dest), |_| ConflictAction::Abort);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(fs::metadata(dest.join("tree/file")).unwrap().mtime(), 1_234_567_890);
        assert_eq!(fs::metadata(dest.join("tree")).unwrap().mtime(), 1_234_567_890);
    }

    #[test]
    fn measure_counts_tree_without_following_symlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("one"), "12345").unwrap();
        fs::write(root.join("a/b/two"), "123").unwrap();
        std::os::unix::fs::symlink("/", root.join("a/root-link")).unwrap();

        let measured = measure(&root, &AtomicBool::new(false));
        assert_eq!(
            measured,
            Measure {
                files: 3,
                dirs: 2,
                bytes: 8,
                errors: 0
            }
        );
    }

    #[test]
    fn commit_missing_staging_rolls_back_original_target() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let staging = temp.path().join("missing-staging");
        let backup = temp.path().join("backup");
        fs::write(&target, "original").unwrap();

        let error = commit_staging(&staging, &target, &backup).unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert!(!backup.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn successful_commit_replaces_target_and_removes_backup() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        let staging = temp.path().join("staging");
        let backup = temp.path().join("backup");
        fs::write(&target, "old").unwrap();
        fs::write(&staging, "new").unwrap();

        commit_staging(&staging, &target, &backup).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!staging.exists());
        assert!(!backup.exists());
    }
}
