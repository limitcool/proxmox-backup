//! Code for extraction of pxar contents onto the file system.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, format_err, Error};
use nix::dir::Dir;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use pathpatterns::{MatchEntry, MatchList, MatchType};
use pxar::accessor::aio::{Accessor, FileContents, FileEntry};
use pxar::decoder::{aio::Decoder, Contents};
use pxar::format::Device;
use pxar::{Entry, EntryKind, Metadata};

use proxmox_io::{sparse_copy, sparse_copy_async};
use proxmox_sys::c_result;
use proxmox_sys::fs::{create_path, CreateOptions};

use proxmox_compression::zip::{ZipEncoder, ZipEntry};

use crate::pxar::dir_stack::PxarDirStack;
use crate::pxar::metadata;
use crate::pxar::Flags;

pub struct PxarExtractOptions<'a> {
    pub match_list: &'a [MatchEntry],
    pub extract_match_default: bool,
    pub allow_existing_dirs: bool,
    pub overwrite: bool,
    pub on_error: Option<ErrorHandler>,
}

pub type ErrorHandler = Box<dyn FnMut(Error) -> Result<(), Error> + Send>;

pub fn extract_archive<T, F>(
    mut decoder: pxar::decoder::Decoder<T>,
    destination: &Path,
    feature_flags: Flags,
    mut callback: F,
    options: PxarExtractOptions,
) -> Result<(), Error>
where
    T: pxar::decoder::SeqRead,
    F: FnMut(&Path),
{
    // we use this to keep track of our directory-traversal
    decoder.enable_goodbye_entries(true);

    let root = decoder
        .next()
        .ok_or_else(|| format_err!("found empty pxar archive"))?
        .map_err(|err| format_err!("error reading pxar archive: {}", err))?;

    if !root.is_dir() {
        bail!("pxar archive does not start with a directory entry!");
    }

    create_path(
        destination,
        None,
        Some(CreateOptions::new().perm(Mode::from_bits_truncate(0o700))),
    )
    .map_err(|err| format_err!("error creating directory {:?}: {}", destination, err))?;

    let dir = Dir::open(
        destination,
        OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|err| format_err!("unable to open target directory {:?}: {}", destination, err,))?;

    let mut extractor = Extractor::new(
        dir,
        root.metadata().clone(),
        options.allow_existing_dirs,
        options.overwrite,
        feature_flags,
    );

    if let Some(on_error) = options.on_error {
        extractor.on_error(on_error);
    }

    let mut match_stack = Vec::new();
    let mut err_path_stack = vec![OsString::from("/")];
    let mut current_match = options.extract_match_default;
    while let Some(entry) = decoder.next() {
        let entry = entry.map_err(|err| format_err!("error reading pxar archive: {}", err))?;

        let file_name_os = entry.file_name();

        // safety check: a file entry in an archive must never contain slashes:
        if file_name_os.as_bytes().contains(&b'/') {
            bail!("archive file entry contains slashes, which is invalid and a security concern");
        }

        let file_name = CString::new(file_name_os.as_bytes())
            .map_err(|_| format_err!("encountered file name with null-bytes"))?;

        let metadata = entry.metadata();

        extractor.set_path(entry.path().as_os_str().to_owned());

        let match_result = options.match_list.matches(
            entry.path().as_os_str().as_bytes(),
            Some(metadata.file_type() as u32),
        );

        let did_match = match match_result {
            Some(MatchType::Include) => true,
            Some(MatchType::Exclude) => false,
            None => current_match,
        };
        match (did_match, entry.kind()) {
            (_, EntryKind::Directory) => {
                callback(entry.path());

                let create = current_match && match_result != Some(MatchType::Exclude);
                extractor
                    .enter_directory(file_name_os.to_owned(), metadata.clone(), create)
                    .map_err(|err| format_err!("error at entry {:?}: {}", file_name_os, err))?;

                // We're starting a new directory, push our old matching state and replace it with
                // our new one:
                match_stack.push(current_match);
                current_match = did_match;

                // When we hit the goodbye table we'll try to apply metadata to the directory, but
                // the Goodbye entry will not contain the path, so push it to our path stack for
                // error messages:
                err_path_stack.push(extractor.clone_path());

                Ok(())
            }
            (_, EntryKind::GoodbyeTable) => {
                // go up a directory

                extractor.set_path(err_path_stack.pop().ok_or_else(|| {
                    format_err!(
                        "error at entry {:?}: unexpected end of directory",
                        file_name_os
                    )
                })?);

                extractor
                    .leave_directory()
                    .map_err(|err| format_err!("error at entry {:?}: {}", file_name_os, err))?;

                // We left a directory, also get back our previous matching state. This is in sync
                // with `dir_stack` so this should never be empty except for the final goodbye
                // table, in which case we get back to the default of `true`.
                current_match = match_stack.pop().unwrap_or(true);

                Ok(())
            }
            (true, EntryKind::Symlink(link)) => {
                callback(entry.path());
                extractor.extract_symlink(&file_name, metadata, link.as_ref())
            }
            (true, EntryKind::Hardlink(link)) => {
                callback(entry.path());
                extractor.extract_hardlink(&file_name, link.as_os_str())
            }
            (true, EntryKind::Device(dev)) => {
                if extractor.contains_flags(Flags::WITH_DEVICE_NODES) {
                    callback(entry.path());
                    extractor.extract_device(&file_name, metadata, dev)
                } else {
                    Ok(())
                }
            }
            (true, EntryKind::Fifo) => {
                if extractor.contains_flags(Flags::WITH_FIFOS) {
                    callback(entry.path());
                    extractor.extract_special(&file_name, metadata, 0)
                } else {
                    Ok(())
                }
            }
            (true, EntryKind::Socket) => {
                if extractor.contains_flags(Flags::WITH_SOCKETS) {
                    callback(entry.path());
                    extractor.extract_special(&file_name, metadata, 0)
                } else {
                    Ok(())
                }
            }
            (true, EntryKind::File { size, .. }) => extractor.extract_file(
                &file_name,
                metadata,
                *size,
                &mut decoder.contents().ok_or_else(|| {
                    format_err!("found regular file entry without contents in archive")
                })?,
                extractor.overwrite,
            ),
            (false, _) => Ok(()), // skip this
        }
        .map_err(|err| format_err!("error at entry {:?}: {}", file_name_os, err))?;
    }

    if !extractor.dir_stack.is_empty() {
        bail!("unexpected eof while decoding pxar archive");
    }

    Ok(())
}

