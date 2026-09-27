// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs::create_dir_all;
use std::io::{Read, copy};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tar::{Archive, Entry};

/// Prefix for "whiteout" files, whose purpose is to hide files from lower layers.
///
/// Docker images use the same whiteout type as AUFS:
///  - A file called ".wh..wh..opq" hides all the files in the same directory from previous layers.
///  - Any other filename starting ".wh." simply hides the corresponding file from previous layers
///    (e.g. ".wh.example.txt" hides "example.txt").
static WHITEOUT_PREFIX: &[u8] = b".wh.";

/// A filename. We don't check/enforce a specific encoding.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Name(Vec<u8>);

impl Name {
    const EMPTY: Name = Name(Vec::new());

    fn is_empty(&self) -> bool {
        self.0 == b""
    }

    fn is_dot(&self) -> bool {
        self.0 == b"."
    }

    fn is_dotdot(&self) -> bool {
        self.0 == b".."
    }

    /// Returns true if this filename is meant to hide the contents of its parent directory.
    fn is_whiteout_opaque(&self) -> bool {
        // https://github.com/opencontainers/image-spec/issues/130 also mentions ".wh.__dir_opaque"
        // in addition to ".wh..wh..opq".
        self.0 == b".wh..wh..opq" || self.0 == b".wh.__dir_opaque"
    }

    /// Returns `Some(name)` if this filename is meant to hide `name`.
    ///
    /// WARNING: Always check is_whiteout_opaque() first to avoid false positives.
    fn strip_whiteout_prefix(&self) -> Option<Name> {
        self.0.strip_prefix(WHITEOUT_PREFIX).map(|slice| Name(slice.to_vec()))
    }
}

/// A sequential ID that is assigned to each added layer.
///
/// We use it to avoid removing entries from the same layer when a whiteout is found.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct LayerIndex(usize);

/// Splits a path into (ancestors, basename).
///
/// Note:
/// - If the path points to the root directory, ([], Name::EMPTY) is returned.
/// - In any other case, ancestors always starts with Name::EMPTY.
fn parse_path(path: &[u8]) -> Result<(Vec<Name>, Name)> {
    // Split path at every '/'.
    let segments: Vec<Name> =
        path.split(|ch| *ch == b'/').map(|slice| Name(slice.to_vec())).collect();

    // We don't support .. in paths.
    if segments.iter().any(|segment| segment.is_dotdot()) {
        bail!("Found \"..\" in path");
    }

    // Ensure there is an empty element at the beginning (as a placeholder for the root directory),
    // but remove any other empty and '.' segments.
    let mut segments: Vec<Name> = [Name::EMPTY]
        .into_iter()
        .chain(segments.into_iter().filter(|segment| !segment.is_empty() && !segment.is_dot()))
        .collect();

    let name = segments.pop().unwrap();
    Ok((segments, name))
}

/// Helper struct to merge several filesystem layers into one.
pub struct LayeredImage {
    /// The root directory.
    root: Rc<Directory>,

    /// ID that will be assigned to the next layer.
    next_layer_index: LayerIndex,

    /// Where files' contents are stored.
    extract_dir: PathBuf,
}

impl LayeredImage {
    /// Create a blank image.
    ///
    /// It can later be populated by calling `add_layer` one or more times. Files added by such
    /// calls will be extracted into the directory given in `extract_dir`, which is created if
    /// non-existing and assumed to be initially empty.
    pub fn new(extract_dir: &Path) -> Result<LayeredImage> {
        create_dir_all(extract_dir)?;
        Ok(LayeredImage {
            root: Rc::new(Directory::default()),
            next_layer_index: LayerIndex(0),
            extract_dir: extract_dir.to_path_buf(),
        })
    }

