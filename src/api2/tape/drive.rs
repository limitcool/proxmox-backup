use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, format_err, Error};
use serde_json::Value;

use proxmox::{
    sortable,
    identity,
    list_subdirs_api_method,
    tools::Uuid,
    sys::error::SysError,
    api::{
        api,
        RpcEnvironment,
        Router,
        SubdirMap,
    },
};

use crate::{
    config::{
        self,
        drive::check_drive_exists,
    },
    api2::{
        types::{
            UPID_SCHEMA,
            DRIVE_NAME_SCHEMA,
            MEDIA_LABEL_SCHEMA,
            MEDIA_POOL_NAME_SCHEMA,
            Authid,
            LinuxTapeDrive,
            ScsiTapeChanger,
            TapeDeviceInfo,
            MediaIdFlat,
            LabelUuidMap,
            MamAttribute,
            LinuxDriveAndMediaStatus,
        },
        tape::restore::restore_media,
    },
    server::WorkerTask,
    tape::{
        TAPE_STATUS_DIR,
        TapeDriver,
        MediaChange,
        MediaPool,
        Inventory,
        MediaStateDatabase,
        MediaCatalog,
        MediaId,
        mtx_load,
        mtx_unload,
        linux_tape_device_list,
        open_drive,
        media_changer,
        update_changer_online_status,
        linux_tape::{
            LinuxTapeHandle,
            open_linux_tape_device,
        },
        file_formats::{
            MediaLabel,
            MediaSetLabel,
        },
    },
};

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            slot: {
                description: "Source slot number",
                minimum: 1,
            },
        },
    },
)]
/// Load media via changer from slot
pub async fn load_slot(
    drive: String,
    slot: u64,
    _param: Value,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    let drive_config: LinuxTapeDrive = config.lookup("linux", &drive)?;

    let changer: ScsiTapeChanger = match drive_config.changer {
        Some(ref changer) => config.lookup("changer", changer)?,
        None => bail!("drive '{}' has no associated changer", drive),
    };

    tokio::task::spawn_blocking(move || {
        let drivenum = drive_config.changer_drive_id.unwrap_or(0);
        mtx_load(&changer.path, slot, drivenum)
    }).await?
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            "changer-id": {
                schema: MEDIA_LABEL_SCHEMA,
            },
        },
    },
)]
/// Load media with specified label
///
/// Issue a media load request to the associated changer device.
pub async fn load_media(drive: String, changer_id: String) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    tokio::task::spawn_blocking(move || {
        let (mut changer, _) = media_changer(&config, &drive, false)?;
        changer.load_media(&changer_id)
    }).await?
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            slot: {
                description: "Target slot number. If omitted, defaults to the slot that the drive was loaded from.",
                minimum: 1,
                optional: true,
            },
        },
    },
)]
/// Unload media via changer
pub async fn unload(
    drive: String,
    slot: Option<u64>,
    _param: Value,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    let mut drive_config: LinuxTapeDrive = config.lookup("linux", &drive)?;

    let changer: ScsiTapeChanger = match drive_config.changer {
        Some(ref changer) => config.lookup("changer", changer)?,
        None => bail!("drive '{}' has no associated changer", drive),
    };

    let drivenum = drive_config.changer_drive_id.unwrap_or(0);

    tokio::task::spawn_blocking(move || {
        if let Some(slot) = slot {
            mtx_unload(&changer.path, slot, drivenum)
        } else {
            drive_config.unload_media()
        }
    }).await?
}

