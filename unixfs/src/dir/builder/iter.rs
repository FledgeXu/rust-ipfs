use super::{DirBuilder, Entry, Leaf, TreeConstructionFailed, TreeOptions};
use cid::Cid;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// Constructs the directory nodes required for a tree.
///
/// Implements the Iterator interface for owned values and the borrowed version, `next_borrowed`.
/// The tree is fully constructed once this has been exhausted.
pub struct PostOrderIterator {
    full_path: String,
    old_depth: usize,
    block_buffer: Vec<u8>,
    // our stack of pending work
    pending: Vec<Visited>,
    // "communication channel" from nested entries back to their parents
    persisted_cids: HashMap<Option<u64>, BTreeMap<String, Leaf>>,
    reused_children: Vec<Visited>,
    cid: Option<Cid>,
    total_size: u64,
    // from TreeOptions
    opts: TreeOptions,
}

#[derive(Debug)]
enum Visited {
    Descent {
        node: DirBuilder,
        name: Option<String>,
        depth: usize,
    },
    Post {
        parent_id: Option<u64>,
        id: u64,
        name: Option<String>,
        depth: usize,
        leaves: Vec<(String, Leaf)>,
    },
}

impl PostOrderIterator {
    pub(super) fn new(root: DirBuilder, opts: TreeOptions) -> Self {
        PostOrderIterator {
            full_path: Default::default(),
            old_depth: 0,
            block_buffer: Default::default(),
            pending: vec![Visited::Descent {
                node: root,
                name: None,
                depth: 0,
            }],
            persisted_cids: Default::default(),
            reused_children: Vec::new(),
            cid: None,
            total_size: 0,
            opts,
        }
    }

    fn render_directory(
        links: &BTreeMap<String, Leaf>,
        buffer: &mut Vec<u8>,
        block_size_limit: &Option<u64>,
    ) -> Result<Leaf, TreeConstructionFailed> {
        use crate::pb::{UnixFs, UnixFsType};
        use quick_protobuf::{BytesWriter, MessageWrite, Writer, WriterBackend};
        use sha2::{Digest, Sha256};

        // FIXME: ideas on how to turn this into a HAMT sharding on some heuristic. we probably
        // need to introduce states in to the "iterator":
        //
        // 1. bucketization
        // 2. another post order visit of the buckets?
        //
        // the nested post order visit should probably re-use the existing infra ("message
        // passing") and new ids can be generated by giving this iterator the counter from
        // BufferedTreeBuilder.
        //
        // could also be that the HAMT shard building should start earlier, since the same
        // heuristic can be detected *at* bufferedtreewriter. there the split would be easier, and
        // this would "just" be a single node rendering, and not need any additional states..

        /// Newtype around Cid to allow embedding it as PBLink::Hash without allocating a vector.
        struct WriteableCid<'a>(&'a Cid);

        impl<'a> MessageWrite for WriteableCid<'a> {
            fn get_size(&self) -> usize {
                use cid::Version::*;
                use quick_protobuf::sizeofs::*;

                match self.0.version() {
                    V0 => self.0.hash().as_bytes().len(),
                    V1 => {
                        let version_len = 1;
                        let codec_len = sizeof_varint(u64::from(self.0.codec()));
                        let hash_len = self.0.hash().as_bytes().len();
                        version_len + codec_len + hash_len
                    }
                }
            }

            fn write_message<W: WriterBackend>(
                &self,
                w: &mut Writer<W>,
            ) -> quick_protobuf::Result<()> {
                use cid::Version::*;

                match self.0.version() {
                    V0 => {
                        for b in self.0.hash().as_bytes() {
                            w.write_u8(*b)?;
                        }
                        Ok(())
                    }
                    V1 => {
                        // it is possible that Cidv1 should not be linked to from a unixfs
                        // directory; at least go-ipfs 0.5 `ipfs files` denies making a cbor link
                        w.write_u8(1)?;
                        w.write_varint(u64::from(self.0.codec()))?;
                        for b in self.0.hash().as_bytes() {
                            w.write_u8(*b)?;
                        }
                        Ok(())
                    }
                }
            }
        }

