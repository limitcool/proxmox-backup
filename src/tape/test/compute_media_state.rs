// Tape Media Pool tests - test compute_media_state() function
//
// # cargo test --release tape::test::compute_media_state

use std::path::PathBuf;
use anyhow::Error;

use proxmox::tools::{
    Uuid,
};

use crate::{
    api2::types::{
        MediaStatus,
        MediaSetPolicy,
        RetentionPolicy,
    },
    tape::{
        Inventory,
        MediaPool,
        file_formats::{
            MediaSetLabel,
        },
    },
};

fn create_testdir(name: &str) -> Result<PathBuf, Error> {
    let mut testdir: PathBuf = String::from("./target/testout").into();
    testdir.push(std::module_path!());
    testdir.push(name);

    let _ = std::fs::remove_dir_all(&testdir);
    let _ = std::fs::create_dir_all(&testdir);

    Ok(testdir)
}

#[test]
fn test_compute_media_state() -> Result<(), Error> {

    let testdir = create_testdir("test_compute_media_state")?;

    let ctime = 0;

    let mut inventory = Inventory::load(&testdir)?;

    // tape1: free, assigned to pool
    let tape1_uuid = inventory.generate_assigned_tape("tape1", "p1", ctime);

    // tape2: single tape media set
    let sl2 = MediaSetLabel::with_data("p1", Uuid::generate(), 0, ctime + 10, None);
    let tape2_uuid = inventory.generate_used_tape("tape2", sl2, 0);

    // tape3: inclomplete two tape media set
    let sl3 = MediaSetLabel::with_data("p1", Uuid::generate(), 0, ctime + 20, None);
    let tape3_uuid = inventory.generate_used_tape("tape3", sl3, 0);

    // tape4,tape5: current_set: complete two tape media set
    let sl4 = MediaSetLabel::with_data("p1", Uuid::generate(), 0, ctime + 30, None);
    let sl5 = MediaSetLabel::with_data("p1", sl4.uuid.clone(), 1, ctime + 35, None);

    let tape4_uuid = inventory.generate_used_tape("tape4", sl4, 0);
    let tape5_uuid = inventory.generate_used_tape("tape5", sl5, 0);

     let pool = MediaPool::new(
        "p1",
         &testdir ,
         MediaSetPolicy::AlwaysCreate,
         RetentionPolicy::KeepForever,
         None,
         None,
    )?;

    // tape1 is free
    assert_eq!(pool.lookup_media(&tape1_uuid)?.status(), &MediaStatus::Writable);

    // intermediate tapes should be Full
    assert_eq!(pool.lookup_media(&tape2_uuid)?.status(), &MediaStatus::Full);
    assert_eq!(pool.lookup_media(&tape3_uuid)?.status(), &MediaStatus::Full);
    assert_eq!(pool.lookup_media(&tape4_uuid)?.status(), &MediaStatus::Full);

    // last tape is writable
    assert_eq!(pool.lookup_media(&tape5_uuid)?.status(), &MediaStatus::Writable);

    Ok(())
}
