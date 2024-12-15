use std::{
  collections::{HashMap, HashSet},
  hash::Hash,
  sync::Arc,
};

use tracing::{debug, info};

use crate::error::{DAGError, DAGResult};

pub trait Identifiable {
  fn id(&self) -> String;
  fn name(&self) -> String;
}

/// Represents a Directed Acyclic Graph (DAG) for task dependencies
#[derive(Clone, Debug)]
pub struct DAG<T: Eq + Hash + Identifiable> {
  nodes: HashSet<Arc<T>>,                  // Stores the nodes in the graph
  edges: HashMap<String, HashSet<Arc<T>>>, // Adjacency list for dependencies
}

impl<T: Eq + Hash + Identifiable> Default for DAG<T> {
  fn default() -> Self {
    Self::new()
  }
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
    self.perform_topological_sort(&mut in_degree)
  }

  /// Prints the graph structure for debugging
  pub fn print_graph(&self) {
    info!("DAG Structure:");
    for node in &self.nodes {
      let deps = self.edges.get(&node.id()).map_or_else(
        || "(no dependencies)".to_string(),
        |deps| {
          let mut d = deps.iter().map(|n| n.id()).collect::<Vec<_>>();
          d.sort();
          format!("{:?}", d)
        },
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  use test_log::test;
  use tracing_test::traced_test;

  #[derive(Debug, Eq, PartialEq, Hash, Clone)]
  struct TestNode {
    id: String,
    name: String,
  }

  impl TestNode {
    fn new(id: &str) -> Arc<Self> {
      Arc::new(Self {
        id: id.to_string(),
        name: id.to_string(),
      })
    }
  }

  impl Identifiable for TestNode {
    fn id(&self) -> String {
      self.id.clone()
    }

    fn name(&self) -> String {
      self.name.clone()
    }
  }

  #[test]
  fn test_new_dag() {
    let dag: DAG<TestNode> = DAG::new();
    assert_eq!(dag.node_count(), 0);
    assert!(dag.edges().is_empty());
    assert!(dag.nodes().is_empty());
  }

  #[test]
  fn test_add_node() {
    let mut dag = DAG::new();
    let node = TestNode::new("A");

    dag.add_node(node.clone());

    assert_eq!(dag.node_count(), 1);
    assert!(dag.nodes().contains(&node));
    assert!(dag.edges().contains_key(&node.id()));
    assert!(dag.edges()[&node.id()].is_empty());
  }

  #[test]
  fn test_add_dependency() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());

    assert!(dag.add_dependency(&node_a, &node_b).is_ok());
    assert!(dag.edges()[&node_a.id()].contains(&node_b));
  }

  #[test]
  fn test_add_dependency_error() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());

    // Test adding dependency with non-existent 'to' node
    assert!(matches!(
      dag.add_dependency(&node_a, &node_b),
      Err(DAGError::NodeNotFound(_))
    ));

    // Test adding dependency with non-existent 'from' node
    assert!(matches!(
      dag.add_dependency(&node_c, &node_a),
      Err(DAGError::NodeNotFound(_))
    ));
  }

  #[test]
  fn test_cycle_detection() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());

    // Create a cycle: A -> B -> C -> A
    dag.add_dependency(&node_a, &node_b).unwrap();
    dag.add_dependency(&node_b, &node_c).unwrap();
    dag.add_dependency(&node_c, &node_a).unwrap();

    assert!(dag.has_cycle().unwrap());
  }

  #[test]
  fn test_no_cycle() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());

    // Create a linear dependency chain: A -> B -> C
    dag.add_dependency(&node_a, &node_b).unwrap();
    dag.add_dependency(&node_b, &node_c).unwrap();

    assert!(!dag.has_cycle().unwrap());
  }

  #[test]
  fn test_complex_dependencies() {
    let mut dag = DAG::new();
    let nodes: Vec<Arc<TestNode>> = (0..5).map(|i| TestNode::new(&i.to_string())).collect();

    // Add all nodes
    for node in &nodes {
      dag.add_node(node.clone());
    }

    // Create a diamond pattern with multiple paths
    // 0 -> 1 -> 3 -> 4
    // 0 -> 2 -> 3 -> 4
    dag.add_dependency(&nodes[0], &nodes[1]).unwrap();
    dag.add_dependency(&nodes[0], &nodes[2]).unwrap();
    dag.add_dependency(&nodes[1], &nodes[3]).unwrap();
    dag.add_dependency(&nodes[2], &nodes[3]).unwrap();
    dag.add_dependency(&nodes[3], &nodes[4]).unwrap();

    assert!(!dag.has_cycle().unwrap());
    assert_eq!(dag.node_count(), 5);
  }

  #[test]
  fn test_multiple_roots() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());

    // Two separate dependency chains: A -> B and C
    dag.add_dependency(&node_a, &node_b).unwrap();

    assert!(!dag.has_cycle().unwrap());
    assert_eq!(dag.node_count(), 3);
    assert!(dag.edges()[&node_c.id()].is_empty());
  }

  #[test]
  fn test_duplicate_nodes() {
    let mut dag = DAG::new();
    let node_a1 = TestNode::new("A");
    let node_a2 = TestNode::new("A");

    dag.add_node(node_a1.clone());
    dag.add_node(node_a2.clone());

    // Should only have one node due to equality comparison
    assert_eq!(dag.node_count(), 1);
  }

  #[test]
  fn test_in_degree_calculation() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());

    dag.add_dependency(&node_a, &node_b).unwrap();
    dag.add_dependency(&node_a, &node_c).unwrap();
    dag.add_dependency(&node_b, &node_c).unwrap();

    let in_degrees = dag.calculate_in_degrees();
    assert_eq!(in_degrees[&node_a.id()], 0);
    assert_eq!(in_degrees[&node_b.id()], 1);
    assert_eq!(in_degrees[&node_c.id()], 2);
  }

  #[test]
  fn test_empty_graph_cycle_detection() {
    let dag: DAG<TestNode> = DAG::new();
    assert!(!dag.has_cycle().unwrap());
  }

  #[test]
  fn test_self_cycle() {
    let mut dag = DAG::new();
    let node = TestNode::new("A");

    dag.add_node(node.clone());
    dag.add_dependency(&node, &node).unwrap();

    assert!(dag.has_cycle().unwrap());
  }

  #[traced_test]
  #[test]
  fn test_print_graph() {
    let mut dag = DAG::new();

    // Create test nodes
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");
    let node_d = TestNode::new("D");

    // Add nodes to DAG
    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());
    dag.add_node(node_d.clone());

    // Create dependencies
    // A -> B -> D
    // A -> C -> D
    dag.add_dependency(&node_a, &node_b).unwrap();
    dag.add_dependency(&node_a, &node_c).unwrap();
    dag.add_dependency(&node_b, &node_d).unwrap();
    dag.add_dependency(&node_c, &node_d).unwrap();

    // Print the graph
    dag.print_graph();

    // Verify log output contains expected structure
    assert!(logs_contain("DAG Structure:"));
    assert!(logs_contain("C -> [\"D\"]"));
    assert!(logs_contain("B -> [\"D\"]"));
    assert!(logs_contain("D -> []"));
    assert!(logs_contain("A -> [\"B\", \"C\"]"));
  }

  #[traced_test]
  #[test]
  fn test_print_empty_graph() {
    let dag: DAG<TestNode> = DAG::new();
    dag.print_graph();

    assert!(logs_contain("DAG Structure:"));
  }

  #[traced_test]
  #[test]
  fn test_print_single_node_graph() {
    let mut dag = DAG::new();
    let node = TestNode::new("A");

    dag.add_node(node.clone());
    dag.print_graph();

    assert!(logs_contain("DAG Structure:"));
    assert!(logs_contain("A -> []"));
  }

  #[traced_test]
  #[test]
  fn test_print_graph_with_cycles() {
    let mut dag = DAG::new();
    let node_a = TestNode::new("A");
    let node_b = TestNode::new("B");
    let node_c = TestNode::new("C");

    dag.add_node(node_a.clone());
    dag.add_node(node_b.clone());
    dag.add_node(node_c.clone());

    // Create a cycle: A -> B -> C -> A
    dag.add_dependency(&node_a, &node_b).unwrap();
    dag.add_dependency(&node_b, &node_c).unwrap();
    dag.add_dependency(&node_c, &node_a).unwrap();

    dag.print_graph();

    assert!(logs_contain("DAG Structure:"));
    assert!(logs_contain("A -> [\"B\"]"));
    assert!(logs_contain("B -> [\"C\"]"));
    assert!(logs_contain("C -> [\"A\"]"));
  }

  #[traced_test]
  #[test]
  fn test_print_complex_graph() {
    let mut dag = DAG::new();
    let nodes: Vec<Arc<TestNode>> = (0..5).map(|i| TestNode::new(&format!("Node{}", i))).collect();

    // Add all nodes
    for node in &nodes {
      dag.add_node(node.clone());
    }

    // Create a complex dependency structure
    dag.add_dependency(&nodes[0], &nodes[1]).unwrap();
    dag.add_dependency(&nodes[0], &nodes[2]).unwrap();
    dag.add_dependency(&nodes[1], &nodes[3]).unwrap();
    dag.add_dependency(&nodes[2], &nodes[3]).unwrap();
    dag.add_dependency(&nodes[2], &nodes[4]).unwrap();
    dag.add_dependency(&nodes[3], &nodes[4]).unwrap();

    dag.print_graph();

    assert!(logs_contain("DAG Structure:"));
    assert!(logs_contain("Node0 -> [\"Node1\", \"Node2\"]"));
    assert!(logs_contain("Node1 -> [\"Node3\"]"));
    assert!(logs_contain("Node2 -> [\"Node3\", \"Node4\"]"));
    assert!(logs_contain("Node3 -> [\"Node4\"]"));
    assert!(logs_contain("Node4 -> []"));
  }
}