        /// Newtype which uses the BTreeMap<String, Leaf> as Vec<PBLink>.
        struct BTreeMappedDir<'a> {
            links: &'a BTreeMap<String, Leaf>,
            data: UnixFs<'a>,
        }

        /// Newtype which represents an entry from BTreeMap<String, Leaf> as PBLink as far as the
        /// protobuf representation goes.
        struct EntryAsPBLink<'a>(&'a String, &'a Leaf);

        impl<'a> MessageWrite for EntryAsPBLink<'a> {
            fn get_size(&self) -> usize {
                use quick_protobuf::sizeofs::*;

                // ones are the tags
                1 + sizeof_len(self.0.len())
                    + 1
                    //+ sizeof_len(WriteableCid(&self.1.link).get_size())
                    + sizeof_len(self.1.link.to_bytes().len())
                    + 1
                    + sizeof_varint(self.1.total_size)
            }

            fn write_message<W: WriterBackend>(
                &self,
                w: &mut Writer<W>,
            ) -> quick_protobuf::Result<()> {
                // w.write_with_tag(10, |w| w.write_message(&WriteableCid(&self.1.link)))?;
                w.write_with_tag(10, |w| w.write_bytes(&self.1.link.to_bytes()))?;
                w.write_with_tag(18, |w| w.write_string(self.0.as_str()))?;
                w.write_with_tag(24, |w| w.write_uint64(self.1.total_size))?;
                Ok(())
            }
        }

        impl<'a> MessageWrite for BTreeMappedDir<'a> {
            fn get_size(&self) -> usize {
                use quick_protobuf::sizeofs::*;

                let links = self
                    .links
                    .iter()
                    .map(|(k, v)| EntryAsPBLink(k, v))
                    .map(|link| 1 + sizeof_len(link.get_size()))
                    .sum::<usize>();

                links + 1 + sizeof_len(self.data.get_size())
            }
            fn write_message<W: WriterBackend>(
                &self,
                w: &mut Writer<W>,
            ) -> quick_protobuf::Result<()> {
                for l in self.links.iter().map(|(k, v)| EntryAsPBLink(k, v)) {
                    w.write_with_tag(18, |w| w.write_message(&l))?;
                }
                w.write_with_tag(10, |w| w.write_message(&self.data))
            }
        }

        let btreed = BTreeMappedDir {
            links,
            data: UnixFs {
                Type: UnixFsType::Directory,
                ..Default::default()
            },
        };

        let size = btreed.get_size();

        if let Some(limit) = block_size_limit {
            let size = size as u64;
            if *limit < size {
                // FIXME: this could probably be detected at
                return Err(TreeConstructionFailed::TooLargeBlock(size));
            }
        }

        // FIXME: we shouldn't be creating too large structures (bitswap block size limit!)
        // FIXME: changing this to autosharding is going to take some thinking

        let cap = buffer.capacity();

        if let Some(additional) = size.checked_sub(cap) {
            buffer.reserve(additional);
        }

        if let Some(needed_zeroes) = size.checked_sub(buffer.len()) {
            buffer.extend(std::iter::repeat(0).take(needed_zeroes));
        }

        let mut writer = Writer::new(BytesWriter::new(&mut buffer[..]));
        btreed
            .write_message(&mut writer)
            .map_err(TreeConstructionFailed::Protobuf)?;

        buffer.truncate(size);

        let mh = multihash::wrap(multihash::Code::Sha2_256, &Sha256::digest(&buffer));
        let cid = Cid::new_v0(mh).expect("sha2_256 is the correct multihash for cidv0");

        let combined_from_links = links
            .values()
            .map(|Leaf { total_size, .. }| total_size)
            .sum::<u64>();

        Ok(Leaf {
            link: cid,
            total_size: buffer.len() as u64 + combined_from_links,
        })
    }

    /// Construct the next dag-pb node, if any.
    ///
    /// Returns a `TreeNode` of the latest constructed tree node.
    pub fn next_borrowed(&mut self) -> Option<Result<TreeNode<'_>, TreeConstructionFailed>> {
        while let Some(visited) = self.pending.pop() {
            let (name, depth) = match &visited {
                Visited::Descent { name, depth, .. } => (name.as_deref(), *depth),
                Visited::Post { name, depth, .. } => (name.as_deref(), *depth),
            };

            update_full_path((&mut self.full_path, &mut self.old_depth), name, depth);

            match visited {
                Visited::Descent { node, name, depth } => {
                    let mut leaves = Vec::new();

                    let children = &mut self.reused_children;

                    for (k, v) in node.nodes {
                        match v {
                            Entry::Directory(node) => children.push(Visited::Descent {
                                node,
                                name: Some(k),
                                depth: depth + 1,
                            }),
                            Entry::Leaf(leaf) => leaves.push((k, leaf)),
                        }
                    }

                    self.pending.push(Visited::Post {
                        parent_id: node.parent_id,
                        id: node.id,
                        name,
                        depth,
                        leaves,
                    });

                    let any_children = !children.is_empty();

                    self.pending.extend(children.drain(..));

                    if any_children {
                        // we could strive to do everything right now but pushing and popping might
                        // turn out easier code wise, or in other words, when there are no child_nodes
                        // we wouldn't need to go through Visited::Post.
                    }
                }
                Visited::Post {
                    parent_id,
                    id,
                    name,
                    leaves,
                    ..
                } => {
                    // all of our children have now been visited; we should be able to find their
                    // Cids in the btreemap
                    let mut collected = self.persisted_cids.remove(&Some(id)).unwrap_or_default();

                    // FIXME: leaves could be drained and reused
                    collected.extend(leaves);

                    if !self.opts.wrap_with_directory && parent_id.is_none() {
                        // we aren't supposed to wrap_with_directory, and we are now looking at the
                        // possibly to-be-generated root directory.

                        assert_eq!(
                            collected.len(),
                            1,
                            "should not have gone this far with multiple added roots"
                        );

                        return None;
                    }

                    let buffer = &mut self.block_buffer;

                    let leaf = match Self::render_directory(
                        &collected,
                        buffer,
                        &self.opts.block_size_limit,
                    ) {
                        Ok(leaf) => leaf,
                        Err(e) => return Some(Err(e)),
                    };

                    self.cid = Some(leaf.link.clone());
                    self.total_size = leaf.total_size;

                    // this reuse strategy is probably good enough
                    collected.clear();

                    if let Some(name) = name {
                        // name is None only for wrap_with_directory, which cannot really be
                        // propagated up but still the parent_id is allowed to be None
                        let previous = self
                            .persisted_cids
                            .entry(parent_id)
                            .or_insert(collected)
                            .insert(name, leaf);

                        assert!(previous.is_none());
                    }

                    return Some(Ok(TreeNode {
                        path: self.full_path.as_str(),
                        cid: self.cid.as_ref().unwrap(),
                        total_size: self.total_size,
                        block: &self.block_buffer,
                    }));
                }
            }
        }
        None
    }
}

