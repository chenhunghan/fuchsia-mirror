# Network connectivity troubleshoot playbook

This playbook is intended to help diagnose connectivity and discovery issues
between your local Linux host machine and a connected Fuchsia target device
(including physical devices over USB or network, emulators, and remote
instances).

The playbook is split into the following three parts, where each part
represents a category of related issues:

* **Part 1**: [Interface configuration][part-1] (USB enumeration, `udev`
  permissions, and CDC network interfaces)
* **Part 2**: [Device discovery][part-2] (mDNS, USB Fastboot, USB VSOCK,
  emulators, manual targets, Fastboot files, GCE, and UART)
* **Part 3**: [SSH daemon][part-3] (SSH authentication and Remote Control
  Service connectivity)

When diagnosing network connectivity issues, it’s often one of these
three categories that is impeding connection. The approaches in this
playbook are explored using available Linux command line tools without
going through any `ffx` workflows.

Lastly, many of the command arguments and example outputs (for instance,
interface names, IP addresses, etc.) will be different on your machine.
Be sure to substitute the command arguments with the actual values on
your machine when necessary.

<!-- Reference links -->

[part-1]: interface-configuration.md
[part-2]: device-discovery.md
[part-3]: ssh-daemon.md
