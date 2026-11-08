// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! link_checker implements the [`DocCheck` trait  used to perform checks on the links and images
//! found in markdown documentation in the Fuchsia project.

use crate::md_element::{CowStr, Element, LinkType};
use crate::path_ext::normalize_path;
use crate::{DocCheck, DocCheckError, DocCheckerArgs, DocLine, DocPathExt, ReachabilityGraph};
use anyhow::{Result, bail};
use async_trait::async_trait;
use fuchsia_hyper::{HttpsClient, TcpOptions, new_https_client_from_tcp_options};
use futures::StreamExt;
use http::uri::Uri;
use http::{Request, StatusCode};
use http_body_util::Full;
use hyper::body::Bytes;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use url::Url;

// path_help is a wrapper to allow mocking path checks
// exists. and is_dir.
cfg_if::cfg_if! {
    if #[cfg(test)] {
       use crate::mock_path_helper_module as path_helper;
    } else {
       use crate::path_helper_module as path_helper;
    }
}
/// Files that are allowed to link to the documentation host site.
const FILES_ALLOWED_TO_LINK_TO_PUBLISHED_DOCS: [&str; 1] = ["navbar.md"];

const GERRIT_HOST: &str = "fuchsia.googlesource.com";

pub(crate) const PUBLISHED_DOCS_HOST: &str = "fuchsia.dev";

/// List of words that cannot be used as single word ALT text for images.
/// This is a pretty small list (n < 5), but if it grows large, it might be
/// better to manages as an external file vs. inline.
const DISALLOWED_ALT_IMAGE_TEXT: [&str; 1] = [""];
// TODO(https://fxbug.dev/42064332): disallow "drawing, "image" for alt text";

/// List of active repos under fuchsia.googlesource.com which can be linked to.
const VALID_PROJECTS: [&str; 23] = [
    "", // root page of all projects
    "antlion",
    "cobalt",
    "cobalt-registry",
    "drivers", // This is a family of projects.
    "experiences",
    "fargo",
    "fidl-misc",
    "fidlbolt",
    "fontdata",
    "fuchsia",
    "infra", // This is a family of projects, there are sub-repos below this path.
    "integration",
    "intellij-language-fidl",
    "jiri",
    "manifest",
    "reference-docs",
    "samples",
    "shac-project",
    "sdk-samples", // This is a family of projects, there are sub-repos below this path.
    "third_party", // This is a family of projects, there are sub-repos below this path.
    "vscode-language-fidl",
    "workstation",
];

/// Top level paths to the published doc site that any page can link to.
/// Links to other locations have to be allowed by adding the source doc
/// page to FILES_ALLOWED_TO_LINK_TO_PUBLISHED_DOCS.
/// "" - is the root of fuchsia.dev
/// "reference" is the generated reference documentation.
/// "schema" is the schema URLs used for parsing.
const PUBLISHED_LINKS_ALLOWED: [&str; 3] = ["", "reference", "schema"];

/// A link (URL, or file path) and it location in the markdown.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct LinkReference {
    pub link: String,
    pub location: DocLine,
}

/// LinkChecker checks the links and images in markdown files.
/// As the links are checked, links to external websites are collected and optionally
/// checked in the post-check.
#[derive(Debug)]
struct LinkChecker {
    pub root_dir: PathBuf,
    pub project: String,
    pub docs_folder: PathBuf,
    pub check_remote_links: bool,
    pub allow_fuchsia_src_links: bool,
    links: Vec<LinkReference>,
    reachability_graph: ReachabilityGraph,
    reference_docs_root: Option<PathBuf>,
    pub(crate) glossary_terms: Option<HashSet<String>>,
}

impl LinkChecker {
    /// Takes the raw link from the markdown parser, and normalizes it into
    /// a string that is a filepath or URL.
    fn make_link_to_check(&self, filename: &Path, link_url: &CowStr<'_>) -> Result<String> {
        let link_to_check: String;
        let link = link_url.trim();
        let filename_string = filename.to_string_lossy();

        // relative_filename is relative to the root e.g. /home/googler/fuchsia/docs/file.md
        // is /docs/file.md.  Note that this form (with the leading /) is used a lot because
        // when published, it is at the root of the documentation site.
        //
        // This does cause confusion in this code since PathBuf::join() does not naively join
        // paths with a leading /. See https://doc.rust-lang.org/nightly/std/path/struct.PathBuf.html#method.push
        // for more details.
        let relative_filename =
            filename_string.strip_prefix(self.root_dir.to_string_lossy().as_ref()).unwrap_or("");

        // relative_parent is the directory of the file name, e.g /docs/file.md is /docs.
        let temp_path = PathBuf::from(relative_filename);
        let relative_parent = temp_path.parent().unwrap_or(&self.docs_folder);

        // External links that have any query parameters are decoded when parsed by the markdown parser.
        // To make things easier later, parse the URL and use the encoded parameters.
        if link.starts_with("http://") || link.starts_with("https://") {
            let url = Url::parse(link).or_else(|e| bail!("Could not parse url {}: {}", link, e))?;
            if let Some(query) = url.query() {
                // split on ? from the original string, to avoid complexities
                // to to-stringizing a url without the query params.
                if let Some((first_part, _)) = link.split_once('?') {
                    let encoded_link = format!("{}?{}", first_part, query);
                    link_to_check = encoded_link;
                } else {
                    bail!(
                        "Cannot parse {}. Appears to have query parameters, but no ? in the string?",
                        link
                    );
                }
            } else {
                link_to_check = url.to_string();
            }
        } else if link.starts_with("/reference") {
            // Generated reference docs are in /reference, and are
            // treated as external since they are not part of the source tree.
            link_to_check = format!("https://{}{}", PUBLISHED_DOCS_HOST, link);
        } else if link.starts_with('/') {
            // paths are used as-is.
            link_to_check =
            // if allowing fuchsia-src, replace it with docs_folder.
            if self.allow_fuchsia_src_links {
                link.replace("/fuchsia-src/", &format!("/{}/", self.docs_folder.display()))
            } else {
                link.to_string()
            };
        } else if link.starts_with('#') {
            // Anchors are appended to the current file.
            link_to_check = format!("{}{}", self.root_dir.join(relative_filename).display(), link);
        } else {
            // Otherwise, see if it is parsable as a URI, if not, append it to the relative_parent
            // and hope for the best. This usually is something relative like "details-subdir/info.md"
            let uri: Uri = match link.parse() {
                Ok(u) => u,
                Err(_e) => {
                    let parent_based_link =
                        format!("{}/{}", relative_parent.to_string_lossy(), link);
                    parent_based_link.parse::<Uri>().or_else(|e| {
                        bail!("Cannot parse parent based uri: {}: {}", parent_based_link, e)
                    })?
                }
            };

            // Check the scheme. If there is one, use it.
            // If there is mailto: (which is commonly used without the //) use the original link
            // Otherwise, it is a relative file path that what parsed.
            link_to_check = match uri.scheme() {
                Some(_) => uri.to_string(),
                None if link.contains("mailto:") => link.to_string(),
                None => format!("{}/{}", relative_parent.to_str().unwrap(), link),
            };
        }
        Ok(link_to_check)
    }
}

#[async_trait]
impl DocCheck for LinkChecker {
    fn name(&self) -> &str {
        "LinkChecker"
    }
    /// Applies the checks for links.
    fn check<'a>(&mut self, element: &'a Element<'_>) -> Result<Option<Vec<DocCheckError>>> {
        let mut errors: Vec<DocCheckError> = vec![];

