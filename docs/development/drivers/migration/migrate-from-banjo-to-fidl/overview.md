# Migrate from Banjo to FIDL

In Fuchsia, all communications between drivers and non-drivers occur over
[FIDL (Fuchsia Interface Definition Language)][fidl] calls. If your driver uses
the Banjo protocol, you'll need to update the driver to make only FIDL calls.

In short, migrating a driver from Banjo to FIDL involves the
steps below:

1. Update the driver's `.fidl` file to create a new FIDL interface.
2. Update the driver's source code to use the new interface.
3. Build and test the driver using the new FIDL interface.

## Before you start {:#before-you-start}

Prior to starting migration tasks, first check out the
[**Frequently asked questions**][faq] page. This can help you identify
special conditions or edge cases that may apply to your driver.

## List of migration tasks {:#list-of-migration-tasks}

- [**Convert Banjo protocols to FIDL protocols**][convert-banjo-to-fidl]:
  Learn how to migrate a driver from using the Banjo protocol to FIDL.

  - [Update the driver from Banjo to FIDL][update-banjo-to-fidl]
  - (Optional) [Update the driver to use two-way communication][update-two-way-communication]
  - [Additional resources][additional-resources]

<!-- Reference links -->

[fidl]: /docs/concepts/fidl/overview.md
[faq]: /docs/development/drivers/migration/migrate-from-banjo-to-fidl/faq.md
[update-banjo-to-fidl]: convert-banjo-protocols-to-fidl-protocols.md#update-the-dfv1-driver-from-banjo-to-fidl
[update-two-way-communication]: convert-banjo-protocols-to-fidl-protocols.md#update-the-dfv1-driver-to-use-two-way-communication
[additional-resources]: convert-banjo-protocols-to-fidl-protocols.md#additional-resources
[convert-banjo-to-fidl]: convert-banjo-protocols-to-fidl-protocols.md

