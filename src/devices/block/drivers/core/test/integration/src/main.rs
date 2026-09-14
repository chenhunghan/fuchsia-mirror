// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use block_client::{BlockClient, BufferSlice, MutableBufferSlice, RemoteBlockClient};
use fidl_fuchsia_storage_block as fblock;
use ramdevice_client::RamdiskClient;

#[fuchsia::test]
async fn test_multiple_sessions() {
    let ramdisk = RamdiskClient::create(512, 1 << 16).await.unwrap();
    let device_channel = ramdisk.open().expect("open failed");
    let device_proxy = device_channel.into_proxy();
    let block_client1 = RemoteBlockClient::new(device_proxy).await.expect("new failed");
    let device_channel = ramdisk.open().expect("open failed");
    let device_proxy = device_channel.into_proxy();
    let block_client2 = RemoteBlockClient::new(device_proxy).await.expect("new failed");

    let data1 = [1; 512];
    block_client1.write_at(BufferSlice::Memory(&data1), 1024).await.expect("write_at failed");

    let mut data2 = [0; 512];
    block_client2
        .read_at(MutableBufferSlice::Memory(&mut data2), 1024)
        .await
        .expect("read_at failed");

    assert_eq!(data1, data2);
}

#[fuchsia::test]
async fn test_multiple_mappings_session() {
    let ramdisk = RamdiskClient::create(512, 1 << 16).await.unwrap();
    let device_channel = ramdisk.open().expect("open failed");
    let remote = device_channel.into_proxy();
    let info = remote.get_info().await.unwrap().unwrap();
    let (session, server) = fidl::endpoints::create_proxy();
    remote
        .open_session_with_options(
            server,
            &[
                fblock::BlockOffsetMapping { target_block_offset: 10, length: 10 },
                fblock::BlockOffsetMapping { target_block_offset: 30, length: 10 },
            ],
        )
        .expect("open_session_with_options failed");
    let mapped_client = RemoteBlockClient::from_session(info, session).await.unwrap();

    let device_channel2 = ramdisk.open().expect("open failed");
    let raw_client = RemoteBlockClient::new(device_channel2.into_proxy()).await.unwrap();

    // Write 512 bytes (1 block) at logical offset 5 blocks (5 * 512 = 2560 bytes) in mapped_client.
    // Since mapping 0 is target block 10, logical block 5 corresponds to physical block 15 (15 * 512 = 7680 bytes).
    let data1 = [42u8; 512];
    mapped_client.write_at(BufferSlice::Memory(&data1), 5 * 512).await.expect("write_at failed");

    let mut data2 = [0u8; 512];
    raw_client
        .read_at(MutableBufferSlice::Memory(&mut data2), 15 * 512)
        .await
        .expect("read_at failed");
    assert_eq!(data1, data2);

    // Write 512 bytes (1 block) at logical offset 12 blocks (12 * 512 = 6144 bytes) in mapped_client.
    // Since mapping 0 has length 10, logical block 12 is offset 2 into mapping 1 (target 30).
    // So physical block is 30 + 2 = 32 (32 * 512 = 16384 bytes).
    let data3 = [99u8; 512];
    mapped_client.write_at(BufferSlice::Memory(&data3), 12 * 512).await.expect("write_at failed");

    let mut data4 = [0u8; 512];
    raw_client
        .read_at(MutableBufferSlice::Memory(&mut data4), 32 * 512)
        .await
        .expect("read_at failed");
    assert_eq!(data3, data4);
}
