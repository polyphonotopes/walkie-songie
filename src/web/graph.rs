//! Polyphonotopes graph visualization integration.
//!
//! This module bridges polyphonotopes-rs with the cobwebs-visualizer custom element,
//! providing interactive scale relationship visualization.
//!
//! Architecture:
//! - Keep full polyphonotopes graph in memory (Rust data structure)
//! - Query for subsets to display (e.g., scales matching active pitches + neighbors)
//! - Feed subset to cobwebs-visualizer for rendering

use cobwebs_visualizer::CobwebsGraph;
use polyphonotopes_rs::graph::{GraphBuilder, PolyphonotopesGraph};
use polyphonotopes_rs::scales;
use polyphonotopes_rs::ScaleBitset;
use std::cell::RefCell;
use std::collections::HashSet;
use wasm_bindgen::prelude::*;

thread_local! {
    /// Cached polyphonotopes graph (base graph with all edges).
    static BASE_GRAPH: RefCell<Option<PolyphonotopesGraph>> = RefCell::new(None);
    /// Current hop level for visualization.
    static CURRENT_HOP: RefCell<u32> = RefCell::new(1);
}

/// Initialize the polyphonotopes graph with Major + Altered scales.
pub fn init_graph() {
    let graph = GraphBuilder::new()
        .with_rotations(&[scales::MAJOR, scales::ALTERED])
        .with_polyphonotopes_edges(6) // Max hops we'll need
        .build();

    BASE_GRAPH.with(|g| {
        *g.borrow_mut() = Some(graph);
    });
}

/// Get the base graph (or initialize if needed).
fn with_graph<F, R>(f: F) -> R
where
    F: FnOnce(&PolyphonotopesGraph) -> R,
{
    BASE_GRAPH.with(|g| {
        let mut guard = g.borrow_mut();
        if guard.is_none() {
            let graph = GraphBuilder::new()
                .with_rotations(&[scales::MAJOR, scales::ALTERED])
                .with_polyphonotopes_edges(6)
                .build();
            *guard = Some(graph);
        }
        f(guard.as_ref().unwrap())
    })
}

/// Find all scales that contain the given set of pitch classes.
/// Returns a list of scale IDs (e.g., "scale_101010110101").
pub fn find_matching_scales(active_pitches: &[u8]) -> Vec<String> {
    if active_pitches.is_empty() {
        return Vec::new();
    }

    // Convert active pitches to a bitset for efficient subset checking
    let active_bitset = ScaleBitset::from_indices(active_pitches);

    with_graph(|graph| {
        graph
            .nodes()
            .filter(|node| {
                // Check if all active pitches are contained in this scale
                active_bitset.is_subset_of(&node.bitset)
            })
            .map(|node| node.id.clone())
            .collect()
    })
}

/// Load a subset of the graph into the cobwebs-visualizer element.
/// If `center_node_ids` is provided, only show those nodes + their neighbors.
/// Otherwise, shows all nodes.
#[wasm_bindgen]
pub fn load_polyphonotopes_graph(hop_level: u32) -> Result<(), JsValue> {
    // Load with no filtering (show all)
    load_graph_subset(hop_level, None)
}

/// Load a subset of the graph centered on specific nodes.
/// Shows the given node IDs plus their direct neighbors.
pub fn load_graph_subset(hop_level: u32, center_node_ids: Option<&[String]>) -> Result<(), JsValue> {
    CURRENT_HOP.with(|h| *h.borrow_mut() = hop_level);

    // Create CobwebsGraph instance using Rust API directly
    let cobwebs_graph = CobwebsGraph::new()?;

    with_graph(|base_graph| {
        // Get the hop-derivative graph
        let graph = if hop_level == 1 {
            base_graph.clone()
        } else {
            base_graph.n_hop_derivative_graph(hop_level)
        };

        // Determine which nodes to include
        let nodes_to_show: HashSet<String> = if let Some(center_ids) = center_node_ids {
            // Start with center nodes
            let mut subset: HashSet<String> = center_ids.iter().cloned().collect();

            // Add direct neighbors of center nodes
            for center_id in center_ids {
                for (from, to, _) in graph.all_edges() {
                    if from == *center_id {
                        subset.insert(to);
                    } else if to == *center_id {
                        subset.insert(from);
                    }
                }
            }
            subset
        } else {
            // Show all nodes
            graph.nodes().map(|n| n.id.clone()).collect()
        };

        // Add nodes with random initial positions (spread in a circle)
        let node_count = nodes_to_show.len();
        let edge_count = graph.all_edges().len();
        web_sys::console::log_1(&format!(
            "Loading graph: {} nodes, {} edges (hop_level={})",
            node_count, edge_count, hop_level
        ).into());

        for (i, node) in graph.nodes().enumerate() {
            if !nodes_to_show.contains(&node.id) {
                continue;
            }

            let group = if node.name.contains("Major") {
                "major"
            } else {
                "altered"
            };

            // Spread nodes in a circle with some randomness
            let angle = (i as f64 / node_count as f64) * std::f64::consts::TAU;
            let radius = 5.0 + (js_sys::Math::random() - 0.5) * 2.0;
            let x = angle.cos() * radius;
            let y = angle.sin() * radius;
            let z = (js_sys::Math::random() - 0.5) * 3.0;

            cobwebs_graph.add_node(&node.id, &node.name, group, x, y, z)?;
        }

        // Add edges (only between nodes we're showing)
        for (from, to, _distance) in graph.all_edges() {
            if !nodes_to_show.contains(&from) || !nodes_to_show.contains(&to) {
                continue;
            }

            let from_group = graph
                .get_node(&from)
                .map(|n| {
                    if n.name.contains("Major") {
                        "major"
                    } else {
                        "altered"
                    }
                })
                .unwrap_or("misc");

            cobwebs_graph.add_edge(&from, &to, 1.0, from_group)?;
        }

        Ok::<(), JsValue>(())
    })?;

    // Attach the graph to the element
    cobwebs_graph.attach()?;

    Ok(())
}

/// Update the active (highlighted) scales based on current pitch classes.
#[wasm_bindgen]
pub fn update_active_scales(pitch_classes: Vec<u8>) -> Result<(), JsValue> {
    let matching_ids = find_matching_scales(&pitch_classes);

    // Call the element's internal set_active which updates DOM directly
    cobwebs_visualizer::graph_set_active(matching_ids, "active")
}

/// Load only scales that match the given pitch classes (+ their neighbors).
/// This shows a focused subset of the graph.
#[wasm_bindgen]
pub fn load_matching_scales(pitch_classes: Vec<u8>, hop_level: u32) -> Result<(), JsValue> {
    let matching_ids = find_matching_scales(&pitch_classes);

    if matching_ids.is_empty() {
        // If nothing matches, show all (or could show nothing)
        return load_graph_subset(hop_level, None);
    }

    load_graph_subset(hop_level, Some(&matching_ids))
}

/// Get the current hop level.
pub fn current_hop_level() -> u32 {
    CURRENT_HOP.with(|h| *h.borrow())
}

/// Set the hop level and reload the graph.
#[wasm_bindgen]
pub fn set_hop_level(hop: u32) -> Result<(), JsValue> {
    // Clear existing graph using element's internal clear
    let _ = cobwebs_visualizer::element::graph_clear();

    load_polyphonotopes_graph(hop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_matching_scales() {
        init_graph();

        // C major scale contains C, E, G (0, 4, 7)
        let matches = find_matching_scales(&[0, 4, 7]);
        assert!(!matches.is_empty());
        // C Major should match
        assert!(matches.iter().any(|id| id == "scale_101010110101"));
    }
}
