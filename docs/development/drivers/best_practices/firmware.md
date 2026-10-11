# Device firmware

Device firmware are binary blobs containing code or configuration data executed
by device hardware.

Drivers, like other Fuchsia components, are packages whose package contents are
mounted at `/pkg` in its incoming namespace (`fdf::Namespace` in C++,
`fdf_component::Incoming` in Rust). Firmware blobs packaged with a driver are
placed under `lib/firmware/` in the driver package and loaded directly by the
driver from `/pkg/lib/firmware/<filename>` using the `fuchsia.io/File` protocol.

Prebuilt device firmware packages are stored in CIPD (Chrome Infrastructure
Package Deployment) and mirrored in Google Storage.

## Create a Firmware Package

To create a firmware package, create a directory containing the following
files:

* One or more firmware files
* A license file
* [README.fuchsia](/docs/development/source_code/third-party-metadata.md)

`README.fuchsia` must contain the required third-party metadata directives:

* `Name`
* `URL`
* `Version`
* `Revision`
* `License`
* `License File`
* `Security Critical`
* `Description`

If this is the first time you uploaded to CIPD from the host system,
authenticate with CIPD:

```posix-terminal
fx cipd auth-login
```

Upload and tag the package in CIPD using the following command:

```posix-terminal
fx cipd create -in <package-directory> -install-mode copy \
    -name <package-name> \
    -tag git_repository:<source-git-repository> \
    -tag git_revision:<source-git-revision>
```

`<package-name>` uses one of the following naming conventions:

* `fuchsia/firmware/<name>` for open-source redistributable firmware.
* `fuchsia_internal/firmware/<name>` or `turquoise_internal/firmware/<name>` for
  internal or non-redistributable vendor firmware.

`<name>` should be a string that identifies the firmware. It may contain
any non-whitespace character. It is helpful to identify the driver that will
use the firmware in the name.

After this step, the package is uploaded to CIPD. Check the
[CIPD browser](https://chrome-infra-packages.appspot.com/#/?path=fuchsia/firmware)
for packages under `fuchsia/firmware`.

## Adding the Firmware Package to the Build

### 1. Add the CIPD package to Jiri manifests

To fetch the prebuilt firmware package into the checkout (typically under
`//prebuilt/...`), add a `<package>` entry to the appropriate Jiri manifest:

* **In `fuchsia.git`**: Add driver firmware packages (both open-source and
  `internal="true"` packages) to `//manifests/prebuilts` (board and bootloader
  firmware packages are declared in `//manifests/firmware`). For example:

  ```xml
  <package name="fuchsia/firmware/<name>"
           version="git_revision:<source-git-revision>"
           path="prebuilt/<subsystem>/firmware/<name>"/>
  ```

  After editing `//manifests/prebuilts` or `//manifests/firmware`, update the
  Jiri lockfile and test fetching the package locally:

  ```posix-terminal
  //manifests/update-lockfiles.sh
  jiri fetch-packages -local-manifest-project=fuchsia
  ```

* **In `integration.git` (internal vendor checkouts)**: Internal vendor firmware
  packages can also be declared in `//integration/internal/vendor/google/firmware`.

### 2. Include the firmware in the driver package

Include the downloaded firmware blob in your driver package at
`lib/firmware/<filename>` so that it appears at `/pkg/lib/firmware/<filename>`
in the driver's incoming namespace.

#### GN

Define a `resource()` target and add it to the `deps` of your
`fuchsia_driver_package()` target. If the firmware package requires internal
access (`internal="true"`), guard the dependency with `if (internal_access)`:

```gn
import("//build/cipd.gni")
import("//build/components.gni")
import("//build/drivers.gni")

resource("my-driver-firmware") {
  sources = [ "//prebuilt/<subsystem>/firmware/<name>/fw.bin" ]
  outputs = [ "lib/firmware/{{source_file_part}}" ]
}

fuchsia_driver_package("my-driver-package") {
  driver_components = [ ":my-driver-component" ]
  deps = []
  if (internal_access) {
    deps += [ ":my-driver-firmware" ]
  }
}
```

#### Bazel

Define a `fuchsia_package_resource()` target and include it in the `resources`
list of your `fuchsia_package()` target:

```bazel
load(
    "@rules_fuchsia//fuchsia:defs.bzl",
    "fuchsia_package",
    "fuchsia_package_resource",
)

fuchsia_package_resource(
    name = "my_driver_firmware",
    src = "//:prebuilt/<subsystem>/firmware/<name>/fw.bin",
    dest = "lib/firmware/fw.bin",
)

fuchsia_package(
    name = "my_driver_pkg",
    package_name = "my-driver",
    components = [":my_driver_component"],
    resources = [":my_driver_firmware"],
)
```

## Loading Firmware in a Driver

In a C++ driver, open `/pkg/lib/firmware/<filename>` from the driver's incoming
namespace (`incoming()`) using `fuchsia.io/File` and call `GetBackingMemory` to
obtain a read-only VMO containing the firmware:

```cpp
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

zx::result<zx::vmo> LoadFirmware(fdf::Namespace& incoming,
                                 std::string_view filename,
                                 size_t* out_size) {
  std::string full_path = std::string("/pkg/lib/firmware/").append(filename);
  constexpr fuchsia_io::Flags kOpenFlags =
      fuchsia_io::Flags::kPermReadBytes | fuchsia_io::Flags::kProtocolFile;

  zx::result client = incoming.Open<fuchsia_io::File>(full_path.c_str(), kOpenFlags);
  if (client.is_error()) {
    return client.take_error();
  }

  fidl::WireResult result =
      fidl::WireCall(*client)->GetBackingMemory(fuchsia_io::wire::VmoFlags::kRead);
  if (!result.ok()) {
    return zx::error(result.is_peer_closed() ? ZX_ERR_NOT_FOUND : result.status());
  }
  if (result->is_error()) {
    return zx::error(result->error_value());
  }

  zx::vmo& vmo = result->value()->vmo;
  if (zx_status_t status = vmo.get_prop_content_size(out_size); status != ZX_OK) {
    return zx::error(status);
  }

  return zx::ok(std::move(vmo));
}
```
