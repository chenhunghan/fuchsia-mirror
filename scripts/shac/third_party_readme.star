# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC check requiring README.fuchsia to be updated alongside third_party changes."""

_README_NAME = "README.fuchsia"

# Commit message footer that waives the check. A reason must follow the colon.
_SKIP_FOOTER = "Skip-Readme-Check:"

# Paths subject to this check, in gitignore syntax (later patterns win).
_CHECKED_GLOBS = [
    "/third_party/**",
    # Rust crates are managed with Cargo (`fx update-rustc-third-party`), and
    # the forks/compat shims under this directory mostly don't track an
    # upstream revision at all.
    "!/third_party/rust_crates/**",
]

def _nearest_readme_dir(path, readme_dirs):
    """Returns the closest ancestor of `path` that contains a README.fuchsia.

    README.fuchsia files live at varying depths under third_party/ (e.g.
    third_party/curl, third_party/pylibs/mypy, third_party/github.com/google/cppdap),
    so the dependency a file belongs to is the nearest enclosing directory with a
    README.fuchsia. Only directories strictly below third_party/ are considered.

    Args:
        path: Repo-relative path of a file under third_party/.
        readme_dirs: Set of repo-relative directories containing a README.fuchsia.

    Returns:
        The directory path, or None if no enclosing README.fuchsia exists.
    """
    parts = path.split("/")
    for depth in range(len(parts) - 1, 1, -1):
        candidate = "/".join(parts[:depth])
        if candidate in readme_dirs:
            return candidate
    return None

def _third_party_readme_update(ctx):
    """Requires README.fuchsia to be updated when a dependency changes.

    Any change to a file under third_party/ (an uprev, a local patch, a build
    file edit, etc.) must also modify the dependency's README.fuchsia, so that
    its Revision field, which the third-party freshness dashboard reads, is
    kept current. Changes to the README.fuchsia alone are always allowed. The
    requirement is waived by adding a `Skip-Readme-Check: <reason>` footer to
    the commit message.

    shac does not expose a file's previous contents, so whether the Revision
    value itself changed cannot be verified; the README's fields are validated
    separately by the readme_fuchsia_required_fields check.

    Only files tracked directly in fuchsia.git are visible to this check. Most
    dependencies live in their own repository (the third_party/<dep>/src
    submodules pinned by the jiri manifest), and rolling one changes neither
    a file under third_party/ nor anything ctx.scm.affected_files() reports.

    Files with no enclosing README.fuchsia (e.g. under third_party/bazel_vendor)
    are not checked; requiring a README.fuchsia to exist is a separate policy.
    Neither is anything under third_party/rust_crates, see _CHECKED_GLOBS.

    Args:
        ctx: A ctx instance.
    """
    affected_files = ctx.scm.affected_files(glob = _CHECKED_GLOBS, include_deleted = True)

    # Under `shac check --all` (how presubmit runs) every file in the repository
    # is returned, and only the files git reports as modified have a non-empty
    # `action`. Only those are changes. When shac is given an explicit file
    # list instead (e.g. `fx lint --files=...`), `action` is never set and the
    # check does not run, since nothing distinguishes changed files from the
    # rest; see the note on `action` in cpp.star.
    modified_files = [path for path, meta in affected_files.items() if meta.action]
    if not modified_files:
        return

    commits = ctx.scm.commits()
    for commit in commits:
        for line in commit.message.splitlines():
            if not line.startswith(_SKIP_FOOTER):
                continue
            if line[len(_SKIP_FOOTER):].strip():
                # The check has been explicitly waived.
                return
            ctx.emit.commit_message_finding(
                level = "error",
                message = "The `%s` footer must be followed by a reason." % _SKIP_FOOTER,
                commit = commit,
            )

    readme_dirs = set()
    for path in ctx.scm.all_files(glob = _README_NAME):
        readme_dirs.add(path[:-len("/" + _README_NAME)])

    # READMEs of the dependencies that have modified files. A README never
    # counts as a modified file of its own dependency: a change to the README
    # alone is always allowed, but once any other file of the dependency
    # changes, the README must change too.
    required = set()
    for path in modified_files:
        readme_dir = _nearest_readme_dir(path, readme_dirs)
        if readme_dir:
            readme = readme_dir + "/" + _README_NAME
            if path != readme:
                required.add(readme)

    for readme in sorted(required):
        if readme in modified_files:
            continue
        readme_dir = readme[:-len("/" + _README_NAME)]
        message = (
            "Files under %s were modified, but %s was not.\n\n" % (readme_dir, readme) +
            "If this change merges a new upstream version, update the Revision " +
            "field in %s.\n\n" % readme +
            "Otherwise, add the following footer to the commit message to " +
            "bypass this check:\n\n%s <reason>" % _SKIP_FOOTER
        )
        if commits:
            # Attach the finding to the commit message, where the footer would
            # be added, since the README may not be in the change at all.
            ctx.emit.commit_message_finding(
                level = "error",
                message = message,
                commit = commits[0],
            )
        else:
            # No commits to attach to (e.g. uncommitted local changes).
            ctx.emit.finding(level = "error", message = message)

def register_third_party_readme_checks():
    shac.register_check(_third_party_readme_update)
