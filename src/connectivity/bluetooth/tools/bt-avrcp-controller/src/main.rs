// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use argh::FromArgs;
use bt_repl::{CommandSet as _, ReplControl, ReplHandler};
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_bluetooth_avrcp::{
    self as fidl_avrcp, AttributeRequestOption, BrowseControllerMarker, BrowseControllerProxy,
    ControllerEvent, ControllerEventStream, ControllerMarker, ControllerProxy, MAX_ATTRIBUTES,
    MediaAttributeId, Notifications, PeerManagerMarker, PlayerApplicationSettingAttributeId,
};
use fidl_fuchsia_bluetooth_avrcp_test::{
    BrowseControllerExtMarker, BrowseControllerExtProxy, ControllerExtMarker, ControllerExtProxy,
    PeerManagerExtMarker,
};
use fuchsia_bluetooth::types::PeerId;
use fuchsia_component::client::connect_to_protocol;
use futures::{FutureExt, TryStreamExt, select};
use hex::FromHex;
use std::pin::pin;
use std::str::FromStr as _;

use crate::commands::{Cmd, avc_keys_help, avc_match_string, command_source};

mod commands;

static PROMPT: &str = "\x1b[34mavrcp>\x1b[0m ";
/// Escape code to clear the pty line on which the cursor is located.
/// Used when evented output is intermingled with the REPL prompt.
static CLEAR_LINE: &str = "\x1b[2K";

/// Define the command line arguments that the tool accepts.
#[derive(FromArgs)]
#[argh(description = "Bluetooth AVRCP Controller CLI")]
struct Options {
    /// target device id.
    #[argh(positional, from_str_fn(peer_id_from_str))]
    device: PeerId,
}

/// Helper to parse `PeerId` from the command line. argh requires the error
/// return type to be a `String`, which is why we couldn't directly use
/// `PeerId::from_str`.
fn peer_id_from_str(s: &str) -> Result<PeerId, String> {
    PeerId::from_str(s).map_err(|err| err.to_string())
}

async fn send_passthrough<'a>(
    args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", Cmd::AvcCommand.usage()));
    }
    let cmd = avc_match_string(args[0]);
    if cmd.is_none() {
        return Err(format_err!("invalid avc command"));
    }

    // `args[0]` is the identifier of the peer to connect to
    match controller.send_command(cmd.unwrap()).await? {
        Ok(_) => Ok(String::from("")),
        Err(e) => Err(format_err!("Error sending AVC Command: {:?}", e)),
    }
}

async fn get_folder_items_with_attrs<'a>(
    cmd: Cmd,
    args: &'a [&'a str],
    controller: &'a BrowseControllerProxy,
) -> Result<String, Error> {
    if args.len() < 2 {
        return Ok(format!("usage: {}", cmd.usage()));
    }

    let start_index = args[0].to_string().parse::<u32>()?;
    let end_index = args[1].to_string().parse::<u32>()?;
    let attr_request = if args.len() == 3 {
        let arg = args[2].to_string();
        match arg.as_str() {
            "All" => AttributeRequestOption::GetAll(true),
            _ => {
                let attrs = arg.split(",");
                let mut attributes = vec![];
                for a in attrs {
                    let raw_attr = a.parse::<u32>()?;
                    attributes
                        .push(MediaAttributeId::from_primitive(raw_attr).ok_or_else(|| {
                        format_err!(
                            "Invalid MediaAttributeId value: {:?}. Valid value range is [1 - 8]",
                            raw_attr,
                        )
                    })?);
                }
                AttributeRequestOption::AttributeList(attributes)
            }
        }
    } else {
        AttributeRequestOption::GetAll(false)
    };

    match cmd {
        Cmd::GetVirtualFileSystem => {
            match controller.get_file_system_items(start_index, end_index, &attr_request).await? {
                Ok(items) => Ok(format!("File system: {:#?}", items)),
                Err(e) => Err(format_err!("Error fetching file system: {:?}", e)),
            }
        }
        Cmd::GetNowPlaying => {
            match controller.get_now_playing_items(start_index, end_index, &attr_request).await? {
                Ok(items) => Ok(format!("Now playing: {:#?}", items)),
                Err(e) => Err(format_err!("Error fetching now playing: {:?}", e)),
            }
        }
        _ => panic!("get_folder_items_with_attrs should not have been called with {:?}", cmd),
    }
}

