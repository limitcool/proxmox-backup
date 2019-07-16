extern crate proxmox_backup;

use failure::*;

use proxmox_backup::tools;
use proxmox_backup::cli::*;
use proxmox_backup::api_schema::*;
use proxmox_backup::api_schema::router::*;

use serde_json::{Value};

use std::io::Write;
use std::path::{Path, PathBuf};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use proxmox_backup::pxar;

fn dump_archive_from_reader<R: std::io::Read>(
    reader: &mut R,
    feature_flags: u64,
    verbose: bool,
) -> Result<(), Error> {
    let mut decoder = pxar::SequentialDecoder::new(reader, feature_flags, |_| Ok(()));

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut path = PathBuf::new();
    decoder.dump_entry(&mut path, verbose, &mut out)?;

    Ok(())
}

fn dump_archive(
    param: Value,
    _info: &ApiMethod,
    _rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let archive = tools::required_string_param(&param, "archive")?;
    let verbose = param["verbose"].as_bool().unwrap_or(false);

    let feature_flags = pxar::CA_FORMAT_DEFAULT;

    if archive == "-" {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        dump_archive_from_reader(&mut reader, feature_flags, verbose)?;
    } else {
        if verbose { println!("PXAR dump: {}", archive); }
        let file = std::fs::File::open(archive)?;
        let mut reader = std::io::BufReader::new(file);
        dump_archive_from_reader(&mut reader, feature_flags, verbose)?;
    }

    Ok(Value::Null)
}

fn extract_archive_from_reader<R: std::io::Read>(
    reader: &mut R,
    target: &str,
    feature_flags: u64,
    verbose: bool,
    pattern: Option<Vec<pxar::PxarExcludePattern>>
) -> Result<(), Error> {
    let mut decoder = pxar::SequentialDecoder::new(reader, feature_flags, |path| {
        if verbose {
            println!("{:?}", path);
        }
        Ok(())
    });

    let pattern = pattern.unwrap_or(Vec::new());
    decoder.restore(Path::new(target), &pattern)?;

    Ok(())
}

fn extract_archive(
    param: Value,
    _info: &ApiMethod,
    _rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let archive = tools::required_string_param(&param, "archive")?;
    let target = param["target"].as_str().unwrap_or(".");
    let verbose = param["verbose"].as_bool().unwrap_or(false);
    let no_xattrs = param["no-xattrs"].as_bool().unwrap_or(false);
    let no_fcaps = param["no-fcaps"].as_bool().unwrap_or(false);
    let no_acls = param["no-acls"].as_bool().unwrap_or(false);
    let files_from = param["files-from"].as_str();

    let mut feature_flags = pxar::CA_FORMAT_DEFAULT;
    if no_xattrs {
        feature_flags ^= pxar::CA_FORMAT_WITH_XATTRS;
    }
    if no_fcaps {
        feature_flags ^= pxar::CA_FORMAT_WITH_FCAPS;
    }
    if no_acls {
        feature_flags ^= pxar::CA_FORMAT_WITH_ACL;
    }

    let pattern = match files_from {
        Some(filename) =>  {
            let dir = nix::dir::Dir::open("./", nix::fcntl::OFlag::O_RDONLY, nix::sys::stat::Mode::empty())?;
            let fd = dir.as_raw_fd();

            pxar::PxarExcludePattern::from_file(fd, filename)?
                .and_then(|(pattern, _, _)| Some(pattern))
        },
        None =>  None,
    };

    if archive == "-" {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        extract_archive_from_reader(&mut reader, target, feature_flags, verbose, pattern)?;
    } else {
        println!("PXAR dump: {}", archive);
        let file = std::fs::File::open(archive)?;
        let mut reader = std::io::BufReader::new(file);
        extract_archive_from_reader(&mut reader, target, feature_flags, verbose, pattern)?;
    }

    Ok(Value::Null)
}

