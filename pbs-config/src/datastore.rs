use anyhow::{Error};
use lazy_static::lazy_static;
use std::collections::HashMap;

use proxmox::api::{
    schema::{ApiType, Schema},
    section_config::{
        SectionConfig,
        SectionConfigData,
        SectionConfigPlugin,
    }
};

use pbs_api_types::{DataStoreConfig, DATASTORE_SCHEMA};

use crate::{open_backup_lockfile, replace_backup_config, BackupLockGuard};

lazy_static! {
    pub static ref CONFIG: SectionConfig = init();
}

fn init() -> SectionConfig {
    let obj_schema = match DataStoreConfig::API_SCHEMA {
        Schema::Object(ref obj_schema) => obj_schema,
        _ => unreachable!(),
    };

    let plugin = SectionConfigPlugin::new("datastore".to_string(), Some(String::from("name")), obj_schema);
    let mut config = SectionConfig::new(&DATASTORE_SCHEMA);
    config.register_plugin(plugin);

    config
}

pub const DATASTORE_CFG_FILENAME: &str = "/etc/proxmox-backup/datastore.cfg";
pub const DATASTORE_CFG_LOCKFILE: &str = "/etc/proxmox-backup/.datastore.lck";

/// Get exclusive lock
pub fn lock_config() -> Result<BackupLockGuard, Error> {
    open_backup_lockfile(DATASTORE_CFG_LOCKFILE, None, true)
}

pub fn config() -> Result<(SectionConfigData, [u8;32]), Error> {

    let content = proxmox::tools::fs::file_read_optional_string(DATASTORE_CFG_FILENAME)?
        .unwrap_or_else(|| "".to_string());

    let digest = openssl::sha::sha256(content.as_bytes());
    let data = CONFIG.parse(DATASTORE_CFG_FILENAME, &content)?;
    Ok((data, digest))
}

pub fn save_config(config: &SectionConfigData) -> Result<(), Error> {
    let raw = CONFIG.write(DATASTORE_CFG_FILENAME, &config)?;
    replace_backup_config(DATASTORE_CFG_FILENAME, raw.as_bytes())
}

// shell completion helper
pub fn complete_datastore_name(_arg: &str, _param: &HashMap<String, String>) -> Vec<String> {
    match config() {
        Ok((data, _digest)) => data.sections.iter().map(|(id, _)| id.to_string()).collect(),
        Err(_) => return vec![],
    }
}

pub fn complete_acl_path(_arg: &str, _param: &HashMap<String, String>) -> Vec<String> {
    let mut list = Vec::new();

    list.push(String::from("/"));
    list.push(String::from("/datastore"));
    list.push(String::from("/datastore/"));

    if let Ok((data, _digest)) = config() {
        for id in data.sections.keys() {
            list.push(format!("/datastore/{}", id));
        }
    }

    list.push(String::from("/remote"));
    list.push(String::from("/remote/"));

    list.push(String::from("/tape"));
    list.push(String::from("/tape/"));
    list.push(String::from("/tape/drive"));
    list.push(String::from("/tape/drive/"));
    list.push(String::from("/tape/changer"));
    list.push(String::from("/tape/changer/"));
    list.push(String::from("/tape/pool"));
    list.push(String::from("/tape/pool/"));
    list.push(String::from("/tape/job"));
    list.push(String::from("/tape/job/"));

    list
}

pub fn complete_calendar_event(_arg: &str, _param: &HashMap<String, String>) -> Vec<String> {
    // just give some hints about possible values
    ["minutely", "hourly", "daily", "mon..fri", "0:0"]
        .iter().map(|s| String::from(*s)).collect()
}