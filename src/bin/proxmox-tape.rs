use anyhow::{format_err, Error};
use serde_json::{json, Value};

use proxmox::{
    api::{
        api,
        cli::*,
        ApiHandler,
        RpcEnvironment,
        section_config::SectionConfigData,
    },
};

use proxmox_backup::{
    tools::format::render_epoch,
    server::{
        UPID,
        worker_is_active_local,
    },
    api2::{
        self,
        types::{
            DRIVE_ID_SCHEMA,
            MEDIA_LABEL_SCHEMA,
            MEDIA_POOL_NAME_SCHEMA,
        },
    },
    config::{
        self,
        drive::complete_drive_name,
        media_pool::complete_pool_name,
    },
    tape::{
        complete_media_changer_id,
    },
};

mod proxmox_tape;
use proxmox_tape::*;

// Note: local workers should print logs to stdout, so there is no need
// to fetch/display logs. We just wait for the worker to finish.
pub async fn wait_for_local_worker(upid_str: &str) -> Result<(), Error> {

    let upid: UPID = upid_str.parse()?;

    let sleep_duration = core::time::Duration::new(0, 100_000_000);

    loop {
        if worker_is_active_local(&upid) {
            tokio::time::delay_for(sleep_duration).await;
        } else {
            break;
        }
    }
    Ok(())
}

fn lookup_drive_name(
    param: &Value,
    config: &SectionConfigData,
) -> Result<String, Error> {

    let drive = param["drive"]
        .as_str()
        .map(String::from)
        .or_else(|| std::env::var("PROXMOX_TAPE_DRIVE").ok())
        .or_else(||  {

            let mut drive_names = Vec::new();

            for (name, (section_type, _)) in config.sections.iter() {

                if !(section_type == "linux" || section_type == "virtual") { continue; }
                drive_names.push(name);
            }

            if drive_names.len() == 1 {
                Some(drive_names[0].to_owned())
            } else {
                None
            }
        })
        .ok_or_else(|| format_err!("unable to get (default) drive name"))?;

    Ok(drive)
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
            fast: {
                description: "Use fast erase.",
                type: bool,
                optional: true,
                default: true,
            },
        },
    },
)]
/// Erase media
fn erase_media(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_ERASE_MEDIA;

    match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
        },
    },
)]
/// Rewind tape
fn rewind(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_REWIND;

    match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
        },
    },
)]
/// Eject/Unload drive media
fn eject_media(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_EJECT_MEDIA;

    match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
            "changer-id": {
                schema: MEDIA_LABEL_SCHEMA,
            },
        },
    },
)]
/// Load media
fn load_media(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_LOAD_MEDIA;

    match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    Ok(())
}

#[api(
    input: {
        properties: {
            pool: {
                schema: MEDIA_POOL_NAME_SCHEMA,
                optional: true,
            },
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
            "changer-id": {
                schema: MEDIA_LABEL_SCHEMA,
            },
       },
    },
)]
/// Label media
async fn label_media(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_LABEL_MEDIA;

    let result = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    wait_for_local_worker(result.as_str().unwrap()).await?;

    Ok(())
}

#[api(
    input: {
        properties: {
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
             "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
             },
        },
    },
)]
/// Read media label
fn read_label(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let output_format = get_output_format(&param);
    let info = &api2::tape::drive::API_METHOD_READ_LABEL;
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("changer-id"))
        .column(ColumnConfig::new("uuid"))
        .column(ColumnConfig::new("ctime").renderer(render_epoch))
        .column(ColumnConfig::new("pool"))
        .column(ColumnConfig::new("media-set-uuid"))
        .column(ColumnConfig::new("media-set-ctime").renderer(render_epoch))
        ;

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(())
}

#[api(
    input: {
        properties: {
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
            "read-labels": {
                description: "Load unknown tapes and try read labels",
                type: bool,
                optional: true,
            },
            "read-all-labels": {
                description: "Load all tapes and try read labels (even if already inventoried)",
                type: bool,
                optional: true,
            },
        },
    },
)]
/// List (and update) media labels (Changer Inventory)
async fn inventory(
    read_labels: Option<bool>,
    read_all_labels: Option<bool>,
    param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let output_format = get_output_format(&param);

    let (config, _digest) = config::drive::config()?;
    let drive = lookup_drive_name(&param, &config)?;

    let do_read = read_labels.unwrap_or(false) || read_all_labels.unwrap_or(false);

    if do_read {
        let mut param = json!({
            "drive": &drive,
        });
        if let Some(true) = read_all_labels {
            param["read-all-labels"] = true.into();
        }
        let info = &api2::tape::drive::API_METHOD_UPDATE_INVENTORY;
        let result = match info.handler {
            ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
            _ => unreachable!(),
        };
        wait_for_local_worker(result.as_str().unwrap()).await?;
    }

    let info = &api2::tape::drive::API_METHOD_INVENTORY;

    let param = json!({ "drive": &drive });
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("changer-id"))
        .column(ColumnConfig::new("uuid"))
        ;

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(())
}

#[api(
    input: {
        properties: {
            pool: {
                schema: MEDIA_POOL_NAME_SCHEMA,
                optional: true,
            },
            drive: {
                schema: DRIVE_ID_SCHEMA,
                optional: true,
            },
        },
    },
)]
/// Label media with barcodes from changer device
async fn barcode_label_media(
    mut param: Value,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<(), Error> {

    let (config, _digest) = config::drive::config()?;

    param["drive"] = lookup_drive_name(&param, &config)?.into();

    let info = &api2::tape::drive::API_METHOD_BARCODE_LABEL_MEDIA;

    let result = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    wait_for_local_worker(result.as_str().unwrap()).await?;

    Ok(())
}

fn main() {

    let cmd_def = CliCommandMap::new()
        .insert(
            "barcode-label",
            CliCommand::new(&API_METHOD_BARCODE_LABEL_MEDIA)
                .completion_cb("drive", complete_drive_name)
                .completion_cb("pool", complete_pool_name)
        )
        .insert(
            "rewind",
            CliCommand::new(&API_METHOD_REWIND)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "erase",
            CliCommand::new(&API_METHOD_ERASE_MEDIA)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "eject",
            CliCommand::new(&API_METHOD_EJECT_MEDIA)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "inventory",
            CliCommand::new(&API_METHOD_INVENTORY)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "read-label",
            CliCommand::new(&API_METHOD_READ_LABEL)
                .completion_cb("drive", complete_drive_name)
        )
        .insert(
            "label",
            CliCommand::new(&API_METHOD_LABEL_MEDIA)
                .completion_cb("drive", complete_drive_name)
                .completion_cb("pool", complete_pool_name)

        )
        .insert("changer", changer_commands())
        .insert("drive", drive_commands())
        .insert("pool", pool_commands())
        .insert(
            "load-media",
            CliCommand::new(&API_METHOD_LOAD_MEDIA)
                .arg_param(&["changer-id"])
                .completion_cb("drive", complete_drive_name)
                .completion_cb("changer-id", complete_media_changer_id)
        )
        ;

    let mut rpcenv = CliEnvironment::new();
    rpcenv.set_auth_id(Some(String::from("root@pam")));

    proxmox_backup::tools::runtime::main(run_async_cli_command(cmd_def, rpcenv));
}