/// Common state for file extraction.
pub struct Extractor {
    feature_flags: Flags,
    allow_existing_dirs: bool,
    overwrite: bool,
    dir_stack: PxarDirStack,

    /// For better error output we need to track the current path in the Extractor state.
    current_path: Arc<Mutex<OsString>>,

    /// Error callback. Includes `current_path` in the reformatted error, should return `Ok` to
    /// continue extracting or the passed error as `Err` to bail out.
    on_error: ErrorHandler,
}

impl Extractor {
    /// Create a new extractor state for a target directory.
    pub fn new(
        root_dir: Dir,
        metadata: Metadata,
        allow_existing_dirs: bool,
        overwrite: bool,
        feature_flags: Flags,
    ) -> Self {
        Self {
            dir_stack: PxarDirStack::new(root_dir, metadata),
            allow_existing_dirs,
            overwrite,
            feature_flags,
            current_path: Arc::new(Mutex::new(OsString::new())),
            on_error: Box::new(Err),
        }
    }

    /// We call this on errors. The error will be reformatted to include `current_path`. The
    /// callback should decide whether this error was fatal (simply return it) to bail out early,
    /// or log/remember/accumulate errors somewhere and return `Ok(())` in its place to continue
    /// extracting.
    pub fn on_error(&mut self, mut on_error: Box<dyn FnMut(Error) -> Result<(), Error> + Send>) {
        let path = Arc::clone(&self.current_path);
        self.on_error = Box::new(move |err: Error| -> Result<(), Error> {
            on_error(format_err!("error at {:?}: {}", path.lock().unwrap(), err))
        });
    }

