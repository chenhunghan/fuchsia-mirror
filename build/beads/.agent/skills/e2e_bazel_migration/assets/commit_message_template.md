# Commit Message Template

This is the commit message template for Bazel migration.

```
[bazel_migration] Migrate <package_path_of_the_migrating_target>

Migrate <package_path_of_the_migrating_target> from GN to Bazel by using "migrating-host-tool-to-bazel" skill.

Bug: <number_of_the_Buganizer_ticket_for_the_migrating_target>

<list_of_migrated_targets>

<list_of_verified_tests>

```

Package Path Example:

*   The package path of a GN target `//path/to/the/ffx/lib/package:my_target` is
    "path/to/the/ffx/lib/package"

Migrated Targets Block Example:
```
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice_test
```

Verified Tests Block Example:
```
Test: fx build //src/storage/lib/ptr_slice:ptr_slice
Test: fx build --host @//src/storage/lib/ptr_slice:ptr_slice
```

Note:

*   The first line of the commit message should be less than 64 characters.
*   Except the first line, other lines should be less than 70 characters.
