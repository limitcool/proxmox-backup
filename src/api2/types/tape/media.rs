use ::serde::{Deserialize, Serialize};

use proxmox::api::api;

use super::{
    MediaStatus,
};

#[api()]
#[derive(Serialize,Deserialize)]
#[serde(rename_all = "lowercase")]
/// Media location
pub enum MediaLocationKind {
    /// Ready for use (inside tape library)
    Online,
    /// Local available, but need to be mounted (insert into tape
    /// drive)
    Offline,
    /// Media is inside a Vault
    Vault,
}

#[api(
    properties: {
        location: {
            type: MediaLocationKind,
        },
        status: {
            type: MediaStatus,
        },
    },
)]
#[derive(Serialize,Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Media list entry
pub struct MediaListEntry {
    /// Media changer ID
    pub changer_id: String,
    /// Media Uuid
    pub uuid: String,
    pub location: MediaLocationKind,
    /// Media location hint (vault name, changer name)
    pub location_hint: Option<String>,
    pub status: MediaStatus,
    /// Expired flag
    pub expired: bool,
    /// Media set name
    #[serde(skip_serializing_if="Option::is_none")]
    pub media_set_name: Option<String>,
    /// Media set uuid
    #[serde(skip_serializing_if="Option::is_none")]
    pub media_set_uuid: Option<String>,
    /// Media set seq_nr
    #[serde(skip_serializing_if="Option::is_none")]
    pub seq_nr: Option<u64>,
    /// Media Pool
    #[serde(skip_serializing_if="Option::is_none")]
    pub pool: Option<String>,
}

#[api()]
#[derive(Serialize,Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Media label info
pub struct MediaLabelInfoFlat {
    /// Unique ID
    pub uuid: String,
    /// Media Changer ID or Barcode
    pub changer_id: String,
    /// Creation time stamp
    pub ctime: i64,
    // All MediaSet properties are optional here
    /// MediaSet Pool
    #[serde(skip_serializing_if="Option::is_none")]
    pub pool: Option<String>,
    /// MediaSet Uuid. We use the all-zero Uuid to reseve an empty media for a specific pool
    #[serde(skip_serializing_if="Option::is_none")]
    pub media_set_uuid: Option<String>,
    /// MediaSet media sequence number
    #[serde(skip_serializing_if="Option::is_none")]
    pub seq_nr: Option<u64>,
    /// MediaSet Creation time stamp
    #[serde(skip_serializing_if="Option::is_none")]
    pub media_set_ctime: Option<i64>,
}

#[api()]
#[derive(Serialize,Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Label with optional Uuid
pub struct LabelUuidMap {
    /// Changer ID (label)
    pub changer_id: String,
    /// Associated Uuid (if any)
    pub uuid: Option<String>,
}