    pub fn set_path(&mut self, path: OsString) {
        *self.current_path.lock().unwrap() = path;
    }

    pub fn clone_path(&self) -> OsString {
        self.current_path.lock().unwrap().clone()
    }

    /// When encountering a directory during extraction, this is used to keep track of it. If
    /// `create` is true it is immediately created and its metadata will be updated once we leave
    /// it. If `create` is false it will only be created if it is going to have any actual content.
    pub fn enter_directory(
        &mut self,
        file_name: OsString,
        metadata: Metadata,
        create: bool,
    ) -> Result<(), Error> {
        self.dir_stack.push(file_name, metadata)?;

        if create {
            self.dir_stack.create_last_dir(self.allow_existing_dirs)?;
        }

        Ok(())
    }

    /// When done with a directory we can apply its metadata if it has been created.
    pub fn leave_directory(&mut self) -> Result<(), Error> {
        let path_info = self.dir_stack.path().to_owned();

        let dir = self
            .dir_stack
            .pop()
            .map_err(|err| format_err!("unexpected end of directory entry: {}", err))?
            .ok_or_else(|| format_err!("broken pxar archive (directory stack underrun)"))?;

        if let Some(fd) = dir.try_as_borrowed_fd() {
            metadata::apply(
                self.feature_flags,
                dir.metadata(),
                fd.as_raw_fd(),
                &path_info,
                &mut self.on_error,
            )
            .map_err(|err| format_err!("failed to apply directory metadata: {}", err))?;
        }

        Ok(())
    }

    fn contains_flags(&self, flag: Flags) -> bool {
        self.feature_flags.contains(flag)
    }

    fn parent_fd(&mut self) -> Result<RawFd, Error> {
        self.dir_stack
            .last_dir_fd(self.allow_existing_dirs)
            .map(|d| d.as_raw_fd())
            .map_err(|err| format_err!("failed to get parent directory file descriptor: {}", err))
    }

    pub fn extract_symlink(
        &mut self,
        file_name: &CStr,
        metadata: &Metadata,
        link: &OsStr,
    ) -> Result<(), Error> {
        let parent = self.parent_fd()?;
        nix::unistd::symlinkat(link, Some(parent), file_name)?;
        metadata::apply_at(
            self.feature_flags,
            metadata,
            parent,
            file_name,
            self.dir_stack.path(),
            &mut self.on_error,
        )
    }

    pub fn extract_hardlink(&mut self, file_name: &CStr, link: &OsStr) -> Result<(), Error> {
        crate::pxar::tools::assert_relative_path(link)?;

        let parent = self.parent_fd()?;
        let root = self.dir_stack.root_dir_fd()?;
        let target = CString::new(link.as_bytes())?;
        nix::unistd::linkat(
            Some(root.as_raw_fd()),
            target.as_c_str(),
            Some(parent),
            file_name,
            nix::unistd::LinkatFlags::NoSymlinkFollow,
        )?;

        Ok(())
    }

    pub fn extract_device(
        &mut self,
        file_name: &CStr,
        metadata: &Metadata,
        device: &Device,
    ) -> Result<(), Error> {
        self.extract_special(file_name, metadata, device.to_dev_t())
    }

    pub fn extract_special(
        &mut self,
        file_name: &CStr,
        metadata: &Metadata,
        device: libc::dev_t,
    ) -> Result<(), Error> {
        let mode = metadata.stat.mode;
        let mode = u32::try_from(mode).map_err(|_| {
            format_err!(
                "device node's mode contains illegal bits: 0x{:x} (0o{:o})",
                mode,
                mode,
            )
        })?;
        let parent = self.parent_fd()?;
        unsafe { c_result!(libc::mknodat(parent, file_name.as_ptr(), mode, device)) }
            .map_err(|err| format_err!("failed to create device node: {}", err))?;

        metadata::apply_at(
            self.feature_flags,
            metadata,
            parent,
            file_name,
            self.dir_stack.path(),
            &mut self.on_error,
        )
    }

