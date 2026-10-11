// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_url::fuchsia_pkg::PinnedAbsolutePackageUrl;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "version", content = "content", deny_unknown_fields)]
enum Packages {
    #[serde(rename = "1")]
    V1(Vec<PinnedAbsolutePackageUrl>),
}

/// ParsePackageError represents any error which might occur while parsing
/// `packages.json` contents.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum ParsePackageError {
    #[error("could not parse url from line: {0:?}")]
    URLParseError(String, #[source] fuchsia_url::errors::ParseError),

    #[error("json parsing error while reading `packages.json`")]
    JsonError(#[source] serde_json::error::Error),
}

/// SerializePackageError represents any error which might occur while writing
/// `packages.json` for an update package.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum SerializePackageError {
    #[error("serialization error while constructing `packages.json`")]
    JsonError(#[source] serde_json::error::Error),
}

/// Returns structured `packages.json` data based on file contents string.
pub fn parse_packages_json(
    contents: &[u8],
) -> Result<Vec<PinnedAbsolutePackageUrl>, ParsePackageError> {
    match serde_json::from_slice(contents).map_err(ParsePackageError::JsonError)? {
        Packages::V1(packages) => Ok(packages),
    }
}

/// Returns serialized `packages.json` contents based package URLs.
pub fn serialize_packages_json(
    pkg_urls: &[PinnedAbsolutePackageUrl],
) -> Result<Vec<u8>, SerializePackageError> {
    serde_json::to_vec(&Packages::V1(pkg_urls.into())).map_err(SerializePackageError::JsonError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    fn pkg_urls<'a>(v: impl IntoIterator<Item = &'a str>) -> Vec<PinnedAbsolutePackageUrl> {
        v.into_iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn smoke_test_parse_packages_json() {
        let pkg_urls = pkg_urls([
            "fuchsia-pkg://fuchsia.com/ls/0?hash=71bad1a35b87a073f72f582065f6b6efec7b6a4a129868f37f6131f02107f1ea",
            "fuchsia-pkg://fuchsia.com/pkg-resolver/0?hash=26d43a3fc32eaa65e6981791874b6ab80fae31fbfca1ce8c31ab64275fd4e8c0",
        ]);
        let packages = Packages::V1(pkg_urls.clone());
        let packages_json = serde_json::to_vec(&packages).unwrap();
        assert_eq!(parse_packages_json(&packages_json).unwrap(), pkg_urls);
    }

    #[test]
    fn smoke_test_serialize_packages_json() {
        let input = pkg_urls([
            "fuchsia-pkg://fuchsia.com/ls/0?hash=71bad1a35b87a073f72f582065f6b6efec7b6a4a129868f37f6131f02107f1ea",
            "fuchsia-pkg://fuchsia.com/pkg-resolver/0?hash=26d43a3fc32eaa65e6981791874b6ab80fae31fbfca1ce8c31ab64275fd4e8c0",
        ]);
        let output =
            parse_packages_json(serialize_packages_json(input.as_slice()).unwrap().as_slice())
                .unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn expect_failure_parse_packages_json() {
        assert_matches!(parse_packages_json(&[]), Err(ParsePackageError::JsonError(_)));
    }

    #[test]
    fn reject_unpinned_urls() {
        assert_matches!(
            parse_packages_json(serde_json::json!({
                "version": "1",
                "content": ["fuchsia-pkg://fuchsia.example/unpinned"]
            })
            .to_string().as_bytes()),
            Err(ParsePackageError::JsonError(e)) if e.to_string().contains("missing hash")
        )
    }
}