    /// Apply a new layer on top of the previous ones.
    ///
    /// If `handle_whiteouts` is true, whiteout files will hide the corresponding files from
    /// previous layers. If false, whiteout files are processed as regular files with no special
    /// meaning.
    pub fn add_layer<R: Read>(
        mut self,
        mut archive: Archive<R>,
        handle_whiteouts: bool,
    ) -> Result<LayeredImage> {
        let current_layer_index = self.next_layer_index;
        self.next_layer_index = LayerIndex(current_layer_index.0 + 1);

        // Mapping from paths to inserted nodes, to resolve hard links.
        let mut path_to_node: HashMap<Vec<u8>, NodeRef> = HashMap::new();

        for (i, entry) in archive.entries()?.enumerate() {
            let mut entry = entry?;

            let (ancestors, name) = parse_path(&entry.path_bytes())?;
            if name.is_empty() {
                assert!(ancestors.is_empty(), "Entry must be the root dir");

                // Keep the root dir's entries but rebuild its metadata.
                let entries = self.root.entries.take();
                self.root = Rc::new(Directory {
                    metadata: Metadata::from_entry(&mut entry)?,
                    entries: RefCell::new(entries),
                });
                continue;
            }

            assert!(ancestors.first() == Some(&Name::EMPTY), "Entry must be within root dir");
            let parent = self.get_or_create_directory(&ancestors, current_layer_index)?;
            let mut parent_entries = parent.entries.borrow_mut();

            // If requested, handle the special meaning of whiteout files.
            if handle_whiteouts {
                if name.is_whiteout_opaque() {
                    // Drop all the entries from previous layers.
                    parent_entries
                        .retain(|_, (_, layer_index)| *layer_index == current_layer_index);
                    continue;
                }
                if let Some(name) = name.strip_whiteout_prefix() {
                    if name.is_empty() {
                        bail!("Whiteout entry has an empty target name");
                    }
                    // Drop only the entry with the specific name, unless it came from the same
                    // layer.
                    if parent_entries
                        .get(&name)
                        .is_some_and(|(_, layer_index)| *layer_index != current_layer_index)
                    {
                        parent_entries.remove(&name);
                    }
                    continue;
                }
            }

            let node = match entry.header().entry_type() {
                tar::EntryType::Regular => {
                    // Generate a unique filename and extract the contents into it.
                    let extracted_name = format!("{}-{}", current_layer_index.0, i);
                    let extracted_path = self.extract_dir.join(&extracted_name);
                    copy(&mut entry, &mut std::fs::File::create(&extracted_path)?)?;

                    let file = File {
                        metadata: Metadata::from_entry(&mut entry)?,
                        data_file_path: extracted_path,
                    };
                    NodeRef::File(Rc::new(file))
                }
                tar::EntryType::Link => {
                    let link_path = entry.link_name_bytes().unwrap().to_vec();
                    let Some(node) = path_to_node.get(&link_path) else {
                        bail!("Hard link does not refer to an already-seen file");
                    };
                    node.clone()
                }
                tar::EntryType::Symlink => {
                    let link_path = entry.link_name_bytes().unwrap().to_vec();
                    let symlink = Symlink {
                        metadata: Metadata::from_entry(&mut entry)?,
                        target: Name(link_path),
                    };
                    NodeRef::Symlink(Rc::new(symlink))
                }
                tar::EntryType::Directory => {
                    // If a directory with the same name already exists, preserve its entries.
                    let entries = match parent_entries.get(&name) {
                        Some((NodeRef::Directory(directory), _)) => directory.entries.take(),
                        _ => HashMap::new(),
                    };

                    let directory = Directory {
                        metadata: Metadata::from_entry(&mut entry)?,
                        entries: RefCell::new(entries),
                    };
                    NodeRef::Directory(Rc::new(directory))
                }
                _ => {
                    unimplemented!("Tar entry type: {:?}", entry.header().entry_type());
                }
            };

            path_to_node.insert(entry.path_bytes().to_vec(), node.clone());
            parent_entries.insert(name, (node, current_layer_index));
        }

        Ok(self)
    }

    /// Ensures that the given path exists as a directory, creating it if necessary.
    pub fn ensure_directory_exists(mut self, path: &str) -> Result<LayeredImage> {
        let current_layer_index = self.next_layer_index;
        self.next_layer_index = LayerIndex(current_layer_index.0 + 1);

        let segments: Vec<Name> =
            path.split("/").map(|substr| Name(substr.as_bytes().to_vec())).collect();
        self.get_or_create_directory(&segments, current_layer_index)?;

        Ok(self)
    }

    /// Seals the file system hierarchy, assigns inode numbers, and returns the resulting root directory.
    pub fn finalize(self, inode_num_generator: &mut dyn FnMut() -> u64) -> Directory {
        let root_dir = Rc::try_unwrap(self.root)
            .map_err(|_| ())
            .expect("No entries should point to the root directory");

        // Execute a visit to assign inode numbers.
        let mut visitor = AssignInodeNumVisitor { inode_num_generator };
        visitor.visit_directory(b"", &root_dir);

        root_dir
    }

