use crate::{
    config::{MAX_VISIBLE_FS_DEPTH, MIN_BOX_SIZE},
    types::{GUINode, NodeID, Rectangle, Tree},
    utils::bytes_display,
};

// a child of the folder being laid out, or (node None) the sum of all
// children too small to ever get a box of MIN_BOX_SIZE
#[derive(Clone, Copy)]
struct Item {
    node: Option<NodeID>,
    size: u64,
}

// wrapper function
pub fn compute_gui_nodes(
    tree: &Tree,
    root_id: NodeID,
    bound: Rectangle,
    text_offset: f32,
) -> Vec<GUINode> {
    let mut gui_nodes = vec![];
    layout_node(tree, root_id, bound, 0, text_offset, &mut gui_nodes);
    gui_nodes
}

fn too_small(bound: &Rectangle) -> bool {
    bound.width < MIN_BOX_SIZE || bound.height < MIN_BOX_SIZE
}

fn layout_node(
    tree: &Tree,
    id: NodeID,
    mut bound: Rectangle,
    dir_level: usize,
    text_offset: f32,
    gui_nodes: &mut Vec<GUINode>,
) {
    if dir_level > MAX_VISIBLE_FS_DEPTH || too_small(&bound) {
        return;
    }
    let node = tree.get_elem(id);
    gui_nodes.push(GUINode {
        rect: bound,
        node_id: id,
        color: node.color(),
        label: format!("{} ({})", tree.name(id), bytes_display(node.size)),
    });
    if !node.has_children() {
        return;
    }

    // add padding for directory
    bound = Rectangle {
        x: bound.x + 3.0,
        y: bound.y + text_offset,
        width: bound.width - 6.0,
        height: bound.height - text_offset - 3.0,
    };
    if too_small(&bound) {
        return;
    }

    let total_size: u64 = tree.children(id).map(|c| tree.get_elem(c).size).sum();
    if total_size == 0 {
        return;
    }
    // areas are exactly proportional to sizes, so anything below this size
    // cannot reach MIN_BOX_SIZE in both dimensions and is merged into one item
    let min_area = (MIN_BOX_SIZE * MIN_BOX_SIZE) as f64;
    let area = (bound.width * bound.height) as f64;
    let min_size = (total_size as f64 * min_area / area) as u64;

    let mut items = vec![];
    let mut rest = 0;
    for child in tree.children(id) {
        let size = tree.get_elem(child).size;
        if size > 0 && size >= min_size {
            items.push(Item {
                node: Some(child),
                size,
            });
        } else {
            rest += size;
        }
    }
    if rest > 0 {
        items.push(Item {
            node: None,
            size: rest,
        });
    }
    // sort once, splitting a sorted slice keeps both halves sorted
    items.sort_unstable_by_key(|item| item.size);
    layout_group(
        tree,
        &items,
        total_size,
        bound,
        dir_level + 1,
        text_offset,
        gui_nodes,
    );
}

fn layout_group(
    tree: &Tree,
    items: &[Item],
    total_size: u64,
    bound: Rectangle,
    dir_level: usize,
    text_offset: f32,
    gui_nodes: &mut Vec<GUINode>,
) {
    if items.is_empty() || total_size == 0 || too_small(&bound) {
        return;
    }
    if let [item] = items {
        if let Some(id) = item.node {
            layout_node(tree, id, bound, dir_level, text_offset, gui_nodes);
        }
        return;
    }

    // split into halves by size, both sides keep at least one item
    let mut size_a = 0;
    let mut split = 0;
    while split < items.len() - 1 && size_a < total_size / 2 {
        size_a += items[split].size;
        split += 1;
    }
    if split == 0 {
        size_a = items[0].size;
        split = 1;
    }
    let (group_a, group_b) = items.split_at(split);
    let ratio = size_a as f32 / total_size as f32;

    // orientation
    let (bound_a, bound_b) = if bound.width > bound.height {
        // horizontal
        let split_width = bound.width * ratio;
        (
            Rectangle {
                width: split_width,
                ..bound
            },
            Rectangle {
                x: bound.x + split_width,
                width: bound.width - split_width,
                ..bound
            },
        )
    } else {
        // vertical
        let split_height = bound.height * ratio;
        (
            Rectangle {
                height: split_height,
                ..bound
            },
            Rectangle {
                y: bound.y + split_height,
                height: bound.height - split_height,
                ..bound
            },
        )
    };

    layout_group(
        tree,
        group_a,
        size_a,
        bound_a,
        dir_level,
        text_offset,
        gui_nodes,
    );
    layout_group(
        tree,
        group_b,
        total_size - size_a,
        bound_b,
        dir_level,
        text_offset,
        gui_nodes,
    );
}
