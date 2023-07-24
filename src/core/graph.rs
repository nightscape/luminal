#![allow(clippy::needless_range_loop)]

use crate::{
    graph_tensor::GraphTensor,
    op::{self, Operator},
    optimizer::GraphOptimizer,
    shape::*,
    tensor::Tensor,
};
use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use petgraph::{graph::NodeIndex, stable_graph::StableGraph, visit::EdgeRef, Direction};

#[derive(Debug, Default)]
pub struct Graph {
    pub(crate) tensors: HashMap<NodeIndex, Tensor>,
    pub(crate) id_remap: HashMap<NodeIndex, NodeIndex>,
    #[allow(clippy::type_complexity)]
    pub(crate) graph: StableGraph<(Box<dyn Operator>, Vec<Vec<usize>>), u8>,
    pub(crate) no_delete: HashSet<NodeIndex>,
}

impl Graph {
    pub fn new() -> Graph {
        Graph::default()
    }

    pub(crate) fn add_op<O: Operator + 'static>(&mut self, op: O) -> NewOp {
        self.graph.free_node = NodeIndex::end(); // Prevent reuse of deleted indexes (screws up remapping)
        NewOp {
            new_op_id: self.graph.add_node((Box::new(op), vec![])),
            graph_ref: self,
            num_srcs: 0,
        }
    }

    pub fn get_tensor(&mut self, mut id: NodeIndex) -> Option<Tensor> {
        // Walk through remaps
        while let Some(new_id) = self.id_remap.get(&id) {
            id = *new_id;
        }

        self.tensors.remove(&id)
    }

    pub fn new_tensor<S: ConstShape>(&mut self) -> GraphTensor<S> {
        self.graph.free_node = NodeIndex::end(); // Prevent reuse of deleted indexes (screws up remapping)
        let tensor = GraphTensor {
            id: self
                .graph
                .add_node((Box::new(op::Input), vec![S::realized_shape()])),
            graph_ref: self,
            _phantom: Default::default(),
        };
        self.no_delete.insert(tensor.id); // This gets set because we want to keep inputs around to run the graph multiple times
        tensor
    }

    pub fn set_tensor<S: ConstShape>(&mut self, graph_tensor: GraphTensor<S>, data: Vec<f32>) {
        self.tensors.insert(
            graph_tensor.id,
            Tensor {
                data: Box::new(data),
                shape: ShapeTracker::new(S::realized_shape()),
            },
        );
    }

    /// Run the full suite of optimizations
    pub fn optimize<O: GraphOptimizer>(&mut self, optimizer: O) {
        optimizer.optimize(self);
    }

    /// Execute the graph.
    pub fn execute(&mut self) {
        let mut dependencies: HashMap<NodeIndex, usize> = self
            .graph
            .node_indices()
            .map(|n| (n, self.graph.edges_directed(n, Direction::Outgoing).count()))
            .collect();
        for node in petgraph::algo::toposort(&self.graph, None).unwrap() {
            if self.tensors.contains_key(&node) {
                continue;
            }
            let srcs = self
                .graph
                .edges_directed(node, Direction::Incoming)
                .sorted_by_key(|e| e.weight())
                .map(|i| self.tensors.get(&i.source()).unwrap())
                .collect_vec();
            // All sources are ready, execute
            let f = self.graph.node_weight(node).unwrap().0.process(srcs);
            self.tensors.insert(node, f);

            // Check if we can delete the source tensors now
            for source in self
                .graph
                .edges_directed(node, Direction::Incoming)
                .map(|e| e.source())
                .filter(|n| !self.no_delete.contains(n))
                .collect_vec()
            {
                let deps = dependencies.get_mut(&source).unwrap();
                *deps -= 1;
                if *deps == 0 {
                    // No more dependencies for this node, let's remove it's data
                    self.tensors.remove(&source);
                }
            }
        }
    }

    /// Convert to debug-viewable graph
    pub fn debug_graph(
        &self,
        show_shapes: bool,
    ) -> petgraph::stable_graph::StableGraph<String, u8, petgraph::Directed, u32> {
        let mut new_graph = petgraph::stable_graph::StableGraph::default();
        let mut id_map = HashMap::new();
        for (id, node) in self.graph.node_indices().zip(self.graph.node_weights()) {
            id_map.insert(
                id,
                new_graph.add_node(if show_shapes {
                    format!("{node:?}")
                } else {
                    format!("{:?}", node.0)
                }),
            );
        }

        for node in self.graph.node_indices() {
            for edge in self
                .graph
                .edges_directed(node, petgraph::Direction::Outgoing)
            {
                new_graph.add_edge(
                    id_map[&edge.source()],
                    id_map[&edge.target()],
                    *edge.weight(),
                );
            }
        }

        new_graph
    }

    pub fn display_graph(&self) {
        display_graph(&self.debug_graph(false));
    }

    /// Transfer all external references from one node to another (this may happen because one node is about to be removed / merged into another)
    pub fn move_references(
        id_remap: &mut HashMap<NodeIndex, NodeIndex>,
        no_delete: &mut HashSet<NodeIndex<u32>>,
        src: NodeIndex,
        trg: NodeIndex,
    ) {
        // Create remap
        id_remap.insert(src, trg);
        // Transfer no_delete
        if no_delete.remove(&src) {
            no_delete.insert(trg);
        }
    }

    /// Get the sources of a node given it's id
    #[allow(clippy::type_complexity)]
    pub fn get_sources(
        &self,
        node_id: NodeIndex,
    ) -> Vec<(NodeIndex, &(Box<dyn Operator>, Vec<Vec<usize>>))> {
        self.graph
            .edges_directed(node_id, Direction::Incoming)
            .map(|e| e.source())
            .map(|n| (n, self.graph.node_weight(n).unwrap()))
            .collect()
    }

    /// Get the dests of a node given it's id
    #[allow(clippy::type_complexity)]
    pub fn get_dests(
        &self,
        node_id: NodeIndex,
    ) -> Vec<(NodeIndex, &(Box<dyn Operator>, Vec<Vec<usize>>))> {
        self.graph
            .edges_directed(node_id, Direction::Outgoing)
            .map(|e| e.target())
            .map(|n| (n, self.graph.node_weight(n).unwrap()))
            .collect()
    }
}