    /// Resolves the given path, whose last segment is assumed to be adirectory, creating
    /// intermediate directories in the process, if they don't exist yet.
    fn get_or_create_directory(
        &self,
        segments: &[Name],
        current_layer_index: LayerIndex,
    ) -> Result<Rc<Directory>> {
        let mut it = segments.iter();

        // Start from the root directory.
        assert!(it.next() == Some(&Name::EMPTY), "Path must start from the root dir");
        let mut cur = self.root.clone();

        while let Some(segment) = it.next() {
            let next = {
                let mut entries = cur.entries.borrow_mut();
                let entry = entries.entry(segment.clone()).or_insert_with(|| {
                    (NodeRef::Directory(Rc::new(Directory::default())), current_layer_index)
                });

                match entry {
                    (NodeRef::Directory(next), layer_index) => {
                        // Record the fact that this layer acknowledges the existence of this dir,
                        // to prevent it from being discarded by a whiteout in the same layer.
                        *layer_index = current_layer_index;
                        next.clone()
                    }
                    (node, layer_index) if *layer_index != current_layer_index => {
                        // A previous layer had a non-directory with the same name, replace it.
                        let new_dir = Rc::new(Directory::default());
                        *node = NodeRef::Directory(new_dir.clone());
                        *layer_index = current_layer_index;
                        new_dir
                    }
                    _ => {
                        bail!(
                            "The same layer references both a directory and a non-directory with the same name"
                        );
                    }
                }
            };
            cur = next;
        }

        Ok(cur)
    }
}

/// Metadata about a given inode.
pub struct Metadata {
    mode: u16,
    uid: u16,
    gid: u16,
    xattrs: BTreeMap<Box<[u8]>, Box<[u8]>>,

    // Assigned by `AssignInodeNumberVisitor` when `LayeredImage::finalize` is called.
    inode_num: Cell<Option<u64>>,
}

impl Metadata {
    fn from_entry<R: Read>(entry: &mut Entry<'_, R>) -> Result<Metadata> {
        let header = entry.header();
        let mode = (header.mode()? & 0o7777).try_into().unwrap();
        let uid = header.uid()?.try_into().context("uid")?;
        let gid = header.gid()?.try_into().context("gid")?;

        let mut xattrs = BTreeMap::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for extension in extensions {
                let extension = extension?;
                if let Some(key) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                    xattrs.insert(key.into(), extension.value_bytes().into());
                }
            }
        }

        Ok(Metadata { mode, uid, gid, xattrs, inode_num: Cell::new(None) })
    }

    pub fn mode(&self) -> u16 {
        self.mode
    }

    pub fn uid(&self) -> u16 {
        self.uid
    }

    pub fn gid(&self) -> u16 {
        self.gid
    }

    pub fn extended_attributes(&self) -> &BTreeMap<Box<[u8]>, Box<[u8]>> {
        &self.xattrs
    }

    pub fn inode_num(&self) -> u64 {
        self.inode_num.get().expect("this method is never called before assigning inode numbers")
    }
}

#[derive(Clone)]
enum NodeRef {
    File(Rc<File>),
    Symlink(Rc<Symlink>),
    Directory(Rc<Directory>),
}

pub struct File {
    metadata: Metadata,
    data_file_path: PathBuf,
}

impl File {
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Returns the path of the local file with the contents of this file.
    pub fn data_file_path(&self) -> &Path {
        &self.data_file_path
    }
}

pub struct Symlink {
    metadata: Metadata,
    target: Name,
}

impl Symlink {
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Returns the target of this symbolik link.
    pub fn target(&self) -> &[u8] {
        &self.target.0
    }
}

pub struct Directory {
    metadata: Metadata,

    /// Children of this directory, with the index of the layer that added them.
    entries: RefCell<HashMap<Name, (NodeRef, LayerIndex)>>,
}

impl Default for Directory {
    /// Creates an empty directory with "normal" metadata.
    ///
    /// It is used for directory that are implicitly referenced in paths as ancestors but never
    /// explicitly listed in the source archive.
    fn default() -> Self {
        Self {
            metadata: Metadata {
                mode: 0o755,
                uid: 0,
                gid: 0,
                xattrs: BTreeMap::new(),
                inode_num: Cell::new(None),
            },
            entries: RefCell::new(HashMap::new()),
        }
    }
}

