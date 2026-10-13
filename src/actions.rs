use crate::{scan::Scan, types::NodeID};
use std::sync::{Arc, atomic::Ordering};

pub async fn show_node(scan: Arc<Scan>, node_id: NodeID) {
    let Ok(path) = scan.tree_mutex.lock().map(|tree| tree.path_of(node_id)) else {
        return;
    };
    showfile::show_path_in_file_manager(&path);
}

pub async fn trash_node(scan: Arc<Scan>, node_id: NodeID) {
    let Ok(path) = scan.tree_mutex.lock().map(|tree| tree.path_of(node_id)) else {
        return;
    };
    // trashing a big folder takes a while, keep the tree unlocked meanwhile
    match trash::delete(&path) {
        Ok(_) => {
            if let Ok(mut tree) = scan.tree_mutex.lock() {
                tree.invalidate_elem(node_id);
            }
            scan.update_signal.store(true, Ordering::SeqCst);
        }
        Err(e) => {
            dbg!(e);
        }
    }
}