#[api(
    input: {
        properties: {},
    },
    returns: {
        description: "The list of autodetected tape drives.",
        type: Array,
        items: {
            type: TapeDeviceInfo,
        },
    },
)]
/// Scan tape drives
pub fn scan_drives(_param: Value) -> Result<Vec<TapeDeviceInfo>, Error> {

    let list = linux_tape_device_list();

    Ok(list)
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            fast: {
                description: "Use fast erase.",
                type: bool,
                optional: true,
                default: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Erase media
pub fn erase_media(
    drive: String,
    fast: Option<bool>,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let (config, _digest) = config::drive::config()?;

    check_drive_exists(&config, &drive)?; // early check before starting worker

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let upid_str = WorkerTask::new_thread(
        "erase-media",
        Some(drive.clone()),
        auth_id,
        true,
        move |_worker| {
            let mut drive = open_drive(&config, &drive)?;
            drive.erase_media(fast.unwrap_or(true))?;
            Ok(())
        }
    )?;

    Ok(upid_str.into())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Rewind tape
pub fn rewind(
    drive: String,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let (config, _digest) = config::drive::config()?;

    check_drive_exists(&config, &drive)?; // early check before starting worker

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let upid_str = WorkerTask::new_thread(
        "rewind-media",
        Some(drive.clone()),
        auth_id,
        true,
        move |_worker| {
            let mut drive = open_drive(&config, &drive)?;
            drive.rewind()?;
            Ok(())
        }
    )?;

    Ok(upid_str.into())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
)]
/// Eject/Unload drive media
pub async fn eject_media(drive: String) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    tokio::task::spawn_blocking(move || {
        let (mut changer, _) = media_changer(&config, &drive, false)?;

        if !changer.eject_on_unload() {
            let mut drive = open_drive(&config, &drive)?;
            drive.eject_media()?;
        }

        changer.unload_media()
    }).await?
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            "changer-id": {
                schema: MEDIA_LABEL_SCHEMA,
            },
            pool: {
                schema: MEDIA_POOL_NAME_SCHEMA,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Label media
///
/// Write a new media label to the media in 'drive'. The media is
/// assigned to the specified 'pool', or else to the free media pool.
///
/// Note: The media need to be empty (you may want to erase it first).
pub fn label_media(
    drive: String,
    pool: Option<String>,
    changer_id: String,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    if let Some(ref pool) = pool {
        let (pool_config, _digest) = config::media_pool::config()?;

        if pool_config.sections.get(pool).is_none() {
            bail!("no such pool ('{}')", pool);
        }
    }

    let (config, _digest) = config::drive::config()?;

    let upid_str = WorkerTask::new_thread(
        "label-media",
        Some(drive.clone()),
        auth_id,
        true,
        move |worker| {

            let mut drive = open_drive(&config, &drive)?;

            drive.rewind()?;

            match drive.read_next_file() {
                Ok(Some(_file)) => bail!("media is not empty (erase first)"),
                Ok(None) => { /* EOF mark at BOT, assume tape is empty */ },
                Err(err) => {
                    if err.is_errno(nix::errno::Errno::ENOSPC) || err.is_errno(nix::errno::Errno::EIO) {
                        /* assume tape is empty */
                    } else {
                        bail!("media read error - {}", err);
                    }
                }
            }

            let ctime = proxmox::tools::time::epoch_i64();
            let label = MediaLabel {
                changer_id: changer_id.to_string(),
                uuid: Uuid::generate(),
                ctime,
            };

            write_media_label(worker, &mut drive, label, pool)
        }
    )?;

    Ok(upid_str.into())
}

fn write_media_label(
    worker: Arc<WorkerTask>,
    drive: &mut Box<dyn TapeDriver>,
    label: MediaLabel,
    pool: Option<String>,
) -> Result<(), Error> {

    drive.label_tape(&label)?;

    let mut media_set_label = None;

    if let Some(ref pool) = pool {
        // assign media to pool by writing special media set label
        worker.log(format!("Label media '{}' for pool '{}'", label.changer_id, pool));
        let set = MediaSetLabel::with_data(&pool, [0u8; 16].into(), 0, label.ctime);

        drive.write_media_set_label(&set)?;
        media_set_label = Some(set);
    } else {
        worker.log(format!("Label media '{}' (no pool assignment)", label.changer_id));
    }

    let media_id = MediaId { label, media_set_label };

    let status_path = Path::new(TAPE_STATUS_DIR);

    // Create the media catalog
    MediaCatalog::overwrite(status_path, &media_id, false)?;

    let mut inventory = Inventory::load(status_path)?;
    inventory.store(media_id.clone())?;

    drive.rewind()?;

    match drive.read_label() {
        Ok(Some(info)) => {
            if info.label.uuid != media_id.label.uuid {
                bail!("verify label failed - got wrong label uuid");
            }
            if let Some(ref pool) = pool {
                match info.media_set_label {
                    Some(set) => {
                        if set.uuid != [0u8; 16].into() {
                            bail!("verify media set label failed - got wrong set uuid");
                        }
                        if &set.pool != pool {
                            bail!("verify media set label failed - got wrong pool");
                        }
                    }
                    None => {
                        bail!("verify media set label failed (missing set label)");
                    }
                }
            }
        },
        Ok(None) => bail!("verify label failed (got empty media)"),
        Err(err) => bail!("verify label failed - {}", err),
    };

    drive.rewind()?;

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
    returns: {
        type: MediaIdFlat,
    },
)]
/// Read media label
pub async fn read_label(drive: String) -> Result<MediaIdFlat, Error> {

    let (config, _digest) = config::drive::config()?;

    tokio::task::spawn_blocking(move || {
        let mut drive = open_drive(&config, &drive)?;

        let media_id = drive.read_label()?;

        let media_id = match media_id {
            Some(media_id) => {
                let mut flat = MediaIdFlat {
                    uuid: media_id.label.uuid.to_string(),
                    changer_id: media_id.label.changer_id.clone(),
                    ctime: media_id.label.ctime,
                    media_set_ctime: None,
                    media_set_uuid: None,
                    pool: None,
                    seq_nr: None,
                };
                if let Some(set) = media_id.media_set_label {
                    flat.pool = Some(set.pool.clone());
                    flat.seq_nr = Some(set.seq_nr);
                    flat.media_set_uuid = Some(set.uuid.to_string());
                    flat.media_set_ctime = Some(set.ctime);
                }
                flat
            }
            None => {
                bail!("Media is empty (no label).");
            }
        };

        Ok(media_id)
    }).await?
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
    returns: {
        description: "The list of media labels with associated media Uuid (if any).",
        type: Array,
        items: {
            type: LabelUuidMap,
        },
    },
)]
/// List known media labels (Changer Inventory)
///
/// Note: Only useful for drives with associated changer device.
///
/// This method queries the changer to get a list of media labels.
///
/// Note: This updates the media online status.
pub async fn inventory(
    drive: String,
) -> Result<Vec<LabelUuidMap>, Error> {

    let (config, _digest) = config::drive::config()?;

    tokio::task::spawn_blocking(move || {
        let (changer, changer_name) = media_changer(&config, &drive, false)?;

        let changer_id_list = changer.list_media_changer_ids()?;

        let state_path = Path::new(TAPE_STATUS_DIR);

        let mut inventory = Inventory::load(state_path)?;
        let mut state_db = MediaStateDatabase::load(state_path)?;

        update_changer_online_status(
            &config,
            &mut inventory,
            &mut state_db,
            &changer_name,
            &changer_id_list,
        )?;

        let mut list = Vec::new();

        for changer_id in changer_id_list.iter() {
            if changer_id.starts_with("CLN") {
                // skip cleaning unit
                continue;
            }

            let changer_id = changer_id.to_string();

            if let Some(media_id) = inventory.find_media_by_changer_id(&changer_id) {
                list.push(LabelUuidMap { changer_id, uuid: Some(media_id.label.uuid.to_string()) });
            } else {
                list.push(LabelUuidMap { changer_id, uuid: None });
            }
        }

        Ok(list)
    }).await?
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            "read-all-labels": {
                description: "Load all tapes and try read labels (even if already inventoried)",
                type: bool,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Update inventory
///
/// Note: Only useful for drives with associated changer device.
///
/// This method queries the changer to get a list of media labels. It
/// then loads any unknown media into the drive, reads the label, and
/// store the result to the media database.
///
/// Note: This updates the media online status.
pub fn update_inventory(
    drive: String,
    read_all_labels: Option<bool>,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let (config, _digest) = config::drive::config()?;

    check_drive_exists(&config, &drive)?; // early check before starting worker

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let upid_str = WorkerTask::new_thread(
        "inventory-update",
        Some(drive.clone()),
        auth_id,
        true,
        move |worker| {

            let (mut changer, changer_name) = media_changer(&config, &drive, false)?;

            let changer_id_list = changer.list_media_changer_ids()?;
            if changer_id_list.is_empty() {
                worker.log(format!("changer device does not list any media labels"));
            }

            let state_path = Path::new(TAPE_STATUS_DIR);

            let mut inventory = Inventory::load(state_path)?;
            let mut state_db = MediaStateDatabase::load(state_path)?;

            update_changer_online_status(&config, &mut inventory, &mut state_db, &changer_name, &changer_id_list)?;

            for changer_id in changer_id_list.iter() {
                if changer_id.starts_with("CLN") {
                    worker.log(format!("skip cleaning unit '{}'", changer_id));
                    continue;
                }

                let changer_id = changer_id.to_string();

                if !read_all_labels.unwrap_or(false) {
                    if let Some(_) = inventory.find_media_by_changer_id(&changer_id) {
                        worker.log(format!("media '{}' already inventoried", changer_id));
                        continue;
                    }
                }

                if let Err(err) = changer.load_media(&changer_id) {
                    worker.warn(format!("unable to load media '{}' - {}", changer_id, err));
                    continue;
                }

                let mut drive = open_drive(&config, &drive)?;
                match drive.read_label() {
                    Err(err) => {
                        worker.warn(format!("unable to read label form media '{}' - {}", changer_id, err));
                    }
                    Ok(None) => {
                        worker.log(format!("media '{}' is empty", changer_id));
                    }
                    Ok(Some(media_id)) => {
                        if changer_id != media_id.label.changer_id {
                            worker.warn(format!("label changer ID missmatch ({} != {})", changer_id, media_id.label.changer_id));
                            continue;
                        }
                        worker.log(format!("inventorize media '{}' with uuid '{}'", changer_id, media_id.label.uuid));
                        inventory.store(media_id)?;
                    }
                }
            }
            Ok(())
        }
    )?;

    Ok(upid_str.into())
}


#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            pool: {
                schema: MEDIA_POOL_NAME_SCHEMA,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Label media with barcodes from changer device
pub fn barcode_label_media(
    drive: String,
    pool: Option<String>,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    if let Some(ref pool) = pool {
        let (pool_config, _digest) = config::media_pool::config()?;

        if pool_config.sections.get(pool).is_none() {
            bail!("no such pool ('{}')", pool);
        }
    }

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let upid_str = WorkerTask::new_thread(
        "barcode-label-media",
        Some(drive.clone()),
        auth_id,
        true,
        move |worker| {
            barcode_label_media_worker(worker, drive, pool)
        }
    )?;

    Ok(upid_str.into())
}

fn barcode_label_media_worker(
    worker: Arc<WorkerTask>,
    drive: String,
    pool: Option<String>,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    let (mut changer, changer_name) = media_changer(&config, &drive, false)?;

    let changer_id_list = changer.list_media_changer_ids()?;

    let state_path = Path::new(TAPE_STATUS_DIR);

    let mut inventory = Inventory::load(state_path)?;
    let mut state_db = MediaStateDatabase::load(state_path)?;

    update_changer_online_status(&config, &mut inventory, &mut state_db, &changer_name, &changer_id_list)?;

    if changer_id_list.is_empty() {
        bail!("changer device does not list any media labels");
    }

    for changer_id in changer_id_list {
        if changer_id.starts_with("CLN") { continue; }

        inventory.reload()?;
        if inventory.find_media_by_changer_id(&changer_id).is_some() {
            worker.log(format!("media '{}' already inventoried (already labeled)", changer_id));
            continue;
        }

        worker.log(format!("checking/loading media '{}'", changer_id));

        if let Err(err) = changer.load_media(&changer_id) {
            worker.warn(format!("unable to load media '{}' - {}", changer_id, err));
            continue;
        }

        let mut drive = open_drive(&config, &drive)?;
        drive.rewind()?;

        match drive.read_next_file() {
            Ok(Some(_file)) => {
                worker.log(format!("media '{}' is not empty (erase first)", changer_id));
                continue;
            }
            Ok(None) => { /* EOF mark at BOT, assume tape is empty */ },
            Err(err) => {
                if err.is_errno(nix::errno::Errno::ENOSPC) || err.is_errno(nix::errno::Errno::EIO) {
                    /* assume tape is empty */
                } else {
                    worker.warn(format!("media '{}' read error (maybe not empty - erase first)", changer_id));
                    continue;
                }
            }
        }

        let ctime = proxmox::tools::time::epoch_i64();
        let label = MediaLabel {
            changer_id: changer_id.to_string(),
            uuid: Uuid::generate(),
            ctime,
        };

        write_media_label(worker.clone(), &mut drive, label, pool.clone())?
    }

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
    returns: {
        description: "A List of medium auxiliary memory attributes.",
        type: Array,
        items: {
            type: MamAttribute,
        },
    },
)]
/// Read Cartridge Memory (Medium auxiliary memory attributes)
pub fn cartridge_memory(drive: String) -> Result<Vec<MamAttribute>, Error> {

    let (config, _digest) = config::drive::config()?;

    let drive_config: LinuxTapeDrive = config.lookup("linux", &drive)?;
    let mut handle = drive_config.open()
        .map_err(|err| format_err!("open drive '{}' ({}) failed - {}", drive, drive_config.path, err))?;

    handle.cartridge_memory()
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
        },
    },
    returns: {
        type: LinuxDriveAndMediaStatus,
    },
)]
/// Get drive/media status
pub fn status(drive: String) -> Result<LinuxDriveAndMediaStatus, Error> {

    let (config, _digest) = config::drive::config()?;

    let drive_config: LinuxTapeDrive = config.lookup("linux", &drive)?;

    // Note: use open_linux_tape_device, because this also works if no medium loaded
    let file = open_linux_tape_device(&drive_config.path)
        .map_err(|err| format_err!("open drive '{}' ({}) failed - {}", drive, drive_config.path, err))?;

    let mut handle = LinuxTapeHandle::new(file);

    handle.get_drive_and_media_status()
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_NAME_SCHEMA,
            },
            force: {
                description: "Force overriding existing index.",
                type: bool,
                optional: true,
            },
            verbose: {
                description: "Verbose mode - log all found chunks.",
                type: bool,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
)]
/// Scan media and record content
pub fn catalog_media(
    drive: String,
    force: Option<bool>,
    verbose: Option<bool>,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let verbose = verbose.unwrap_or(false);
    let force = force.unwrap_or(false);

    let (config, _digest) = config::drive::config()?;

    check_drive_exists(&config, &drive)?; // early check before starting worker

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let upid_str = WorkerTask::new_thread(
        "catalog-media",
        Some(drive.clone()),
        auth_id,
        true,
        move |worker| {

            let mut drive = open_drive(&config, &drive)?;

            drive.rewind()?;

            let media_id = match drive.read_label()? {
                Some(media_id) => {
                    worker.log(format!(
                        "found media label: {}",
                        serde_json::to_string_pretty(&serde_json::to_value(&media_id)?)?
                    ));
                    media_id
                },
                None => bail!("media is empty (no media label found)"),
            };

            let status_path = Path::new(TAPE_STATUS_DIR);

            let mut inventory = Inventory::load(status_path)?;
            inventory.store(media_id.clone())?;

            let pool = match media_id.media_set_label {
                None => {
                    worker.log("media is empty");
                    MediaCatalog::destroy(status_path, &media_id.label.uuid)?;
                    return Ok(());
                }
                Some(ref set) => {
                    if set.uuid.as_ref() == [0u8;16] { // media is empty
                        worker.log("media is empty");
                        MediaCatalog::destroy(status_path, &media_id.label.uuid)?;
                        return Ok(());
                    }
                    set.pool.clone()
                }
            };

            let _lock = MediaPool::lock(status_path, &pool)?;

            if MediaCatalog::exists(status_path, &media_id.label.uuid) {
                if !force {
                    bail!("media catalog exists (please use --force to overwrite)");
                }
            }

            restore_media(&worker, &mut drive, &media_id, None, verbose)?;

            Ok(())

        }
    )?;

    Ok(upid_str.into())
}

