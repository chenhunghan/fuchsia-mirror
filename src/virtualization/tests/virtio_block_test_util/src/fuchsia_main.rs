// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use block_client::{BlockClient as _, BufferSlice, MutableBufferSlice, RemoteBlockClient};
use clap::{Parser, Subcommand};
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_storage_block::{BlockMarker, BlockProxy};
use fuchsia_component::client;
use fuchsia_fs::{PERM_READABLE, directory};
use futures::StreamExt as _;

const BLOCK_PATH: &str = "/block";

#[derive(Parser, Debug)]
struct Config {
    block_size: u32,
    pci_bus: u8,
    pci_device: u8,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(name = "check")]
    Check { block_count: u64 },
    #[command(name = "read")]
    Read { offset: u64, expected: u8 },
    #[command(name = "write")]
    Write { offset: u64, value: u8 },
}

async fn find_block_by_bus_path(bus_path: &str) -> Result<BlockProxy, anyhow::Error> {
    let dir = directory::open_in_namespace(BLOCK_PATH, PERM_READABLE)?;
    let mut watcher = directory::Watcher::new(&dir).await?;
    while let Some(message) = watcher.next().await {
        let message = message?;
        match message.event {
            directory::WatchEvent::ADD_FILE | directory::WatchEvent::EXISTING => {
                let block_subdir = message.filename.to_str().unwrap();
                if block_subdir == "." {
                    continue;
                }
                let bus_path_file_path = format!("{block_subdir}/bus_path");
                if let Ok(content) = directory::read_file_to_string(&dir, &bus_path_file_path).await
                {
                    if content.trim() == bus_path {
                        let block_path =
                            format!("{BLOCK_PATH}/{block_subdir}/{}", BlockMarker::PROTOCOL_NAME);
                        return client::connect_to_protocol_at_path::<BlockMarker>(&block_path);
                    }
                }
            }
            _ => (),
        }
    }
    Err(anyhow::anyhow!("Watch stream unexpectedly ended"))
}

#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    let Config { block_size, pci_bus, pci_device, cmd } = Config::parse();

    // The bus_path will contain the BDF in the form
    // pci<bus>:<device>.<function>. The function is always zero for virtio
    // block devices.
    let bus_path = format!("pci{:02X}:{:02X}.0", pci_bus, pci_device);

    let block_proxy = find_block_by_bus_path(&bus_path).await?;
    let block_client = RemoteBlockClient::new(block_proxy).await?;

    let result = match cmd {
        Command::Check { block_count } => {
            let actual_block_size = block_client.block_size();
            let actual_block_count = block_client.block_count();
            if actual_block_size != block_size || actual_block_count != block_count {
                Err(anyhow::anyhow!(
                    "actual_block_size={} != block_size={} || actual_block_count={} != block_count={}",
                    actual_block_size,
                    block_size,
                    actual_block_count,
                    block_count,
                ))
            } else {
                Ok(())
            }
        }
        Command::Read { offset, expected } => {
            let device_offset = offset.checked_mul(block_size.into()).ok_or_else(|| {
                anyhow::anyhow!("offset={} * block_size={} overflows", offset, block_size)
            })?;
            let block_size = block_size.try_into()?;
            let mut data = {
                let mut data = Vec::new();
                let () = data.resize(block_size, !expected);
                data.into_boxed_slice()
            };
            let () = block_client
                .read_at(MutableBufferSlice::Memory(&mut (*data)[..]), device_offset)
                .await?;
            // TODO(https://github.com/rust-lang/rust/issues/59878): Box<[T]> is not IntoIter.
            let mismatches =
                data.iter().copied().enumerate().try_fold(String::new(), |mut acc, (i, b)| {
                    use std::fmt::Write as _;

                    if b != expected {
                        let () = write!(&mut acc, "\n{}:{:b}", i, b)?;
                    }
                    Ok::<_, anyhow::Error>(acc)
                })?;
            if !mismatches.is_empty() {
                Err(anyhow::anyhow!(
                    "offset={} expected={:b} mismatches={}",
                    offset,
                    expected,
                    mismatches
                ))
            } else {
                Ok(())
            }
        }
        Command::Write { offset, value } => {
            let device_offset = offset.checked_mul(block_size.into()).ok_or_else(|| {
                anyhow::anyhow!("offset={} * block_size={} overflows", offset, block_size)
            })?;
            let block_size = block_size.try_into()?;
            let data = {
                let mut data = Vec::new();
                let () = data.resize(block_size, value);
                data.into_boxed_slice()
            };
            let () =
                block_client.write_at(BufferSlice::Memory(&(*data)[..]), device_offset).await?;
            let () = block_client.flush().await?;
            Ok(())
        }
    };
    match result.as_ref() {
        Ok(()) => {
            println!("PASS")
        }
        Err(err) => {
            println!("FAIL: {:#?}", err)
        }
    }
    result
}