async fn get_media<'a>(
    _args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    match controller.get_media_attributes().await? {
        Ok(media) => Ok(format!("Media attributes: {:#?}", media)),
        Err(e) => Err(format_err!("Error fetching media attributes: {:?}", e)),
    }
}

async fn get_media_player_list<'a>(
    args: &'a [&'a str],
    controller: &'a BrowseControllerProxy,
) -> Result<String, Error> {
    if args.len() != 2 {
        return Ok(format!("usage: {}", Cmd::GetMediaPlayerList.usage()));
    }

    let start_index = args[0].to_string().parse::<u32>()?;
    let end_index = args[1].to_string().parse::<u32>()?;

    match controller.get_media_player_items(start_index, end_index).await? {
        Ok(players) => Ok(format!("Media players: {:#?}", players)),
        Err(e) => Err(format_err!("Error fetching media players: {:?}", e)),
    }
}

async fn get_play_status<'a>(
    _args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    match controller.get_play_status().await? {
        Ok(status) => Ok(format!("Play status {:#?}", status)),
        Err(e) => Err(format_err!("Error fetching play status {:?}", e)),
    }
}

fn parse_pas_ids(ids: Vec<&str>) -> Result<Vec<PlayerApplicationSettingAttributeId>, Error> {
    let mut attribute_ids = vec![];
    for attr_id in ids {
        match attr_id {
            "1" => attribute_ids.push(PlayerApplicationSettingAttributeId::Equalizer),
            "2" => attribute_ids.push(PlayerApplicationSettingAttributeId::RepeatStatusMode),
            "3" => attribute_ids.push(PlayerApplicationSettingAttributeId::ShuffleMode),
            "4" => attribute_ids.push(PlayerApplicationSettingAttributeId::ScanMode),
            _ => return Err(format_err!("Invalid attribute id.")),
        }
    }

    Ok(attribute_ids)
}

async fn get_player_application_settings<'a>(
    args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    if args.len() > MAX_ATTRIBUTES as usize {
        return Ok(format!("usage: {}", Cmd::GetPlayerApplicationSettings.usage()));
    }

    let ids = match parse_pas_ids(args.to_vec()) {
        Ok(ids) => ids,
        Err(_) => return Err(format_err!("Invalid id in args {:?}", args)),
    };
    let get_pas_fut = controller.get_player_application_settings(&ids);

    match get_pas_fut.await? {
        Ok(settings) => Ok(format!("Player application setting attribute value: {:#?}", settings)),
        Err(e) => Err(format_err!("Error fetching player application attributes: {:?}", e)),
    }
}

async fn set_player_application_settings<'a>(
    _args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    // Send canned response to AVRCP with Equalizer off.
    let mut settings = fidl_avrcp::PlayerApplicationSettings::default();
    settings.equalizer = Some(fidl_avrcp::Equalizer::Off);

    match controller.set_player_application_settings(&settings).await? {
        Ok(set_settings) => Ok(format!("Set settings with: {:?}", set_settings)),
        Err(e) => Err(format_err!("Error in set settings {:?}", e)),
    }
}

async fn get_events_supported<'a>(
    _args: &'a [&'a str],
    controller: &'a ControllerExtProxy,
) -> Result<String, Error> {
    match controller.get_events_supported().await? {
        Ok(events) => Ok(format!("Supported events: {:#?}", events)),
        Err(e) => Err(format_err!("Error fetching supported events: {:?}", e)),
    }
}

