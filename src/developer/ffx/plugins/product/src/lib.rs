// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fho::subtool_suite::{FfxSubtoolSuite, Subtool, SubtoolBox, SubtoolSuite};

mod args;
use args::{ProductCommand, ProductSubCommand};

pub struct ProductSuite;

impl SubtoolSuite for ProductSuite {
    type Command = ProductCommand;

    fn new_subtool(cmd: ProductCommand) -> Box<dyn SubtoolBox> {
        match cmd.subcommand {
            ProductSubCommand::CreateOld(cmd) => {
                Subtool::<ffx_product_create_old::ProductCreateTool>::new(cmd)
            }
            ProductSubCommand::Download(cmd) => {
                Subtool::<ffx_product_download::PbDownloadTool>::new(cmd)
            }
            ProductSubCommand::ExtractBlobs(cmd) => {
                Subtool::<ffx_product_extract_blobs::PbExtractBlobsTool>::new(cmd)
            }
            ProductSubCommand::GetArtifacts(cmd) => {
                Subtool::<ffx_product_get_artifacts::PbGetArtifactsTool>::new(cmd)
            }
            ProductSubCommand::GetImagePath(cmd) => {
                Subtool::<ffx_product_get_image_path::PbGetImagePathTool>::new(cmd)
            }
            ProductSubCommand::GetRepository(cmd) => {
                Subtool::<ffx_product_get_repository::ProductGetRepoTool>::new(cmd)
            }
            ProductSubCommand::GetVersion(cmd) => {
                Subtool::<ffx_product_get_version::PbGetVersionTool>::new(cmd)
            }
            ProductSubCommand::List(cmd) => Subtool::<ffx_product_list::ProductListTool>::new(cmd),
            ProductSubCommand::Lookup(cmd) => Subtool::<ffx_product_lookup::PbLookupTool>::new(cmd),
            ProductSubCommand::Show(cmd) => Subtool::<ffx_product_show::ProductShowTool>::new(cmd),
        }
    }
}

pub type ProductSuiteTool = FfxSubtoolSuite<ProductSuite>;
