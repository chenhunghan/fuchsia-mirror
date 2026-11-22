# Troubleshoot SSH keys for Fuchsia devices

If you're having trouble establishing an SSH connection to your Fuchsia device
using `ffx` commands, this guide can help you diagnose and resolve common SSH
key issues.

## Concepts

A common issue when connecting to a Fuchsia device over SSH is a mismatch
between the SSH keys on your host machine and the authorized keys on the device.
To help with this, `ffx` provides tools to verify your SSH keys. This feature is
only available on `eng` and `userdebug` builds for security reasons.

The `ffx verify-ssh-keys` tool checks for key mismatches. This check is also
integrated into `ffx doctor` for more general troubleshooting.

When a mismatch is detected, you may need to update the keys on your device or
ensure your host machine has the correct keys. For information on creating and
managing SSH keys, see [Create SSH keys for Fuchsia devices][create-ssh-keys].

## Verify SSH keys manually

To manually check if your local public key matches one of the public keys that
the device expects (one included in the `authorized_keys` file), run the
following command:

```posix-terminal
ffx verify-ssh-keys
```

If your keys are set up correctly, the command will exit silently.

If there is a mismatch, you will see an error message indicating that the public
key on the device does not match the local private key.

## Automatic verification with ffx doctor

The `ffx doctor` command runs a series of checks to diagnose issues with your
development setup, including SSH key verification.

To run `ffx doctor`, use the following command:

```posix-terminal
ffx doctor
```

If `ffx doctor` finds an issue with your SSH keys, it will report it along with
other potential problems.

## Resolving SSH key issues

If either `ffx verify-ssh-keys` or `ffx doctor` reports an SSH key mismatch,
follow these steps:

1.  **Check your key configuration**:

    *   Private keys:

        ```posix-terminal
        ffx config get ssh.priv
        ```

    *   Public keys:

        ```posix-terminal
        ffx config get ssh.pub
        ```

    This shows where `ffx` is looking for your SSH keys. Ensure these are the
    correct locations.

2.  **Ensure keys exist**: If the keys are missing from the configured paths,
    you can generate keys with the following command:

    ```posix-terminal
    ffx config check-ssh-keys --create
    ```

    This will generate new keys if they don't exist, or update the public key
    file if it's missing the public key corresponding to your private key.

3.  **Update keys on device**: If you have multiple development machines or have
    regenerated your keys, the device might have an old set of authorized keys.
    You may need to re-flash the device or update the `fuchsia_authorized_keys`
    on the device. For more details, see
    [Create SSH keys for Fuchsia devices][create-ssh-keys].

## Manual verification using Linux commands {:#manual-linux-verification}

You can also inspect and compare SSH keys manually without `ffx`:

1.  **Check file permissions and local public keys**:

    Verify that your private key has `0600` permissions (OpenSSH rejects
    world- or group-readable private keys) and inspect the public keys in
    `~/.ssh/` and your `ssh-agent`:

    ```posix-terminal
    ls -la ~/.ssh/fuchsia_ed25519*
    cat ~/.ssh/fuchsia_authorized_keys
    ssh-add -L
    ```

2.  **Query the device's `authorized_keys` over HTTP (`eng` / `userdebug`
    builds)**:

    On `eng` and `userdebug` builds, the target serves its `authorized_keys`
    over HTTP on port `9797` (from the host, `ffx` queries the port configured
    via `ssh.authorized_keys_server_port`, which defaults to `9797`). You can
    fetch the device's expected public keys directly with `curl`:

    ```posix-terminal
    curl "http://[fe80::ca63:14ff:fe70:51db%zx-c863147051da]:9797"
    ```

    Compare the returned `ssh-ed25519` key strings against your local
    `~/.ssh/fuchsia_authorized_keys` (or
    `ssh-keygen -y -f ~/.ssh/fuchsia_ed25519`) and `ssh-add -L` output.

<!-- Reference links -->

[create-ssh-keys]: /docs/development/tools/ffx/workflows/create-ssh-keys-for-devices.md