        // Get all the links from the element. This is needed since the element is commonly
        // a Block or some other collection of elements.
        if let Some(links) = element.get_links() {
            for ele in links {
                let link: &'a CowStr<'a> = match ele {
                    Element::Link(link_type, link_url, link_title, elements, _) => {
                        let link = match link_type {
                            LinkType::Inline => link_url,
                            LinkType::Reference => link_url,
                            LinkType::ReferenceUnknown => {
                                // Glossary reference links (e.g. [text][glossary.term]) are
                                // resolved by DocContext::handle_broken_link to canonical in-tree
                                // links to /docs/glossary/README.md#<anchor>. Process them as normal
                                // links so their targets and fragment anchors are validated against
                                // docs/glossary/_glossary.yaml without routing to handle_shortcut_unknown.
                                if link_url.starts_with("/docs/glossary") {
                                    link_url
                                } else {
                                    errors.extend(handle_shortcut_unknown(
                                        ele, link_url, link_title, elements,
                                    ));
                                    continue;
                                }
                            }
                            LinkType::Collapsed => link_url,
                            LinkType::CollapsedUnknown => {
                                errors.push(DocCheckError::new_error(
                                    ele.doc_line().line_num,
                                    ele.doc_line().file_name.clone(),
                                    &format!(
                                        "Unknown collapsed link to {} ({})",
                                        link_url, link_title
                                    ),
                                ));
                                continue;
                            }
                            LinkType::Shortcut => link_url,
                            LinkType::ShortcutUnknown => {
                                // Shortcut glossary links (e.g. [glossary.term]) are resolved by
                                // DocContext::handle_broken_link to canonical in-tree links
                                // to /docs/glossary/README.md#<anchor>. Process them as normal links
                                // so their targets and fragment anchors are validated against
                                // docs/glossary/_glossary.yaml without routing to handle_shortcut_unknown.
                                if link_url.starts_with("/docs/glossary") {
                                    link_url
                                } else {
                                    errors.extend(handle_shortcut_unknown(
                                        ele, link_url, link_title, elements,
                                    ));
                                    continue;
                                }
                            }
                            LinkType::Autolink => link_url,
                            LinkType::Email => continue,
                            LinkType::WikiLink { .. } => link_url,
                        };
                        if link.starts_with("mailto:") {
                            // do nothing.
                            continue;
                        }
                        link
                    }
                    Element::Image(link_type, link_url, link_title, elements, _) => {
                        // Unknown reference labels are reported here; the alt text is still
                        // checked, but there is no target to look up on disk.
                        let mut is_unknown_ref = false;
                        let link = match link_type {
                            LinkType::Inline => link_url,
                            LinkType::Reference => link_url,
                            LinkType::ReferenceUnknown => {
                                errors.push(DocCheckError::new_error(
                                    ele.doc_line().line_num,
                                    ele.doc_line().file_name.clone(),
                                    &format!(
                                        "Unknown image reference link to {} ({})",
                                        link_url, link_title
                                    ),
                                ));
                                is_unknown_ref = true;
                                link_url
                            }
                            LinkType::Collapsed => link_url,
                            LinkType::CollapsedUnknown => {
                                errors.push(DocCheckError::new_error(
                                    ele.doc_line().line_num,
                                    ele.doc_line().file_name.clone(),
                                    &format!(
                                        "Unknown collapsed image link to {} ({})",
                                        link_url, link_title
                                    ),
                                ));
                                is_unknown_ref = true;
                                link_url
                            }
                            LinkType::Shortcut => link_url,
                            LinkType::ShortcutUnknown => {
                                // Check if this is a case where the text is in [].
                                let text = elements
                                    .iter()
                                    .map(|e| e.get_contents())
                                    .collect::<Vec<String>>()
                                    .join("");
                                if text == link_url.to_string() && link_url == link_title {
                                    errors.push(DocCheckError::new_error_helpful(
                                        ele.doc_line().line_num,
                                        ele.doc_line().file_name.clone(),
                                        &format!(
                                            "unescaped brackets - treated as text [{}]",
                                            link_url
                                        ),
                                        &format!(
                                            "escaping \\[{}\\] or an inline link [{}][/docs/{}",
                                            link_title, link_url, link_url
                                        ),
                                    ));
                                    continue;
                                } else {
                                    errors.push(DocCheckError::new_error(
                                        ele.doc_line().line_num,
                                        ele.doc_line().file_name.clone(),
                                        &format!(
                                            "Unknown image shortcut link to {} ({})",
                                            link_url, link_title
                                        ),
                                    ));
                                    is_unknown_ref = true;
                                }
                                link_url
                            }
                            LinkType::Autolink => link_url,
                            LinkType::Email => continue,
                            LinkType::WikiLink { .. } => link_url,
                        };
                        if link.starts_with("mailto:") {
                            // do nothing.
                            continue;
                        }
                        // Check for alt text
                        let alt = ele.get_contents().trim().to_string();
                        if DISALLOWED_ALT_IMAGE_TEXT.contains(&alt.as_str()) {
                            errors.push(DocCheckError::new_error(
                                ele.doc_line().line_num,
                                ele.doc_line().file_name,
                                &format!(
                                    "Invalid image alt text: {:?}, cannot  be one of {:?}",
                                    alt, DISALLOWED_ALT_IMAGE_TEXT
                                ),
                            ))
                        }
                        if is_unknown_ref {
                            continue;
                        }
                        link
                    }
                    _ => {
                        return Ok(None);
                    }
                };

                let link_to_check =
                    match self.make_link_to_check(&element.doc_line().file_name, link) {
                        Ok(link) => link,
                        Err(e) => {
                            errors.push(DocCheckError::new_error(
                                element.doc_line().line_num,
                                element.doc_line().file_name,
                                &e.to_string(),
                            ));
                            String::from("")
                        }
                    };
                if link_to_check.is_empty() {
                    continue;
                }

                let saw_error = do_check_link(
                    &element.doc_line(),
                    &link_to_check,
                    &self.project,
                    self.allow_fuchsia_src_links,
                )
                .map_err(|err| errors.push(err))
                .is_err();

                match is_intree_link(
                    &self.project,
                    &self.root_dir,
                    &self.docs_folder,
                    &link_to_check,
                ) {
                    Ok(Some(in_tree_path)) => {
                        match do_in_tree_check(
                            &element.doc_line(),
                            &self.root_dir,
                            &self.docs_folder,
                            &link_to_check,
                            &in_tree_path,
                        ) {
                            Ok(target_file) => {
                                let current_file = element.doc_line().file_name.clone();
                                self.reachability_graph
                                    .lock()
                                    .unwrap()
                                    .entry(current_file)
                                    .or_default()
                                    .insert(target_file.clone());

                                if target_file.is_hidden_doc(
                                    &self.root_dir,
                                    self.reference_docs_root.as_deref(),
                                ) {
                                    errors.push(DocCheckError::new_warning(
                                        element.doc_line().line_num,
                                        element.doc_line().file_name.clone(),
                                        &format!(
                                            "Do not link directly to hidden file {}. Hidden files are only meant for inclusion.",
                                            link_to_check
                                        ),
                                    ));
                                }

                                // If this link targets the glossary README, validate that the anchor
                                // fragment corresponds to an actual defined term in _glossary.yaml.
                                if let Some(ref glossary_terms) = self.glossary_terms {
                                    if target_file
                                        .ends_with(self.docs_folder.join("glossary/README.md"))
                                    {
                                        if let Some((_, anchor)) = link_to_check.split_once('#') {
                                            let clean_anchor =
                                                anchor.trim_matches('#').to_lowercase();
                                            if !clean_anchor.is_empty()
                                                && !glossary_terms.contains(&clean_anchor)
                                            {
                                                errors.push(DocCheckError::new_error(
                                                    element.doc_line().line_num,
                                                    element.doc_line().file_name.clone(),
                                                    &format!(
                                                        "invalid glossary anchor '{}' in link '{}': target term does not exist in the glossary",
                                                        anchor, link_to_check
                                                    ),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            Err(link_error) => {
                                errors.push(link_error);
                            }
                        }
                    }
                    Ok(None) => {
                        if self.check_remote_links && !saw_error {
                            self.links.push(LinkReference {
                                link: link_to_check.clone(),
                                location: element.doc_line(),
                            });
                        }
                    }
                    Err(e) => errors.push(DocCheckError::new_error(
                        element.doc_line().line_num,
                        element.doc_line().file_name,
                        &e.to_string(),
                    )),
                };
            }
        }

        if errors.is_empty() { Ok(None) } else { Ok(Some(errors)) }
    }

    /// At the end, check that the out of tree links work, if requested.
    async fn post_check(&self) -> Result<Option<Vec<DocCheckError>>> {
        let mut errors = vec![];

        if let Some(link_errors) = check_external_links(&self.links).await {
            errors.extend(link_errors);
        }

        if !errors.is_empty() { Ok(Some(errors)) } else { Ok(None) }
    }
}

/// Checks specific to a link to an in-tree path.
pub(crate) fn do_in_tree_check(
    doc_line: &DocLine,
    root_dir: &Path,
    docs_folder: &Path,
    link_to_check: &str,
    in_tree_path: &Path,
) -> Result<PathBuf, DocCheckError> {
    let rel_path = in_tree_path.strip_prefix("/").unwrap_or(in_tree_path);
    let filepath = root_dir.join(rel_path);
    if !path_helper::exists(&filepath) {
        // Look for missing the file extension.
        if filepath.extension().is_none() {
            let mut md_path = filepath.clone();
            md_path.set_extension("md");
            if path_helper::exists(&md_path) {
                let file_name = md_path.file_name().ok_or_else(|| {
                    DocCheckError::new_error(
                        doc_line.line_num,
                        doc_line.file_name.clone(),
                        &format!("invalid file name for markdown path {:?}", md_path),
                    )
                })?;
                return Err(DocCheckError::new_error_helpful(
                    doc_line.line_num,
                    doc_line.file_name.clone(),
                    &format!(
                        "in-tree link to {} could not be found at {:?}",
                        link_to_check, filepath
                    ),
                    &format!("{:#?}", file_name),
                ));
            }
        }
        return Err(DocCheckError::new_error(
            doc_line.line_num,
            doc_line.file_name.clone(),
            &format!("in-tree link to {} could not be found at {:?}", link_to_check, filepath),
        ));
    }

    // Directories under the docs folder publish their README.md; any other directory is the
    // target itself. docs_folder may be spelled with or without a leading slash.
    let docs_rel = docs_folder.strip_prefix("/").unwrap_or(docs_folder);
    let target_file = if path_helper::is_dir(&filepath) && rel_path.starts_with(docs_rel) {
        let readme_path = filepath.join("README.md");
        if !path_helper::exists(&readme_path) {
            return Err(DocCheckError::new_error(
                doc_line.line_num,
                doc_line.file_name.clone(),
                &format!(
                    "in-tree link to {} could not be found at {:?} or  {:?}",
                    link_to_check, filepath, readme_path
                ),
            ));
        }
        readme_path
    } else {
        filepath.clone()
    };

    let normalized_target = normalize_path(&target_file).map_err(|e| {
        DocCheckError::new_error(
            doc_line.line_num,
            doc_line.file_name.clone(),
            &format!("Error normalizing path: {:?}: {}", target_file, e),
        )
    })?;

    let normalized_root = normalize_path(root_dir).unwrap_or_else(|_| root_dir.to_path_buf());
    if !normalized_target.starts_with(&normalized_root) {
        return Err(DocCheckError::new_error(
            doc_line.line_num,
            doc_line.file_name.clone(),
            &format!(
                "relative path {:?} points outside root directory {:?}",
                in_tree_path, root_dir
            ),
        ));
    }

    Ok(normalized_target)
}

/// Parse the link into a Uri, and check that it is either a path or that the http/https
/// links are valid for the host they are pointing to.
pub(crate) fn do_check_link(
    doc_line: &DocLine,
    link: &str,
    project_being_checked: &str,
    allow_fuchsia_src_links: bool,
) -> Result<(), DocCheckError> {
    match link.parse::<Uri>() {
        Ok(uri) => {
            match uri.scheme() {
                Some(scheme) => match scheme.as_str() {
                    "http" | "https" => {}
                    _ => return Ok(()),
                },
                None => return Ok(()),
            }
            check_link_authority(doc_line, &uri, project_being_checked, allow_fuchsia_src_links)?;

            // Check for host language parameter on google owned urls.
            if uri
                .authority()
                .map(|a| a.host())
                .map(|host| host.ends_with(".dev") || host.ends_with(".google.com"))
                .unwrap_or(false)
                && uri.query().map(|query| query.contains("hl=")).unwrap_or(false)
            {
                return Err(DocCheckError::new_error(
                    doc_line.line_num,
                    doc_line.file_name.clone(),
                    &format!("Do not add host language parameter `hl` {}", link),
                ));
            }

            Ok(())
        }
        Err(e) => Err(DocCheckError::new_error(
            doc_line.line_num,
            doc_line.file_name.clone(),
            &format!("Invalid link {} : {}", link, e),
        )),
    }
}

/// Returns the relative path from the root_dir if the link it to a file in the fuchsia source tree.
pub(crate) fn is_intree_link(
    project: &str,
    root_dir: impl AsRef<Path>,
    docs_folder: &Path,
    link_to_check: &str,
) -> Result<Option<PathBuf>> {
    let root_path = root_dir.as_ref();
    let (mut filepath, _) = link_to_check.split_once('#').unwrap_or((link_to_check, ""));
    // Split off the query parameters, if any
    (filepath, _) = filepath.split_once('?').unwrap_or((filepath, ""));
    if !root_path.as_os_str().is_empty()
        && let Ok(stripped) = Path::new(filepath).strip_prefix(root_path)
    {
        return normalize_path(&Path::new("/").join(stripped)).map(Some);
    } else if link_to_check.starts_with(&format!("https://{}/{}", GERRIT_HOST, project)) {
        let uri: Uri = match link_to_check.parse::<Uri>() {
            Ok(uri) => uri,
            Err(e) => bail!("Invalid Url {}: {:?}", link_to_check, e),
        };
        let parts = uri.path().split('/').collect::<Vec<&str>>();
        if parts.len() <= 3 {
            let p = parts.join("/");
            if p == format!("/{}/", project) || p == format!("/{}", project) {
                return Ok(None);
            }
        }
        // skip over any branch spec. The first part is empty. It is +, or +show, or +log, etc.
        // Short URLs such as .../+/HEAD, .../+/refs/heads or .../+/refs/heads/main have no file
        // path; they are not in-tree links and must not index past the end of `parts`.
        if parts.len() > 3 && parts[2].starts_with('+') {
            let filepath = PathBuf::from("/");
            // Include all refs/heads except for "releases", which should be directly linked.
            if parts[3] == "refs"
                && parts.get(4) == Some(&"heads")
                && parts.get(5).is_some_and(|branch| *branch != "releases")
                && parts.get(6).is_some_and(|segment| !segment.is_empty())
            {
                return Ok(Some(filepath.join(parts[6..].join("/"))));
            } else if parts[3] == "HEAD"
                && parts.get(4).is_some_and(|folder| *folder == docs_folder.to_string_lossy())
            {
                return Ok(Some(filepath.join(parts[4..].join("/"))));
            } else {
                return Ok(None);
            }
        } else {
            return Ok(Some(PathBuf::from(parts.join("/"))));
        }
    } else if link_to_check.starts_with('/') {
        return normalize_path(Path::new(filepath)).map(Some);
    }
    Ok(None)
}

/// Checks that URLs are not incorrectly pointing to fuchsia.dev or fuchsia.googlesource.com
fn check_link_authority(
    doc_line: &DocLine,
    uri: &Uri,
    project_being_checked: &str,
    allow_fuchsia_src_links: bool,
) -> Result<(), DocCheckError> {
    let link_to_fuchsia_gerrit_host = match uri.authority() {
        Some(a) => *a == GERRIT_HOST,
        None => false,
    };
    let link_to_published_docs_host = match uri.authority() {
        Some(a) => *a == PUBLISHED_DOCS_HOST,
        None => false,
    };

    let parts = uri.path().split('/').collect::<Vec<&str>>();
    let project = parts[1];

    /*
    Links to gerrit source code should be to HEAD or refs/heads/main or equivalent.
    The links can also not be to unknown or obsolete projects.
     */
    if link_to_fuchsia_gerrit_host {
        if !VALID_PROJECTS.contains(&project) {
            return Err(DocCheckError::new_error(
                doc_line.line_num,
                doc_line.file_name.clone(),
                &format!("Obsolete or invalid project {}: {}", project, uri),
            ));
        }
        if !on_gerrit_main(uri) && project == project_being_checked {
            // on_gerrit_main returns true when there is no "+" segment, so one exists here.
            let Some(branch_index) = parts.iter().position(|x| *x == "+") else {
                return Ok(());
            };
            // `file_path` is only the hint in the error below; a branch spec with no trailing
            // path (.../+/refs/heads) yields an empty hint rather than a panic.
            let file_path = if parts.get(branch_index + 1) == Some(&"refs")
                && parts.get(branch_index + 2) == Some(&"heads")
            {
                parts.get(branch_index + 4..).map_or_else(String::new, |rest| rest.join("/"))
            } else {
                parts.get(branch_index + 2..).map_or_else(String::new, |rest| rest.join("/"))
            };

            //Possible point of discussion: Non-HEAD links are open discussion for non- //docs links.

            // Allow files that are not markdown, such as OWNERS to be links.
            if parts.contains(&"docs") && uri.path().ends_with(".md") {
                return Err(DocCheckError::new_error_helpful(
                    doc_line.line_num,
                    doc_line.file_name.clone(),
                    &format!("Invalid link to non-main branch: {}", uri),
                    &format!("filepath: /{}", file_path),
                ));
            }
        }
    }

    /*
      Links to fuchsia.dev (where the docs are published) should not use http(s):, but
      rather use relative paths from the fuchsia source root. For example, /docs/some/file.md.
      The some projects in fuchsia.dev are allowed to be directly linked since they are not
      part of the markdown checked in.

      There is also a list of exceptions of files that can link via https, these allow gtiles to
      point to fuchsia.dev, and could be removed at some point in the future.
    */
    if link_to_published_docs_host && !PUBLISHED_LINKS_ALLOWED.contains(&project) {
        let base_name = doc_line
            .file_name
            .file_name()
            .unwrap_or_else(|| OsStr::new(""))
            .to_string_lossy()
            .to_string();
        if !FILES_ALLOWED_TO_LINK_TO_PUBLISHED_DOCS.contains(&base_name.as_str()) {
            // If the link is to the published docs directory (fuchsia-src), then
            // a path should be used instead.
            if parts.contains(&"fuchsia-src") && !allow_fuchsia_src_links {
                return Err(DocCheckError::new_error(
                    doc_line.line_num,
                    doc_line.file_name.clone(),
                    &format!(
                        "Should not link to {} via {}, use relative filepath",
                        uri,
                        uri.scheme_str().unwrap_or_default()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn handle_shortcut_unknown(
    ele: &Element<'_>,
    link_url: &CowStr<'_>,
    link_title: &CowStr<'_>,
    elements: &Vec<Element<'_>>,
) -> Vec<DocCheckError> {
    let mut errors: Vec<DocCheckError> = vec![];
    // Check if this is a case where the text is in [].
    // Skip gen/build_arguments.md - since it is mostly code and generates a lot of noise.
    if ele.doc_line().file_name.ends_with("gen/build_arguments.md") {
        return errors;
    }
    let text = elements.iter().map(|e| e.get_contents()).collect::<Vec<String>>().join("");
    let trimmed_link_url = link_url.trim();
    if trimmed_link_url.starts_with("\"")
        && (trimmed_link_url.ends_with("\"") || trimmed_link_url.ends_with(","))
    {
        // This is not a link but an array of quoted strings.
        return errors;
    }
    if text == link_url.to_string() && link_url == link_title {
        errors.push(DocCheckError::new_error_helpful(
            ele.doc_line().line_num,
            ele.doc_line().file_name.clone(),
            &format!("unescaped [{}] not treating this as a shortcut link.", link_url),
            &format!(
                "escaping \\[{}\\] or make a link [{}](/docs/{})",
                link_title, link_url, link_url
            ),
        ));
    } else {
        errors.push(DocCheckError::new_error_helpful(
            ele.doc_line().line_num,
            ele.doc_line().file_name.clone(),
            &format!("Unknown reference link to [{}][{}]", text, link_url),
            &format!(
                // TODO(https://fxbug.dev/373449734): Remove the second sentence when fixed.
                "making sure you added a matching [{}]: YOUR_LINK_HERE below this reference. \
                If this is in a triple tick code block, consider removing empty lines - see \
                https://fxbug.dev/373449734.",
                link_url
            ),
        ));
    }
    errors
}

/// Checks whether the URI points to the main branch of a Gerrit (i.e.,
/// googlesource.com) project.
fn on_gerrit_main(uri: &Uri) -> bool {
    let parts: Vec<&str> = uri.path().split('/').collect();
    let Some(branch_index) = parts.iter().position(|p| *p == "+") else {
        // no + branch spec in the URL, so defaults to main.
        return true;
    };
    // Links to gerrit are of the form:
    // https://fuchsia.googlesource.com/<project>/+/refs/heads/<branch>/<path>
    // https://fuchsia.googlesource.com/<project>/+/<HEAD|main>/<path>
    matches!(
        parts.get(branch_index + 1..),
        Some([]) | Some(["main" | "HEAD", ..]) | Some(["refs", "heads", "main", ..])
    )
}

pub async fn check_external_links(links: &Vec<LinkReference>) -> Option<Vec<DocCheckError>> {
    // sort the links to take advantage of keep alive
    // HashMap is <authority, set<links>.
    let mut domain_sorted_links = HashMap::<String, HashSet<&LinkReference>>::new();
    let mut errors = vec![];
    for link in links {
        match link.link.parse::<Uri>() {
            Ok(uri) => {
                if let Some(authority) = uri.authority() {
                    let key = authority.to_string();

                    let set = domain_sorted_links.entry(key).or_default();
                    set.insert(link);
                } else {
                    errors.push(DocCheckError::new_error(
                        link.location.line_num,
                        link.location.file_name.clone(),
                        &format!("Error parsing {}: no authority found", link.link),
                    ));
                }
            }
            Err(e) => errors.push(DocCheckError::new_error(
                link.location.line_num,
                link.location.file_name.clone(),
                &format!("Error parsing {}: {}", link.link, e),
            )),
        };
    }

    let client: HttpsClient = new_https_client_from_tcp_options(tcp_options());
    let mut domain_futures = vec![];

    const CONCURRENT_DOMAINS: usize = 10;
    const CONCURRENT_REQUESTS_PER_DOMAIN: usize = 10;

    for (authority, links) in domain_sorted_links {
        let client = client.clone();
        domain_futures.push(async move {
            let mut pending_requests = vec![];
            eprintln!("checking {authority} {link_count} links", link_count = links.len());
            for link in links {
                let p = check_url_link(client.clone(), link);
                pending_requests.push(p);
            }
            futures::stream::iter(pending_requests)
                .buffer_unordered(CONCURRENT_REQUESTS_PER_DOMAIN)
                .collect::<Vec<_>>()
                .await
        });
    }

    let results = futures::stream::iter(domain_futures)
        .buffer_unordered(CONCURRENT_DOMAINS)
        .collect::<Vec<_>>()
        .await;
    errors.extend(results.into_iter().flatten().flatten());

    if errors.is_empty() { None } else { Some(errors) }
}

/// Check that the URL is valid (200 or 301 or 302).
async fn check_url_link(client: HttpsClient, link: &LinkReference) -> Option<DocCheckError> {
    let request = match Request::get(&link.link).body(Full::<Bytes>::default()) {
        Ok(request) => request,
        Err(e) => {
            return Some(DocCheckError::new_error(
                link.location.line_num,
                link.location.file_name.clone(),
                &format!("Error {} requesting {}", e, link.link),
            ));
        }
    };

    match client.request(request).await {
        Ok(response) => match response.status() {
            StatusCode::OK | StatusCode::FOUND | StatusCode::MOVED_PERMANENTLY => None,
            _ => Some(DocCheckError::new_error(
                link.location.line_num,
                link.location.file_name.clone(),
                &format!("Error response {} reading {}", response.status(), &link.link),
            )),
        },
        Err(e) => Some(DocCheckError::new_error(
            link.location.line_num,
            link.location.file_name.clone(),
            &format!("Error {} reading {}", e, link.link),
        )),
    }
}

fn tcp_options() -> TcpOptions {
    let mut options: TcpOptions = std::default::Default::default();

    // Use TCP keepalive to notice stuck connections.
    // After 60s with no data received send a probe every 15s.
    options.keepalive_idle = Some(std::time::Duration::from_secs(60));
    options.keepalive_interval = Some(std::time::Duration::from_secs(15));
    // After 8 probes go unacknowledged treat the connection as dead.
    options.keepalive_count = Some(8);

    options
}

/// Loads the valid glossary term slugs and keys from docs/glossary/_glossary.yaml if present.
fn load_glossary_terms(root_dir: &Path, docs_folder: &Path) -> Option<HashSet<String>> {
    let glossary_path = root_dir.join(docs_folder).join("glossary/_glossary.yaml");
    if !path_helper::exists(&glossary_path) {
        return None;
    }
    let content = match std::fs::read_to_string(&glossary_path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    #[derive(serde::Deserialize)]
    struct TermEntry {
        term: String,
        #[serde(default)]
        short_description: String,
    }
    let entries: Vec<TermEntry> = match serde_yaml::from_str(&content) {
        Ok(e) => e,
        Err(_) => return None,
    };
    let mut terms = HashSet::new();
    // As defined in docs/_common/_doc_widgets.md:34, DevSite only matches terms
    // that have a non-empty short_description.
    for entry in entries
        .into_iter()
        .filter(|e| !e.term.trim().is_empty() && !e.short_description.trim().is_empty())
    {
        let term = entry.term.trim();
        // Slug form: lowercase, non-alphanumerics collapsed to '-'
        let mut slug = String::new();
        let mut last_dash = true;
        for c in term.chars() {
            if c.is_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
        let trimmed_slug = slug.trim_matches('-').to_string();
        if !trimmed_slug.is_empty() {
            terms.insert(trimmed_slug);
        }
    }
    Some(terms)
}

/// Called from main to register all the checks to preform which are implemented in this module.
pub(crate) fn register_markdown_checks(
    opt: &DocCheckerArgs,
    reachability_graph: ReachabilityGraph,
) -> Result<Vec<Box<dyn DocCheck>>> {
    let glossary_terms = load_glossary_terms(&opt.root, &opt.docs_folder);
    let checker = LinkChecker {
        root_dir: opt.root.clone(),
        project: opt.project.clone(),
        docs_folder: opt.docs_folder.clone(),
        check_remote_links: opt.check_external_links,
        links: vec![],
        allow_fuchsia_src_links: opt.allow_fuchsia_src_links,
        reachability_graph,
        reference_docs_root: opt.reference_docs_root.clone(),
        glossary_terms,
    };
    Ok(vec![Box::new(checker)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocContext;

    #[test]
    fn test_make_link_to_check() -> Result<()> {
        let checker = LinkChecker {
            root_dir: PathBuf::from("/my/root/fuchsia"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_remote_links: false,
            links: vec![],
            allow_fuchsia_src_links: false,
            reachability_graph: Default::default(),
            reference_docs_root: None,
            glossary_terms: None,
        };
        let filename = PathBuf::from("/my/root/fuchsia/docs/index.md");

        let test_data = [
            ("README.md", "/docs/README.md"),
            ("https://my-server.com", "https://my-server.com/"),
            (
                "http://my-server.com/page?qp=1&words=one two three",
                "http://my-server.com/page?qp=1&words=one%20two%20three",
            ),
            ("/docs/some-file.md", "/docs/some-file.md"),
            ("#Anchor-name", "/docs/index.md#Anchor-name"),
            ("path/to/sub/info.md", "/docs/path/to/sub/info.md"),
            ("mailto:someone@somewhere.tld", "mailto:someone@somewhere.tld"),
            ("https:///bad-url?x=", "https:///bad-url?x="),
            ("/reference/to/generated.md", "https://fuchsia.dev/reference/to/generated.md"),
        ];

        for (input, expected) in test_data {
            let actual =
                checker.make_link_to_check(filename.as_path(), &CowStr::Borrowed(input))?;

            assert_eq!(actual, expected)
        }
        Ok(())
    }

    #[test]
    fn test_happy_path() -> Result<()> {
        let opt = DocCheckerArgs {
            root: PathBuf::from("/path/to/fuchsia"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_external_links: false,
            json: false,
            allow_fuchsia_src_links: false,
            reference_docs_root: None,
            skip_link_check: false,
            allow_unreferenced_hidden: false,
        };

        let mut checks = register_markdown_checks(&opt, Default::default())?;
        assert_eq!(checks.len(), 1);

        let file = PathBuf::from("/docs/README.md");
        let input = "This is a line to [something](/docs/something.md)";

        let callback = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input)
        };
        let ctx = DocContext::new(file, input, Some(callback));

        if let Some(check) = checks.first_mut() {
            for ele in ctx {
                let errors = check.check(&ele)?;
                assert!(errors.is_none(), "expected none, got {:?}", errors);
            }
        }

        Ok(())
    }

    #[test]
    fn test_is_in_tree_link() -> Result<()> {
        let root_dir = "/some/root/dir";
        let project = "fuchsia";
        let docs_folder = PathBuf::from("docs");

        let test_cases = [
            ("/docs/README.md", Some(PathBuf::from("/docs/README.md"))),
            ("/docs/somewhere/file.md#header1", Some(PathBuf::from("/docs/somewhere/file.md"))),
            ("https://google.com", None),
            ("mailto:someone@email.com", None),
            ("/src/to/a/program.cc", Some(PathBuf::from("/src/to/a/program.cc"))),
            ("https://fuchsia.googlesource.com/fuchsia/+/HEAD/sdk/lib/fdio", None),
            (
                "https://fuchsia.googlesource.com/fuchsia/docs/README.md",
                Some(PathBuf::from("/fuchsia/docs/README.md")),
            ),
            (
                "https://fuchsia.googlesource.com/fuchsia/+/7461d8882167e7a9d1b494e3b1734d2c063830fc/build/package.gni#604",
                None,
            ),
            (
                "https://fuchsia.googlesource.com/fuchsia/+show/HEAD/docs/concepts/kernel/_toc.yaml",
                Some(PathBuf::from("/docs/concepts/kernel/_toc.yaml")),
            ),
            ("https://fuchsia.googlesource.com/fuchsia", None),
            ("https://fuchsia.googlesource.com/fuchsia/", None),
            // Since this is not to the /docs dir, it should not be an in-tree link.
            (
                "https://fuchsia.googlesource.com/fuchsia/+log/d381548c6aef76926e6203a2ad2265dd510d1e9b",
                None,
            ),
            // Exclude releases
            ("https://fuchsia.googlesource.com/fuchsia/+/refs/heads/releases/f16", None),
            // A branch spec followed by a single path segment is the shortest in-tree form.
            (
                "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/.gitignore",
                Some(PathBuf::from("/.gitignore")),
            ),
            // Absolute paths under root_dir are re-rooted at "/", with anchors stripped.
            ("/some/root/dir/docs/README.md#heading", Some(PathBuf::from("/docs/README.md"))),
            // A sibling directory sharing a string prefix with root_dir is not under root_dir.
            (
                "/some/root/dir_sibling/docs/README.md",
                Some(PathBuf::from("/some/root/dir_sibling/docs/README.md")),
            ),
        ];
        for (link_to_check, expected) in test_cases {
            let result = is_intree_link(project, root_dir, &docs_folder, link_to_check)?;
            assert_eq!(result, expected, "link: {link_to_check}");
        }
        // An empty root_dir never matches as a prefix.
        assert_eq!(
            is_intree_link(project, "", &docs_folder, "https://google.com/x")?,
            None,
            "empty root_dir must not strip-prefix a URL"
        );
        // Gerrit URLs with a branch spec but no file path are not in-tree links and must not
        // index past the end of the path segments or resolve to the repository root.
        for short_url in [
            "https://fuchsia.googlesource.com/fuchsia/+/HEAD",
            "https://fuchsia.googlesource.com/fuchsia/+/refs",
            "https://fuchsia.googlesource.com/fuchsia/+/refs/heads",
            "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main",
            "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/",
        ] {
            assert_eq!(
                is_intree_link(project, root_dir, &docs_folder, short_url)?,
                None,
                "link: {short_url}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_gerrit_branch_links() -> Result<()> {
        let doc_line = DocLine { line_num: 1, file_name: PathBuf::from("/docs/README.md") };
        // The project being checked is "cobalt" so these fixtures exercise the same-project
        // branch rules without being documentation links to fuchsia/+/.../docs.
        // Links to the main branch of the project being checked are valid.
        for ok_link in [
            "https://fuchsia.googlesource.com/cobalt/+/HEAD/docs/README.md",
            "https://fuchsia.googlesource.com/cobalt/+/main/docs/README.md",
            "https://fuchsia.googlesource.com/cobalt/+/refs/heads/main/docs/README.md",
            // Branch specs with no file path report nothing rather than panicking.
            "https://fuchsia.googlesource.com/cobalt/+/refs",
            "https://fuchsia.googlesource.com/cobalt/+/refs/heads",
            "https://fuchsia.googlesource.com/cobalt/+/HEAD",
        ] {
            assert!(
                do_check_link(&doc_line, ok_link, "cobalt", false).is_ok(),
                "expected ok: {ok_link}"
            );
        }
        // Markdown links to docs on any other branch, including the legacy default branch
        // name, are rejected with a filepath hint.
        for bad_link in [
            "https://fuchsia.googlesource.com/cobalt/+/refs/heads/feature/docs/README.md",
            "https://fuchsia.googlesource.com/cobalt/+/refs/heads/master/docs/README.md",
            "https://fuchsia.googlesource.com/cobalt/+/master/docs/README.md",
            "https://fuchsia.googlesource.com/cobalt/+/abc123/docs/README.md",
        ] {
            let err = do_check_link(&doc_line, bad_link, "cobalt", false)
                .expect_err(&format!("expected non-main branch error: {bad_link}"));
            assert!(err.message.contains("Invalid link to non-main branch"), "{}", err.message);
            assert_eq!(err.help_suggestion.as_deref(), Some("filepath: /docs/README.md"));
        }
        Ok(())
    }

    #[test]
    fn test_check() -> Result<()> {
        let opt = DocCheckerArgs {
            root: PathBuf::from("/path/to/fuchsia"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_external_links: false,
            json: false,
            allow_fuchsia_src_links: false,
            reference_docs_root: None,
            skip_link_check: false,
            allow_unreferenced_hidden: false,
        };

        let mut checks = register_markdown_checks(&opt, Default::default())?;
        assert_eq!(checks.len(), 1);

        let test_data: Vec<(PathBuf, &str, Option<Vec<DocCheckError>>)> = vec![
            (

                PathBuf::from("/docs/README.md"),
                "This is a line to [something](/docs/something.md)",

            None,
        ),
        (

                PathBuf::from("/docs/README.md"),
                "invalid image text ![](/docs/something.png)",

            Some(
                [DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
                    "Invalid image alt text: \"\", cannot  be one of [\"\"]"),
                 DocCheckError::new_error(1,PathBuf::from("/docs/README.md"),
                    "in-tree link to /docs/something.png could not be found at \"/path/to/fuchsia/docs/something.png\"")].to_vec()),
        ),
        (

                PathBuf::from("/docs/README.md"),
                "invalid url [oops](https:///nowhere/something.md?xx)",

            Some([DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
             "Invalid link https:///nowhere/something.md?xx : invalid format")].to_vec())
        ),
        (

                PathBuf::from("/docs/README.md"),
                "relative path outside root  [oops](/docs/../../illegal.md)",

            Some([DocCheckError::new_error(1,PathBuf::from("/docs/README.md"),
             "Cannot normalize /docs/../../illegal.md, references parent beyond root.")].to_vec())
        ),
        (

                PathBuf::from("/docs/README.md"),
                "hl param is not allowed [hl](https://google.com/something?hl=en)",

            None,
        ),
        (

                PathBuf::from("/docs/README.md"),
                "invalid project link [garnet](https://fuchsia.googlesource.com/garnet/+/HEAD/src/file.cc)",

            Some([DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
             "Obsolete or invalid project garnet: https://fuchsia.googlesource.com/garnet/+/HEAD/src/file.cc")].to_vec())
        ),
        (

                PathBuf::from("/docs/README.md"),
                "valid project link [cobalt-registry](https://fuchsia.googlesource.com/cobalt-registry)",

            None,
        ),
        (

                PathBuf::from("/docs/README.md"),
                "A reference link to [`topaz`][flutter-gni]\n\n\
                [flutter-gni]: https://fuchsia.googlesource.com/topaz/+/HEAD/runtime/flutter_runner/flutter_app.gni \"Flutter GN build template\"",

            Some([DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
            "Obsolete or invalid project topaz: https://fuchsia.googlesource.com/topaz/+/HEAD/runtime/flutter_runner/flutter_app.gni")].to_vec())
        ),
        (

                PathBuf::from("/docs/README.md"),
                "non-master branch link to docs [old doc](https://fuchsia.googlesource.com/fuchsia/+/some-branch/docs/file.md)",

            Some([DocCheckError::new_error_helpful(1,PathBuf::from("/docs/README.md"),
              "Invalid link to non-main branch: https://fuchsia.googlesource.com/fuchsia/+/some-branch/docs/file.md","filepath: /docs/file.md")].to_vec())
        ),
        (

                PathBuf::from("/docs/README.md"),
                "non-master branch link to src ok [old source](https://fuchsia.googlesource.com/fuchsia/+/some-branch/tools/file.cc)",

            None,
        ),
        (

                PathBuf::from("/docs/README.md"),
                "non-markdown file OK to link to docs [non-source](https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/docs/OWNERS)",

            None,
        )
        ];

        for (file, input, expected_errors) in test_data {
            let callback = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
                DocContext::handle_broken_link(broken_link, input)
            };
            let ctx = DocContext::new(file, input, Some(callback));
            for ele in ctx {
                let errors = checks[0].check(&ele)?;
                if let Some(ref expected_list) = expected_errors {
                    let mut expected_iter = expected_list.iter();
                    if let Some(actual_errors) = errors {
                        for actual in actual_errors {
                            if let Some(expected) = expected_iter.next() {
                                assert_eq!(&actual, expected);
                            } else {
                                panic!("Got unexpected error returned: {:?}", actual);
                            }
                        }
                        let unused_errors: Vec<&DocCheckError> = expected_iter.collect();
                        if !unused_errors.is_empty() {
                            panic!("Expected more errors: {:?}", unused_errors);
                        }
                    } else if expected_errors.is_some() {
                        panic!("No errors, but expected {:?}", expected_errors);
                    }
                } else if errors.is_some() {
                    panic!("Got unexpected errors {:?}", errors.unwrap());
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_do_intree_check() -> Result<()> {
        let doc_line = DocLine { line_num: 1, file_name: PathBuf::from("some/file.md") };
        let root_dir = PathBuf::from("/path/to/fuchsia");
        let docs_folder = PathBuf::from("docs");

        let test_data = [
            ("/docs/exists/something.md", "/docs/exists/something.md", None),
            (
                "/docs/no_readme",
                "/docs/no_readme",
                Some(DocCheckError::new_error(
                    1,
                    PathBuf::from("some/file.md"),
                    "in-tree link to /docs/no_readme could not be found at \"/path/to/fuchsia/docs/no_readme\" or  \"/path/to/fuchsia/docs/no_readme/README.md\"",
                )),
            ),
            (
                "/docs/no-extension",
                "/docs/no-extension",
                Some(DocCheckError::new_error_helpful(
                    1,
                    PathBuf::from("some/file.md"),
                    "in-tree link to /docs/no-extension could not be found at \"/path/to/fuchsia/docs/no-extension\"",
                    "\"no-extension.md\"",
                )),
            ),
        ];

        for (link_to_check, in_tree_path, expected_error) in test_data {
            let result = do_in_tree_check(
                &doc_line,
                &root_dir,
                &docs_folder,
                link_to_check,
                &PathBuf::from(in_tree_path),
            );
            assert_eq!(result.err(), expected_error);
        }

        // A directory under the docs folder resolves to its README.md; any other directory is
        // the target itself. The docs folder may be spelled with a leading slash.
        for docs_folder in [Path::new("docs"), Path::new("/docs")] {
            assert_eq!(
                do_in_tree_check(
                    &doc_line,
                    &root_dir,
                    docs_folder,
                    "/docs/exists",
                    Path::new("/docs/exists")
                )
                .ok(),
                Some(PathBuf::from("/path/to/fuchsia/docs/exists/README.md")),
                "docs_folder = {docs_folder:?}"
            );
            assert_eq!(
                do_in_tree_check(
                    &doc_line,
                    &root_dir,
                    docs_folder,
                    "/src/lib",
                    Path::new("/src/lib")
                )
                .ok(),
                Some(PathBuf::from("/path/to/fuchsia/src/lib")),
                "docs_folder = {docs_folder:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_email_and_collapsed_unknown_do_not_swallow_or_panic() -> Result<()> {
        let opt = DocCheckerArgs {
            root: PathBuf::from("/path/to/fuchsia"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_external_links: false,
            json: false,
            allow_fuchsia_src_links: false,
            reference_docs_root: None,
            skip_link_check: false,
            allow_unreferenced_hidden: false,
        };
        let mut checks = register_markdown_checks(&opt, Default::default())?;
        let file = PathBuf::from("/docs/README.md");
        // An e-mail autolink or mailto: link used to return early and drop every error already
        // collected for the paragraph; collapsed unknown references used to panic.
        let input = "Broken [l](/docs/../../escape.md) and <user@example.com> and [m](mailto:a@b.c) \
                     and [also](/docs/missing.md) and ![ci][] and [cl][].";
        let callback = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input)
        };
        let mut messages = vec![];
        for ele in DocContext::new(file, input, Some(callback)) {
            if let Some(errors) = checks[0].check(&ele)? {
                messages.extend(errors.into_iter().map(|e| e.message));
            }
        }
        let has = |needle: &str| messages.iter().any(|m| m.contains(needle));
        assert!(has("Cannot normalize /docs/../../escape.md"), "{messages:?}");
        assert!(has("/docs/missing.md could not be found"), "{messages:?}");
        assert!(has("Unknown collapsed image link to ci"), "{messages:?}");
        assert!(has("Unknown collapsed link to cl"), "{messages:?}");
        // Unknown references are reported once, not followed by a second not-found error.
        assert!(!has("ci could not be found") && !has("cl could not be found"), "{messages:?}");
        Ok(())
    }

    #[test]
    fn test_check_reference_links() -> Result<()> {
        let opt = DocCheckerArgs {
            root: PathBuf::from("/path/to/fuchsia"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_external_links: false,
            json: false,
            allow_fuchsia_src_links: false,
            reference_docs_root: None,
            skip_link_check: false,
            allow_unreferenced_hidden: false,
        };

        let mut checks = register_markdown_checks(&opt, Default::default())?;
        assert_eq!(checks.len(), 1);

        let test_data: Vec<(PathBuf, &str, Option<Vec<DocCheckError>>)> = vec![
        (
            PathBuf::from("/docs/README.md"),
            "This is a line to [something](/docs/something.md)",
            None,
        ),
        (

            PathBuf::from("/docs/README.md"),
            "invalid url [oops](https:///nowhere/something.md?xx)",
            Some([DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
                "Invalid link https:///nowhere/something.md?xx : invalid format")].to_vec())
        ),
        (
            PathBuf::from("/docs/README.md"),
            "A reference link to [`topaz`][flutter-gni]\n\n\
                [flutter-gni]: https://fuchsia.googlesource.com/topaz/+/HEAD/runtime/flutter_runner/flutter_app.gni \"Flutter GN build template\"",
            Some([DocCheckError::new_error(1, PathBuf::from("/docs/README.md"),
                "Obsolete or invalid project topaz: https://fuchsia.googlesource.com/topaz/+/HEAD/runtime/flutter_runner/flutter_app.gni")].to_vec())
        ),
        (
            PathBuf::from("/docs/README.md"),
            "brackets which are not a link [your name here]",
            Some([DocCheckError::new_error_helpful(1, PathBuf::from("/docs/README.md"),
                "Unknown reference link to [your name here][your name here]",
                "making sure you added a matching [your name here]: YOUR_LINK_HERE below this reference. If this is in a triple tick code block, consider removing empty lines - see https://fxbug.dev/373449734.")
            ].to_vec()),
        ),
        (

            PathBuf::from("/docs/README.md"),
            "missing [text][link-to-text]",
            Some([DocCheckError::new_error_helpful(1, PathBuf::from("/docs/README.md"),
                "Unknown reference link to [text][link-to-text]",
                "making sure you added a matching [link-to-text]: YOUR_LINK_HERE below this reference. If this is in a triple tick code block, consider removing empty lines - see https://fxbug.dev/373449734."
            )].to_vec())
        ),
        (
            PathBuf::from("/docs/README.md"),
            r#"pw_toolchain_STATIC_ANALYSIS_SKIP_INCLUDE_PATHS = [".*/third_party/.*"]"#,
            None
        ),
        ];

        for (file, input, expected_errors) in test_data {
            let callback = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
                DocContext::handle_broken_link(broken_link, input)
            };
            let ctx = DocContext::new(file, input, Some(callback));
            for ele in ctx {
                let errors = checks[0].check(&ele)?;
                if let Some(ref expected_list) = expected_errors {
                    let mut expected_iter = expected_list.iter();
                    if let Some(actual_errors) = errors {
                        for actual in actual_errors {
                            if let Some(expected) = expected_iter.next() {
                                assert_eq!(&actual, expected);
                            } else {
                                panic!("Got unexpected error returned: {:?}", actual);
                            }
                        }
                        let unused_errors: Vec<&DocCheckError> = expected_iter.collect();
                        if !unused_errors.is_empty() {
                            panic!("Expected more errors: {:?}", unused_errors);
                        }
                    } else if expected_errors.is_some() {
                        panic!("No errors, but expected {:?}", expected_errors);
                    }
                } else if errors.is_some() {
                    panic!("Got unexpected errors {:?}", errors.unwrap());
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_allowing_fuchsia_src_links() -> Result<()> {
        let opt = DocCheckerArgs {
            root: PathBuf::from("/path/to/fuchsia/somewhere/else"),
            project: "fuchsia".to_string(),
            docs_folder: PathBuf::from("docs"),
            check_external_links: false,
            json: false,
            allow_fuchsia_src_links: true,
            reference_docs_root: None,
            skip_link_check: false,
            allow_unreferenced_hidden: false,
        };

        let mut checks = register_markdown_checks(&opt, Default::default())?;
        assert_eq!(checks.len(), 1);

        let test_data: Vec<(PathBuf, &str, Option<Vec<DocCheckError>>)> = vec![(
            PathBuf::from("/docs/README.md"),
            "This is a link to fuchsia docs as external url [something](https://fuchsia.dev/fuchsia-src/README.md)",
            None,
        )];
        for (file, input, expected_errors) in test_data {
            let callback = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
                DocContext::handle_broken_link(broken_link, input)
            };
            let ctx = DocContext::new(file, input, Some(callback));
            for ele in ctx {
                let errors = checks[0].check(&ele)?;
                if let Some(ref expected_list) = expected_errors {
                    let mut expected_iter = expected_list.iter();
                    if let Some(actual_errors) = errors {
                        for actual in actual_errors {
                            if let Some(expected) = expected_iter.next() {
                                assert_eq!(&actual, expected);
                            } else {
                                panic!("Got unexpected error returned: {:?}", actual);
                            }
                        }
                        let unused_errors: Vec<&DocCheckError> = expected_iter.collect();
                        if !unused_errors.is_empty() {
                            panic!("Expected more errors: {:?}", unused_errors);
                        }
                    } else if expected_errors.is_some() {
                        panic!("No errors, but expected {:?}", expected_errors);
                    }
                } else if errors.is_some() {
                    panic!("Got unexpected errors {:?}", errors.unwrap());
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_glossary_link_validation() -> Result<()> {
        let root_dir = PathBuf::from("/my/root/fuchsia");
        let docs_folder = PathBuf::from("docs");
        let mut glossary_terms = HashSet::new();
        glossary_terms.insert("product-bundle".to_string());
        glossary_terms.insert("session-component".to_string());

        let mut checker = LinkChecker {
            root_dir: root_dir.clone(),
            project: "fuchsia".to_string(),
            docs_folder: docs_folder.clone(),
            check_remote_links: false,
            links: vec![],
            allow_fuchsia_src_links: false,
            reachability_graph: Default::default(),
            reference_docs_root: None,
            glossary_terms: Some(glossary_terms),
        };

        let file = PathBuf::from("/docs/README.md");

        // 1. Valid glossary reference link
        let input_valid = "See [product bundle][glossary.product-bundle].";
        let callback_valid = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input_valid)
        };
        let ctx = DocContext::new(file.clone(), input_valid, Some(callback_valid));
        for ele in ctx {
            let errors = checker.check(&ele)?;
            assert!(
                errors.is_none(),
                "Expected no errors for valid glossary link, got {:?}",
                errors
            );
        }

        // 2. Invalid glossary reference link (typo)
        let input_invalid = "See [bad bundle][glossary.invalid-bundle-typo].";
        let callback_invalid = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input_invalid)
        };
        let ctx = DocContext::new(file.clone(), input_invalid, Some(callback_invalid));
        let mut saw_error = false;
        for ele in ctx {
            if let Some(errs) = checker.check(&ele)? {
                for err in errs {
                    if err.message.contains("invalid glossary anchor 'invalid-bundle-typo'") {
                        saw_error = true;
                    }
                }
            }
        }
        assert!(saw_error, "Expected error for invalid glossary anchor 'invalid-bundle-typo'");

        // 3. Valid shortcut glossary link
        let input_shortcut_valid = "See [glossary.session-component].";
        let callback_shortcut_valid = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input_shortcut_valid)
        };
        let ctx =
            DocContext::new(file.clone(), input_shortcut_valid, Some(callback_shortcut_valid));
        for ele in ctx {
            let errors = checker.check(&ele)?;
            assert!(
                errors.is_none(),
                "Expected no errors for valid shortcut glossary link, got {:?}",
                errors
            );
        }

        // 4. Invalid shortcut glossary link (typo)
        let input_shortcut_invalid = "See [glossary.invalid-shortcut-typo].";
        let callback_shortcut_invalid = &mut |broken_link: pulldown_cmark::BrokenLink<'_>| {
            DocContext::handle_broken_link(broken_link, input_shortcut_invalid)
        };
        let ctx =
            DocContext::new(file.clone(), input_shortcut_invalid, Some(callback_shortcut_invalid));
        let mut saw_shortcut_error = false;
        for ele in ctx {
            if let Some(errs) = checker.check(&ele)? {
                for err in errs {
                    if err.message.contains("invalid glossary anchor 'invalid-shortcut-typo'") {
                        saw_shortcut_error = true;
                    }
                }
            }
        }
        assert!(
            saw_shortcut_error,
            "Expected error for invalid shortcut glossary anchor 'invalid-shortcut-typo'"
        );

        // 5. Invalid inline glossary link
        let input_inline_invalid = "See [bad inline](/docs/glossary/README.md#nonexistent-term).";
        let ctx = DocContext::new(file.clone(), input_inline_invalid, None::<&mut fn(_) -> _>);
        let mut saw_inline_error = false;
        for ele in ctx {
            if let Some(errs) = checker.check(&ele)? {
                for err in errs {
                    if err.message.contains("invalid glossary anchor 'nonexistent-term'") {
                        saw_inline_error = true;
                    }
                }
            }
        }
        assert!(
            saw_inline_error,
            "Expected error for invalid inline glossary anchor 'nonexistent-term'"
        );

        Ok(())
    }

    #[test]
    fn test_load_glossary_terms() -> Result<()> {
        let temp_dir =
            std::env::temp_dir().join(format!("doc_checker_glossary_{}", std::process::id()));
        let docs_dir = temp_dir.join("docs/glossary");
        std::fs::create_dir_all(&docs_dir)?;

        // Test missing file returns None
        assert!(load_glossary_terms(&temp_dir, Path::new("nonexistent_docs")).is_none());

        // Write a test _glossary.yaml
        let glossary_yaml = r#"
- term: Valid Term
  short_description: A valid description.
- term: Another-Term_With Punctuation!
  short_description: Another description.
- term: Empty Description
  short_description: "   "
- term: "   "
  short_description: Missing term.
- term: No Description
"#;
        std::fs::write(docs_dir.join("_glossary.yaml"), glossary_yaml)?;

        let terms = load_glossary_terms(&temp_dir, Path::new("docs"))
            .expect("Expected glossary terms to be loaded");

        assert!(terms.contains("valid-term"));
        assert!(terms.contains("another-term-with-punctuation"));
        assert!(!terms.contains("empty-description"));
        assert_eq!(terms.len(), 2);

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
