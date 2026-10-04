# power_stats_util

`power_stats_util` is a command-line diagnostic utility that prints the power
state residencies reported by a `fuchsia.hardware.power.stats.Service`
provider.

## Usage

Typically run inside the provider's sandbox using `ffx component explore`:

```bash
$ ffx component explore <provider_moniker>
$ power_stats_util
```

To read a single power entity:

```bash
$ power_stats_util <entity>
```

For each entity it prints the entry count and total residency (in
milliseconds) of every state.