    pub fn extract_file(
        &mut self,
        file_name: &CStr,
        metadata: &Metadata,
        size: u64,
        contents: &mut dyn io::Read,
        overwrite: bool,
    ) -> Result<(), Error> {
        let parent = self.parent_fd()?;
        let mut oflags = OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_CLOEXEC;
        if overwrite {
            oflags |= OFlag::O_TRUNC;
        } else {
            oflags |= OFlag::O_EXCL;
        }
        let mut file = unsafe {
            std::fs::File::from_raw_fd(
                nix::fcntl::openat(parent, file_name, oflags, Mode::from_bits(0o600).unwrap())
                    .map_err(|err| format_err!("failed to create file {:?}: {}", file_name, err))?,
            )
        };

        metadata::apply_initial_flags(
            self.feature_flags,
            metadata,
            file.as_raw_fd(),
            &mut self.on_error,
        )
        .map_err(|err| format_err!("failed to apply initial flags: {}", err))?;

        let result = sparse_copy(&mut *contents, &mut file)
            .map_err(|err| format_err!("failed to copy file contents: {}", err))?;

        if size != result.written {
            bail!(
                "extracted {} bytes of a file of {} bytes",
                result.written,
                size
            );
        }

        if result.seeked_last {
            while match nix::unistd::ftruncate(file.as_raw_fd(), size as i64) {
                Ok(_) => false,
                Err(errno) if errno == nix::errno::Errno::EINTR => true,
                Err(err) => bail!("error setting file size: {}", err),
            } {}
        }

        metadata::apply(
            self.feature_flags,
            metadata,
            file.as_raw_fd(),
            self.dir_stack.path(),
            &mut self.on_error,
        )
    }

    pub async fn async_extract_file<T: tokio::io::AsyncRead + Unpin>(
        &mut self,
        file_name: &CStr,
        metadata: &Metadata,
        size: u64,
        contents: &mut T,
        overwrite: bool,
    ) -> Result<(), Error> {
        let parent = self.parent_fd()?;
        let mut oflags = OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_CLOEXEC;
        if overwrite {
            oflags |= OFlag::O_TRUNC;
        } else {
            oflags |= OFlag::O_EXCL;
        }
        let mut file = tokio::fs::File::from_std(unsafe {
            std::fs::File::from_raw_fd(
                nix::fcntl::openat(parent, file_name, oflags, Mode::from_bits(0o600).unwrap())
                    .map_err(|err| format_err!("failed to create file {:?}: {}", file_name, err))?,
            )
        });

        metadata::apply_initial_flags(
            self.feature_flags,
            metadata,
            file.as_raw_fd(),
            &mut self.on_error,
        )
        .map_err(|err| format_err!("failed to apply initial flags: {}", err))?;

        let result = sparse_copy_async(&mut *contents, &mut file)
            .await
            .map_err(|err| format_err!("failed to copy file contents: {}", err))?;

        if size != result.written {
            bail!(
                "extracted {} bytes of a file of {} bytes",
                result.written,
                size
            );
        }

        if result.seeked_last {
            while match nix::unistd::ftruncate(file.as_raw_fd(), size as i64) {
                Ok(_) => false,
                Err(errno) if errno == nix::errno::Errno::EINTR => true,
                Err(err) => bail!("error setting file size: {}", err),
            } {}
        }

        metadata::apply(
            self.feature_flags,
            metadata,
            file.as_raw_fd(),
            self.dir_stack.path(),
            &mut self.on_error,
        )
    }
}

fn add_metadata_to_header(header: &mut tar::Header, metadata: &Metadata) {
    header.set_mode(metadata.stat.mode as u32);
    header.set_mtime(metadata.stat.mtime.secs as u64);
    header.set_uid(metadata.stat.uid as u64);
    header.set_gid(metadata.stat.gid as u64);
}

