// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::gen_commands;

// `Cmd` is the declarative specification of all commands that bt-avdtp-tool accepts.
// TODO(https://fxbug.dev/42112689): Add support for printing with arguments (merge PrintPeer, PrintPeers).
gen_commands! {
    pub enum Cmd {
        AbortStream = ("abort-stream", [], ["generic id"], "Initiate an abort stream command"),
        EstablishStream = ("establish-stream", [], ["generic id"], "Establish streaming on the device"),
        ReleaseStream = ("release-stream", [], ["generic id"], "Release the stream"),
        StartStream = ("start-stream", [], ["generic id"], "Initiate a start stream command"),
        SetConfig = ("set-configuration", [], ["generic id"], "Set configuration"),
        GetConfig = ("get-configuration", [], ["generic id"], "Get configuration"),
        GetCapabilities = ("get-capabilities", [], ["generic id"], "Get capabilities"),
        GetAllCapabilities = ("get-all-capabilities", [], ["generic id"], "Get all capabilities"),
        Reconfigure = ("reconfigure", [], ["generic id"], "Reconfigure command"),
        Suspend = ("suspend", [], ["generic id"], "Suspend command"),
        SuspendReconfigure = ("suspend-reconfigure", [], ["generic id"],
            "Suspend command followed by reconfigure."),
    }
}
