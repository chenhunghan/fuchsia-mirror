# Routing a storage partition to a component

While most components interact with storage through a filesystem using their
storage capabilities, certain components may need to be able to directly read
and write a partition.  This guide explains how to route a partition to a
component.

The steps, at a high level, are:

1. **Define a semantic label in the board configuration**:  Define a mapping of
   a physical partition label to a "semantic" label, which will be the name of
   the partition used in capability routing.
2. **Update component manifests**: Routing the directory capability for the
   block device to the component.
3. **Access the block device in the component**: The component uses the routed
   directory capability to access the block device.

## Steps

### 1. Define a semantic label in the board configuration

The first step is to declare a semantic label for the physical partition in your
board's configuration, which is typically a `.bazel` or `.bzl` file located in
`//boards/` or a vendor-specific equivalent.

In the board's configuration, add a `block_devices` list to the `filesystems`
dictionary. Each entry in this list is a dictionary that defines a new named
device.

* `device`: The semantic label (or `/`-separated relative path, such as
  `"ota/zircon_a"`) you will use to refer to this partition under `/block` and
  in component manifests. Empty path segments (such as leading, trailing, or
  consecutive `/`) are invalid, and an intermediate directory name cannot
  collide with another `device` leaf path.
* `from`: Specifies where the partition should be found on the disk. Multiple
  entries in `block_devices` may specify the same `from` (`label` and `parent`)
  with distinct `device` paths to publish the same physical partition at
  multiple locations under `/block`.
  + `label`: The partition label to be routed.
  + `parent`: The parent device, which is `"gpt"` for the system GPT or `"dev"`
    for a driver-provided block device.

**Example**: In your board's `BUILD.bazel` file:

```python
# In a board definition
fuchsia_board_configuration(
    name = "my_board",
    ...
    filesystems = {
        "block_devices": [
            {
                "device": "my_partition",
                "from": {
                    "label": "my-partition-label",
                    "parent": "gpt",
                },
            },
            {
                "device": "my_other_partition",
                "from": {
                    "label": "my-other-partition-label",
                    "parent": "gpt",
                },
            },
        ],
        ...
    },
    ...
)
```

This configuration is provided to `fshost` , which will match for GPT partitions
with the label "my-partition-label" and provide them at "/block/my_partition"
(and similarly for my-other-partition-label).

### 2. Route the Block Device to Your Component

Once the board is configured, you must route the capability for the block device
from `fshost` down to your component. This is done by adding `offer` stanzas to
the component manifests ( `.cml` files) of the parent components in the
topology.

The capability is a `directory` provided by `fshost` under the path `/block` .
The specific partition is selected by using the `subdir` parameter in the `offer`
stanza, which must match the `device` name you defined in the board
configuration.

In this example, we are routing a partition to a component in the `core` realm,
but steps are similar for other realms.  Modify the core shard for the component
to route the partition to it:

```cml
// my_component.core_shard.cml
{
    offer: [
        {
            directory: "block",
            from: "parent",
            to: "#my_component",
            subdir: "my_partition",
            as: "my_partition",
        },
        {
            directory: "block",
            from: "parent",
            to: "#my_component",
            subdir: "my_other_partition",
            as: "my_other_partition",
        },
        ...
    ],
    ...
}
```

### 3. Use the Block Device in Your Component

The final step is for your component to `use` the routed directory capability.

In your component's manifest ( `my_component.cml` ), add a `use` stanza for the
directory.

**Example**: `my_component.cml`

```cml
{
    use: [
        {
            directory: "my_partition",
            rights: [ "r*" ],
            path: "/block/my_partition",
        },
        {
            directory: "my_other_partition",
            rights: [ "r*" ],
            path: "/block/my_other_partition",
        },
        ...
    ],
}
```

Your component can now access the partitions in its namespace at `/block` .  To
connect to the `fuchsia.storage.block.Block` protocol for a given
partition, you connect via this path:

```rust
use fidl_fuchsia_storage_block::BlockMarker;
use block_client::RemoteBlockClient;

async fn connect_to_my_partition() -> Result<(), anyhow::Error> {
    let proxy = fuchsia_component::client::connect_to_protocol_at::<BlockMarker>(
        "/block/my_partition/fuchsia.storage.block.Block"
    )?;
    let block_client = RemoteBlockClient::new(proxy).await?;
    // ... use the block client
    Ok(())
}
```

## Grouping and sharing partitions

### Grouping partitions under a subdirectory

If a component needs access to a set of related partitions (for example, OTA
target partitions), you can group them under a shared subdirectory prefix in
`device`:

```python
    filesystems = {
        "block_devices": [
            {
                "device": "ota/zircon_a",
                "from": {
                    "label": "zircon_a",
                    "parent": "gpt",
                },
            },
            {
                "device": "ota/zircon_b",
                "from": {
                    "label": "zircon_b",
                    "parent": "gpt",
                },
            },
        ],
    },
```

`fshost` creates the intermediate `/block/ota` directory and stages each entry
inside it. You can then route the entire group directory to the component with a
single `offer` and `use` stanza using `subdir: "ota"`:

```cml
// In the parent realm manifest (e.g., my_component.core_shard.cml)
{
    offer: [
        {
            directory: "block",
            from: "parent",
            to: "#my_component",
            subdir: "ota",
            as: "block_ota",
        },
    ],
}
```

```cml
// In my_component.cml
{
    use: [
        {
            directory: "block_ota",
            rights: [ "r*" ],
            path: "/block/ota",
        },
    ],
}
```

Within `my_component`, each partition's `fuchsia.storage.block.Block` protocol
is available at `/block/ota/<name>/fuchsia.storage.block.Block` (or you can
still route a single nested entry individually using `subdir: "ota/zircon_a"`).

### Publishing a partition to multiple paths

When two components require the same physical partition through different
capability scopes (for example, one component receives only `/block/durable_boot`
while another receives the grouped `/block/ota` directory), list multiple
`block_devices` entries with the same `from` identifiers and distinct `device`
paths:

```python
    filesystems = {
        "block_devices": [
            {
                "device": "durable_boot",
                "from": {
                    "label": "durable_boot",
                    "parent": "gpt",
                },
            },
            {
                "device": "ota/durable_boot",
                "from": {
                    "label": "durable_boot",
                    "parent": "gpt",
                },
            },
        ],
    },
```

When `fshost` matches the first physical partition with `(parent, label)`, it
publishes that partition to every configured `device` path.