async fn tar_add_file<'a, W, T>(
    tar: &mut proxmox_compression::tar::Builder<W>,
    contents: Option<Contents<'a, T>>,
    size: u64,
    metadata: &Metadata,
    path: &Path,
) -> Result<(), Error>
where
    T: pxar::decoder::SeqRead + Unpin + Send + Sync + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    add_metadata_to_header(&mut header, metadata);
    header.set_cksum();
    match contents {
        Some(content) => tar.add_entry(&mut header, path, content).await,
        None => tar.add_entry(&mut header, path, tokio::io::empty()).await,
    }
    .map_err(|err| format_err!("could not send file entry: {}", err))?;
    Ok(())
}

/// Creates a tar file from `path` and writes it into `output`
pub async fn create_tar<T, W, P>(output: W, accessor: Accessor<T>, path: P) -> Result<(), Error>
where
    T: Clone + pxar::accessor::ReadAt + Unpin + Send + Sync + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    P: AsRef<Path>,
{
    let root = accessor.open_root().await?;
    let file = root
        .lookup(&path)
        .await?
        .ok_or_else(|| format_err!("error opening '{:?}'", path.as_ref()))?;

    let mut components = file.entry().path().components();
    components.next_back(); // discard last
    let prefix = components.as_path();

    let mut tarencoder = proxmox_compression::tar::Builder::new(output);
    let mut hardlinks: HashMap<PathBuf, PathBuf> = HashMap::new();

    if let Ok(dir) = file.enter_directory().await {
        let entry = dir.lookup_self().await?;
        let path = entry.path().strip_prefix(prefix)?;

        if path != Path::new("/") {
            let metadata = entry.metadata();
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            add_metadata_to_header(&mut header, metadata);
            header.set_size(0);
            header.set_cksum();
            tarencoder
                .add_entry(&mut header, path, tokio::io::empty())
                .await
                .map_err(|err| format_err!("could not send dir entry: {}", err))?;
        }

        let mut decoder = dir.decode_full().await?;
        decoder.enable_goodbye_entries(false);
        while let Some(entry) = decoder.next().await {
            let entry = entry.map_err(|err| format_err!("cannot decode entry: {}", err))?;

            let metadata = entry.metadata();
            let path = entry.path().strip_prefix(prefix)?;

            match entry.kind() {
                EntryKind::File { .. } => {
                    let size = decoder.content_size().unwrap_or(0);
                    tar_add_file(&mut tarencoder, decoder.contents(), size, metadata, path).await?
                }
                EntryKind::Hardlink(link) => {
                    if !link.data.is_empty() {
                        let entry = root
                            .lookup(&path)
                            .await?
                            .ok_or_else(|| format_err!("error looking up '{:?}'", path))?;
                        let realfile = accessor.follow_hardlink(&entry).await?;
                        let metadata = realfile.entry().metadata();
                        let realpath = Path::new(link);

                        log::debug!("adding '{}' to tar", path.display());

                        let stripped_path = match realpath.strip_prefix(prefix) {
                            Ok(path) => path,
                            Err(_) => {
                                // outside of our tar archive, add the first occurrence to the tar
                                if let Some(path) = hardlinks.get(realpath) {
                                    path
                                } else {
                                    let size = decoder.content_size().unwrap_or(0);
                                    tar_add_file(
                                        &mut tarencoder,
                                        decoder.contents(),
                                        size,
                                        metadata,
                                        path,
                                    )
                                    .await?;
                                    hardlinks.insert(realpath.to_owned(), path.to_owned());
                                    continue;
                                }
                            }
                        };
                        let mut header = tar::Header::new_gnu();
                        header.set_entry_type(tar::EntryType::Link);
                        add_metadata_to_header(&mut header, metadata);
                        header.set_size(0);
                        tarencoder
                            .add_link(&mut header, path, stripped_path)
                            .await
                            .map_err(|err| format_err!("could not send hardlink entry: {}", err))?;
                    }
                }
                EntryKind::Symlink(link) if !link.data.is_empty() => {
                    log::debug!("adding '{}' to tar", path.display());
                    let realpath = Path::new(link);
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    add_metadata_to_header(&mut header, metadata);
                    header.set_size(0);
                    tarencoder
                        .add_link(&mut header, path, realpath)
                        .await
                        .map_err(|err| format_err!("could not send symlink entry: {}", err))?;
                }
                EntryKind::Fifo => {
                    log::debug!("adding '{}' to tar", path.display());
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Fifo);
                    add_metadata_to_header(&mut header, metadata);
                    header.set_size(0);
                    header.set_device_major(0)?;
                    header.set_device_minor(0)?;
                    header.set_cksum();
                    tarencoder
                        .add_entry(&mut header, path, tokio::io::empty())
                        .await
                        .map_err(|err| format_err!("could not send fifo entry: {}", err))?;
                }
                EntryKind::Directory => {
                    log::debug!("adding '{}' to tar", path.display());
                    // we cannot add the root path itself
                    if path != Path::new("/") {
                        let mut header = tar::Header::new_gnu();
                        header.set_entry_type(tar::EntryType::Directory);
                        add_metadata_to_header(&mut header, metadata);
                        header.set_size(0);
                        header.set_cksum();
                        tarencoder
                            .add_entry(&mut header, path, tokio::io::empty())
                            .await
                            .map_err(|err| format_err!("could not send dir entry: {}", err))?;
                    }
                }
                EntryKind::Device(device) => {
                    log::debug!("adding '{}' to tar", path.display());
                    let entry_type = if metadata.stat.is_chardev() {
                        tar::EntryType::Char
                    } else {
                        tar::EntryType::Block
                    };
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(entry_type);
                    header.set_device_major(device.major as u32)?;
                    header.set_device_minor(device.minor as u32)?;
                    add_metadata_to_header(&mut header, metadata);
                    header.set_size(0);
                    tarencoder
                        .add_entry(&mut header, path, tokio::io::empty())
                        .await
                        .map_err(|err| format_err!("could not send device entry: {}", err))?;
                }
                _ => {} // ignore all else
            }
        }
    }

    tarencoder.finish().await.map_err(|err| {
        log::error!("error during finishing of zip: {}", err);
        err
    })?;
    Ok(())
}

