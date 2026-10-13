use crate::node_color;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub struct Rectangle {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rectangle {
    pub fn contains_point(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.width && py >= self.y && py <= self.y + self.height
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RGBA {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

pub type NodeID = u32;
pub const NO_NODE: NodeID = NodeID::MAX;

// 32 bytes per node: names live in a shared arena, full paths are rebuilt
// from the parent chain on demand, children form an intrusive sibling list
#[derive(Debug, Clone, Copy)]
pub struct Node {
    pub size: u64,
    name_start: u32,
    pub parent: NodeID,
    pub first_child: NodeID,
    pub next_sibling: NodeID,
    name_len: u16,
    pub depth: u16,
    pub is_file: bool,
}

impl Node {
    pub fn color(&self) -> RGBA {
        match self.is_file {
            false => node_color::depth_dir_color(self.depth as usize),
            true => node_color::depth_file_color(self.depth as usize),
        }
    }
    pub fn has_children(&self) -> bool {
        self.first_child != NO_NODE
    }
}

#[derive(Debug)]
pub struct Tree {
    elems: Vec<Node>,
    // OsStr encoded bytes of every node name, back to back
    names: Vec<u8>,
}

impl Tree {
    pub fn new(root_name: &str) -> Tree {
        let mut tree = Tree {
            elems: Vec::new(),
            names: Vec::new(),
        };
        let (name_start, name_len) = tree.push_name(OsStr::new(root_name));
        tree.elems.push(Node {
            size: 0,
            name_start,
            parent: NO_NODE,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
            name_len,
            depth: 0,
            is_file: false,
        });
        tree
    }
    fn push_name(&mut self, name: &OsStr) -> (u32, u16) {
        let bytes = name.as_encoded_bytes();
        let start = self.names.len();
        // real file systems cap names far below u16::MAX; anything that does not
        // fit is stored as an empty name rather than truncated mid-character
        if bytes.len() > u16::MAX as usize || start + bytes.len() > u32::MAX as usize {
            return (0, 0);
        }
        self.names.extend_from_slice(bytes);
        (start as u32, bytes.len() as u16)
    }
    pub fn os_name(&self, id: NodeID) -> &OsStr {
        let node = &self.elems[id as usize];
        let start = node.name_start as usize;
        let bytes = &self.names[start..start + node.name_len as usize];
        // SAFETY: every range stored in a node is either empty or exactly the
        // bytes of one whole OsStr::as_encoded_bytes() call made in push_name
        unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
    }
    pub fn name(&self, id: NodeID) -> Cow<'_, str> {
        self.os_name(id).to_string_lossy()
    }
    pub fn path_of(&self, id: NodeID) -> PathBuf {
        let mut chain = vec![id];
        let mut node = id;
        while let Some(p) = self.parent_of(node) {
            chain.push(p);
            node = p;
        }
        let mut path = PathBuf::new();
        for n in chain.into_iter().rev() {
            path.push(self.os_name(n));
        }
        path
    }
    fn parent_of(&self, id: NodeID) -> Option<NodeID> {
        let parent = self.elems[id as usize].parent;
        (parent != NO_NODE).then_some(parent)
    }
    pub fn children(&self, id: NodeID) -> Children<'_> {
        Children {
            tree: self,
            next: self.elems[id as usize].first_child,
        }
    }
    // the size is not propagated to ancestors, call propagate_size once per batch
    pub fn add_child(&mut self, parent: NodeID, name: &OsStr, is_file: bool, size: u64) -> NodeID {
        let id = self.elems.len() as NodeID;
        let (name_start, name_len) = self.push_name(name);
        let parent_node = &mut self.elems[parent as usize];
        let next_sibling = parent_node.first_child;
        parent_node.first_child = id;
        let depth = parent_node.depth.saturating_add(1);
        self.elems.push(Node {
            size,
            name_start,
            parent,
            first_child: NO_NODE,
            next_sibling,
            name_len,
            depth,
            is_file,
        });
        id
    }
    // adds size to node and all its ancestors
    pub fn propagate_size(&mut self, mut node: NodeID, size: u64) {
        if size == 0 {
            return;
        }
        loop {
            let elem = &mut self.elems[node as usize];
            elem.size += size;
            if elem.parent == NO_NODE {
                break;
            }
            node = elem.parent;
        }
    }
    pub fn invalidate_elem(&mut self, node: NodeID) {
        let Some(parent) = self.parent_of(node) else {
            return;
        };
        let size = self.elems[node as usize].size;
        let next = self.elems[node as usize].next_sibling;
        if self.elems[parent as usize].first_child == node {
            self.elems[parent as usize].first_child = next;
        } else {
            let mut prev = self.elems[parent as usize].first_child;
            while prev != NO_NODE {
                if self.elems[prev as usize].next_sibling == node {
                    self.elems[prev as usize].next_sibling = next;
                    break;
                }
                prev = self.elems[prev as usize].next_sibling;
            }
        }
        let mut ancestor = parent;
        loop {
            let elem = &mut self.elems[ancestor as usize];
            elem.size = elem.size.saturating_sub(size);
            if elem.parent == NO_NODE {
                break;
            }
            ancestor = elem.parent;
        }
        // detach it, so entries still arriving from the scan for this subtree
        // no longer reach the ancestors' sizes
        let elem = &mut self.elems[node as usize];
        elem.parent = NO_NODE;
        elem.next_sibling = NO_NODE;
    }
    pub fn get_elem(&self, id: NodeID) -> &Node {
        &self.elems[id as usize]
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.elems.len()
    }
    #[cfg(test)]
    pub fn heap_bytes(&self) -> usize {
        self.elems.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }
    pub fn clear(&mut self) {
        self.elems = Vec::new();
        self.names = Vec::new();
    }
}

impl Default for Tree {
    fn default() -> Self {
        Tree::new("")
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeID,
}

impl Iterator for Children<'_> {
    type Item = NodeID;
    fn next(&mut self) -> Option<NodeID> {
        if self.next == NO_NODE {
            return None;
        }
        let id = self.next;
        self.next = self.tree.elems[id as usize].next_sibling;
        Some(id)
    }
}

#[derive(Debug, Clone)]
pub struct GUINode {
    pub rect: Rectangle,
    pub node_id: NodeID,
    pub color: RGBA,
    pub label: String,
}
