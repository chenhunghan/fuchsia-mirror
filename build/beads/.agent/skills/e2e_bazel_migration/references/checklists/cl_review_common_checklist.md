# Common CL Review Checklist for Bazel Migration

This checklist defines the universal review criteria and standards for code changes migrating build targets from GN to Bazel across all target types.

---

## 1. File Structure, Copyright & Licensing

* [ ] **Copyright Header (Bazel):** Newly created `BUILD.bazel` files include the standard Fuchsia copyright header with the current year (or the original upload year if applicable).
* [ ] **Copyright Header (GN):** Existing copyright headers in `BUILD.gn` files remain intact and unmodified.
* [ ] **Default Applicable Licenses:** Newly created `BUILD.bazel` files include `package(default_applicable_licenses = ["//:license"])`, placed after all `load(...)` statements to comply with Bazel syntax.
* [ ] **Target Name Parity:** Target names defined in `BUILD.bazel` match the legacy target names in `BUILD.gn`.

---

## 2. Comment & Metadata Preservation

* [ ] **Full Comment Migration:** All comments, inline `TODO` items, and bug links from `BUILD.gn` (except the file copyright block) are preserved and migrated to `BUILD.bazel`.
* [ ] **Relative Placement:** Comments located above or on the same line as an attribute in `BUILD.gn` are placed in the identical relative position with respect to the mapped attribute in `BUILD.bazel` *(e.g., a comment above `excluded_checks = [` sits directly above `excluded_checks = [` in `BUILD.bazel`)*.
* [ ] **Comment Integrity:** Existing comments are not reworded or dropped unless they reference GN-specific mechanisms that are fully decommissioned.

---

## 3. Visibility Scoping

* [ ] **No Package Default Visibility:** Avoid setting package-level default visibility (`package(default_visibility = [...])`).
* [ ] **Restrictive Target Visibility:** Set target-level `visibility` as restrictively as possible on individual targets based on the `determining_bazel_visibility` skill (prefer private/package-local or specific packages over `"//visibility:public"`).

---

## 4. Attribute Mapping & Parity

* [ ] **Universal Field Mappings:** Universal attribute mappings are correctly applied:
  - `sources` -> `srcs`
  - `public_deps` -> `deps`
  - `deps` -> `implementation_deps` or `deps`
* [ ] **Boolean Capitalization:** Boolean values in Starlark are capitalized (`True` / `False`).
* [ ] **Value Parity:** Mapped attribute values match between GN and Bazel configurations.
* [ ] **No Extra Attributes:** Extra attributes (except for `visibility` and platform compatibility constraints) are not added to `BUILD.bazel` if not present in `BUILD.gn`.

---

## 5. Code Formatting & Graph Consistency

* [ ] **Code Formatting:** All modified and created `BUILD.gn` and `BUILD.bazel` files pass buildifier and code formatting (`fx format-code --parallel`).
* [ ] **GN Graph Validation:** `fx gen` executes cleanly without build graph errors or broken dependencies.

---

## 6. Commit Message Requirements

* [ ] **Subject Tag:** Subject line includes the `[bazel_migration]` prefix tag *(e.g., `[bazel_migration] Migrate <target_path> to Bazel`)*.
* [ ] **Subject Line Length:** The first line of the commit message is fewer than 64 characters and uses the imperative mood.
* [ ] **Body Line Length:** Body lines are wrapped at fewer than 70 characters.
* [ ] **Bug Footer:** Commit message includes a `Bug: <issue-id>` footer linking to the tracking issue.
* [ ] **Test Footer:** Commit message includes a `Test:` footer documenting how the migration was verified.
