# Troubleshoot network connectivity: SSH daemon

Once the target device's IP address is known, you can verify SSH and Remote
Control Service (RCS) connectivity:

* [SSH connectivity](#ssh-connectivity)
* [Remote Control Service (RCS) over SSH](#remote-control-service)

## SSH connectivity {:#ssh-connectivity}

You can attempt to directly connect to the target device using the `ssh`
command, for example:

```posix-terminal
ssh -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
```

This command will connect and attempt to authenticate using the SSH
private key located at `~/.ssh/fuchsia_ed25519`. If connection fails,
pass the `-v` flag to ssh for more verbose diagnostics, for example:

```posix-terminal
ssh -v -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
```

A lot of output will be produced. Before looking at failures, it helps
to understand what a successful connection involves. Here are the lines
you want to look for:

```none {:.devsite-disable-click-to-copy}
$ ssh -v -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da
...
debug1: Connecting to fe80::ca63:14ff:fe70:51db%zx-c863147051da [fe80::ca63:14ff:fe70:51db%zx-c863147051da] port 22.
debug1: using TCP window size of 4194304 / 4194304
debug1: fd 3 clearing O_NONBLOCK
debug1: Connection established.
...
debug1: Offering public key: /home/user/.ssh/fuchsia_ed25519 ED25519 SHA256:<snip>
debug1: Server accepts key: /home/user/.ssh/fuchsia_ed25519 ED25519 SHA256:<snip>
Authenticated to fe80::ca63:14ff:fe70:51db%zx-c863147051da ([fe80::ca63:14ff:fe70:51db%zx-c863147051da]:22) using "publickey"
...
$
```

You can see the client first establishes a connection, then offers an accepted
key for authentication. If the key is rejected, the session will end with the
following error messages:

```none {:.devsite-disable-click-to-copy}
debug1: Next authentication method: keyboard-interactive
debug1: Authentications that can continue: publickey,keyboard-interactive
debug1: No more authentication methods to try.
user@fe80::ca63:14ff:fe70:51db%zx-c863147051da: Permission denied (publickey,keyboard-interactive).
```

This output indicates that the server is not configured with the public half
of the corresponding key. This often means either the Fuchsia target device was
not provisioned with keys, or there is a key mismatch between the local host
machine and Fuchsia target device. For more details on diagnosing key
mismatches, see
[Troubleshoot SSH keys for Fuchsia devices][troubleshoot-ssh-keys].

## Remote Control Service (RCS) over SSH {:#remote-control-service}

Once an SSH connection succeeds, `ffx` invokes `fdomain_runner` (or
`remote_control_runner`) over the SSH session to establish a connection to the
target's Remote Control Service (RCS) and query `/core/hwinfo`.

If interactive SSH authentication succeeds manually but `ffx` fails to connect
to RCS:

1. Verify non-interactive SSH command execution to isolate SSH transport or
   shell execution issues:

   ```posix-terminal
   ssh -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da echo "SSH connected"
   ```

2. Test invoking `fdomain_runner` directly over SSH (closing `stdin` so the
   runner exits after completing the handshake) to verify that it starts and
   connects to `/core/remote-control` (which prints `OK` on `stdout`):

   ```posix-terminal
   ssh -i ~/.ssh/fuchsia_ed25519 fe80::ca63:14ff:fe70:51db%zx-c863147051da fdomain_runner </dev/null
   ```

   For legacy Overnet connections, invoke
   `remote_control_runner --circuit 0 </dev/null` instead.

If the SSH session immediately drops with `early eof` or the runner fails to
connect to RCS, check the target's serial console logs (`dmesg` / `klog`) to
verify that `/core/sshd-host` and `/core/remote-control` are running and that
the target filesystem is not out of space (`ENOSPC`).

<!-- Reference links -->

[troubleshoot-ssh-keys]: /docs/development/tools/ffx/workflows/troubleshoot-ssh-keys.md
