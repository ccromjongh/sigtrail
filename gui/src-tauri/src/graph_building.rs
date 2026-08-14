use std::{collections::{HashMap, HashSet}, fs::{read_to_string, File}, io::BufReader, path, sync::{Arc, RwLock}, time::SystemTime};
use std::path::{Path, PathBuf};
use sigtrail_rs::{conversion::{dpdg_make_exportable, pdg_convert_to_source}, graphbuilder::{GraphBuilder, GraphProcessingType}, pdg_spec::{ExportablePDG, ExportablePDGNode, PDGSpec}, sim_data_injection::TywavesInterface};
use serde::Deserialize;
use tauri::State;
use anyhow::{anyhow, Result};
use sigtrail_rs::graphbuilder::LanguageMode;
use log::info;
use crate::{app_state::{AppState, GraphNodeHierarchy, HierarchicalGraph, ViewableGraph}, errors::map_err_to_string_async};
use crate::app_state::PDGConfig;

/// Tauri command that builds a Dynamic Program Dependence Graph (DPDG) from PDG and VCD files.
///
/// This function orchestrates the entire DPDG building process:
/// 1. Loads and deserializes the PDG from file
/// 2. Builds the DPDG using VCD simulation data
/// 3. Converts to source representation (if configured)
/// 4. Injects simulation data from VCD
/// 5. Builds node hierarchy for grouping (if enabled)
/// 6. Creates lookup tables for efficient graph traversal
/// 7. Loads source files for display
///
/// # Arguments
/// * `state` - Shared application state containing configuration and graph data
///
/// # Returns
/// * `Ok(())` if graph building succeeds
/// * `Err(String)` with error message if any step fails
#[tauri::command]
pub async fn make_dpdg(state: State<'_, RwLock<AppState>>) -> Result<(), String> {
    map_err_to_string_async(async {
        let mut enable_grouping = false;
        {
            // Extract PDG configuration from state without holding lock during graph building
            let pdg_config = {
                let state_guard = state.read().map_err(|_| anyhow::anyhow!("RwLock poisoned"))?;
                state_guard.pdg_config.clone()
            };

            let Some(pdg_config) = pdg_config else {
                anyhow::bail!("Tried building PDG before config was known.");
            };

            enable_grouping = pdg_config.group_nodes;

            let start_time = SystemTime::now();
            let mut now = SystemTime::now();

            // Load and deserialize PDG from JSON file
            let reader = BufReader::new(File::open(&pdg_config.pdg_path)?);
            let mut deser = serde_json::Deserializer::from_reader(reader);
            deser.disable_recursion_limit();
            let pdg_raw = PDGSpec::deserialize(&mut deser)
                .map_err(|e| anyhow::anyhow!("Failed to parse PDG JSON file: {}", e))?;
            info!("Processing PDG with {} nodes and {} edges", pdg_raw.vertices.len(), pdg_raw.edges.len());
            let sliced = pdg_raw;

            info!("PDG read: {}", (now.elapsed().unwrap().as_nanos() as f64) / 1e6);
            now = SystemTime::now();

            info!("Read PDG from file");

            // Build the Dynamic PDG by analyzing simulation trace data
            let mut builder = GraphBuilder::new(&pdg_config.vcd_path, pdg_config.extra_scopes.clone(), sliced, pdg_config.language_mode.clone())?;
            let processing_type = if pdg_config.data_only { GraphProcessingType::DataOnly } else { GraphProcessingType::Normal };
            let dpdg = builder.process(&pdg_config.criterion, pdg_config.max_timesteps.map(|t| t as i64), processing_type)?;

            info!("DPDG build: {}", (now.elapsed().unwrap().as_nanos() as f64) / 1e6);
            now = SystemTime::now();
            info!("DPDG build complete");

            let dpdg = dpdg_make_exportable(dpdg);

            info!("Exportable: {}", (now.elapsed().unwrap().as_nanos() as f64) / 1e6);
            now = SystemTime::now();
            info!("Made DPDG exportable");

            // Convert from FIRRTL to Chisel source language representation unless FIR or other mode is used
            let mut converted_pdg = if let LanguageMode::Chisel = pdg_config.language_mode {
                pdg_convert_to_source(dpdg, false, true)
            } else {
                dpdg
            };

            info!("Conversion: {}", (now.elapsed().unwrap().as_nanos() as f64) / 1e6);
            now = SystemTime::now();
            info!("Converted to source representation");

            info!("DPDG has {} nodes and {} edges", converted_pdg.vertices.len(), converted_pdg.edges.len());

            // Inject simulation values into graph nodes using Tywaves
            let tywaves = TywavesInterface::new(&pdg_config.hgldd_path, pdg_config.extra_scopes.clone(), &pdg_config.top_module)?;
            let vcd_path: &PathBuf = if let LanguageMode::Chisel = pdg_config.language_mode {
                &Path::new(&tywaves.vcd_rewrite(&pdg_config.vcd_path)?).to_path_buf()
            } else {
                &pdg_config.vcd_path
            };
            info!("VCD rewrite done");
            tywaves.inject_sim_data(&mut converted_pdg, &vcd_path, &pdg_config.extra_scopes, pdg_config.language_mode)?;

            info!("Tywaves: {}", (now.elapsed().unwrap().as_nanos() as f64) / 1e6);

            // Adjust timestamps (convert from 0-indexed to 1-indexed)
            for v in &mut converted_pdg.vertices {
                v.timestamp += 1;
            }

            info!("Total: {}", (start_time.elapsed().unwrap().as_nanos() as f64) / 1e6);

            info!("Data injection done");

            // Build hierarchical grouping structure if enabled
            let (node_hierarchy, node_hierarchy_lookup) = if pdg_config.group_nodes {
                let (x, y) = build_node_hierarchy(&converted_pdg);
                (Some(x), Some(y))
            } else { (None, None) };

            // Build lookup tables for efficient graph queries

            // Map timestamp -> node indices at that timestamp
            let mut time_to_nodes = HashMap::new();
            for (idx, v) in converted_pdg.vertices.iter().enumerate() {
                time_to_nodes.entry(v.timestamp).and_modify(|nodes: &mut Vec<usize>| nodes.push(idx)).or_insert(vec![idx]);
            }

            // Map node index -> outgoing edge indices (dependencies)
            let mut dep_to_edges = HashMap::new();
            for (idx, e) in converted_pdg.edges.iter().enumerate() {
                dep_to_edges.entry(e.from).and_modify(|edges: &mut Vec<usize>| edges.push(idx)).or_insert(vec![idx]);
            }

            // Map node index -> incoming edge indices (provenance)
            let mut prov_to_edges = HashMap::new();
            for (idx, e) in converted_pdg.edges.iter().enumerate() {
                prov_to_edges.entry(e.to).and_modify(|edges: &mut Vec<usize>| edges.push(idx)).or_insert(vec![idx]);
            }

            let n_timestamps = converted_pdg.vertices.iter().fold(0, |acc, x| acc.max(x.timestamp)) as u64;

            // Load source files referenced by graph nodes for display in UI
            // All file contents are loaded in memory
            let mut source_paths = HashSet::new();
            for v in &converted_pdg.vertices {
                source_paths.insert(v.file.clone());
            }

            let mut source_files = HashMap::new();
            for p in source_paths {
                // Workaround: PDG export may be missing leading '/' for home directory paths
                // TODO: This is Unix-specific and won't work on Windows
                let read_path = if p.starts_with("home") {
                    &("/".to_string() + &p)
                } else {
                    &p
                };

                if let Ok(contents) = read_to_string(&read_path) {
                    source_files.insert(p, contents.lines().map(String::from).collect());
                }
            }

            let viewable_graph = ViewableGraph {
                dpdg: converted_pdg.clone(),
                shown_ids: (0..converted_pdg.vertices.len()).collect(),
                time_to_nodes,
                dep_to_edges,
                prov_to_edges,
                n_timestamps,
                source_files,
                should_group_nodes: pdg_config.group_nodes,
                node_hierarchy,
                node_hierarchy_lookup,
                current_hier_dpdg: None,
            };

            let mut state_guard = state.write().map_err(|_| anyhow::anyhow!("RwLock poisoned"))?;
            state_guard.graph = Some(viewable_graph);
        }

        // Build initial hierarchical view if grouping is enabled
        if enable_grouping {
            rebuild_hier_graph(&state)?;
        }
        Ok(())
    }).await
}

