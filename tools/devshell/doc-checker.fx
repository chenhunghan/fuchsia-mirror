# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

#### CATEGORY=Code submission and review
#### EXECUTABLE=${HOST_TOOLS_DIR}/doc-checker
### Check the markdown documentation using a variety of checks.
## USAGE:
##     fx doc-checker [--root <root>] [--project <project>]
##                    [--docs-folder <docs-folder>] [--check-external-links]
##                    [--json] [--allow-fuchsia-src-links]
##                    [--reference-docs-root <reference-docs-root>]
##                    [--skip-link-check] [--allow-unreferenced-hidden]
##                    [--reference-folder <reference-folder>]
## Options:
##  --root            path to the root of the checkout of the project.
##  --project         name of project to check, defaults to fuchsia.
##  --docs-folder     (Experimental) Name of the folder inside the project which
##                    contains documents to check. Defaults to 'docs'.
##  --check-external-links
##                    resolve http(s) links (skipped by default).
##  --json            output in JSON format.
##  --allow-fuchsia-src-links
##                    allow links to fuchsia-src. Usually links to fuchsia-src
##                    should be written as file paths to /docs.
##  --reference-docs-root
##                    path to reference docs. Links to the main docs folder.
##  --skip-link-check do not check links between docs.
##  --allow-unreferenced-hidden
##                    allow unreferenced hidden files (files starting with _) from
##                    being published.
##  --reference-folder
##                    path on the published site where generated reference docs
##                    live. Defaults to '/reference'.
##  --help, help      display usage information.