#[sortable]
pub const SUBDIRS: SubdirMap = &sorted!([
    (
        "barcode-label-media",
        &Router::new()
            .put(&API_METHOD_BARCODE_LABEL_MEDIA)
    ),
    (
        "catalog",
        &Router::new()
            .put(&API_METHOD_CATALOG_MEDIA)
    ),
    (
        "eject-media",
        &Router::new()
            .put(&API_METHOD_EJECT_MEDIA)
    ),
    (
        "erase-media",
        &Router::new()
            .put(&API_METHOD_ERASE_MEDIA)
    ),
    (
        "inventory",
        &Router::new()
            .get(&API_METHOD_INVENTORY)
            .put(&API_METHOD_UPDATE_INVENTORY)
    ),
    (
        "label-media",
        &Router::new()
            .put(&API_METHOD_LABEL_MEDIA)
    ),
    (
        "load-slot",
        &Router::new()
            .put(&API_METHOD_LOAD_SLOT)
    ),
    (
        "cartridge-memory",
        &Router::new()
            .put(&API_METHOD_CARTRIDGE_MEMORY)
    ),
    (
        "read-label",
        &Router::new()
            .get(&API_METHOD_READ_LABEL)
    ),
    (
        "rewind",
        &Router::new()
            .put(&API_METHOD_REWIND)
    ),
    (
        "scan",
        &Router::new()
            .get(&API_METHOD_SCAN_DRIVES)
    ),
    (
        "status",
        &Router::new()
            .get(&API_METHOD_STATUS)
    ),
    (
        "unload",
        &Router::new()
            .put(&API_METHOD_UNLOAD)
    ),
]);

pub const ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(SUBDIRS))
    .subdirs(SUBDIRS);