/// View a debug graph in the browser
pub fn display_graph(
    graph: &petgraph::stable_graph::StableGraph<String, u8, petgraph::Directed, u32>,
) {
    let url = format!(
        "https://dreampuf.github.io/GraphvizOnline/#{}",
        urlencoding::encode(
            &petgraph::dot::Dot::with_config(&graph, &[petgraph::dot::Config::EdgeNoLabel,])
                .to_string()
        )
    );
    if let Err(e) = webbrowser::open(&url) {
        panic!("Error displaying graph: {:?}", e);
    }
}

pub trait JoinGraph {
    fn join(
        self,
        rhs: &petgraph::stable_graph::StableGraph<String, u8, petgraph::Directed, u32>,
    ) -> Self;
}

impl JoinGraph for petgraph::stable_graph::StableGraph<String, u8, petgraph::Directed, u32> {
    /// Join two debug graphs together
    fn join(
        mut self,
        rhs: &petgraph::stable_graph::StableGraph<String, u8, petgraph::Directed, u32>,
    ) -> Self {
        let mut id_map = HashMap::new(); // We track the node id remapping here so they don't overlap
        for (index, node) in rhs.node_indices().zip(rhs.node_weights()) {
            id_map.insert(index, self.add_node(node.clone()));
        }

        for node in rhs.node_indices() {
            for edge in rhs.edges_directed(node, petgraph::Direction::Outgoing) {
                self.add_edge(
                    id_map[&edge.source()],
                    id_map[&edge.target()],
                    *edge.weight(),
                );
            }
        }

        self
    }
}

pub struct NewOp<'a> {
    new_op_id: NodeIndex,
    graph_ref: &'a mut Graph,
    num_srcs: u8,
}

impl<'a> NewOp<'a> {
    pub fn finish(self) -> NodeIndex {
        self.new_op_id
    }

    pub fn input(mut self, id: NodeIndex, shape: Vec<usize>) -> Self {
        self.graph_ref
            .graph
            .add_edge(id, self.new_op_id, self.num_srcs);
        self.graph_ref
            .graph
            .node_weight_mut(self.new_op_id)
            .unwrap()
            .1
            .push(shape);
        self.num_srcs += 1;
        self
    }
}