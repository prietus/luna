//! In-memory tree view over a flat list of archive entries, plus the on-disk
//! manifest cache.
use super::models::ArchiveEntry;
use super::paths;
use std::cmp::Ordering;
use std::collections::HashMap;

pub type NodeId = usize;

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    /// `None` for synthetic intermediate directories.
    pub entry: Option<ArchiveEntry>,
    pub children: Vec<NodeId>,
    pub children_by_name: HashMap<String, NodeId>,
    pub parent: Option<NodeId>,
    /// Total size of this subtree in bytes.
    pub total_size: u64,
}

impl TreeNode {
    pub fn is_directory(&self) -> bool {
        // Synthetic intermediate nodes act as dirs.
        self.entry.as_ref().map(|e| e.is_directory()).unwrap_or(true)
    }
}

/// Arena-backed tree. Built once per archive load and never mutated after.
/// Node 0 is the root (`name == "/"`, `path == ""`).
#[derive(Debug, Clone)]
pub struct ArchiveTree {
    pub nodes: Vec<TreeNode>,
}

impl ArchiveTree {
    pub const ROOT: NodeId = 0;

    pub fn build(entries: &[ArchiveEntry]) -> ArchiveTree {
        let mut nodes = vec![TreeNode {
            name: "/".into(),
            path: String::new(),
            entry: None,
            children: vec![],
            children_by_name: HashMap::new(),
            parent: None,
            total_size: 0,
        }];
        for entry in entries {
            let parts: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                continue;
            }
            let mut node = Self::ROOT;
            for (i, part) in parts.iter().enumerate() {
                let is_last = i == parts.len() - 1;
                if let Some(&child) = nodes[node].children_by_name.get(*part) {
                    if is_last && nodes[child].entry.is_none() {
                        // Replace synthetic node with the real entry.
                        nodes[child].entry = Some(entry.clone());
                    }
                    node = child;
                } else {
                    let id = nodes.len();
                    nodes.push(TreeNode {
                        name: (*part).to_string(),
                        path: parts[..=i].join("/"),
                        entry: if is_last { Some(entry.clone()) } else { None },
                        children: vec![],
                        children_by_name: HashMap::new(),
                        parent: Some(node),
                        total_size: 0,
                    });
                    nodes[node].children.push(id);
                    nodes[node].children_by_name.insert((*part).to_string(), id);
                    node = id;
                }
            }
        }
        let mut tree = ArchiveTree { nodes };
        tree.finish();
        tree
    }

    /// Sorts children (directories first, then natural name order) and
    /// computes subtree sizes bottom-up.
    fn finish(&mut self) {
        // Children always have a larger index than their parent, so a
        // reverse sweep visits every child before its parent.
        for id in (0..self.nodes.len()).rev() {
            let own = self.nodes[id].entry.as_ref().map(|e| e.size_bytes()).unwrap_or(0);
            let sum: u64 = self.nodes[id].children.iter().map(|&c| self.nodes[c].total_size).sum();
            self.nodes[id].total_size = own + sum;
        }
        for id in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[id].children);
            children.sort_by(|&a, &b| {
                let (na, nb) = (&self.nodes[a], &self.nodes[b]);
                match (na.is_directory(), nb.is_directory()) {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    _ => natural_cmp(&na.name, &nb.name),
                }
            });
            self.nodes[id].children = children;
        }
    }

    pub fn root(&self) -> &TreeNode {
        &self.nodes[Self::ROOT]
    }

    pub fn get(&self, id: NodeId) -> &TreeNode {
        &self.nodes[id]
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = (NodeId, &TreeNode)> {
        self.nodes[id].children.iter().map(move |&c| (c, &self.nodes[c]))
    }

    /// Walks `path` from the root; stops at the deepest segment that exists.
    pub fn node_at(&self, path: &[String]) -> NodeId {
        let mut current = Self::ROOT;
        for segment in path {
            match self.nodes[current].children_by_name.get(segment) {
                Some(&next) => current = next,
                None => break,
            }
        }
        current
    }

    /// Path segments from the root down to `id`.
    pub fn segments_of(&self, id: NodeId) -> Vec<String> {
        let mut out = vec![];
        let mut cur = id;
        while let Some(parent) = self.nodes[cur].parent {
            out.push(self.nodes[cur].name.clone());
            cur = parent;
        }
        out.reverse();
        out
    }
}

/// Case-insensitive comparison that orders embedded numbers numerically
/// ("file2" < "file10"), like Finder's `localizedStandardCompare`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                let ord = ca.to_lowercase().cmp(cb.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// On-disk cache of archive file listings, keyed by the archive's Borg id.
/// Archive ids are immutable, so cached entries never need invalidation.
pub mod cache {
    use super::*;

    fn file(archive_id: &str) -> std::path::PathBuf {
        let safe: String = archive_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        paths::archive_cache_dir().join(format!("{safe}.json"))
    }

    pub fn load(archive_id: &str) -> Option<Vec<ArchiveEntry>> {
        let data = std::fs::read(file(archive_id)).ok()?;
        serde_json::from_slice(&data).ok()
    }

    pub fn save(archive_id: &str, entries: &[ArchiveEntry]) {
        if let Ok(data) = serde_json::to_vec(entries) {
            let _ = std::fs::write(file(archive_id), data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: &str, path: &str, size: u64) -> ArchiveEntry {
        ArchiveEntry { kind: kind.into(), path: path.into(), size: Some(size), mtime: None }
    }

    #[test]
    fn builds_sorted_tree_with_sizes() {
        let tree = ArchiveTree::build(&[
            e("-", "home/u/b10.txt", 10),
            e("-", "home/u/B2.txt", 5),
            e("d", "home/u/zdir", 0),
            e("-", "home/u/zdir/x", 100),
            e("d", "home", 0),
        ]);
        let u = tree.node_at(&["home".into(), "u".into()]);
        let names: Vec<&str> = tree.children(u).map(|(_, n)| n.name.as_str()).collect();
        assert_eq!(names, ["zdir", "B2.txt", "b10.txt"]);
        assert_eq!(tree.root().total_size, 115);
        assert!(tree.get(tree.node_at(&["home".into()])).entry.is_some());
        assert_eq!(tree.segments_of(u), ["home", "u"]);
    }

    #[test]
    fn natural_order() {
        assert_eq!(natural_cmp("a2", "a10"), Ordering::Less);
        assert_eq!(natural_cmp("Abc", "abd"), Ordering::Less);
    }
}
