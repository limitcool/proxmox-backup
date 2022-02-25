use std::collections::{HashSet, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::convert::TryFrom;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, format_err, Error};
use lazy_static::lazy_static;

use proxmox_schema::ApiType;

use proxmox_sys::fs::{replace_file, file_read_optional_string, CreateOptions};
use proxmox_sys::process_locker::ProcessLockSharedGuard;
use proxmox_sys::WorkerTaskContext;
use proxmox_sys::{task_log, task_warn};
use proxmox_sys::fs::{lock_dir_noblock, DirLockGuard};

use pbs_api_types::{
    UPID, DataStoreConfig, Authid, GarbageCollectionStatus, HumanByte,
    ChunkOrder, DatastoreTuning,
};
use pbs_config::{open_backup_lockfile, BackupLockGuard, ConfigVersionCache};

use crate::DataBlob;
use crate::backup_info::{BackupGroup, BackupDir};
use crate::chunk_store::ChunkStore;
use crate::dynamic_index::{DynamicIndexReader, DynamicIndexWriter};
use crate::fixed_index::{FixedIndexReader, FixedIndexWriter};
use crate::index::IndexFile;
use crate::manifest::{
    MANIFEST_BLOB_NAME, MANIFEST_LOCK_NAME, CLIENT_LOG_BLOB_NAME,
    ArchiveType, BackupManifest,
    archive_type,
};

lazy_static! {
    static ref DATASTORE_MAP: Mutex<HashMap<String, Arc<DataStore>>> = Mutex::new(HashMap::new());
}

/// checks if auth_id is owner, or, if owner is a token, if
/// auth_id is the user of the token
pub fn check_backup_owner(
    owner: &Authid,
    auth_id: &Authid,
) -> Result<(), Error> {
    let correct_owner = owner == auth_id
        || (owner.is_token() && &Authid::from(owner.user().clone()) == auth_id);
    if !correct_owner {
        bail!("backup owner check failed ({} != {})", auth_id, owner);
    }
    Ok(())
}

/// Datastore Management
///
/// A Datastore can store severals backups, and provides the
/// management interface for backup.
pub struct DataStore {
    chunk_store: Arc<ChunkStore>,
    gc_mutex: Mutex<()>,
    last_gc_status: Mutex<GarbageCollectionStatus>,
    verify_new: bool,
    chunk_order: ChunkOrder,
    last_generation: usize,
    last_update: i64,
}

impl DataStore {
    pub fn lookup_datastore(name: &str) -> Result<Arc<DataStore>, Error> {
        let version_cache = ConfigVersionCache::new()?;
        let generation = version_cache.datastore_generation();
        let now = proxmox_time::epoch_i64();

        let mut map = DATASTORE_MAP.lock().unwrap();
        let entry = map.get(name);

        if let Some(datastore) = &entry {
            if datastore.last_generation == generation && now < (datastore.last_update + 60) {
                return Ok(Arc::clone(datastore));
            }
        }

        let (config, _digest) = pbs_config::datastore::config()?;
        let config: DataStoreConfig = config.lookup("datastore", name)?;
        let path = PathBuf::from(&config.path);

        let datastore = DataStore::open_with_path(name, &path, config, generation, now)?;

        let datastore = Arc::new(datastore);
        map.insert(name.to_string(), datastore.clone());

        Ok(datastore)
    }

    /// removes all datastores that are not configured anymore
    pub fn remove_unused_datastores() -> Result<(), Error>{
        let (config, _digest) = pbs_config::datastore::config()?;

        let mut map = DATASTORE_MAP.lock().unwrap();
        // removes all elements that are not in the config
        map.retain(|key, _| {
            config.sections.contains_key(key)
        });
        Ok(())
    }

