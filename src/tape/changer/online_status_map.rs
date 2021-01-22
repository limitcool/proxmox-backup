use std::path::Path;
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Error};

use proxmox::tools::Uuid;
use proxmox::api::section_config::SectionConfigData;

use crate::{
    api2::types::{
        VirtualTapeDrive,
        ScsiTapeChanger,
    },
    tape::{
        Inventory,
        changer::{
            MediaChange,
            MtxStatus,
            ElementStatus,
            mtx_status,
        },
    },
};

/// Helper to update media online status
///
/// A tape media is considered online if it is accessible by a changer
/// device. This class can store the list of available changes,
/// together with the accessible media ids.
pub struct OnlineStatusMap {
    map: HashMap<String, Option<HashSet<Uuid>>>,
    changer_map: HashMap<Uuid, String>,
}

impl OnlineStatusMap {

    /// Creates a new instance with one map entry for each configured
    /// changer (or 'VirtualTapeDrive', which has an internal
    /// changer). The map entry is set to 'None' to indicate that we
    /// do not have information about the online status.
    pub fn new(config: &SectionConfigData) -> Result<Self, Error> {

        let mut map = HashMap::new();

        let changers: Vec<ScsiTapeChanger> = config.convert_to_typed_array("changer")?;
        for changer in changers {
            map.insert(changer.name.clone(), None);
        }

        let vtapes: Vec<VirtualTapeDrive> = config.convert_to_typed_array("virtual")?;
        for vtape in vtapes {
            map.insert(vtape.name.clone(), None);
        }

        Ok(Self { map, changer_map: HashMap::new() })
    }

    /// Returns the assiciated changer name for a media.
    pub fn lookup_changer(&self, uuid: &Uuid) -> Option<&String> {
        self.changer_map.get(uuid)
    }

    /// Returns the map which assiciates media uuids with changer names.
    pub fn changer_map(&self) -> &HashMap<Uuid, String> {
        &self.changer_map
    }

    /// Returns the set of online media for the specified changer.
    pub fn online_map(&self, changer_name: &str) -> Option<&Option<HashSet<Uuid>>> {
        self.map.get(changer_name)
    }

    /// Update the online set for the specified changer
    pub fn update_online_status(&mut self, changer_name: &str, online_set: HashSet<Uuid>) -> Result<(), Error> {

        match self.map.get(changer_name) {
            None => bail!("no such changer '{}' device", changer_name),
            Some(None) => { /* Ok */ },
            Some(Some(_)) => {
                // do not allow updates to keep self.changer_map consistent
                bail!("update_online_status '{}' called twice", changer_name);
            }
        }

        for uuid in online_set.iter() {
            self.changer_map.insert(uuid.clone(), changer_name.to_string());
        }

        self.map.insert(changer_name.to_string(), Some(online_set));

        Ok(())
    }
}

/// Extract the list of online media from MtxStatus
///
/// Returns a HashSet containing all found media Uuid. This only
/// returns media found in Inventory.
pub fn mtx_status_to_online_set(status: &MtxStatus, inventory: &Inventory) -> HashSet<Uuid> {

    let mut online_set = HashSet::new();

    for drive_status in status.drives.iter() {
        if let ElementStatus::VolumeTag(ref label_text) = drive_status.status {
            if let Some(media_id) = inventory.find_media_by_label_text(label_text) {
                online_set.insert(media_id.label.uuid.clone());
            }
        }
    }

    for (import_export, slot_status) in status.slots.iter() {
        if *import_export { continue; }
        if let ElementStatus::VolumeTag(ref label_text) = slot_status {
            if let Some(media_id) = inventory.find_media_by_label_text(label_text) {
                online_set.insert(media_id.label.uuid.clone());
            }
        }
    }

    online_set
}

/// Update online media status
///
/// Simply ask all changer devices.
pub fn update_online_status(state_path: &Path) -> Result<OnlineStatusMap, Error> {

    let (config, _digest) = crate::config::drive::config()?;

    let mut inventory = Inventory::load(state_path)?;

    let changers: Vec<ScsiTapeChanger> = config.convert_to_typed_array("changer")?;

    let mut map = OnlineStatusMap::new(&config)?;

    for changer in changers {
        let status = match mtx_status(&changer) {
            Ok(status) => status,
            Err(err) => {
                eprintln!("unable to get changer '{}' status - {}", changer.name, err);
                continue;
            }
        };

        let online_set = mtx_status_to_online_set(&status, &inventory);
        map.update_online_status(&changer.name, online_set)?;
    }

    let vtapes: Vec<VirtualTapeDrive> = config.convert_to_typed_array("virtual")?;
    for mut vtape in vtapes {
        let media_list = match vtape.online_media_label_texts() {
            Ok(media_list) => media_list,
            Err(err) => {
                eprintln!("unable to get changer '{}' status - {}", vtape.name, err);
                continue;
            }
        };

        let mut online_set = HashSet::new();
        for label_text in media_list {
            if let Some(media_id) = inventory.find_media_by_label_text(&label_text) {
                online_set.insert(media_id.label.uuid.clone());
            }
        }
        map.update_online_status(&vtape.name, online_set)?;
    }

    inventory.update_online_status(&map)?;

    Ok(map)
}

/// Update online media status with data from a single changer device
pub fn update_changer_online_status(
    drive_config: &SectionConfigData,
    inventory: &mut Inventory,
    changer_name: &str,
    label_text_list: &[String],
) -> Result<(), Error> {

    let mut online_map = OnlineStatusMap::new(drive_config)?;
    let mut online_set = HashSet::new();
    for label_text in label_text_list.iter() {
        if let Some(media_id) = inventory.find_media_by_label_text(&label_text) {
            online_set.insert(media_id.label.uuid.clone());
        }
    }
    online_map.update_online_status(&changer_name, online_set)?;
    inventory.update_online_status(&online_map)?;

    Ok(())
}