/// Rebuilds the hierarchical DPDG view based on which hierarchical groups are expanded/collapsed.
///
/// This function traverses all edges in the original DPDG and for each edge:
/// 1. Determines the highest collapsed hierarchical group for source and destination nodes
/// 2. Redirects edges to group nodes when their children are collapsed
/// 3. Deduplicates edges that now connect the same groups
/// 4. Rebuilds lookup tables for the new graph structure
///
/// # Arguments
/// * `state` - Shared application state containing the graph and hierarchy data
///
/// # Returns
/// * `Ok(())` if rebuild succeeds
/// * `Err` if state is uninitialized or lock is poisoned
pub fn rebuild_hier_graph(state: &State<'_, RwLock<AppState>>) -> Result<()> {
    let mut state_guard = state.write().map_err(|_| anyhow!("RwLock poisoned"))?;
    let Some(vgraph) = &mut state_guard.graph else {
        anyhow::bail!("Uninitialized graph!");
    };

    let Some(node_hier_lookup) = &vgraph.node_hierarchy_lookup else {
        anyhow::bail!("Uninitialized reverse hierarchy lookup!");
    };

    let pdg = &vgraph.dpdg;

    let mut node_to_index = HashMap::new();
    let mut new_nodes = vec![];
    let mut new_edges = HashSet::new();
    let mut original_ids = vec![];
    let mut group_ids = HashMap::new();

    // Process each edge, redirecting to hierarchical groups as needed
    for edge in &pdg.edges {
        // Determine the effective source node (either original or collapsed group)
        let from_hier = &node_hier_lookup[&(edge.from as usize)];
        let mut from_is_group = true;
        let from_pdg_node = get_highest_hier_node(&from_hier).unwrap_or_else(|| {
            from_is_group = false;
            vgraph.dpdg.vertices[edge.from as usize].clone()
        });

        // Determine the effective destination node (either original or collapsed group)
        let to_hier = &node_hier_lookup[&(edge.to as usize)];
        let mut to_is_group = true;
        let to_pdg_node = get_highest_hier_node(&to_hier).unwrap_or_else(|| {
            to_is_group = false;
            vgraph.dpdg.vertices[edge.to as usize].clone()
        });

        // Get or create index for source node in new graph
        let new_from_index = *node_to_index.entry(from_pdg_node.clone()).or_insert_with(|| {
            new_nodes.push(from_pdg_node);
            if from_is_group {
                group_ids.insert(new_nodes.len() - 1, from_hier.clone());
                original_ids.push(from_hier.read().unwrap().group_id);
            } else {
                original_ids.push(edge.from as usize);
            }
            new_nodes.len() - 1
        });

        // Get or create index for destination node in new graph
        let new_to_index = *node_to_index.entry(to_pdg_node.clone()).or_insert_with(|| {
            new_nodes.push(to_pdg_node);
            if to_is_group {
                group_ids.insert(new_nodes.len() - 1, to_hier.clone());
                original_ids.push(to_hier.read().unwrap().group_id);
            } else {
                original_ids.push(edge.to as usize);
            }
            new_nodes.len() - 1
        });

        // Skip self-loops (edges within collapsed groups)
        if new_from_index == new_to_index {
            continue;
        }

        // Create redirected edge (HashSet will deduplicate)
        let mut new_edge = edge.clone();
        new_edge.from = new_from_index as u32;
        new_edge.to = new_to_index as u32;

        new_edges.insert(new_edge);
    }

    // Rebuild lookup tables for the new hierarchical graph

    let mut time_to_nodes = HashMap::new();
    for (idx, v) in new_nodes.iter().enumerate() {
        time_to_nodes.entry(v.timestamp).and_modify(|nodes: &mut Vec<usize>| nodes.push(idx)).or_insert(vec![idx]);
    }

    let mut dep_to_edges = HashMap::new();
    for (idx, e) in new_edges.iter().enumerate() {
        dep_to_edges.entry(e.from).and_modify(|edges: &mut Vec<usize>| edges.push(idx)).or_insert(vec![idx]);
    }

    let mut prov_to_edges = HashMap::new();
    for (idx, e) in new_edges.iter().enumerate() {
        prov_to_edges.entry(e.to).and_modify(|edges: &mut Vec<usize>| edges.push(idx)).or_insert(vec![idx]);
    }

    vgraph.current_hier_dpdg = Some(HierarchicalGraph {
        dpdg: ExportablePDG { vertices: new_nodes, edges: new_edges.into_iter().collect::<Vec<_>>() },
        group_ids,
        original_ids,
        time_to_nodes,
        dep_to_edges,
        prov_to_edges,
    });

    Ok(())
}

