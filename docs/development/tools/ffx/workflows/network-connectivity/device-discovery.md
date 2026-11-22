# Troubleshoot network connectivity: Device discovery

Once your physical or virtual connection is configured, you can probe for the
presence of the Fuchsia target device across each discovery mechanism supported
by `ffx`.

The sections below show how to manually verify and debug each discovery source
using standard Linux command-line tools:

* [Multicast DNS (mDNS) resolution](#multicast-dns-resolution)
* [mDNS packet inspection](#mdns-packet-inspection)
* [mDNS firewall and netfilter rules](#mdns-firewall-and-netfilter-rules)
* [Multicast ping](#multicast-ping)
* [USB Fastboot discovery](#usb-fastboot-discovery)
* [USB VSOCK discovery](#usb-vsock-discovery)
* [Emulator discovery](#emulator-discovery)
* [Manual target discovery](#manual-target-discovery)
* [Network Fastboot discovery](#fastboot-file-discovery)
* [GCE instance discovery](#gce-discovery)
* [UART discovery](#uart-discovery)

The mDNS examples on this page use [`avahi`][avahi] to inspect mDNS records. The
`avahi` toolchain communicates with a local daemon, which may or may not be
running. If the daemon is not running, `avahi` commands will generally
fail with a message saying `Daemon not running`.

When needed, you can start a daemon as a foreground process. In a terminal,
run the following command:

```posix-terminal
sudo avahi-daemon
```

The daemon will cache mDNS queries as they are encountered, and it can be safely
killed when finished.

## Multicast DNS resolution {:#multicast-dns-resolution}

Device discovery uses multicast DNS (mDNS) to determine the IP address of the
Fuchsia target device. Fuchsia target devices broadcast their IP information to
mDNS with a service type of `_fuchsia._udp`.

To query this information with `avahi`, run the following command:

```posix-terminal
avahi-browse --resolve _fuchsia._udp
```

This command will appear to hang. It first dumps any records in the `avahi`
cache, and then sits and listens for mDNS broadcasts. If you use the `--resolve`
flag, `avahi` subsequently tries to resolve any records encountered. Resolution
yields the IPv6 address of the Fuchsia target device. For example:

```none {:.devsite-disable-click-to-copy}
$ avahi-browse --resolve _fuchsia._udp
+ zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
= zx-c863147051da IPv6 fuchsia-c863-1470-51da  _fuchsia._udp  local
   hostname = [fuchsia-c863-1470-51da.local]
   address = [fe80::ca63:14ff:fe70:51db]
   port = [5353]
   txt = []
```

In this example, you can see that a host record was found that contains the
hostname of `fuchsia-c863-1470-51da` which then resolved to
`fe80::ca63:14ff:fe70:51db`. This is the address of the connected Fuchsia
target device.

## mDNS packet inspection {:#mdns-packet-inspection}

If no records are found, you can look to see if the target device is replying
to mDNS queries. mDNS operates on port 5353, which can be inspected with
`tcpdump`. When you run this command, restrict the command to your target
interface and filter the results by the IPv6 protocol and mDNS port. For
example:

```posix-terminal
sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
```

This command prints output similar to the following, which shows an mDNS query
and the response cycle:

```none {:.devsite-disable-click-to-copy}
$ sudo tcpdump -n -i zx-c863147051da "ip6 && port 5353"
13:32:41.725007 IP 169.254.31.167.58994 > 224.0.0.251.5353:
    0 PTR (QU)? _fuchsia._udp.local. (37)
13:32:41.725057 IP 169.254.31.167.58994 > 224.0.0.251.5353:
    0 PTR (QU)? _fastboot._tcp.local. (38)
13:32:43.724131 IP6 fe80::ca63:14ff:fe70:51da.34810 > ff02::fb.5353:
    0 PTR (QU)? _fuchsia._udp.local. (37)
13:32:43.724183 IP6 fe80::ca63:14ff:fe70:51da.34810 > ff02::fb.5353:
    0 PTR (QU)? _fastboot._tcp.local. (38)
13:32:43.725635 IP6 fe80::ca63:14ff:fe70:51db.5353 > fe80::ca63:14ff:fe70:51da.34810:
    0*- [0q] 1/0/3 PTR fuchsia-c863-1470-51da._fuchsia._udp.local. (152)
```

## mDNS firewall and netfilter rules {:#mdns-firewall-and-netfilter-rules}

In some cases, you may be a need to check the kernel’s [`netfilter`][netfilter]
rules to discover a device through mDNS.

### ufw rules {:#ufw-rules}

`ufw` is a utility for managing the kernel's netfilter rules. If you have this
installed on your machine, you can check that your firewall is properly set up,
run the following:

```posix-terminal
sudo ufw status
```

This should return something similar to the following:

```none {:.devsite-disable-click-to-copy}
Status: active

To                         Action      From
--                         ------      ----
Anywhere (v6)              ALLOW       fe80::/10 5353/udp         # Fuchsia MDNS
33331:33340/udp            ALLOW       fe80::/10                  # Fuchsia Netboot Protocol
8083/tcp                   ALLOW       fe80::/10                  # Fuchsia Package Server
33331:33340/udp            ALLOW       fc00::/7                   # Fuchsia Netboot Prot
ocol
8083/tcp                   ALLOW       fc00::/7                   # Fuchsia Package Server
Anywhere (v6)              ALLOW       fc00::/7 5353/udp          # Fuchsia MDNS
```

If this isn't the case (i.e. the output is empty), run the following command:

```posix-terminal
fx setup-ufw
```

Running this command sets up several UFW rules for netfilter.

### netfilter rules

There may also be additional netfilter restrictions in place.

#### firewalld {:#firewalld}

To check the rules that are running on your machine, run the following:

```posix-terminal
sudo nft list tables
```

You may need to adjust rules if the output looks like the following:

```none {:.devsite-disable-click-to-copy}
$ sudo nft list tables
table ip filter
table ip nat
table inet firewalld
```

Most setups will likely have the first two rules:

```none {:.devsite-disable-click-to-copy}
table ip filter
table ip nat
```

To remove the `firewalld` rule, you can run the following commands:

```posix-terminal
sudo systemctl stop firewalld
sudo systemctl disable firewalld
sudo nft delete table inet firewalld
```

mDNS traffic occurs on port 5353 over the `224.0.0.251` and `fe02::fb` multicast
addresses (IPv4 and IPv6 respectively). It may be worth inspecting the `filter`
table, looking for any policy applied to port 5353 or these addresses. For
example:

```none {:.devsite-disable-click-to-copy}
$ sudo nft list table ip filter
table ip filter {
    ...
}
```

### Additional netfilter checks

If you've tried the above and the following is true:

* You do not have `ufw` installed on your machine. See [ufw rules](#ufw-rules).
* You do not have `firewalld` running on your machine. See
  [firewalld](#firewalld).
* You are not able to resolve the device's address with `ffx target list`.
* You _are_ able to resolve the device's address with `avahi`. See
  [Multicast DNS resolution](#multicast-dns-resolution).

You may still have some firewall rule blocking unicast mDNS messages. You may
need to add exceptions `iptables` manually in order to allow mDNS for link-local
IPv6 messages:

```posix-terminal
sudo ip6tables -A INPUT -p udp -s fe80::/10 --sport 5353 -m comment --comment 'Fuchsia MDNS' -j ACCEPT
sudo ip6tables -A INPUT -p udp -s fc00::/7 --sport 5353 -m comment --comment 'Fuchsia MDNS' -j ACCEPT
```

If this does not work, you may want to observe what kinds of firewall rules
might be affecting IPv6 traffic and address them accordingly:

```posix-terminal
sudo iptables -L -n
```

## Multicast ping {:#multicast-ping}

Another option to discover the target’s IPv6 address is to ping the local
multicast `ff02::1` device address. For example:

```posix-terminal
ping6 ff02::1%zx-c863147051da
```

This is a predetermined address specified by IANA. For more information, see
[section 2.7.1 of ch 2.7.1 of RFC-4291][rfc-4291].

Note that the local network interface was designated using the `scope-id`
component of an IPv6 address, which is a requirement of pinging a multicast
address. The `-I` flag could have been used to similar effect. Ping responses
come from the Fuchsia device and include their IP address information. For
example:

```none {:.devsite-disable-click-to-copy}
$ ping6 ff02::1%zx-c863147051da
PING ff02::1%zx-c863147051da (ff02::1%zx-c863147051da) 56 data bytes
64 bytes from fe80::ca63:14ff:fe70:51da%zx-c863147051da: icmp_seq=1 ttl=64 time=0.088 ms
64 bytes from fe80::ca63:14ff:fe70:51db%zx-c863147051da: icmp_seq=1 ttl=64 time=0.958 ms
```

Be aware that the localhost may itself reply to the multicast ping. In this
example, `fe80::ca63:14ff:fe70:51da` is the address of the host-side interface
(as determined above), and `fe80::ca63:14ff:fe70:51db` is the address of the
Fuchsia target device.

A good rule of thumb is that the device responding in far less than a
millisecond is generally the localhost responding, and anything approaching one
or more milliseconds to respond is likely the Fuchsia target device.

## USB Fastboot discovery {:#usb-fastboot-discovery}

`ffx` discovers USB Fastboot devices on Linux by scanning `/sys/bus/usb/devices`
for USB devices with Google's Vendor ID (`0x18d1`) that expose a Fastboot USB
interface descriptor:

* `idVendor`: `18d1`
* `bInterfaceClass`: `ff` (Vendor Specific)
* `bInterfaceSubClass`: `42` (Fastboot)
* `bInterfaceProtocol`: `03` (Fastboot)

To manually verify if a connected USB device exposes the Fastboot interface,
inspect its USB descriptors with `lsusb`:

```posix-terminal
lsusb -v -d 18d1:
```

Or inspect the kernel `sysfs` interface descriptors and serial number directly:

```posix-terminal
grep -H . /sys/bus/usb/devices/*/idVendor /sys/bus/usb/devices/*/*/bInterface{Class,SubClass,Protocol}
```

For a matching device directory (for example, `/sys/bus/usb/devices/1-2`), check
its serial number and verify that its `usbfs` device node is writable by your
user:

```posix-terminal
cat /sys/bus/usb/devices/1-2/serial
```

If the device is visible in `sysfs` and `lsusb` with interface `ff/42/03`, you
can also verify direct Fastboot communication using the standalone `fastboot`
CLI tool:

```posix-terminal
fastboot devices -l
fastboot getvar serialno
```

If `fastboot devices` reports `no permissions` or fails to open the device,
check the character device node permissions under `/dev/bus/usb/<BUS>/<DEVICE>`.
See [Are the USB device node permissions configured?][usb-permissions].

## USB VSOCK discovery {:#usb-vsock-discovery}

When `connectivity.enable_usb` is enabled, `ffx` discovers Fuchsia devices in
Product mode over USB using the USB VSOCK bridge protocol. A Fuchsia target
supporting USB VSOCK exposes a vendor-specific USB interface with:

* `idVendor`: `18d1`
* `bInterfaceClass`: `ff` (Vendor Specific)
* `bInterfaceSubClass`: `43` (`VSOCK_BRIDGE`)
* `bInterfaceProtocol`: `00`

On the host, the background `ffx-usb-driver` daemon claims this USB interface
and publishes discovered targets over a local Unix domain socket (by default at
`${XDG_RUNTIME_DIR}/ffx_usb.sock`, configured via
`connectivity.usb_socket_path`).

To manually diagnose USB VSOCK discovery using standard Linux commands:

1. Verify that the device is enumerated on the USB bus with the `ff/43/00`
   VSOCK bridge interface:

   ```posix-terminal
   grep -H . /sys/bus/usb/devices/*/idVendor /sys/bus/usb/devices/*/*/bInterface{Class,SubClass,Protocol}
   ```

2. Verify that your user has write permission to the
   `/dev/bus/usb/<BUS>/<DEVICE>` node (see
   [Are the USB device node permissions configured?][usb-permissions]), and
   check whether another process is holding the USB device open:

   ```posix-terminal
   lsof /dev/bus/usb/001/015
   ```

3. Check that the background `ffx-usb-driver` process is running and listening
   on the Unix domain socket:

   ```posix-terminal
   ps aux | grep usb-driver
   ss -xlp | grep ffx_usb.sock
   ls -l "${XDG_RUNTIME_DIR}/ffx_usb.sock"
   ```

4. Inspect the `ffx-usb-driver` log files for USB bulk endpoint or VSOCK
   synchronization errors:

   ```posix-terminal
   ls -lt "${XDG_STATE_HOME:-$HOME/.local/share}/ffx_usb/"
   tail -n 50 "${XDG_STATE_HOME:-$HOME/.local/share}/ffx_usb/"ffx_usb.*.0.log
   ```

## Emulator discovery {:#emulator-discovery}

`ffx` discovers local emulator instances by scanning the emulator instance
directory (by default `~/.local/share/Fuchsia/ffx/shared/emu/instances`,
configured via `emu.instance_dir`) for `<instance-name>/engine.json` files and
checking whether the emulator process `pid` recorded in `engine.json` is
currently alive.

To manually diagnose emulator discovery using standard Linux commands:

1. Check if any `qemu` or `aemu` emulator processes are running on the host:

   ```posix-terminal
   ps aux | grep -E 'qemu|aemu'
   ```

2. List the instance directories and inspect the `engine.json` state file:

   ```posix-terminal
   ls -la ~/.local/share/Fuchsia/ffx/shared/emu/instances/
   cat ~/.local/share/Fuchsia/ffx/shared/emu/instances/fuchsia-emulator/engine.json
   ```

3. Verify that the process ID (`pid`) recorded in `engine.json` is running:

   ```posix-terminal
   ps -p <pid>
   ```

4. If the emulator process is running in user networking mode (`--net user`),
   check the `port_map` in `engine.json` and verify that the host `ssh` port is
   listening:

   ```posix-terminal
   ss -tlnp | grep <ssh-port>
   ```

   If the emulator was started in Tun/Tap networking mode (`--net tap` without
   VSOCK), `ffx` discovers it via [mDNS](#multicast-dns-resolution) over the
   `qemu` tap interface rather than `engine.json`. Verify that the `qemu` tap
   network interface exists and is up:

   ```posix-terminal
   ip addr show qemu
   ```

5. Inspect the emulator host and serial logs (by default under
   `~/.local/share/Fuchsia/ffx/cache/logs/`, or at the path recorded in
   `emulator_configuration.host.log` in `engine.json`) for boot failures or
   kernel panics:

   ```posix-terminal
   tail -n 50 ~/.local/share/Fuchsia/ffx/cache/logs/emulator-fuchsia-emulator.log
   tail -n 50 ~/.local/share/Fuchsia/ffx/cache/logs/emulator-fuchsia-emulator.log.serial
   ```

For more information on managing emulator instances, see
[Start the Fuchsia emulator][start-emulator].

## Manual target discovery {:#manual-target-discovery}

`ffx` discovers manual targets by reading the `targets.manual` object from the
user configuration file (by default
`~/.local/share/Fuchsia/ffx/config/.ffx_user_config.json`) and attempting a TCP
connection (with a 1-second timeout) to each configured `<ip>:<port>` address.
If the TCP connection fails or times out, `ffx` treats the manual target as
disconnected and omits it from discovery results.

To manually diagnose manual target discovery using standard Linux commands:

1. Inspect the `targets.manual` entries in the user configuration file:

   ```posix-terminal
   cat "${XDG_CONFIG_HOME:-$HOME/.local/share}/Fuchsia/ffx/config/.ffx_user_config.json"
   ```

2. Test TCP reachability to the configured IP address and port directly with
   `nc` (netcat):

   ```posix-terminal
   nc -zv -w 1 <ip> <port>
   ```

3. If the manual target points to a local forwarded port (such as
   `127.0.0.1:8022` from an SSH tunnel), verify that a local process is actively
   listening on that port:

   ```posix-terminal
   ss -tlnp | grep <port>
   ```

4. If the manual target uses a link-local IPv6 address
   (`[fe80::...%<interface>]:<port>`), verify that the network interface scope
   ID exists and has a link-local IPv6 address assigned:

   ```posix-terminal
   ip -6 --oneline addr
   ```

## Network Fastboot discovery {:#fastboot-file-discovery}

`ffx` discovers network-attached Fastboot targets listed in the Fastboot devices
file (by default `~/.fastboot/devices`, configured via
`fastboot.devices_file.path`), as well as Fastboot-over-TCP/UDP targets
advertising `_fastboot._tcp.local.` or `_fastboot._udp.local.` over
[mDNS](#multicast-dns-resolution). In `~/.fastboot/devices`, each non-empty line
must be formatted as `tcp:<ip>:<port>` or `udp:<ip>:<port>`. During discovery,
`ffx` connects to each target and sends a Fastboot `getvar:serialno` query.

To manually diagnose network Fastboot discovery using standard Linux commands:

1. If using a Fastboot devices file, inspect the file and verify that each line
   uses lowercase `tcp:` or `udp:` followed by a valid socket address
   (`<ip>:<port>`):

   ```posix-terminal
   cat ~/.fastboot/devices
   ```

   If the target uses Fastboot over mDNS, check whether `_fastboot._tcp` or
   `_fastboot._udp` is advertised on the network (see
   [Multicast DNS resolution](#multicast-dns-resolution)):

   ```posix-terminal
   avahi-browse --resolve _fastboot._tcp
   avahi-browse --resolve _fastboot._udp
   ```

2. For `tcp:<ip>:<port>` entries, test TCP reachability with `nc`:

   ```posix-terminal
   nc -zv -w 1 <ip> <port>
   ```

3. Test Fastboot protocol communication directly using the standalone `fastboot`
   CLI tool:

   ```posix-terminal
   fastboot -s tcp:<ip>:<port> getvar serialno
   ```

   (Or for UDP targets, use `fastboot -s udp:<ip>:<port> getvar serialno`.)

## GCE instance discovery {:#gce-discovery}

`ffx` discovers Google Compute Engine (GCE) instances by watching the GCE
instance directory (by default
`~/.local/share/Fuchsia/ffx/shared/gce_instances`, configured via
`gce.instance_dir`) for JSON state files named
`<project>_<zone>_<instance-name>.json`. `ffx` reports an instance as
discovered only if the local background SSH tunnel `pid` recorded in the JSON
file is running and `ssh_port` is non-zero.

To manually diagnose GCE discovery using standard Linux commands:

1. List and inspect the GCE instance state files:

   ```posix-terminal
   ls -la ~/.local/share/Fuchsia/ffx/shared/gce_instances/
   cat ~/.local/share/Fuchsia/ffx/shared/gce_instances/<project>_<zone>_<instance-name>.json
   ```

2. Verify that the background SSH tunnel process (`pid` in the JSON file) is
   running and listening on `127.0.0.1:<ssh_port>`:

   ```posix-terminal
   ps -p <pid>
   ss -tlnp | grep <ssh_port>
   ```

3. Test local TCP connectivity to the forwarded SSH port:

   ```posix-terminal
   nc -zv 127.0.0.1 <ssh_port>
   ```

## UART discovery {:#uart-discovery}

When `connectivity.enable_uart` is enabled, `ffx` discovers UART-connected
targets by monitoring the UART runtime directory (by default
`~/.local/share/Fuchsia/ffx/shared/ffx_uart/`) for connection metadata files
matching `ffx_uart_*.json`. `ffx` reports a target as discovered only when its
metadata `status` is `"Connected"` and the `ffx-uart-driver` daemon `pid` is
running.

To manually diagnose UART discovery using standard Linux commands:

1. Verify that the serial character device exists (for example, `/dev/ttyUSB0`)
   and that your user belongs to the `dialout` group:

   ```posix-terminal
   ls -l /dev/ttyUSB*
   id
   ```

2. Check whether another process (such as `minicom`, `screen`, `picocom`, or a
   stale driver instance) holds an exclusive lock or open file descriptor on the
   serial port:

   ```posix-terminal
   lsof /dev/ttyUSB0
   fuser -v /dev/ttyUSB0
   ```

3. Inspect the UART metadata files and verify that the `ffx-uart-driver` daemon
   process is running:

   ```posix-terminal
   ls -la ~/.local/share/Fuchsia/ffx/shared/ffx_uart/
   cat ~/.local/share/Fuchsia/ffx/shared/ffx_uart/ffx_uart_*.json
   ps aux | grep ffx-uart-driver
   ```

For more details on setting up and troubleshooting the UART transport, see
[UART Transport][uart-transport].

<!-- Reference links -->

[avahi]: https://avahi.org/
[netfilter]: https://www.netfilter.org/
[rfc-4291]: https://datatracker.ietf.org/doc/html/rfc4291#section-2.7.1
[usb-permissions]: interface-configuration.md#usb-device-permissions
[start-emulator]: /docs/development/tools/ffx/workflows/start-the-fuchsia-emulator.md
[uart-transport]: /docs/development/tools/ffx/uart.md#troubleshooting