fn create_archive(
    param: Value,
    _info: &ApiMethod,
    _rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let archive = tools::required_string_param(&param, "archive")?;
    let source = tools::required_string_param(&param, "source")?;
    let verbose = param["verbose"].as_bool().unwrap_or(false);
    let all_file_systems = param["all-file-systems"].as_bool().unwrap_or(false);
    let no_xattrs = param["no-xattrs"].as_bool().unwrap_or(false);
    let no_fcaps = param["no-fcaps"].as_bool().unwrap_or(false);
    let no_acls = param["no-acls"].as_bool().unwrap_or(false);

    let source = PathBuf::from(source);

    let mut dir = nix::dir::Dir::open(
        &source, nix::fcntl::OFlag::O_NOFOLLOW, nix::sys::stat::Mode::empty())?;

    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o640)
        .open(archive)?;

    let mut writer = std::io::BufWriter::with_capacity(1024*1024, file);
    let mut feature_flags = pxar::CA_FORMAT_DEFAULT;
    if no_xattrs {
        feature_flags ^= pxar::CA_FORMAT_WITH_XATTRS;
    }
    if no_fcaps {
        feature_flags ^= pxar::CA_FORMAT_WITH_FCAPS;
    }
    if no_acls {
        feature_flags ^= pxar::CA_FORMAT_WITH_ACL;
    }

    pxar::Encoder::encode(source, &mut dir, &mut writer, all_file_systems, verbose, feature_flags)?;

    writer.flush()?;

    Ok(Value::Null)
}

fn main() {

    let cmd_def = CliCommandMap::new()
        .insert("create", CliCommand::new(
            ApiMethod::new(
                create_archive,
                ObjectSchema::new("Create new .pxar archive.")
                    .required("archive", StringSchema::new("Archive name"))
                    .required("source", StringSchema::new("Source directory."))
                    .optional("verbose", BooleanSchema::new("Verbose output.").default(false))
                    .optional("no-xattrs", BooleanSchema::new("Ignore extended file attributes.").default(false))
                    .optional("no-fcaps", BooleanSchema::new("Ignore file capabilities.").default(false))
                    .optional("no-acls", BooleanSchema::new("Ignore access control list entries.").default(false))
                    .optional("all-file-systems", BooleanSchema::new("Include mounted sudirs.").default(false))
           ))
            .arg_param(vec!["archive", "source"])
            .completion_cb("archive", tools::complete_file_name)
            .completion_cb("source", tools::complete_file_name)
           .into()
        )
        .insert("extract", CliCommand::new(
            ApiMethod::new(
                extract_archive,
                ObjectSchema::new("Extract an archive.")
                    .required("archive", StringSchema::new("Archive name."))
                    .optional("target", StringSchema::new("Target directory."))
                    .optional("verbose", BooleanSchema::new("Verbose output.").default(false))
                    .optional("no-xattrs", BooleanSchema::new("Ignore extended file attributes.").default(false))
                    .optional("no-fcaps", BooleanSchema::new("Ignore file capabilities.").default(false))
                    .optional("no-acls", BooleanSchema::new("Ignore access control list entries.").default(false))
                    .optional("files-from", StringSchema::new("Match pattern for files to restore."))
          ))
            .arg_param(vec!["archive"])
            .completion_cb("archive", tools::complete_file_name)
            .completion_cb("target", tools::complete_file_name)
            .completion_cb("files-from", tools::complete_file_name)
            .into()
        )
        .insert("list", CliCommand::new(
            ApiMethod::new(
                dump_archive,
                ObjectSchema::new("List the contents of an archive.")
                    .required("archive", StringSchema::new("Archive name."))
                    .optional("verbose", BooleanSchema::new("Verbose output.").default(false))
            ))
            .arg_param(vec!["archive"])
            .completion_cb("archive", tools::complete_file_name)
            .into()
        );

    run_cli_command(cmd_def.into());
}