/// Traverses the hierarchy upwards to find the highest collapsed (non-expanded) ancestor node.
///
/// # Arguments
/// * `hierarchy` - Starting hierarchy node to traverse from
///
/// # Returns
/// * `Some(ExportablePDGNode)` - The PDG node of the highest collapsed ancestor
/// * `None` - If all ancestors are expanded (node should be shown directly)
fn get_highest_hier_node(hierarchy: &Arc<RwLock<GraphNodeHierarchy>>) -> Option<ExportablePDGNode> {
    let mut parent = hierarchy.clone();
    let mut highest_level = None;

    // Traverse upwards through parent hierarchy
    loop {
        let new_parent = {
            let guard = parent.read().unwrap();

            // If this level is collapsed, it's a candidate for highest collapsed ancestor
            if !guard.expanded {
                highest_level = Some(guard.pdg_node.clone());
            }

            // Try to move to parent level
            if let Some(p) = &guard.parent {
                if let Some(p) = p.upgrade() {
                    p.clone()
                } else {
                    break; // Weak reference invalid, reached root
                }
            } else {
                break; // No parent, reached root
            }
        };
        parent = new_parent;
    }

    highest_level
}

/// Creates a synthetic PDG node representing a hierarchical group/module.
///
/// # Arguments
/// * `name` - Display name for the hierarchical node
/// * `timestamp` - Timestamp to associate with this node
/// * `module_path` - Path in the module hierarchy
///
/// # Returns
/// * `ExportablePDGNode` with minimal fields populated for display
fn create_hier_pdg_node(name: String, timestamp: i64, module_path: Vec<String>) -> ExportablePDGNode {
    ExportablePDGNode {
        file: "".into(),
        line: 0,
        char: 0,
        name,
        kind: sigtrail_rs::pdg_spec::PDGSpecNodeKind::Definition,
        clocked: false,
        module_path,
        related_signal: None,
        sim_data: None,
        timestamp,
        is_chisel_assignment: false,
    }
}

