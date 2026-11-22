// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod gerrit;
mod git;
mod graph;
mod jiri;
mod message;
mod revert;

use anyhow::Result;
use argh::FromArgs;
use revert::{RevertEngine, RevertOptions};

/// Revert a submitted Gerrit change via Gerrit's REST API.
#[derive(FromArgs, Debug)]
struct Args {
    /// full Gerrit URL to the change (e.g. https://fuchsia-review.googlesource.com/c/fuchsia/+/123456)
    #[argh(positional)]
    gerrit_url: String,

    /// custom reason for revert in commit message
    #[argh(option, short = 'm')]
    reason: Option<String>,

    /// link to builder failure / test failure (e.g. go/bbid/... or https://ci.chromium.org/...)
    #[argh(option, short = 'f')]
    failure_link: Option<String>,

    /// associated bug / issue ID (e.g. 12345 or b/12345)
    #[argh(option, short = 'b')]
    bug: Option<String>,

    /// upload generated revert commits to Gerrit via Gerrit REST API with Rubber Stamper reviewer
    /// (without this flag, only the plan is printed)
    #[argh(switch)]
    upload: bool,

    /// preview the planned revert without calling the Gerrit revert API
    #[argh(switch)]
    dry_run: bool,

    /// apply Fuchsia-Auto-Submit +1 vote to created revert CLs (default: off)
    #[argh(switch)]
    auto_submit: bool,
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let engine = RevertEngine::new();
    let opts = RevertOptions {
        gerrit_url: args.gerrit_url,
        reason: args.reason,
        failure_link: args.failure_link,
        bug: args.bug,
        upload: args.upload,
        dry_run: args.dry_run,
        auto_submit: args.auto_submit,
        fuchsia_dir: None,
    };
    engine.run(opts)
}