impl Iterator for PostOrderIterator {
    type Item = Result<OwnedTreeNode, TreeConstructionFailed>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_borrowed()
            .map(|res| res.map(TreeNode::into_owned))
    }
}

/// Borrowed representation of a node in the tree.
pub struct TreeNode<'a> {
    /// Full path to the node.
    pub path: &'a str,
    /// The Cid of the document.
    pub cid: &'a Cid,
    /// Cumulative total size of the subtree in bytes.
    pub total_size: u64,
    /// Raw dag-pb document.
    pub block: &'a [u8],
}

impl<'a> fmt::Debug for TreeNode<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("TreeNode")
            .field("path", &format_args!("{:?}", self.path))
            .field("cid", &format_args!("{}", self.cid))
            .field("total_size", &self.total_size)
            .field("size", &self.block.len())
            .finish()
    }
}

impl TreeNode<'_> {
    /// Convert to an owned and detached representation.
    pub fn into_owned(self) -> OwnedTreeNode {
        OwnedTreeNode {
            path: self.path.to_owned(),
            cid: self.cid.to_owned(),
            total_size: self.total_size,
            block: self.block.into(),
        }
    }
}

/// Owned representation of a node in the tree.
pub struct OwnedTreeNode {
    /// Full path to the node.
    pub path: String,
    /// The Cid of the document.
    pub cid: Cid,
    /// Cumulative total size of the subtree in bytes.
    pub total_size: u64,
    /// Raw dag-pb document.
    pub block: Box<[u8]>,
}

fn update_full_path(
    (full_path, old_depth): (&mut String, &mut usize),
    name: Option<&str>,
    depth: usize,
) {
    if depth < 2 {
        // initially thought it might be a good idea to add a slash to all components; removing it made
        // it impossible to get back down to empty string, so fixing this for depths 0 and 1.
        full_path.clear();
        *old_depth = 0;
    } else {
        while *old_depth >= depth && *old_depth > 0 {
            // we now want to pop the last segment
            // this would be easier with PathBuf
            let slash_at = full_path.bytes().rposition(|ch| ch == b'/');
            if let Some(slash_at) = slash_at {
                full_path.truncate(slash_at);
                *old_depth -= 1;
            } else {
                todo!(
                    "no last slash_at in {:?} yet {} >= {}",
                    full_path,
                    old_depth,
                    depth
                );
            }
        }
    }

    debug_assert!(*old_depth <= depth);

    if let Some(name) = name {
        if !full_path.is_empty() {
            full_path.push_str("/");
        }
        full_path.push_str(name);
        *old_depth += 1;
    }

    assert_eq!(*old_depth, depth);
}
