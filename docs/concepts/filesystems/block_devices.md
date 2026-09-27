# Block Devices

Fuchsia Block device drivers are, like other drivers on the system, implemented
as userspace services that are accessible via IPC. Programs using block devices
will have one or more handles to these underlying servers. Similar to filesystem
clients, which may send “read” or “write” requests to servers by encoding these
requests within RPC messages, programs act as clients to block devices using the
[`fuchsia.storage.block.Block`][block-fidl] FIDL protocol, which allows clients
to query the block device, open block sessions, and queue FIFO transactions.

[`fuchsia.storage.block.Block`][block-fidl] is the protocol served by any
block-like server, whereas
[`fuchsia.hardware.block.volume.Service`][volume-service] is a FIDL service
exposed by block device drivers (USB, AHCI / SATA, NVMe, SDMMC, UFS, Virtio,
Ramdisk, etc) whose `volume` member implements
[`fuchsia.storage.block.Block`][block-fidl]. Block-like servers that are not
drivers may implement [`fuchsia.storage.block.Block`][block-fidl] without
exposing the volume service; for example, the GPT component exposes one
`Block` instance for each partition. Both drivers and other block servers can
use the [`block_server`][block-server] library to implement
[`fuchsia.storage.block.Block`][block-fidl].

## Fast Block I/O

Block device drivers are often responsible for taking large portions of memory,
and queueing requests to a particular device to either “read into” or “write
from” a portion of memory. Transmitting messages of a limited size from an RPC
protocol into an “I/O transaction” would require repeated copying of large
buffers to access block devices.

To avoid this performance bottleneck, instead of transmitting “read” or “write”
messages with large buffers over FIDL RPCs, the block protocol uses a fast,
FIFO-based protocol which acts on a shared VMO. Filesystems (or any other
client wishing to interact with a block device) open a session
(`fuchsia.storage.block.Session`) on a block device, acquire its FIFO handle,
and attach VMOs (`AttachVmo`) to the session. A client can then send a fast,
lightweight control message on the FIFO, indicating that the block device driver
should act directly on an already-registered VMO. For example, when writing to a
file, rather than passing bytes over IPC primitives directly and copying them to
a new location in the block device’s memory, a filesystem (representing the file
as a VMO) can send a small FIFO message indicating “write N blocks directly from
block offset X of VMO Y to block offset Z on a disk”. When combined with the
“mmap” memory-mapping tools, this provides a “zero-copy” pathway directly from
client programs to disk (or in the other direction) when accessing files.

[volume-service]: /sdk/fidl/fuchsia.hardware.block.volume/volume.fidl
[block-fidl]: /sdk/fidl/fuchsia.storage.block/block.fidl
[block-server]: /src/storage/lib/block_server/src/lib.rs
