// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::{CommandSource, Commands, gen_commands};
use fidl_fuchsia_bluetooth_avrcp::AvcPanelCommand;

macro_rules! gen_avc_commands {
($name:ident {
    $($variant:ident = ($long:expr, $short:expr)),*,
}) => {

        static AVC_COMMAND_LONG_VARIANTS: &[&str] = &[$($long,)*];
        static AVC_COMMAND_SHORT_VARIANTS: &[&str] = &[$($short,)*];

        pub fn avc_match_string(s: &str) -> Option<AvcPanelCommand> {
            let lc = s.to_lowercase();
            match lc.as_str() {
                $($short => return Some(AvcPanelCommand::$variant)),*,
                _ => {}
            }
            match lc.as_str() {
                $($long => Some(AvcPanelCommand::$variant)),*,
                _ => None
            }
        }
    }
}

gen_avc_commands! {
    AvcCommand {
        Select = ("select", "sel"),
        Up = ("up", "up"),
        Down = ("down", "do"),
        Left = ("left", "le"),
        Right = ("right", "ri"),
        RootMenu = ("rootmenu", "rm"),
        ContentsMenu = ("contentsmenu", "cm"),
        FavoriteMenu = ("favoritemenu", "fm"),
        Exit = ("exit", "ex"),
        OnDemandMenu = ("ondemandmenu", "om"),
        AppsMenu = ("appsmenu", "am"),
        Key0 = ("key0", "0"),
        Key1 = ("key1", "1"),
        Key2 = ("key2", "2"),
        Key3 = ("key3", "3"),
        Key4 = ("key4", "4"),
        Key5 = ("key5", "5"),
        Key6 = ("key6", "6"),
        Key7 = ("key7", "7"),
        Key8 = ("key8", "8"),
        Key9 = ("key9", "9"),
        Dot = ("dot", "."),
        Enter = ("enter", "en"),
        ChannelUp = ("channelup", "cu"),
        ChannelDown = ("channeldown", "cd"),
        ChannelPrevious = ("channelprevious", "cp"),
        InputSelect = ("inputselect", "ip"),
        Info = ("info", "in"),
        Help = ("help", "he"),
        PageUp = ("pageup", "pu"),
        PageDown = ("pagedown", "pd"),
        Lock = ("lock", "lo"),
        Power = ("power", "po"),
        VolumeUp = ("volumeup", "vu"),
        VolumeDown = ("volumedown", "vd"),
        Mute = ("mute", "m"),
        Play = ("play", "pl"),
        Stop = ("stop", "s"),
        Pause = ("pause", "pa"),
        Record = ("record", "rec"),
        Rewind = ("rewind", "rew"),
        FastForward = ("fastforward", "ff"),
        Eject = ("eject", "ej"),
        Forward = ("forward", "fw"),
        Backward = ("backward", "bw"),
        List = ("list", "l"),
        F1 = ("f1","f1"),
        F2 = ("f2","f2"),
        F3 = ("f3","f3"),
        F4 = ("f4","f4"),
        F5 = ("f5","f5"),
        F6 = ("f6","f6"),
        F7 = ("f7","f7"),
        F8 = ("f8","f8"),
        F9 = ("f9","f9"),
        Red = ("red", "re"),
        Green = ("green", "gr"),
        Blue = ("blue", "bl"),
        Yellow = ("yellow", "ye"),
    }
}