async fn send_raw_vendor<'a>(
    args: &'a [&'a str],
    controller: &'a ControllerExtProxy,
) -> Result<String, Error> {
    if args.len() < 2 {
        return Err(format_err!("usage: {}", Cmd::SendRawVendorCommand.usage()));
    }

    match parse_raw_packet(args) {
        Ok((pdu_id, buf)) => {
            println!("Sending {:#?}", buf);

            match controller.send_raw_vendor_dependent_command(pdu_id, &buf).await? {
                Ok(response) => Ok(format!("response: {:#?}", response)),
                Err(e) => Err(format_err!("Error sending raw dependent command: {:?}", e)),
            }
        }
        Err(message) => Err(format_err!("{:?}", message)),
    }
}

fn parse_raw_packet(args: &[&str]) -> Result<(u8, Vec<u8>), String> {
    let pdu_id;

    if args[0].starts_with("0x") || args[0].starts_with("0X") {
        if let Ok(hex) = Vec::from_hex(&args[0][2..]) {
            if hex.len() < 1 {
                return Err(format!("invalid pdu_id {}", args[0]));
            }
            pdu_id = hex[0];
        } else {
            return Err(format!("invalid pdu_id {}", args[0]));
        }
    } else {
        if let Ok(b) = args[0].parse::<u8>() {
            pdu_id = b;
        } else {
            return Err(format!("invalid pdu_id {}", args[0]));
        }
    }

    let byte_string = args[1..].join(" ").replace(",", " ");

    let bytes = byte_string.split(" ");

    let mut buf = vec![];
    for b in bytes {
        let b = b.trim();
        if b.eq("") {
            continue;
        } else if b.starts_with("0x") || b.starts_with("0X") {
            if let Ok(hex) = Vec::from_hex(&b[2..]) {
                buf.extend_from_slice(&hex[..]);
            } else {
                return Err(format!("invalid hex string at {}", b));
            }
        } else {
            if let Ok(hex) = Vec::from_hex(&b[..]) {
                buf.extend_from_slice(&hex[..]);
            } else {
                return Err(format!("invalid hex string at {}", b));
            }
        }
    }

    Ok((pdu_id, buf))
}

async fn set_volume<'a>(
    args: &'a [&'a str],
    controller: &'a ControllerProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", Cmd::SetVolume.usage()));
    }

    let volume = if let Ok(val) = args[0].parse::<u8>() {
        if val > 127 {
            return Err(format_err!("invalid volume range {}", args[0]));
        }
        val
    } else {
        return Err(format_err!("unable to parse volume {}", args[0]));
    };

    match controller.set_absolute_volume(volume).await? {
        Ok(set_volume) => Ok(format!("Volume set to: {:?}", set_volume)),
        Err(e) => Err(format_err!("Error setting volume: {:?}", e)),
    }
}

async fn change_path<'a>(
    args: &'a [&'a str],
    controller: &'a BrowseControllerProxy,
) -> Result<String, Error> {
    if args.len() < 1 {
        return Ok(format!("usage: {}", Cmd::ChangePath.usage()));
    }
    let path = match args[0] {
        ".." | "up" => fidl_avrcp::Path::Parent(fidl_avrcp::Parent {}),
        uid => {
            let folder_uid = uid.to_string().parse::<u64>()?;
            fidl_avrcp::Path::ChildFolderUid(folder_uid)
        }
    };
    match controller.change_path(&path).await? {
        Ok(num_of_items) => {
            Ok(format!("Changed path successfully. Current directory has {:?} items", num_of_items))
        }
        Err(e) => Err(format_err!("Failed to change path: {:?}", e)),
    }
}

async fn play_item<'a>(
    cmd: Cmd,
    args: &'a [&'a str],
    controller: &'a BrowseControllerProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", cmd.usage()));
    }

    let uid = args[0].parse::<u64>()?;

    let command = match cmd {
        Cmd::PlayVirtualFileSystem => controller.play_file_system_item(uid),
        Cmd::PlayNowPlaying => controller.play_now_playing_item(uid),
        _ => panic!("play_item should not have been called with {:?}", cmd),
    };
    Ok(command
        .await
        .map(|_| "Successfully played item".to_string())
        .map_err(|e| format_err!("Error playing item: {:?}", e))?)
}

