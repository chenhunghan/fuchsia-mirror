// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "suspend",
    description = "Allows, but does not force suspend of the power elements of the matching driver node and its dependents.",
    note = "This does not force suspend: it only removes the system-wide power dependency
keeping the node (and the nodes bound beneath it) active while the system is
awake (for example, from wake leases or execution/activity leases). If another
entity holds a direct lease on the driver's power element (or another element
that depends on it), the driver will remain active and will not suspend. The
node must have a driver bound to it. `driver node list -v` and `driver node show`
report a node's moniker, disambiguator, and owner (which is a driver URL if a
driver is bound to it). When multiple nodes share a moniker, pass
--disambiguator (or `<moniker>#<disambiguator>`) to disambiguate.
The suspend session remains active until the command is interrupted (e.g. with
Ctrl+C).",
    example = "To suspend a node by moniker

    $ driver suspend dev.sys.platform.example.example-device

To disambiguate colliding monikers

    $ driver suspend dev.sys.example#0123456789abcdef
    $ driver suspend dev.sys.example --disambiguator 0123456789abcdef",
    error_code(1, "Failed to connect to the driver development service")
)]
pub struct SuspendCommand {
    #[argh(
        positional,
        description = "component moniker of the node to be suspended (optionally `<moniker>#<disambiguator>`)."
    )]
    pub moniker: String,

    #[argh(
        option,
        long = "disambiguator",
        description = "16-character structural disambiguator (from `driver node list -v` or `driver node show`) to disambiguate colliding monikers."
    )]
    pub disambiguator: Option<String>,

    /// if this exists, the user will be prompted for a component to select.
    #[argh(switch, short = 's', long = "select")]
    pub select: bool,
}