// `Cmd` is the declarative specification of all commands that bt-avrcp-controller accepts.
gen_commands! {
    pub enum Cmd {
        AvcCommand = (
            "key",
            [],
            ["command"],
            "send an AVC passthrough keypress event",
        ),
        ChangePath = (
            "change-path",
            [],
            ["folder_uid"],
            "Navigates one folder down if folder_uid is specified. \
             Special value \"..\" or \"up\" navigates one folder up to the parent folder instead.",
        ),
        GetVirtualFileSystem = (
            "get-file-system",
            [],
            ["start_index", "end_index", "attributes"],
            "Get media player virtual file system items from target. \
             Note that `attributes` is a comma-separated attribute IDs. \
             Use special keyword \"ALL\" to specify all attributes. \
             Leave empty to specify no attributes. \
             Example: `<command> <start_index> <end_index> 1,2,3` OR \
             `<command> <start_index> <end_index> ALL`",
        ),
        GetMediaAttributes = (
            "get-media",
            [],
            [],
            "gets currently playing media attributes",
        ),
        GetMediaPlayerList = (
            "get-media-players",
            [],
            ["start_index", "end_index"],
            "Get list of media players from target",
        ),
        GetNowPlaying = (
            "get-now-playing",
            [],
            ["start_index", "end_index", "attributes"],
            "Get now playing list from target. \
             Note that `attributes` is a comma-separated attribute IDs. \
             Use special keyword \"ALL\" to specify all attributes. \
             Leave empty to specify no attributes. \
             Example: `<command> <start_index> <end_index> 1,2,3` OR \
             `<command> <start_index> <end_index> ALL`",
        ),
        GetPlayStatus = (
            "get-play-status",
            [],
            [],
            "gets the status of the currently playing media at the TG",
        ),
        GetPlayerApplicationSettings = (
            "get-player-application-settings",
            [],
            ["Optional: id1 id2 ..."],
            "Gets currently set attribute values if ids are specified. \
             Otherwise, all possible attribute values on the TG.",
        ),
        PlayVirtualFileSystem = (
            "play-file",
            [],
            ["uid"],
            "Play an item from the TG's virtual file system.",
        ),
        PlayNowPlaying = (
            "play-now-playing",
            [],
            ["uid"],
            "Play an item from the TG's now playing list.",
        ),
        SetPlayerApplicationSettings = (
            "set-player-application-settings",
            [],
            [],
            "Sets player application settings with a default Equalizer=Off setting.",
        ),
        SupportedEvents = (
            "get-supported-events",
            [],
            [],
            "gets the supported events of the target",
        ),
        SendRawVendorCommand = (
            "send-raw-vendor-command",
            [],
            ["pdu_id", "payload"],
            "send a raw vendor AVC command",
        ),
        SetBrowsedPlayer = (
            "set-browsed-player",
            [],
            ["player_id"],
            "Sets the browsed player by the specified player ID",
        ),
        SetVolume = (
            "set-volume",
            [],
            ["volume"],
            "send a set absolute volume command (0-127)",
        ),
        IsConnected = (
            "connection-status",
            [],
            [],
            "checks if the current device is current connected \
             via control channel and browse channel",
        ),
    }
}

/// Help footer listing supported AVC key names for `key`.
pub fn avc_keys_help() -> String {
    format!("AVC keys:\n\tkey {}", AVC_COMMAND_LONG_VARIANTS.join("\n\tkey "))
}

/// Creates the [`CommandSource`] for `bt-avrcp-controller`, providing first-argument
/// completion for `key` (`Cmd::AvcCommand`) against long and short AVC key names.
pub fn command_source() -> impl CommandSource {
    Commands::<Cmd>::new().with_arg_completer(|cmd, partial_arg| {
        let mut candidates = Vec::new();
        if cmd == Cmd::AvcCommand {
            for key in AVC_COMMAND_LONG_VARIANTS.iter().chain(AVC_COMMAND_SHORT_VARIANTS.iter()) {
                if key.starts_with(partial_arg) && !candidates.iter().any(|c| c == key) {
                    candidates.push((*key).to_string());
                }
            }
        }
        candidates
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_avc_match_string() {
        assert_eq!(Some(AvcPanelCommand::Up), avc_match_string("up"));
        assert_eq!(Some(AvcPanelCommand::Up), avc_match_string("UP"));
        assert_eq!(None, avc_match_string("UPS"));
        assert_eq!(Some(AvcPanelCommand::Play), avc_match_string("play"));
        assert_eq!(Some(AvcPanelCommand::Play), avc_match_string("pl"));
        assert_eq!(None, avc_match_string("p"));
        assert_eq!(None, avc_match_string(""));
        assert_eq!(None, avc_match_string("12"));
        assert_eq!(Some(AvcPanelCommand::Dot), avc_match_string("."));
        assert_eq!(Some(AvcPanelCommand::ChannelUp), avc_match_string("channelup"));
    }

    #[fuchsia::test]
    fn test_completer() {
        let source = command_source();
        let names = source.command_names();
        assert!(names.contains(&"key".to_string()));
        assert!(names.contains(&"get-media".to_string()));
        assert!(names.contains(&"connection-status".to_string()));
        assert!(names.contains(&"send-raw-vendor-command".to_string()));
        assert!(names.contains(&"get-supported-events".to_string()));
        assert!(names.contains(&"get-play-status".to_string()));
        assert!(names.contains(&"get-player-application-settings".to_string()));
        assert!(names.contains(&"set-player-application-settings".to_string()));
        assert!(names.contains(&"set-volume".to_string()));

        let key_candidates = source.complete_argument("key", "ex");
        assert!(key_candidates.contains(&"exit".to_string()));
        assert!(key_candidates.contains(&"ex".to_string()));
        assert_eq!(source.complete_argument("key", "up"), vec!["up".to_string()]);

        assert!(source.complete_argument("get-media", "ex").is_empty());
        assert!(avc_keys_help().starts_with("AVC keys:\n\tkey select"));
    }
}