impl Directory {
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn visit(&self, visitor: &mut dyn DirectoryVisitor) {
        // Sort entries to avoid nondeterminism in the visit order.
        let mut entries: Vec<_> = self
            .entries
            .borrow()
            .iter()
            .map(|(name, (node, _))| (name.clone(), node.clone()))
            .collect();
        entries.sort_by(|(name_a, _), (name_b, _)| name_a.cmp(name_b));

        // Run visitor on each entry.
        for (Name(name), node) in &entries {
            match node {
                NodeRef::File(file) => visitor.visit_file(name, file),
                NodeRef::Symlink(symlink) => visitor.visit_symlink(name, symlink),
                NodeRef::Directory(directory) => visitor.visit_directory(name, directory),
            };
        }
    }
}

pub trait DirectoryVisitor {
    fn visit_file(&mut self, name: &[u8], file: &File);
    fn visit_symlink(&mut self, name: &[u8], symlink: &Symlink);
    fn visit_directory(&mut self, name: &[u8], directory: &Directory);
}

/// A directory visitor that assigns inode numbers to all reachable nodes.
struct AssignInodeNumVisitor<'a> {
    inode_num_generator: &'a mut dyn FnMut() -> u64,
}

impl AssignInodeNumVisitor<'_> {
    fn visit_metadata(&mut self, metadata: &Metadata) -> bool {
        if metadata.inode_num.get().is_none() {
            metadata.inode_num.set(Some((self.inode_num_generator)()));
            true
        } else {
            false
        }
    }
}

