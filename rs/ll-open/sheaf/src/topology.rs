//! Shared restriction graph topology.
//!
//! Both [`CellComplex`](crate::complex::CellComplex) and
//! [`SheafCache`](crate::cache::SheafCache) operate on a graph of regions
//! connected by restriction edges. This module provides the shared topology
//! that both can consume, avoiding duplicate graph construction.

use std::collections::{HashMap, HashSet};

/// A region (node) in the restriction graph.
pub type RegionId = u32;

/// An edge in the restriction graph connecting two regions.
#[derive(Debug, Clone)]
pub struct Edge {
    pub source: RegionId,
    pub target: RegionId,
    /// Domain-specific label (e.g. "contains", "dependency", "shared_token").
    pub label: Option<String>,
}

/// The restriction graph: regions connected by edges.
///
/// This is the shared topology that both the algebraic complex and the
/// operational cache build on. Consumers add their own data (stalks,
/// restriction maps, cache entries) keyed by the same RegionIds.
#[derive(Debug, Clone, Default)]
pub struct RestrictionGraph {
    regions: HashSet<RegionId>,
    edges: Vec<Edge>,
    /// region → [neighbor regions]
    adjacency: HashMap<RegionId, Vec<RegionId>>,
}

impl RestrictionGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a region to the graph.
    pub fn add_region(&mut self, id: RegionId) {
        self.regions.insert(id);
    }

    /// Add an undirected edge between two regions.
    pub fn add_edge(&mut self, source: RegionId, target: RegionId, label: Option<String>) {
        self.regions.insert(source);
        self.regions.insert(target);
        self.edges.push(Edge {
            source,
            target,
            label,
        });
        self.adjacency.entry(source).or_default().push(target);
        self.adjacency.entry(target).or_default().push(source);
    }

    /// Get all neighbors of a region.
    pub fn neighbors(&self, id: RegionId) -> &[RegionId] {
        self.adjacency.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get all region IDs.
    pub fn regions(&self) -> &HashSet<RegionId> {
        &self.regions
    }

    /// Get all edges.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Number of regions.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// BFS walk from `start` up to `max_depth` hops. Returns visited regions.
    pub fn bfs(&self, start: RegionId, max_depth: u32) -> Vec<RegionId> {
        let mut visited = HashSet::new();
        let mut frontier = vec![(start, 0u32)];
        let mut result = Vec::new();

        while let Some((region, depth)) = frontier.pop() {
            if !visited.insert(region) {
                continue;
            }
            result.push(region);
            if depth < max_depth {
                for &neighbor in self.neighbors(region) {
                    if !visited.contains(&neighbor) {
                        frontier.push((neighbor, depth + 1));
                    }
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph() {
        let g = RestrictionGraph::new();
        assert_eq!(g.region_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn add_edge_creates_regions() {
        let mut g = RestrictionGraph::new();
        g.add_edge(0, 1, None);
        assert_eq!(g.region_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.neighbors(0), &[1]);
        assert_eq!(g.neighbors(1), &[0]);
    }

    #[test]
    fn bfs_linear_chain() {
        let mut g = RestrictionGraph::new();
        g.add_edge(0, 1, None);
        g.add_edge(1, 2, None);
        g.add_edge(2, 3, None);

        // depth 1 from 0: reaches 0, 1
        let reached = g.bfs(0, 1);
        assert!(reached.contains(&0));
        assert!(reached.contains(&1));
        assert!(!reached.contains(&2));

        // depth 3 from 0: reaches all
        let reached = g.bfs(0, 3);
        assert_eq!(reached.len(), 4);
    }

    #[test]
    fn bfs_with_cycle() {
        let mut g = RestrictionGraph::new();
        g.add_edge(0, 1, None);
        g.add_edge(1, 2, None);
        g.add_edge(2, 0, None);

        let reached = g.bfs(0, 10);
        assert_eq!(reached.len(), 3); // doesn't loop
    }
}
