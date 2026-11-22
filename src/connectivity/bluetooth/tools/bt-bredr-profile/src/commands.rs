// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_repl::gen_commands;

gen_commands! {
    pub enum Cmd {
        Advertise = ("advertise", [], ["psm", "channel-mode", "max-rx-du-size"],
                      "\n\t\tRegister a service with the SDP server.\n\
                       \t\t'psm' is a protocol id that this service will advertise support for.\n\
                       \t\t'channel-mode' is {basic|ertm}.\n\
                       \t\t'max-rx-sdu-size' is an integer in the range 0 - 65535.\n\n\
                       \t\tExample: advertise 25 basic 672"),
        SetupRfcomm = ("setup-rfcomm", [], [],
                      "\n\t\t Registers an advertisement for an example SPP service.\n\
                       \t\t Registers a search for compatible SPP services."),
        Services = ("services", [], [], "List registered services"),
        RemoveService = ("remove-service", [], ["service-id"],
                         "\n\t\tUnregister service corresponding to 'service-id'\n\
                          \t\tExample: remove-service 0"),
        Channels = ("channels", [], [], "List connected L2CAP channels and their Ids assigned by the REPL"),
        ConnectL2cap = ("connect-l2cap", [], ["peer-id", "psm", "channel-mode", "max-rx-sdu-size", "security-requirements"],
                        "\n\t\tCreate an l2cap channel to the remote device 'peer-id'. \n\
                         \t\t'channel-mode' must be {basic|ertm}. 'psm' and 'max-rx-sdu-size' must be\n\
                         \t\tpositive integers in the range 0 - 65535. 'security-requirements' must be\n\
                         \t\t{none|auth|sc|auth-sc}.\n\n\
                         \t\tExample: connect-l2cap 028565803f1368b2 1 basic 672 none"),
        ConnectRfcomm = ("connect-rfcomm", [], ["peer-id", "server-channel"],
                        "\n\t\tCreate an RFCOMM channel to the remote device 'peer-id'. \n\
                         \t\t'server-channel' must be in the range [1,30]. \n\
                         \t\tExample: connect-rfcomm 028565803f1368b2 1"),
        DisconnectL2cap = ("disconnect-l2cap", [], ["channel-id"],
                           "\n\t\tDrop socket corresponding to 'channel-id', which will disconnect\n\
                            \t\tthe l2cap channel.\n\
                            \t\t'channel-id' must correspond to a connected channel listed by the \n\
                            \t\t'channels' command\n\n\
                            \t\tExample: disconnect-l2cap 0"),
        DisconnectRfcomm = ("disconnect-rfcomm", [], ["peer-id", "server-channel"],
                           "\n\t\tDisconnect the RFCOMM channel corresponding to `server-channel`.\n\
                            \t\tExample: disconnect-rfcomm 028565803f1368b2 5"),
        DisconnectRfcommSession = ("disconnect-rfcomm-session", [], ["peer-id"],
                           "\n\t\tDisconnect the RFCOMM Session with the connected remote device `peer-id`.\n\
                            \t\tExample: disconnect-rfcomm-session 028565803f1368b2"),
        SendRls = ("send-remote-line-status", [], ["peer-id", "server-channel"], "\n\t\tSend a Remote Line Status update for the `server-channel`"),
        WriteL2cap = ("write-l2cap", [], ["channel-id", "data"],
                "\n\t\tWrite 'data' on the L2CAP socket/channel represented by 'channel-id'\n\n\
                 \t\tExample: write 0 0123456789abcd"),
        WriteRfcomm = ("write-rfcomm", [], ["peer-id", "channel-id", "data"],
                "\n\t\tWrite 'data' on the RFCOMM channel represented by 'server-channel'\n\n\
                 \t\tExample: write 028565803f1368b2 1 myexampleuserdata"),
        WriteLargeRfcomm = ("write-large-rfcomm", [], ["peer-id", "channel-id"],
                 "\n\t\tWrite a fixed data buf on the RFCOMM channel represented by 'server-channel'\n\n\
                  \t\tExample: write-large-rfcomm 028565803f1368b2 1"),
    }
}
