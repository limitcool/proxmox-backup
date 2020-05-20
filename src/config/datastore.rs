use anyhow::{bail, Error};
use lazy_static::lazy_static;
use std::collections::HashMap;
use serde::{Serialize, Deserialize};

use proxmox::api::{
    api,
    schema::*,
    section_config::{
        SectionConfig,
        SectionConfigData,
        SectionConfigPlugin,
    }
};

use proxmox::tools::{fs::replace_file, fs::CreateOptions};

use crate::api2::types::*;

lazy_static! {
    static ref CONFIG: SectionConfig = init();
}

// fixme: define better schemas
pub const DIR_NAME_SCHEMA: Schema = StringSchema::new("Directory name").schema();

#[api(
    properties: {
        comment: {
            optional: true,
            schema: SINGLE_LINE_COMMENT_SCHEMA,
        },
        "gc-schedule": {
            schema: GC_SCHEDULE_SCHEMA,
            optional: true,
        },
        path: {
            schema: DIR_NAME_SCHEMA,
        },
    }
)]
#[serde(rename_all="kebab-case")]
#[derive(Serialize,Deserialize)]
/// Datastore configuration properties.
pub struct DataStoreConfig {
    #[serde(skip_serializing_if="Option::is_none")]
    pub comment: Option<String>,
    pub path: String,
    #[serde(skip_serializing_if="Option::is_none")]
    pub gc_schedule: Option<String>,
 }

fn init() -> SectionConfig {
    let obj_schema = match DataStoreConfig::API_SCHEMA {
        Schema::Object(ref obj_schema) => obj_schema,
        _ => unreachable!(),
    };

    let plugin = SectionConfigPlugin::new("datastore".to_string(), None, obj_schema);
    let mut config = SectionConfig::new(&DATASTORE_SCHEMA);
    config.register_plugin(plugin);

    config
}

pub const DATASTORE_CFG_FILENAME: &str = "/etc/proxmox-backup/datastore.cfg";
pub const DATASTORE_CFG_LOCKFILE: &str = "/etc/proxmox-backup/.datastore.lck";

pub fn config() -> Result<(SectionConfigData, [u8;32]), Error> {
    let content = match std::fs::read_to_string(DATASTORE_CFG_FILENAME) {
        Ok(c) => c,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                String::from("")
            } else {
                bail!("unable to read '{}' - {}", DATASTORE_CFG_FILENAME, err);
            }
        }
    };

    let digest = openssl::sha::sha256(content.as_bytes());
    let data = CONFIG.parse(DATASTORE_CFG_FILENAME, &content)?;
    Ok((data, digest))
}

pub fn save_config(config: &SectionConfigData) -> Result<(), Error> {
    let raw = CONFIG.write(DATASTORE_CFG_FILENAME, &config)?;

    let backup_user = crate::backup::backup_user()?;
    let mode = nix::sys::stat::Mode::from_bits_truncate(0o0640);
    // set the correct owner/group/permissions while saving file
    // owner(rw) = root, group(r)= backup
    let options = CreateOptions::new()
        .perm(mode)
        .owner(nix::unistd::ROOT)
        .group(backup_user.gid);

    replace_file(DATASTORE_CFG_FILENAME, raw.as_bytes(), options)?;

    Ok(())
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
    list.push(String::from("/storage"));
    list.push(String::from("/storage/"));

    if let Ok((data, _digest)) = config() {
        for id in data.sections.keys() {
            list.push(format!("/storage/{}", id));
        }
    }

    list
}