pub async fn create_zip<T, W, P>(output: W, accessor: Accessor<T>, path: P) -> Result<(), Error>
where
    T: Clone + pxar::accessor::ReadAt + Unpin + Send + Sync + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    P: AsRef<Path>,
{
    let root = accessor.open_root().await?;
    let file = root
        .lookup(&path)
        .await?
        .ok_or_else(|| format_err!("error opening '{:?}'", path.as_ref()))?;

    let prefix = {
        let mut components = file.entry().path().components();
        components.next_back(); // discar last
        components.as_path().to_owned()
    };

    let mut zip = ZipEncoder::new(output);

    if let Ok(dir) = file.enter_directory().await {
        let entry = dir.lookup_self().await?;
        let path = entry.path().strip_prefix(&prefix)?;
        if path != Path::new("/") {
            let metadata = entry.metadata();
            let entry = ZipEntry::new(
                path,
                metadata.stat.mtime.secs,
                metadata.stat.mode as u16,
                false,
            );
            zip.add_entry::<FileContents<T>>(entry, None).await?;
        }

        let mut decoder = dir.decode_full().await?;
        decoder.enable_goodbye_entries(false);
        while let Some(entry) = decoder.next().await {
            let entry = entry?;
            let metadata = entry.metadata();
            let path = entry.path().strip_prefix(&prefix)?;

            match entry.kind() {
                EntryKind::File { .. } => {
                    log::debug!("adding '{}' to zip", path.display());
                    let entry = ZipEntry::new(
                        path,
                        metadata.stat.mtime.secs,
                        metadata.stat.mode as u16,
                        true,
                    );
                    zip.add_entry(entry, decoder.contents())
                        .await
                        .map_err(|err| format_err!("could not send file entry: {}", err))?;
                }
                EntryKind::Hardlink(_) => {
                    let entry = root
                        .lookup(&path)
                        .await?
                        .ok_or_else(|| format_err!("error looking up '{:?}'", path))?;
                    let realfile = accessor.follow_hardlink(&entry).await?;
                    let metadata = realfile.entry().metadata();
                    log::debug!("adding '{}' to zip", path.display());
                    let entry = ZipEntry::new(
                        path,
                        metadata.stat.mtime.secs,
                        metadata.stat.mode as u16,
                        true,
                    );
                    zip.add_entry(entry, decoder.contents())
                        .await
                        .map_err(|err| format_err!("could not send file entry: {}", err))?;
                }
                EntryKind::Directory => {
                    log::debug!("adding '{}' to zip", path.display());
                    let entry = ZipEntry::new(
                        path,
                        metadata.stat.mtime.secs,
                        metadata.stat.mode as u16,
                        false,
                    );
                    zip.add_entry::<FileContents<T>>(entry, None).await?;
                }
                _ => {} // ignore all else
            };
        }
    }

    zip.finish().await.map_err(|err| {
        eprintln!("error during finishing of zip: {}", err);
        err
    })
}