async fn set_browsed_player<'a>(
    args: &'a [&'a str],
    controller: &'a BrowseControllerProxy,
) -> Result<String, Error> {
    if args.len() != 1 {
        return Ok(format!("usage: {}", Cmd::SetBrowsedPlayer.usage()));
    }

    let player_id = args[0].to_string().parse::<u16>()?;

    match controller.set_browsed_player(player_id).await? {
        Ok(_) => Ok(format!("Browsed player set to: {:?}", player_id)),
        Err(e) => Err(format_err!("Error setting browsed player: {:?}", e)),
    }
}

async fn is_connected<'a>(
    _args: &'a [&'a str],
    controller: &'a ControllerExtProxy,
    browse_controller: &'a BrowseControllerExtProxy,
) -> Result<String, Error> {
    let mut s = Vec::new();
    match controller.is_connected().await {
        Ok(status) => s.push(format!("Is control connected: {}", status)),
        Err(e) => return Err(format_err!("Error checking control connection status: {:?}", e)),
    };
    match browse_controller.is_connected().await {
        Ok(status) => s.push(format!("Is browse connected: {}", status)),
        Err(e) => return Err(format_err!("Error checking browse connection status: {:?}", e)),
    };
    Ok(s.iter().fold("".to_owned(), |msg, m| format!("{}\n{}", msg, m)))
}

struct AvrcpHandler<'a> {
    controller: &'a ControllerProxy,
    test_controller: &'a ControllerExtProxy,
    browse_controller: &'a BrowseControllerProxy,
    test_browse_controller: &'a BrowseControllerExtProxy,
}

impl ReplHandler for AvrcpHandler<'_> {
    type Command = Cmd;

    fn help_footer(&self) -> Option<String> {
        Some(avc_keys_help())
    }

    async fn handle(&mut self, cmd: Cmd, args: &[&str]) -> Result<ReplControl, Error> {
        let res = match cmd {
            Cmd::AvcCommand => send_passthrough(args, self.controller).await,
            Cmd::ChangePath => change_path(args, self.browse_controller).await,
            Cmd::GetVirtualFileSystem => {
                get_folder_items_with_attrs(Cmd::GetVirtualFileSystem, args, self.browse_controller)
                    .await
            }
            Cmd::GetMediaAttributes => get_media(args, self.controller).await,
            Cmd::GetMediaPlayerList => get_media_player_list(args, self.browse_controller).await,
            Cmd::GetNowPlaying => {
                get_folder_items_with_attrs(Cmd::GetNowPlaying, args, self.browse_controller).await
            }
            Cmd::GetPlayStatus => get_play_status(args, self.controller).await,
            Cmd::GetPlayerApplicationSettings => {
                get_player_application_settings(args, self.controller).await
            }
            Cmd::PlayVirtualFileSystem => {
                play_item(Cmd::PlayVirtualFileSystem, args, self.browse_controller).await
            }
            Cmd::PlayNowPlaying => {
                play_item(Cmd::PlayNowPlaying, args, self.browse_controller).await
            }
            Cmd::SetPlayerApplicationSettings => {
                set_player_application_settings(args, self.controller).await
            }
            Cmd::SendRawVendorCommand => send_raw_vendor(args, self.test_controller).await,
            Cmd::SupportedEvents => get_events_supported(args, self.test_controller).await,
            Cmd::SetVolume => set_volume(args, self.controller).await,
            Cmd::SetBrowsedPlayer => set_browsed_player(args, self.browse_controller).await,
            Cmd::IsConnected => {
                is_connected(args, self.test_controller, self.test_browse_controller).await
            }
        }?;
        if !res.is_empty() {
            println!("{res}");
        }
        Ok(ReplControl::Continue)
    }
}

async fn controller_listener(
    controller: &ControllerProxy,
    mut stream: ControllerEventStream,
) -> Result<(), Error> {
    while let Some(evt) = stream.try_next().await? {
        print!("{}", CLEAR_LINE);
        match evt {
            ControllerEvent::OnNotification { timestamp, notification } => {
                if let Some(value) = notification.pos {
                    println!("Pos event: {:?} {:?}", timestamp, value);
                } else if let Some(value) = notification.status {
                    println!("Status event: {:?} {:?}", timestamp, value);
                } else if let Some(value) = notification.track_id {
                    println!("Track event: {:?} {:?}", timestamp, value);
                } else if let Some(value) = notification.volume {
                    println!("Volume event: {:?} {:?}", timestamp, value);
                } else {
                    println!("Other event: {:?} {:?}", timestamp, notification);
                }
                controller.notify_notification_handled()?;
            }
        }
    }
    Ok(())
}