    fn open_with_path(
        store_name: &str,
        path: &Path,
        config: DataStoreConfig,
        last_generation: usize,
        last_update: i64,
    ) -> Result<Self, Error> {
        let chunk_store = ChunkStore::open(store_name, path)?;

        let mut gc_status_path = chunk_store.base_path();
        gc_status_path.push(".gc-status");

        let gc_status = if let Some(state) = file_read_optional_string(gc_status_path)? {
            match serde_json::from_str(&state) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("error reading gc-status: {}", err);
                    GarbageCollectionStatus::default()
                }
            }
        } else {
            GarbageCollectionStatus::default()
        };

        let tuning: DatastoreTuning = serde_json::from_value(
            DatastoreTuning::API_SCHEMA.parse_property_string(config.tuning.as_deref().unwrap_or(""))?
        )?;
        let chunk_order = tuning.chunk_order.unwrap_or(ChunkOrder::Inode);

        Ok(Self {
            chunk_store: Arc::new(chunk_store),
            gc_mutex: Mutex::new(()),
            last_gc_status: Mutex::new(gc_status),
            verify_new: config.verify_new.unwrap_or(false),
            chunk_order,
            last_generation,
            last_update,
        })
    }

    pub fn get_chunk_iterator(
        &self,
    ) -> Result<
        impl Iterator<Item = (Result<proxmox_sys::fs::ReadDirEntry, Error>, usize, bool)>,
        Error
    > {
        self.chunk_store.get_chunk_iterator()
    }

    pub fn create_fixed_writer<P: AsRef<Path>>(&self, filename: P, size: usize, chunk_size: usize) -> Result<FixedIndexWriter, Error> {

        let index = FixedIndexWriter::create(self.chunk_store.clone(), filename.as_ref(), size, chunk_size)?;

        Ok(index)
    }

    pub fn open_fixed_reader<P: AsRef<Path>>(&self, filename: P) -> Result<FixedIndexReader, Error> {

        let full_path =  self.chunk_store.relative_path(filename.as_ref());

        let index = FixedIndexReader::open(&full_path)?;

        Ok(index)
    }

    pub fn create_dynamic_writer<P: AsRef<Path>>(
        &self, filename: P,
    ) -> Result<DynamicIndexWriter, Error> {

        let index = DynamicIndexWriter::create(
            self.chunk_store.clone(), filename.as_ref())?;

        Ok(index)
    }

    pub fn open_dynamic_reader<P: AsRef<Path>>(&self, filename: P) -> Result<DynamicIndexReader, Error> {

        let full_path =  self.chunk_store.relative_path(filename.as_ref());

        let index = DynamicIndexReader::open(&full_path)?;

        Ok(index)
    }

    pub fn open_index<P>(&self, filename: P) -> Result<Box<dyn IndexFile + Send>, Error>
    where
        P: AsRef<Path>,
    {
        let filename = filename.as_ref();
        let out: Box<dyn IndexFile + Send> =
            match archive_type(filename)? {
                ArchiveType::DynamicIndex => Box::new(self.open_dynamic_reader(filename)?),
                ArchiveType::FixedIndex => Box::new(self.open_fixed_reader(filename)?),
                _ => bail!("cannot open index file of unknown type: {:?}", filename),
            };
        Ok(out)
    }

    /// Fast index verification - only check if chunks exists
    pub fn fast_index_verification(
        &self,
        index: &dyn IndexFile,
        checked: &mut HashSet<[u8;32]>,
    ) -> Result<(), Error> {

        for pos in 0..index.index_count() {
            let info = index.chunk_info(pos).unwrap();
            if checked.contains(&info.digest) {
                continue;
            }

            self.stat_chunk(&info.digest).
                map_err(|err| {
                    format_err!(
                        "fast_index_verification error, stat_chunk {} failed - {}",
                        hex::encode(&info.digest),
                        err,
                    )
                })?;

            checked.insert(info.digest);
        }

        Ok(())
    }

    pub fn name(&self) -> &str {
        self.chunk_store.name()
    }

    pub fn base_path(&self) -> PathBuf {
        self.chunk_store.base_path()
    }

    /// Cleanup a backup directory
    ///
    /// Removes all files not mentioned in the manifest.
    pub fn cleanup_backup_dir(&self, backup_dir: &BackupDir, manifest: &BackupManifest
    ) ->  Result<(), Error> {

        let mut full_path = self.base_path();
        full_path.push(backup_dir.relative_path());

        let mut wanted_files = HashSet::new();
        wanted_files.insert(MANIFEST_BLOB_NAME.to_string());
        wanted_files.insert(CLIENT_LOG_BLOB_NAME.to_string());
        manifest.files().iter().for_each(|item| { wanted_files.insert(item.filename.clone()); });

        for item in proxmox_sys::fs::read_subdir(libc::AT_FDCWD, &full_path)?.flatten() {
            if let Some(file_type) = item.file_type() {
                if file_type != nix::dir::Type::File { continue; }
            }
            let file_name = item.file_name().to_bytes();
            if file_name == b"." || file_name == b".." { continue; };
            if let Ok(name) = std::str::from_utf8(file_name) {
                if wanted_files.contains(name) { continue; }
            }
            println!("remove unused file {:?}", item.file_name());
            let dirfd = item.parent_fd();
            let _res = unsafe { libc::unlinkat(dirfd, item.file_name().as_ptr(), 0) };
        }

        Ok(())
    }

    /// Returns the absolute path for a backup_group
    pub fn group_path(&self, backup_group: &BackupGroup) -> PathBuf {
        let mut full_path = self.base_path();
        full_path.push(backup_group.group_path());
        full_path
    }

    /// Returns the absolute path for backup_dir
    pub fn snapshot_path(&self, backup_dir: &BackupDir) -> PathBuf {
        let mut full_path = self.base_path();
        full_path.push(backup_dir.relative_path());
        full_path
    }

    /// Remove a complete backup group including all snapshots, returns true
    /// if all snapshots were removed, and false if some were protected
    pub fn remove_backup_group(&self, backup_group: &BackupGroup) ->  Result<bool, Error> {

        let full_path = self.group_path(backup_group);

        let _guard = proxmox_sys::fs::lock_dir_noblock(&full_path, "backup group", "possible running backup")?;

        log::info!("removing backup group {:?}", full_path);

        let mut removed_all = true;

        // remove all individual backup dirs first to ensure nothing is using them
        for snap in backup_group.list_backups(&self.base_path())? {
            if snap.backup_dir.is_protected(self.base_path()) {
                removed_all = false;
                continue;
            }
            self.remove_backup_dir(&snap.backup_dir, false)?;
        }

        if removed_all {
            // no snapshots left, we can now safely remove the empty folder
            std::fs::remove_dir_all(&full_path)
                .map_err(|err| {
                    format_err!(
                        "removing backup group directory {:?} failed - {}",
                        full_path,
                        err,
                    )
                })?;
        }

        Ok(removed_all)
    }

    /// Remove a backup directory including all content
    pub fn remove_backup_dir(&self, backup_dir: &BackupDir, force: bool) ->  Result<(), Error> {

        let full_path = self.snapshot_path(backup_dir);

        let (_guard, _manifest_guard);
        if !force {
            _guard = lock_dir_noblock(&full_path, "snapshot", "possibly running or in use")?;
            _manifest_guard = self.lock_manifest(backup_dir)?;
        }

        if backup_dir.is_protected(self.base_path()) {
            bail!("cannot remove protected snapshot");
        }

        log::info!("removing backup snapshot {:?}", full_path);
        std::fs::remove_dir_all(&full_path)
            .map_err(|err| {
                format_err!(
                    "removing backup snapshot {:?} failed - {}",
                    full_path,
                    err,
                )
            })?;

        // the manifest does not exists anymore, we do not need to keep the lock
        if let Ok(path) = self.manifest_lock_path(backup_dir) {
            // ignore errors
            let _ = std::fs::remove_file(path);
        }

        Ok(())
    }

    /// Returns the time of the last successful backup
    ///
    /// Or None if there is no backup in the group (or the group dir does not exist).
    pub fn last_successful_backup(&self, backup_group: &BackupGroup) -> Result<Option<i64>, Error> {
        let base_path = self.base_path();
        let mut group_path = base_path.clone();
        group_path.push(backup_group.group_path());

        if group_path.exists() {
            backup_group.last_successful_backup(&base_path)
        } else {
            Ok(None)
        }
    }

    /// Returns the backup owner.
    ///
    /// The backup owner is the entity who first created the backup group.
    pub fn get_owner(&self, backup_group: &BackupGroup) -> Result<Authid, Error> {
        let mut full_path = self.base_path();
        full_path.push(backup_group.group_path());
        full_path.push("owner");
        let owner = proxmox_sys::fs::file_read_firstline(full_path)?;
        owner.trim_end().parse() // remove trailing newline
    }

    pub fn owns_backup(&self, backup_group: &BackupGroup, auth_id: &Authid) -> Result<bool, Error> {
        let owner = self.get_owner(backup_group)?;

        Ok(check_backup_owner(&owner, auth_id).is_ok())
    }

    /// Set the backup owner.
    pub fn set_owner(
        &self,
        backup_group: &BackupGroup,
        auth_id: &Authid,
        force: bool,
    ) -> Result<(), Error> {
        let mut path = self.base_path();
        path.push(backup_group.group_path());
        path.push("owner");

        let mut open_options = std::fs::OpenOptions::new();
        open_options.write(true);
        open_options.truncate(true);

        if force {
            open_options.create(true);
        } else {
            open_options.create_new(true);
        }

        let mut file = open_options.open(&path)
            .map_err(|err| format_err!("unable to create owner file {:?} - {}", path, err))?;

        writeln!(file, "{}", auth_id)
            .map_err(|err| format_err!("unable to write owner file  {:?} - {}", path, err))?;

        Ok(())
    }

    /// Create (if it does not already exists) and lock a backup group
    ///
    /// And set the owner to 'userid'. If the group already exists, it returns the
    /// current owner (instead of setting the owner).
    ///
    /// This also acquires an exclusive lock on the directory and returns the lock guard.
    pub fn create_locked_backup_group(
        &self,
        backup_group: &BackupGroup,
        auth_id: &Authid,
    ) -> Result<(Authid, DirLockGuard), Error> {
        // create intermediate path first:
        let mut full_path = self.base_path();
        full_path.push(backup_group.backup_type());
        std::fs::create_dir_all(&full_path)?;

        full_path.push(backup_group.backup_id());

        // create the last component now
        match std::fs::create_dir(&full_path) {
            Ok(_) => {
                let guard = lock_dir_noblock(&full_path, "backup group", "another backup is already running")?;
                self.set_owner(backup_group, auth_id, false)?;
                let owner = self.get_owner(backup_group)?; // just to be sure
                Ok((owner, guard))
            }
            Err(ref err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let guard = lock_dir_noblock(&full_path, "backup group", "another backup is already running")?;
                let owner = self.get_owner(backup_group)?; // just to be sure
                Ok((owner, guard))
            }
            Err(err) => bail!("unable to create backup group {:?} - {}", full_path, err),
        }
    }

    /// Creates a new backup snapshot inside a BackupGroup
    ///
    /// The BackupGroup directory needs to exist.
    pub fn create_locked_backup_dir(&self, backup_dir: &BackupDir)
        -> Result<(PathBuf, bool, DirLockGuard), Error>
    {
        let relative_path = backup_dir.relative_path();
        let mut full_path = self.base_path();
        full_path.push(&relative_path);

        let lock = ||
            lock_dir_noblock(&full_path, "snapshot", "internal error - tried creating snapshot that's already in use");

        match std::fs::create_dir(&full_path) {
            Ok(_) => Ok((relative_path, true, lock()?)),
            Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => Ok((relative_path, false, lock()?)),
            Err(e) => Err(e.into())
        }
    }

    pub fn list_images(&self) -> Result<Vec<PathBuf>, Error> {
        let base = self.base_path();

        let mut list = vec![];

        use walkdir::WalkDir;

        let walker = WalkDir::new(&base).into_iter();

        // make sure we skip .chunks (and other hidden files to keep it simple)
        fn is_hidden(entry: &walkdir::DirEntry) -> bool {
            entry.file_name()
                .to_str()
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
        }
        let handle_entry_err = |err: walkdir::Error| {
            if let Some(inner) = err.io_error() {
                if let Some(path) = err.path() {
                    if inner.kind() == io::ErrorKind::PermissionDenied {
                        // only allow to skip ext4 fsck directory, avoid GC if, for example,
                        // a user got file permissions wrong on datastore rsync to new server
                        if err.depth() > 1 || !path.ends_with("lost+found") {
                            bail!("cannot continue garbage-collection safely, permission denied on: {:?}", path)
                        }
                    } else {
                        bail!("unexpected error on datastore traversal: {} - {:?}", inner, path)
                    }
                } else {
                    bail!("unexpected error on datastore traversal: {}", inner)
                }
            }
            Ok(())
        };
        for entry in walker.filter_entry(|e| !is_hidden(e)) {
            let path = match entry {
                Ok(entry) => entry.into_path(),
                Err(err) => {
                    handle_entry_err(err)?;
                    continue
                },
            };
            if let Ok(archive_type) = archive_type(&path) {
                if archive_type == ArchiveType::FixedIndex || archive_type == ArchiveType::DynamicIndex {
                    list.push(path);
                }
            }
        }

        Ok(list)
    }

    // mark chunks  used by ``index`` as used
    fn index_mark_used_chunks<I: IndexFile>(
        &self,
        index: I,
        file_name: &Path, // only used for error reporting
        status: &mut GarbageCollectionStatus,
        worker: &dyn WorkerTaskContext,
    ) -> Result<(), Error> {

        status.index_file_count += 1;
        status.index_data_bytes += index.index_bytes();

        for pos in 0..index.index_count() {
            worker.check_abort()?;
            worker.fail_on_shutdown()?;
            let digest = index.index_digest(pos).unwrap();
            if !self.chunk_store.cond_touch_chunk(digest, false)? {
                task_warn!(
                    worker,
                    "warning: unable to access non-existent chunk {}, required by {:?}",
                    hex::encode(digest),
                    file_name,
                );

                // touch any corresponding .bad files to keep them around, meaning if a chunk is
                // rewritten correctly they will be removed automatically, as well as if no index
                // file requires the chunk anymore (won't get to this loop then)
                for i in 0..=9 {
                    let bad_ext = format!("{}.bad", i);
                    let mut bad_path = PathBuf::new();
                    bad_path.push(self.chunk_path(digest).0);
                    bad_path.set_extension(bad_ext);
                    self.chunk_store.cond_touch_path(&bad_path, false)?;
                }
            }
        }
        Ok(())
    }

    fn mark_used_chunks(
        &self,
        status: &mut GarbageCollectionStatus,
        worker: &dyn WorkerTaskContext,
    ) -> Result<(), Error> {

        let image_list = self.list_images()?;
        let image_count = image_list.len();

        let mut last_percentage: usize = 0;

        let mut strange_paths_count: u64 = 0;

        for (i, img) in image_list.into_iter().enumerate() {

            worker.check_abort()?;
            worker.fail_on_shutdown()?;

            if let Some(backup_dir_path) = img.parent() {
                let backup_dir_path = backup_dir_path.strip_prefix(self.base_path())?;
                if let Some(backup_dir_str) = backup_dir_path.to_str() {
                    if BackupDir::from_str(backup_dir_str).is_err() {
                        strange_paths_count += 1;
                    }
                }
            }

            match std::fs::File::open(&img) {
                Ok(file) => {
                    if let Ok(archive_type) = archive_type(&img) {
                        if archive_type == ArchiveType::FixedIndex {
                            let index = FixedIndexReader::new(file).map_err(|e| {
                                format_err!("can't read index '{}' - {}", img.to_string_lossy(), e)
                            })?;
                            self.index_mark_used_chunks(index, &img, status, worker)?;
                        } else if archive_type == ArchiveType::DynamicIndex {
                            let index = DynamicIndexReader::new(file).map_err(|e| {
                                format_err!("can't read index '{}' - {}", img.to_string_lossy(), e)
                            })?;
                            self.index_mark_used_chunks(index, &img, status, worker)?;
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => (), // ignore vanished files
                Err(err) => bail!("can't open index {} - {}", img.to_string_lossy(), err),
            }

            let percentage = (i + 1) * 100 / image_count;
            if percentage > last_percentage {
                task_log!(
                    worker,
                    "marked {}% ({} of {} index files)",
                    percentage,
                    i + 1,
                    image_count,
                );
                last_percentage = percentage;
            }
        }

        if strange_paths_count > 0 {
            task_log!(
                worker,
                "found (and marked) {} index files outside of expected directory scheme",
                strange_paths_count,
            );
        }


        Ok(())
    }

    pub fn last_gc_status(&self) -> GarbageCollectionStatus {
        self.last_gc_status.lock().unwrap().clone()
    }

    pub fn garbage_collection_running(&self) -> bool {
        !matches!(self.gc_mutex.try_lock(), Ok(_))
    }

    pub fn garbage_collection(&self, worker: &dyn WorkerTaskContext, upid: &UPID) -> Result<(), Error> {

        if let Ok(ref mut _mutex) = self.gc_mutex.try_lock() {

            // avoids that we run GC if an old daemon process has still a
            // running backup writer, which is not save as we have no "oldest
            // writer" information and thus no safe atime cutoff
            let _exclusive_lock =  self.chunk_store.try_exclusive_lock()?;

            let phase1_start_time = proxmox_time::epoch_i64();
            let oldest_writer = self.chunk_store.oldest_writer().unwrap_or(phase1_start_time);

            let mut gc_status = GarbageCollectionStatus::default();
            gc_status.upid = Some(upid.to_string());

            task_log!(worker, "Start GC phase1 (mark used chunks)");

            self.mark_used_chunks(&mut gc_status, worker)?;

            task_log!(worker, "Start GC phase2 (sweep unused chunks)");
            self.chunk_store.sweep_unused_chunks(
                oldest_writer,
                phase1_start_time,
                &mut gc_status,
                worker,
            )?;

            task_log!(
                worker,
                "Removed garbage: {}",
                HumanByte::from(gc_status.removed_bytes),
            );
            task_log!(worker, "Removed chunks: {}", gc_status.removed_chunks);
            if gc_status.pending_bytes > 0 {
                task_log!(
                    worker,
                    "Pending removals: {} (in {} chunks)",
                    HumanByte::from(gc_status.pending_bytes),
                    gc_status.pending_chunks,
                );
            }
            if gc_status.removed_bad > 0 {
                task_log!(worker, "Removed bad chunks: {}", gc_status.removed_bad);
            }

            if gc_status.still_bad > 0 {
                task_log!(worker, "Leftover bad chunks: {}", gc_status.still_bad);
            }

            task_log!(
                worker,
                "Original data usage: {}",
                HumanByte::from(gc_status.index_data_bytes),
            );

            if gc_status.index_data_bytes > 0 {
                let comp_per = (gc_status.disk_bytes as f64 * 100.)/gc_status.index_data_bytes as f64;
                task_log!(
                    worker,
                    "On-Disk usage: {} ({:.2}%)",
                    HumanByte::from(gc_status.disk_bytes),
                    comp_per,
                );
            }

            task_log!(worker, "On-Disk chunks: {}", gc_status.disk_chunks);

            let deduplication_factor = if gc_status.disk_bytes > 0 {
                (gc_status.index_data_bytes as f64)/(gc_status.disk_bytes as f64)
            } else {
                1.0
            };

            task_log!(worker, "Deduplication factor: {:.2}", deduplication_factor);

            if gc_status.disk_chunks > 0 {
                let avg_chunk = gc_status.disk_bytes/(gc_status.disk_chunks as u64);
                task_log!(worker, "Average chunk size: {}", HumanByte::from(avg_chunk));
            }

            if let Ok(serialized) = serde_json::to_string(&gc_status) {
                let mut path = self.base_path();
                path.push(".gc-status");

                let backup_user = pbs_config::backup_user()?;
                let mode = nix::sys::stat::Mode::from_bits_truncate(0o0644);
                // set the correct owner/group/permissions while saving file
                // owner(rw) = backup, group(r)= backup
                let options = CreateOptions::new()
                    .perm(mode)
                    .owner(backup_user.uid)
                    .group(backup_user.gid);

                // ignore errors
                let _ = replace_file(path, serialized.as_bytes(), options, false);
            }

            *self.last_gc_status.lock().unwrap() = gc_status;

        } else {
            bail!("Start GC failed - (already running/locked)");
        }

        Ok(())
    }

    pub fn try_shared_chunk_store_lock(&self) -> Result<ProcessLockSharedGuard, Error> {
        self.chunk_store.try_shared_lock()
    }

    pub fn chunk_path(&self, digest:&[u8; 32]) -> (PathBuf, String) {
        self.chunk_store.chunk_path(digest)
    }

    pub fn cond_touch_chunk(&self, digest: &[u8; 32], fail_if_not_exist: bool) -> Result<bool, Error> {
        self.chunk_store.cond_touch_chunk(digest, fail_if_not_exist)
    }

    pub fn insert_chunk(
        &self,
        chunk: &DataBlob,
        digest: &[u8; 32],
    ) -> Result<(bool, u64), Error> {
        self.chunk_store.insert_chunk(chunk, digest)
    }

    pub fn load_blob(&self, backup_dir: &BackupDir, filename: &str) -> Result<DataBlob, Error> {
        let mut path = self.base_path();
        path.push(backup_dir.relative_path());
        path.push(filename);

        proxmox_lang::try_block!({
            let mut file = std::fs::File::open(&path)?;
            DataBlob::load_from_reader(&mut file)
        }).map_err(|err| format_err!("unable to load blob '{:?}' - {}", path, err))
    }


    pub fn stat_chunk(&self, digest: &[u8; 32]) -> Result<std::fs::Metadata, Error> {
        let (chunk_path, _digest_str) = self.chunk_store.chunk_path(digest);
        std::fs::metadata(chunk_path).map_err(Error::from)
    }

    pub fn load_chunk(&self, digest: &[u8; 32]) -> Result<DataBlob, Error> {

        let (chunk_path, digest_str) = self.chunk_store.chunk_path(digest);

        proxmox_lang::try_block!({
            let mut file = std::fs::File::open(&chunk_path)?;
            DataBlob::load_from_reader(&mut file)
        }).map_err(|err| format_err!(
            "store '{}', unable to load chunk '{}' - {}",
            self.name(),
            digest_str,
            err,
        ))
    }

    /// Returns the filename to lock a manifest
    ///
    /// Also creates the basedir. The lockfile is located in
    /// '/run/proxmox-backup/locks/{datastore}/{type}/{id}/{timestamp}.index.json.lck'
    fn manifest_lock_path(
        &self,
        backup_dir: &BackupDir,
    ) -> Result<String, Error> {
        let mut path = format!(
            "/run/proxmox-backup/locks/{}/{}/{}",
            self.name(),
            backup_dir.group().backup_type(),
            backup_dir.group().backup_id(),
        );
        std::fs::create_dir_all(&path)?;
        use std::fmt::Write;
        write!(path, "/{}{}", backup_dir.backup_time_string(), &MANIFEST_LOCK_NAME)?;

        Ok(path)
    }

    fn lock_manifest(
        &self,
        backup_dir: &BackupDir,
    ) -> Result<BackupLockGuard, Error> {
        let path = self.manifest_lock_path(backup_dir)?;

        // update_manifest should never take a long time, so if someone else has
        // the lock we can simply block a bit and should get it soon
        open_backup_lockfile(&path, Some(Duration::from_secs(5)), true)
            .map_err(|err| {
                format_err!(
                    "unable to acquire manifest lock {:?} - {}", &path, err
                )
            })
    }

    /// Load the manifest without a lock. Must not be written back.
    pub fn load_manifest(
        &self,
        backup_dir: &BackupDir,
    ) -> Result<(BackupManifest, u64), Error> {
        let blob = self.load_blob(backup_dir, MANIFEST_BLOB_NAME)?;
        let raw_size = blob.raw_size();
        let manifest = BackupManifest::try_from(blob)?;
        Ok((manifest, raw_size))
    }

    /// Update the manifest of the specified snapshot. Never write a manifest directly,
    /// only use this method - anything else may break locking guarantees.
    pub fn update_manifest(
        &self,
        backup_dir: &BackupDir,
        update_fn: impl FnOnce(&mut BackupManifest),
    ) -> Result<(), Error> {

        let _guard = self.lock_manifest(backup_dir)?;
        let (mut manifest, _) = self.load_manifest(backup_dir)?;

        update_fn(&mut manifest);

        let manifest = serde_json::to_value(manifest)?;
        let manifest = serde_json::to_string_pretty(&manifest)?;
        let blob = DataBlob::encode(manifest.as_bytes(), None, true)?;
        let raw_data = blob.raw_data();

        let mut path = self.base_path();
        path.push(backup_dir.relative_path());
        path.push(MANIFEST_BLOB_NAME);

        // atomic replace invalidates flock - no other writes past this point!
        replace_file(&path, raw_data, CreateOptions::new(), false)?;

        Ok(())
    }

    /// Updates the protection status of the specified snapshot.
    pub fn update_protection(
        &self,
        backup_dir: &BackupDir,
        protection: bool
    ) -> Result<(), Error> {
        let full_path = self.snapshot_path(backup_dir);

        let _guard = lock_dir_noblock(&full_path, "snapshot", "possibly running or in use")?;

        let protected_path = backup_dir.protected_file(self.base_path());
        if protection {
            std::fs::File::create(protected_path)
                .map_err(|err| format_err!("could not create protection file: {}", err))?;
        } else if let Err(err) = std::fs::remove_file(protected_path) {
            // ignore error for non-existing file
            if err.kind() != std::io::ErrorKind::NotFound {
                bail!("could not remove protection file: {}", err);
            }
        }

        Ok(())
    }

    pub fn verify_new(&self) -> bool {
        self.verify_new
    }

    /// returns a list of chunks sorted by their inode number on disk
    /// chunks that could not be stat'ed are at the end of the list
    pub fn get_chunks_in_order<F, A>(
        &self,
        index: &Box<dyn IndexFile + Send>,
        skip_chunk: F,
        check_abort: A,
    ) -> Result<Vec<(usize, u64)>, Error>
    where
        F: Fn(&[u8; 32]) -> bool,
        A: Fn(usize) -> Result<(), Error>,
    {
        let index_count = index.index_count();
        let mut chunk_list = Vec::with_capacity(index_count);
        use std::os::unix::fs::MetadataExt;
        for pos in 0..index_count {
            check_abort(pos)?;

            let info = index.chunk_info(pos).unwrap();

            if skip_chunk(&info.digest) {
                continue;
            }

            let ino = match self.chunk_order {
                ChunkOrder::Inode => {
                    match self.stat_chunk(&info.digest) {
                        Err(_) => u64::MAX, // could not stat, move to end of list
                        Ok(metadata) => metadata.ino(),
                    }
                }
                ChunkOrder::None => 0,
            };

            chunk_list.push((pos, ino));
        }

        match self.chunk_order {
            // sorting by inode improves data locality, which makes it lots faster on spinners
            ChunkOrder::Inode => {
                chunk_list.sort_unstable_by(|(_, ino_a), (_, ino_b)| ino_a.cmp(ino_b))
            }
            ChunkOrder::None => {}
        }

        Ok(chunk_list)
    }
}
