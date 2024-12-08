use std::{
  collections::{HashMap, HashSet},
  hash::Hash,
  sync::Arc,
};

use tracing::{debug, info};

use crate::error::{DAGError, DAGResult};

pub trait Identifiable {
  fn id(&self) -> String;
}

/// Represents a Directed Acyclic Graph (DAG) for task dependencies
#[derive(Clone, Debug)]
pub struct DAG<T: Eq + Hash + Identifiable> {
  nodes: HashSet<Arc<T>>,                  // Stores the nodes in the graph
  edges: HashMap<String, HashSet<Arc<T>>>, // Adjacency list for dependencies
}

impl<T: Eq + Hash + Identifiable> DAG<T> {
  /// Creates a new empty DAG
  pub fn new() -> Self {
    DAG {
      nodes: HashSet::new(),
      edges: HashMap::new(),
    }
  }

  /// Adds a new node to the graph
  pub fn add_node(&mut self, node: Arc<T>) {
    debug!("Adding node: {}", node.id());
    self.nodes.insert(node.clone());
    self.edges.entry(node.id().clone()).or_default();
  }

  /// Adds a dependency between two nodes
  pub fn add_dependency(&mut self, from: &Arc<T>, to: &Arc<T>) -> DAGResult<()> {
    if !self.nodes.contains(from) {
      return Err(DAGError::NodeNotFound(from.id().clone()));
    }
    if !self.nodes.contains(to) {
      return Err(DAGError::NodeNotFound(to.id().clone()));
    }

    debug!("Adding dependency: {} -> {}", from.id(), to.id());
    self.edges.entry(from.id().clone()).or_default().insert(to.clone());

    Ok(())
  }

  pub fn node_count(&self) -> usize {
    self.nodes.len()
  }

  pub fn edges(&self) -> &HashMap<String, HashSet<Arc<T>>> {
    &self.edges
  }

  pub fn nodes(&self) -> &HashSet<Arc<T>> {
    &self.nodes
  }

  /// Detects cycles in the graph
  pub fn has_cycle(&self) -> DAGResult<bool> {
    let mut in_degree = self.calculate_in_degrees();
    Ok(self.perform_topological_sort(&mut in_degree)?)
  }

  /// Prints the graph structure for debugging
  pub fn print_graph(&self) {
    info!("DAG Structure:");
    for node in &self.nodes {
      let deps = self.edges.get(&node.id()).map_or_else(
        || "(no dependencies)".to_string(),
        |deps| format!("{:?}", deps.iter().map(|n| n.id()).collect::<Vec<_>>()),
      );
      info!("{} -> {}", node.id(), deps);
    }
  }

  fn perform_topological_sort(&self, in_degree: &mut HashMap<String, usize>) -> DAGResult<bool> {
    let mut queue: Vec<_> = self
      .nodes
      .iter()
      .filter(|n| in_degree[&n.id()] == 0)
      .map(|n| n.id().clone())
      .collect();

    let mut visited = 0;

    while let Some(node) = queue.pop() {
      visited += 1;

      if let Some(deps) = self.edges.get(&node) {
        for dep in deps {
          let count = in_degree
            .get_mut(&dep.id())
            .ok_or_else(|| DAGError::NodeNotFound(dep.id()))?;
          *count -= 1;
          if *count == 0 {
            queue.push(dep.id().clone());
          }
        }
      }
    }

    Ok(visited != self.nodes.len())
  }

  fn calculate_in_degrees(&self) -> HashMap<String, usize> {
    let mut in_degree: HashMap<String, usize> = self.nodes.iter().map(|n| (n.id().clone(), 0)).collect();

    for edges in self.edges.values() {
      for node in edges {
        *in_degree.get_mut(&node.id()).unwrap() += 1;
      }
    }
    in_degree
  }
}