fn get_extractor<DEST>(destination: DEST, metadata: Metadata) -> Result<Extractor, Error>
where
    DEST: AsRef<Path>,
{
    create_path(
        &destination,
        None,
        Some(CreateOptions::new().perm(Mode::from_bits_truncate(0o700))),
    )
    .map_err(|err| {
        format_err!(
            "error creating directory {:?}: {}",
            destination.as_ref(),
            err
        )
    })?;

    let dir = Dir::open(
        destination.as_ref(),
        OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|err| {
        format_err!(
            "unable to open target directory {:?}: {}",
            destination.as_ref(),
            err,
        )
    })?;

    Ok(Extractor::new(dir, metadata, false, false, Flags::DEFAULT))
}

pub async fn extract_sub_dir<T, DEST, PATH>(
    destination: DEST,
    decoder: Accessor<T>,
    path: PATH,
) -> Result<(), Error>
where
    T: Clone + pxar::accessor::ReadAt + Unpin + Send + Sync + 'static,
    DEST: AsRef<Path>,
    PATH: AsRef<Path>,
{
    let root = decoder.open_root().await?;

    let mut extractor = get_extractor(
        destination,
        root.lookup_self().await?.entry().metadata().clone(),
    )?;

    let file = root
        .lookup(&path)
        .await?
        .ok_or_else(|| format_err!("error opening '{:?}'", path.as_ref()))?;

    recurse_files_extractor(&mut extractor, file).await
}

pub async fn extract_sub_dir_seq<S, DEST>(
    destination: DEST,
    mut decoder: Decoder<S>,
) -> Result<(), Error>
where
    S: pxar::decoder::SeqRead + Unpin + Send + 'static,
    DEST: AsRef<Path>,
{
    decoder.enable_goodbye_entries(true);
    let root = match decoder.next().await {
        Some(Ok(root)) => root,
        Some(Err(err)) => bail!("error getting root entry from pxar: {}", err),
        None => bail!("cannot extract empty archive"),
    };

    let mut extractor = get_extractor(destination, root.metadata().clone())?;

    if let Err(err) = seq_files_extractor(&mut extractor, decoder).await {
        log::error!("error extracting pxar archive: {}", err);
    }

    Ok(())
}

fn extract_special(
    extractor: &mut Extractor,
    entry: &Entry,
    file_name: &CStr,
) -> Result<(), Error> {
    let metadata = entry.metadata();
    match entry.kind() {
        EntryKind::Symlink(link) => {
            extractor.extract_symlink(file_name, metadata, link.as_ref())?;
        }
        EntryKind::Hardlink(link) => {
            extractor.extract_hardlink(file_name, link.as_os_str())?;
        }
        EntryKind::Device(dev) => {
            if extractor.contains_flags(Flags::WITH_DEVICE_NODES) {
                extractor.extract_device(file_name, metadata, dev)?;
            }
        }
        EntryKind::Fifo => {
            if extractor.contains_flags(Flags::WITH_FIFOS) {
                extractor.extract_special(file_name, metadata, 0)?;
            }
        }
        EntryKind::Socket => {
            if extractor.contains_flags(Flags::WITH_SOCKETS) {
                extractor.extract_special(file_name, metadata, 0)?;
            }
        }
        _ => bail!("extract_special used with unsupported entry kind"),
    }
    Ok(())
}

