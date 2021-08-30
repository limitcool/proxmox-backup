use std::path::Path;

use anyhow::{format_err, Error};
use proxmox::api::cli::{
    format_and_print_result, get_output_format, CliCommand, CliCommandMap, CommandLineInterface,
};
use proxmox::api::{api, cli::*};
use serde_json::{json, Value};
use walkdir::WalkDir;

use proxmox_backup::backup::{
    load_and_decrypt_key, CryptConfig, DataBlob, DynamicIndexReader, FixedIndexReader, IndexFile,
};

use pbs_client::tools::key_source::get_encryption_key_password;

use proxmox_backup::tools::outfile_or_stdout;

/// Decodes a blob and writes its content either to stdout or into a file
fn decode_blob(
    mut output_path: Option<&Path>,
    key_file: Option<&Path>,
    digest: Option<&[u8; 32]>,
    blob: &DataBlob,
) -> Result<(), Error> {
    let mut crypt_conf_opt = None;
    let crypt_conf;

    if blob.is_encrypted() && key_file.is_some() {
        let (key, _created, _fingerprint) =
            load_and_decrypt_key(&key_file.unwrap(), &get_encryption_key_password)?;
        crypt_conf = CryptConfig::new(key)?;
        crypt_conf_opt = Some(&crypt_conf);
    }

    output_path = match output_path {
        Some(path) if path.eq(Path::new("-")) => None,
        _ => output_path,
    };

    outfile_or_stdout(output_path)?.write_all(blob.decode(crypt_conf_opt, digest)?.as_slice())?;
    Ok(())
}

#[api(
    input: {
        properties: {
            chunk: {
                description: "The chunk file.",
                type: String,
            },
            "reference-filter": {
                description: "Path to the directory that should be searched for references.",
                type: String,
                optional: true,
            },
            "digest": {
                description: "Needed when searching for references, if set, it will be used for verification when decoding.",
                type: String,
                optional: true,
            },
            "decode": {
                description: "Path to the file to which the chunk should be decoded, '-' -> decode to stdout.",
                type: String,
                optional: true,
            },
            "keyfile": {
                description: "Path to the keyfile with which the chunk was encrypted.",
                type: String,
                optional: true,
            },
            "use-filename-as-digest": {
                description: "The filename should be used as digest for reference search and decode verification, if no digest is specified.",
                type: bool,
                optional: true,
                default: true,
            },
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
        }
    }
)]
/// Inspect a chunk
fn inspect_chunk(
    chunk: String,
    reference_filter: Option<String>,
    mut digest: Option<String>,
    decode: Option<String>,
    keyfile: Option<String>,
    use_filename_as_digest: bool,
    param: Value,
) -> Result<(), Error> {
    let output_format = get_output_format(&param);
    let chunk_path = Path::new(&chunk);

    if digest.is_none() && use_filename_as_digest {
        digest = Some(if let Some((_, filename)) = chunk.rsplit_once("/") {
            String::from(filename)
        } else {
            chunk.clone()
        });
    };

    let digest_raw: Option<[u8; 32]> = digest
        .map(|ref d| {
            proxmox::tools::hex_to_digest(d)
                .map_err(|e| format_err!("could not parse chunk - {}", e))
        })
        .map_or(Ok(None), |r| r.map(Some))?;

    let search_path = reference_filter.as_ref().map(Path::new);
    let key_file_path = keyfile.as_ref().map(Path::new);
    let decode_output_path = decode.as_ref().map(Path::new);

    let blob = DataBlob::load_from_reader(
        &mut std::fs::File::open(&chunk_path)
            .map_err(|e| format_err!("could not open chunk file - {}", e))?,
    )?;

    let referenced_by = if let (Some(search_path), Some(digest_raw)) = (search_path, digest_raw) {
        let mut references = Vec::new();
        for entry in WalkDir::new(search_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            use std::os::unix::ffi::OsStrExt;
            let file_name = entry.file_name().as_bytes();

            let index: Box<dyn IndexFile> = if file_name.ends_with(b".fidx") {
                match FixedIndexReader::open(entry.path()) {
                    Ok(index) => Box::new(index),
                    Err(_) => continue,
                }
            } else if file_name.ends_with(b".didx") {
                match DynamicIndexReader::open(entry.path()) {
                    Ok(index) => Box::new(index),
                    Err(_) => continue,
                }
            } else {
                continue;
            };

            for pos in 0..index.index_count() {
                if let Some(index_chunk_digest) = index.index_digest(pos) {
                    if digest_raw.eq(index_chunk_digest) {
                        references.push(entry.path().to_string_lossy().into_owned());
                        break;
                    }
                }
            }
        }
        if !references.is_empty() {
            Some(references)
        } else {
            None
        }
    } else {
        None
    };

    if decode_output_path.is_some() {
        decode_blob(
            decode_output_path,
            key_file_path,
            digest_raw.as_ref(),
            &blob,
        )?;
    }

    let crc_status = format!(
        "{}({})",
        blob.compute_crc(),
        blob.verify_crc().map_or("BAD", |_| "OK")
    );

    let val = match referenced_by {
        Some(references) => json!({
            "crc": crc_status,
            "encryption": blob.crypt_mode()?,
            "referenced-by": references
        }),
        None => json!({
             "crc": crc_status,
             "encryption": blob.crypt_mode()?,
        }),
    };

    if output_format == "text" {
        println!("CRC: {}", val["crc"]);
        println!("encryption: {}", val["encryption"]);
        if let Some(refs) = val["referenced-by"].as_array() {
            println!("referenced by:");
            for reference in refs {
                println!("  {}", reference);
            }
        }
    } else {
        format_and_print_result(&val, &output_format);
    }
    Ok(())
}

pub fn inspect_commands() -> CommandLineInterface {
    let cmd_def = CliCommandMap::new().insert(
        "chunk",
        CliCommand::new(&API_METHOD_INSPECT_CHUNK).arg_param(&["chunk"]),
    );

    cmd_def.into()
}