impl DirectoryVisitor for AssignInodeNumVisitor<'_> {
    fn visit_file(&mut self, _name: &[u8], file: &File) {
        self.visit_metadata(file.metadata());
    }

    fn visit_symlink(&mut self, _name: &[u8], symlink: &Symlink) {
        self.visit_metadata(symlink.metadata());
    }

    fn visit_directory(&mut self, _name: &[u8], directory: &Directory) {
        if self.visit_metadata(directory.metadata()) {
            directory.visit(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    const DEFAULT_DIR_MODE: u32 = 0o755;
    const DEFAULT_FILE_MODE: u32 = 0o644;

    /// An entry to write into a synthetic tar archive.
    enum TarEntry {
        Dir { path: String, mode: u32, uid: u64, gid: u64 },
        File { path: String, contents: String, mode: u32, uid: u64, gid: u64 },
        Symlink { path: String, target: String },
        HardLink { path: String, target: String },
    }

    fn dir(path: &str) -> TarEntry {
        TarEntry::Dir { path: path.to_string(), mode: DEFAULT_DIR_MODE, uid: 0, gid: 0 }
    }

    fn file(path: &str, contents: &str) -> TarEntry {
        TarEntry::File {
            path: path.to_string(),
            contents: contents.to_string(),
            mode: DEFAULT_FILE_MODE,
            uid: 0,
            gid: 0,
        }
    }

    fn symlink(path: &str, target: &str) -> TarEntry {
        TarEntry::Symlink { path: path.to_string(), target: target.to_string() }
    }

    fn hard_link(path: &str, target: &str) -> TarEntry {
        TarEntry::HardLink { path: path.to_string(), target: target.to_string() }
    }

    /// Builds an in-memory tar archive containing `entries`, in the given order.
    fn build_archive(entries: Vec<TarEntry>) -> Archive<Cursor<Vec<u8>>> {
        let mut builder = tar::Builder::new(Vec::new());
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            match entry {
                TarEntry::Dir { path, mode, uid, gid } => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(mode);
                    header.set_uid(uid);
                    header.set_gid(gid);
                    header.set_size(0);
                    builder.append_data(&mut header, path, std::io::empty()).unwrap();
                }
                TarEntry::File { path, contents, mode, uid, gid } => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(mode);
                    header.set_uid(uid);
                    header.set_gid(gid);
                    header.set_size(contents.len() as u64);
                    builder.append_data(&mut header, path, contents.as_bytes()).unwrap();
                }
                TarEntry::Symlink { path, target } => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_size(0);
                    builder.append_link(&mut header, path, target).unwrap();
                }
                TarEntry::HardLink { path, target } => {
                    header.set_entry_type(tar::EntryType::Link);
                    header.set_mode(DEFAULT_FILE_MODE);
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_size(0);
                    builder.append_link(&mut header, path, target).unwrap();
                }
            }
        }
        Archive::new(Cursor::new(builder.into_inner().unwrap()))
    }

    /// Merges the given layers, bottom-up, into a finalized image.
    ///
    /// Each layer is paired with the `handle_whiteouts` flag to apply to it.
    fn merge(extract_dir: &Path, layers: Vec<(Vec<TarEntry>, bool)>) -> Result<Directory> {
        let mut image = LayeredImage::new(extract_dir)?;
        for (entries, handle_whiteouts) in layers {
            image = image.add_layer(build_archive(entries), handle_whiteouts)?;
        }

        let mut next_inode_num = ext4_metadata::ROOT_INODE_NUM;
        Ok(image.finalize(&mut || {
            let result = next_inode_num;
            next_inode_num += 1;
            result
        }))
    }

    /// A flattened view of a finalized image, so that tests can make whole-tree assertions.
    #[derive(Default)]
    struct Snapshot {
        /// Description of each node, keyed by absolute path.
        nodes: BTreeMap<String, String>,

        /// Inode number of each node, keyed by absolute path.
        inode_nums: BTreeMap<String, u64>,

        /// (mode, uid, gid) of each node, keyed by absolute path.
        attributes: BTreeMap<String, (u16, u16, u16)>,

        /// Absolute path of the directory currently being visited.
        prefix: String,
    }

    impl Snapshot {
        fn of(root: &Directory) -> Snapshot {
            let mut snapshot = Snapshot::default();
            root.visit(&mut snapshot);
            snapshot
        }

        fn record(&mut self, name: &[u8], metadata: &Metadata, description: String) -> String {
            let path = format!("{}/{}", self.prefix, String::from_utf8_lossy(name));
            self.nodes.insert(path.clone(), description);
            self.inode_nums.insert(path.clone(), metadata.inode_num());
            self.attributes.insert(path.clone(), (metadata.mode(), metadata.uid(), metadata.gid()));
            path
        }

        /// Returns every node as a (path, description) pair, sorted by path.
        fn nodes(&self) -> Vec<(&str, &str)> {
            self.nodes.iter().map(|(path, description)| (&**path, &**description)).collect()
        }
    }

    impl DirectoryVisitor for Snapshot {
        fn visit_file(&mut self, name: &[u8], file: &File) {
            let contents = std::fs::read_to_string(file.data_file_path()).unwrap();
            self.record(name, file.metadata(), format!("file:{contents}"));
        }

        fn visit_symlink(&mut self, name: &[u8], symlink: &Symlink) {
            let target = String::from_utf8_lossy(symlink.target()).into_owned();
            self.record(name, symlink.metadata(), format!("symlink:{target}"));
        }

        fn visit_directory(&mut self, name: &[u8], directory: &Directory) {
            let path = self.record(name, directory.metadata(), "dir".to_string());
            let parent_prefix = std::mem::replace(&mut self.prefix, path);
            directory.visit(self);
            self.prefix = parent_prefix;
        }
    }

    #[test]
    fn single_layer_is_preserved() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![(
                vec![
                    dir("etc"),
                    file("etc/hosts", "localhost"),
                    symlink("etc/mtab", "/proc/mounts"),
                ],
                false,
            )],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![
                ("/etc", "dir"),
                ("/etc/hosts", "file:localhost"),
                ("/etc/mtab", "symlink:/proc/mounts"),
            ]
        );
    }

    #[test]
    fn missing_parent_directories_are_created() {
        // Layers routinely omit directory entries for ancestors they did not modify.
        let extract_dir = TempDir::new().unwrap();
        let root =
            merge(extract_dir.path(), vec![(vec![file("usr/lib/libc.so", "elf")], false)]).unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/usr", "dir"), ("/usr/lib", "dir"), ("/usr/lib/libc.so", "file:elf")]
        );
    }

    #[test]
    fn upper_layer_replaces_lower_layer_file() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("etc/hosts", "lower")], false),
                (vec![file("etc/hosts", "upper")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/etc", "dir"), ("/etc/hosts", "file:upper")]
        );
    }

    #[test]
    fn whiteout_hides_file_from_lower_layer() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("etc/hosts", "lower"), file("etc/passwd", "root")], false),
                (vec![file("etc/.wh.hosts", "")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/etc", "dir"), ("/etc/passwd", "file:root")]
        );
    }

    #[test]
    fn whiteout_keeps_entry_from_the_same_layer() {
        // An explicit whiteout hides lower layers only, even if the entry in its own layer
        // appeared before the whiteout in the archive.
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("etc/hosts", "lower")], false),
                (vec![file("etc/hosts", "upper"), file("etc/.wh.hosts", "")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/etc", "dir"), ("/etc/hosts", "file:upper")]
        );
    }

    #[test]
    fn bare_whiteout_prefix_is_rejected() {
        let extract_dir = TempDir::new().unwrap();
        let Err(error) = merge(extract_dir.path(), vec![(vec![file("etc/.wh.", "")], true)]) else {
            panic!("a '.wh.' entry with no basename to delete should fail");
        };

        assert!(error.to_string().contains("Whiteout"), "unexpected error: {error}");
    }

    #[test]
    fn whiteouts_are_literal_when_disabled() {
        // The base layer of a Docker image cannot contain whiteouts, so a file that merely looks
        // like one must be kept verbatim.
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![(vec![file("etc/hosts", "lower"), file("etc/.wh.hosts", "surprise")], false)],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![
                ("/etc", "dir"),
                ("/etc/.wh.hosts", "file:surprise"),
                ("/etc/hosts", "file:lower"),
            ]
        );
    }

    #[test]
    fn opaque_whiteout_hides_lower_layer_directory_contents() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("etc/hosts", "lower"), file("etc/passwd", "root")], false),
                (vec![file("etc/.wh..wh..opq", ""), file("etc/resolv.conf", "nameserver")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/etc", "dir"), ("/etc/resolv.conf", "file:nameserver")]
        );
    }

    #[test]
    fn opaque_whiteout_keeps_entries_from_the_same_layer() {
        // The opaque marker hides lower layers only, even for entries its own layer added before
        // the marker appeared in the archive.
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("etc/hosts", "lower")], false),
                (vec![file("etc/resolv.conf", "nameserver"), file("etc/.wh..wh..opq", "")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/etc", "dir"), ("/etc/resolv.conf", "file:nameserver")]
        );
    }

    #[test]
    fn hard_links_share_an_inode() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![(vec![file("bin/busybox", "binary"), hard_link("bin/sh", "bin/busybox")], false)],
        )
        .unwrap();

        let snapshot = Snapshot::of(&root);
        assert_eq!(
            snapshot.nodes(),
            vec![("/bin", "dir"), ("/bin/busybox", "file:binary"), ("/bin/sh", "file:binary")]
        );
        assert_eq!(snapshot.inode_nums["/bin/busybox"], snapshot.inode_nums["/bin/sh"]);
    }

    #[test]
    fn directory_replaces_file_from_lower_layer() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("opt", "not-a-directory")], false),
                (vec![file("opt/app/data", "hello")], true),
            ],
        )
        .unwrap();

        assert_eq!(
            Snapshot::of(&root).nodes(),
            vec![("/opt", "dir"), ("/opt/app", "dir"), ("/opt/app/data", "file:hello")]
        );
    }

    #[test]
    fn file_replaces_directory_from_lower_layer() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![
                (vec![file("opt/app/data", "hello")], false),
                (vec![file("opt", "not-a-directory")], true),
            ],
        )
        .unwrap();

        assert_eq!(Snapshot::of(&root).nodes(), vec![("/opt", "file:not-a-directory")]);
    }

    #[test]
    fn metadata_is_preserved() {
        let extract_dir = TempDir::new().unwrap();
        let root = merge(
            extract_dir.path(),
            vec![(
                vec![
                    TarEntry::Dir { path: "etc".to_string(), mode: 0o751, uid: 1, gid: 2 },
                    TarEntry::File {
                        path: "etc/shadow".to_string(),
                        contents: "secret".to_string(),
                        mode: 0o600,
                        uid: 42,
                        gid: 43,
                    },
                ],
                false,
            )],
        )
        .unwrap();

        let snapshot = Snapshot::of(&root);
        assert_eq!(snapshot.attributes["/etc"], (0o751, 1, 2));
        assert_eq!(snapshot.attributes["/etc/shadow"], (0o600, 42, 43));
    }

    #[test]
    fn hard_link_to_unknown_path_is_rejected() {
        let extract_dir = TempDir::new().unwrap();
        let Err(error) =
            merge(extract_dir.path(), vec![(vec![hard_link("bin/sh", "bin/busybox")], false)])
        else {
            panic!("hard link to a file that was never seen should fail");
        };

        assert!(error.to_string().contains("Hard link"), "unexpected error: {error}");
    }
}