/// Builds a hierarchical tree structure from the flat list of DPDG nodes.
///
/// For each timestamp, this creates a tree where:
/// - The root is a "top" module
/// - Intermediate nodes represent module instances in the design hierarchy
/// - Leaf nodes contain references to actual PDG nodes at that hierarchy level
///
/// The function also creates a reverse lookup map from node indices to their hierarchy nodes,
/// which is used during graph traversal to determine which hierarchical group a node belongs to.
///
/// # Arguments
/// * `dpdg` - The DPDG containing nodes to organize hierarchically
///
/// # Returns
/// * Tuple of:
///   - Vector of root hierarchy nodes (one per timestamp)
///   - HashMap mapping node index to its hierarchy node (for reverse lookup)
fn build_node_hierarchy(dpdg: &ExportablePDG) -> (Vec<Arc<RwLock<GraphNodeHierarchy>>>, HashMap<usize, Arc<RwLock<GraphNodeHierarchy>>>) {
    let num_timestamps = dpdg.vertices.iter().map(|v| v.timestamp).max().unwrap();

    let mut groups = vec![];
    let mut reverse_hier_lookup = HashMap::new();

    // Assign unique IDs to hierarchy groups for vis.js rendering
    let global_id_offset = dpdg.vertices.len() * 5;
    let mut group_count = 0;

    // Build hierarchy tree separately for each timestamp
    for timestamp in 0..=num_timestamps {
        // Create root node for this timestamp
        let top = Arc::new(RwLock::new(GraphNodeHierarchy {
            instance_name: "top".into(),
            expanded: true,
            pdg_node: create_hier_pdg_node("module_top".into(), timestamp, vec![]),
            node_indices: vec![],
            children: vec![],
            parent: None,
            group_id: global_id_offset + group_count,
        }));
        group_count += 1;

        let mut unique_paths = HashSet::new();

        let filtered_nodes = dpdg.vertices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.timestamp == timestamp);

        // Extract all unique module paths for this timestamp
        for (_, node) in filtered_nodes.clone() {
            unique_paths.insert(node.module_path.clone());
        }

        // Sort paths by length to ensure parent nodes are created before children
        let mut unique_paths = unique_paths.into_iter().collect::<Vec<_>>();
        unique_paths.sort_by_key(|p| p.len());

        // Build hierarchy tree by processing each unique path
        for path in &unique_paths {
            let mut parent = top.clone();
            let (head, tail) = if path.len() > 1 {
                (&path[0..path.len() - 1], &path[path.len() - 1..])
            } else {
                (&[] as &[String], &path[..])
            };

            if tail.len() == 0 || tail[0] == "" {
                continue;
            }

            // Traverse/create intermediate hierarchy nodes (all path parts except last)
            for path_part in head {
                let new_parent = {
                    let mut parent_lock = parent.write().unwrap();

                    // Check if this hierarchy level already exists
                    if let Some(p) = parent_lock.children.iter().find(|n| n.read().unwrap().instance_name.eq(path_part)) {
                        p.clone()
                    } else {
                        // Create new hierarchy node
                        let mut my_modpath = parent_lock.pdg_node.module_path.clone();
                        my_modpath.push(path_part.clone());

                        parent_lock.children.push(Arc::new(RwLock::new(GraphNodeHierarchy {
                            instance_name: path_part.clone(),
                            expanded: false,
                            pdg_node: create_hier_pdg_node(format!("module_{}", path_part.clone()), timestamp, my_modpath),
                            node_indices: vec![],
                            children: vec![],
                            parent: Some(Arc::downgrade(&parent)),
                            group_id: global_id_offset + group_count,
                        })));
                        group_count += 1;
                        parent_lock.children.last().unwrap().clone()
                    }
                };
                parent = new_parent;
            }

            // Create the leaf hierarchy node (last path component)
            let mut parent_lock = parent.write().unwrap();
            let mut my_modpath = parent_lock.pdg_node.module_path.clone();
            my_modpath.push(tail[0].clone());
            parent_lock.children.push(Arc::new(RwLock::new(GraphNodeHierarchy {
                instance_name: tail[0].clone(),
                expanded: false,
                pdg_node: create_hier_pdg_node(format!("module_{}", tail[0].clone()), timestamp, my_modpath),
                node_indices: vec![],
                children: vec![],
                parent: Some(Arc::downgrade(&parent)),
                group_id: global_id_offset + group_count,
            })));
            group_count += 1;
        }

        // Assign each PDG node to its corresponding hierarchy leaf node
        for (idx, node) in filtered_nodes {
            let mut parent = top.clone();

            // Traverse to the correct leaf node
            for path_part in &node.module_path {
                if path_part == "" {
                    continue;
                }
                let new_parent = {
                    let parent_lock = parent.read().unwrap();
                    parent_lock.children.iter().find(|n| n.read().unwrap().instance_name.eq(path_part)).unwrap().clone()
                };
                parent = new_parent;
            }

            // Add node index to leaf and create reverse lookup
            parent.write().unwrap().node_indices.push(idx);
            reverse_hier_lookup.insert(idx, parent.clone());
        }

        groups.push(top);
    }

    (groups, reverse_hier_lookup)
}