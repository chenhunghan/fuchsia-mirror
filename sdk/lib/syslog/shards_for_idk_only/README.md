# Syslog SDK Shards

The component manifest shards in this directory (`client.shard.cml`,
`offer.shard.cml`, and `use.shard.cml`) are for **out-of-tree SDK users only**
and are **not intended for in-tree use**.

In-tree components should instead use the shards in the parent directory
(`//sdk/lib/syslog/`):

- `//sdk/lib/syslog/client.shard.cml` (included as `syslog/client.shard.cml`)
- `//sdk/lib/syslog/offer.shard.cml` (included as `syslog/offer.shard.cml`)
- `//sdk/lib/syslog/use.shard.cml` (included as `syslog/use.shard.cml`)

Two sets of shards are maintained for compatibility with older SDK consumers.
