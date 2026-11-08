// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use dml_config::dump::run_dump_parsed_cli;
use dml_config::parser::DEFAULT_DML_PARSER_CONFIG;

fn main() -> anyhow::Result<()> {
    run_dump_parsed_cli(&DEFAULT_DML_PARSER_CONFIG, None, None, None, None)
}
