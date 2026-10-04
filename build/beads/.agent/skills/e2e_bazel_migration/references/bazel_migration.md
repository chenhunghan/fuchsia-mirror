# Bazel Migration

## dependency check

Check the targets within the **migration package** and their external dependencies.
    * If any of them have external dependencies that have not yet been migrated to Bazel, or if no appropriate skill is available to migrate them, then skip the migration for these targets.
    * If all targets cannot be migrated, fail the Bazel Migration step with the reason: "All targets in the `<package_name>` have unmigrated external dependencies or cannot be migrated."
        * `<package_name>` : The GN label of the **migration package**.


## migration

Use the appropriate agent skill based on the type of the migrated target.
    * If any errors happened during the migration and block the migration process, fail the Bazel Migration step with the reason: "Fail to migrate `<target_name>` because `<the_error_description>`."


### host tools targets:

Use the "migrating-host-tool-to-bazel" skill to migrate the host tool related targets.


### FIDL libraries under //sdk/fidl directory:

Use the "migrating-fidl-to-bazel" skill to migrate the FIDL libraries under `//sdk/fidl` directory.