#[fuchsia::main(logging = false)]
async fn main() -> Result<(), Error> {
    let opt: Options = argh::from_env();

    let device_id = opt.device;

    // Connect to test controller service first so we fail early if it's not available.

    let test_avrcp_svc = connect_to_protocol::<PeerManagerExtMarker>()
        .context("Failed to connect to Bluetooth Test AVRCP interface")?;

    // Create a channel for our Request<TestController> to live
    let (t_client, t_server) = create_endpoints::<ControllerExtMarker>();

    let _status = test_avrcp_svc.get_controller_for_target(&device_id.into(), t_server).await?;
    eprintln!(
        "Test controller obtained to device \"{device}\" AVRCP remote target service",
        device = device_id,
    );

    // Create a channel for our Request<TestBrowseController> to live
    let (tb_client, tb_server) = create_endpoints::<BrowseControllerExtMarker>();

    let _status =
        test_avrcp_svc.get_browse_controller_for_target(&device_id.into(), tb_server).await?;
    eprintln!(
        "Test browse controller obtained to device \"{device}\" AVRCP remote target service",
        device = device_id,
    );

    // Connect to avrcp controller service.

    let avrcp_svc = connect_to_protocol::<PeerManagerMarker>()
        .context("Failed to connect to Bluetooth AVRCP interface")?;

    // Create a channel for our Request<Controller> to live
    let (c_client, c_server) = create_endpoints::<ControllerMarker>();

    let _status = avrcp_svc.get_controller_for_target(&device_id.into(), c_server).await?;
    eprintln!(
        "Controller obtained to device \"{device}\" AVRCP remote target service",
        device = device_id,
    );

    // Create a channel for our Request<Controller> to live
    let (bc_client, bc_server) = create_endpoints::<BrowseControllerMarker>();

    let _status = avrcp_svc.get_browse_controller_for_target(&device_id.into(), bc_server).await?;
    eprintln!(
        "Browse controller obtained to device \"{device}\" AVRCP remote target service",
        device = device_id,
    );

    // setup repl

    let controller = c_client.into_proxy();
    let test_controller = t_client.into_proxy();
    let browse_controller = bc_client.into_proxy();
    let test_browse_controller = tb_client.into_proxy();

    let evt_stream = controller.clone().take_event_stream();

    // set controller event filter to ones we support.
    let _ = controller.set_notification_filter(
        Notifications::PLAYBACK_STATUS
            | Notifications::TRACK
            | Notifications::TRACK_POS
            | Notifications::VOLUME,
        1,
    )?;

    let handler = AvrcpHandler {
        controller: &controller,
        test_controller: &test_controller,
        browse_controller: &browse_controller,
        test_browse_controller: &test_browse_controller,
    };
    let event_fut = controller_listener(&controller, evt_stream).fuse();
    let repl_fut = bt_repl::run(PROMPT, command_source(), handler).fuse();

    let mut event_fut = pin!(event_fut);
    let mut repl_fut = pin!(repl_fut);

    // These futures should only return when something fails.
    select! {
        result = event_fut => {
            eprintln!(
                "Service connection returned {status:?}", status = result);
        },
        result = repl_fut => result?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy;

    #[fuchsia::test]
    fn test_raw_packet_parsing_01() {
        assert_eq!(parse_raw_packet(&["0x40", "0xaaaa"]), Ok((0x40, vec![0xaa, 0xaa])));
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_02() {
        assert_eq!(parse_raw_packet(&["0x40", "0xaa", "0xaa"]), Ok((0x40, vec![0xaa, 0xaa])));
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_03() {
        assert_eq!(
            parse_raw_packet(&["0x40", "0xAa", "0XaA", "0x1234"]),
            Ok((0x40, vec![0xaa, 0xaa, 0x12, 0x34]))
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_04() {
        assert_eq!(
            parse_raw_packet(&["40", "0xaa", "0xaa", "0x1234"]),
            Ok((40, vec![0xaa, 0xaa, 0x12, 0x34]))
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_05() {
        assert_eq!(
            parse_raw_packet(&["40", "aa", "aa", "1234"]),
            Ok((40, vec![0xaa, 0xaa, 0x12, 0x34]))
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_06() {
        assert_eq!(parse_raw_packet(&["40", "aa,aa,1234"]), Ok((40, vec![0xaa, 0xaa, 0x12, 0x34])));
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_07() {
        assert_eq!(
            parse_raw_packet(&["40", "0xaa, 0xaa,    0x1234"]),
            Ok((40, vec![0xaa, 0xaa, 0x12, 0x34]))
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_08_err_pdu_overflow() {
        assert_eq!(
            parse_raw_packet(&["300", "0xaa, 0xaa,    0x1234"]),
            Err("invalid pdu_id 300".to_string())
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_09_err_invalid_hex_long() {
        assert_eq!(
            parse_raw_packet(&["40", "0xzz, 0xaa,    0x1234"]),
            Err("invalid hex string at 0xzz".to_string())
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_10_err_invalid_hex_short() {
        assert_eq!(
            parse_raw_packet(&["40", "zz, 0xaa, 0xqqqq"]),
            Err("invalid hex string at zz".to_string())
        );
    }
    #[fuchsia::test]
    fn test_raw_packet_parsing_11_err_invalid_hex() {
        assert_eq!(
            parse_raw_packet(&["40", "ab, 0xaa, 0xqqqq"]),
            Err("invalid hex string at 0xqqqq".to_string())
        );
    }

    #[fuchsia::test]
    fn test_parse_pas_id_success() {
        let ids = vec!["1", "2", "3"];
        let result = parse_pas_ids(ids);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(
            result,
            vec![
                PlayerApplicationSettingAttributeId::Equalizer,
                PlayerApplicationSettingAttributeId::RepeatStatusMode,
                PlayerApplicationSettingAttributeId::ShuffleMode
            ]
        );
    }

    #[fuchsia::test]
    fn test_parse_pas_id_error() {
        let ids = vec!["fake", "id", "1"];
        let result = parse_pas_ids(ids);
        assert!(result.is_err());
    }

    #[fuchsia::test]
    fn test_parse_pas_id_invalid_id_error() {
        let ids = vec!["1", "2", "5"];
        let result = parse_pas_ids(ids);
        assert!(result.is_err());
    }

    #[fuchsia::test]
    /// Tests a set_volume command with no input args does not result in error.
    /// Instead, a help message should be returned.
    async fn test_set_volume_no_args() {
        let (proxy, _stream) = create_proxy::<ControllerMarker>();
        let args = [];

        let res = set_volume(&args, &proxy).await;
        assert_matches!(
            res,
            Ok(m) if m.contains("usage")
        );
    }

    #[fuchsia::test]
    async fn test_avrcp_handler() {
        let (controller, _c_stream) = create_proxy::<ControllerMarker>();
        let (test_controller, _tc_stream) = create_proxy::<ControllerExtMarker>();
        let (browse_controller, _bc_stream) = create_proxy::<BrowseControllerMarker>();
        let (test_browse_controller, _tbc_stream) = create_proxy::<BrowseControllerExtMarker>();
        let mut handler = AvrcpHandler {
            controller: &controller,
            test_controller: &test_controller,
            browse_controller: &browse_controller,
            test_browse_controller: &test_browse_controller,
        };

        assert!(handler.help_footer().unwrap().starts_with("AVC keys:"));
        assert_eq!(handler.handle(Cmd::SetVolume, &[]).await.unwrap(), ReplControl::Continue);
        assert_eq!(
            handler.handle(Cmd::SetBrowsedPlayer, &[]).await.unwrap(),
            ReplControl::Continue
        );
    }
}