fn get_filename(entry: &Entry) -> Result<(OsString, CString), Error> {
    let file_name_os = entry.file_name().to_owned();

    // safety check: a file entry in an archive must never contain slashes:
    if file_name_os.as_bytes().contains(&b'/') {
        bail!("archive file entry contains slashes, which is invalid and a security concern");
    }

    let file_name = CString::new(file_name_os.as_bytes())
        .map_err(|_| format_err!("encountered file name with null-bytes"))?;

    Ok((file_name_os, file_name))
}

async fn recurse_files_extractor<T>(
    extractor: &mut Extractor,
    file: FileEntry<T>,
) -> Result<(), Error>
where
    T: Clone + pxar::accessor::ReadAt + Unpin + Send + Sync + 'static,
{
    let entry = file.entry();
    let metadata = entry.metadata();
    let (file_name_os, file_name) = get_filename(entry)?;

    log::debug!("extracting: {}", file.path().display());

    match file.kind() {
        EntryKind::Directory => {
            extractor
                .enter_directory(file_name_os.to_owned(), metadata.clone(), true)
                .map_err(|err| format_err!("error at entry {:?}: {}", file_name_os, err))?;

            let dir = file.enter_directory().await?;
            let mut seq_decoder = dir.decode_full().await?;
            seq_decoder.enable_goodbye_entries(true);
            seq_files_extractor(extractor, seq_decoder).await?;
            extractor.leave_directory()?;
        }
        EntryKind::File { size, .. } => {
            extractor
                .async_extract_file(
                    &file_name,
                    metadata,
                    *size,
                    &mut file.contents().await.map_err(|_| {
                        format_err!("found regular file entry without contents in archive")
                    })?,
                    extractor.overwrite,
                )
                .await?
        }
        EntryKind::GoodbyeTable => {} // ignore
        _ => extract_special(extractor, entry, &file_name)?,
    }
    Ok(())
}

async fn seq_files_extractor<T>(
    extractor: &mut Extractor,
    mut decoder: pxar::decoder::aio::Decoder<T>,
) -> Result<(), Error>
where
    T: pxar::decoder::SeqRead,
{
    let mut dir_level = 0;
    loop {
        let entry = match decoder.next().await {
            Some(entry) => entry?,
            None => return Ok(()),
        };

        let metadata = entry.metadata();
        let (file_name_os, file_name) = get_filename(&entry)?;

        if !matches!(entry.kind(), EntryKind::GoodbyeTable) {
            log::debug!("extracting: {}", entry.path().display());
        }

        if let Err(err) = async {
            match entry.kind() {
                EntryKind::Directory => {
                    dir_level += 1;
                    extractor
                        .enter_directory(file_name_os.to_owned(), metadata.clone(), true)
                        .map_err(|err| format_err!("error at entry {:?}: {}", file_name_os, err))?;
                }
                EntryKind::File { size, .. } => {
                    extractor
                        .async_extract_file(
                            &file_name,
                            metadata,
                            *size,
                            &mut decoder.contents().ok_or_else(|| {
                                format_err!("found regular file entry without contents in archive")
                            })?,
                            extractor.overwrite,
                        )
                        .await?
                }
                EntryKind::GoodbyeTable => {
                    dir_level -= 1;
                    extractor.leave_directory()?;
                }
                _ => extract_special(extractor, &entry, &file_name)?,
            }
            Ok(()) as Result<(), Error>
        }
        .await
        {
            let display = entry.path().display().to_string();
            log::error!(
                "error extracting {}: {}",
                if matches!(entry.kind(), EntryKind::GoodbyeTable) {
                    "<directory>"
                } else {
                    &display
                },
                err
            );
        }

        if dir_level < 0 {
            // we've encountered one Goodbye more then Directory, meaning we've left the dir we
            // started in - exit early, otherwise the extractor might panic
            return Ok(());
        }
    }
}
