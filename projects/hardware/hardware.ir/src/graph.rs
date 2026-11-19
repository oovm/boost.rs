use std::collections::BTreeSet;

use boost_collection::{GraphRead, OutgoingEdges};
use serde::{Deserialize, Serialize};

use hardware_types::{DepKind, ResourceId, ResourceNode, TaskId, TaskNode};

use crate::error::{Error, Result};

/// Directed dependency from a task and/or resource into a consumer task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepEdge {
    /// Producer task, when the edge is task-ordered.
    pub from_task: Option<TaskId>,
    /// Producer or operand resource, when the edge carries data/memory.
    pub from_resource: Option<ResourceId>,
    /// Consumer task that cannot start until the source is ready.
    pub to_task: TaskId,
    /// Kind of dependency.
    pub kind: DepKind,
}

/// Resource nodes belonging to one IR document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceGraph {
    /// Resource declarations.
    pub nodes: Vec<ResourceNode>,
}

/// Tasks and dependency edges belonging to one IR document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    /// Task declarations.
    pub nodes: Vec<TaskNode>,
    /// Ordering and data/memory edges.
    pub edges: Vec<DepEdge>,
}

/// Combined IR document exchanged between frontends, scheduler, and ucf-backends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    /// Resource subgraph.
    pub resources: ResourceGraph,
    /// Task subgraph.
    pub tasks: TaskGraph,
}

impl Graph {
    /// Validate structural contracts, then ensure the task graph is acyclic.
    ///
    /// Checks:
    /// - resource and task ids are unique
    /// - every edge names an existing `to_task`
    /// - every edge has at least one source (`from_task` and/or `from_resource`)
    /// - named sources exist in this graph
    /// - task-to-task edges form a DAG
    pub fn validate(&self) -> Result<()> {
        self.check_unique_ids()?;
        self.check_edges()?;
        self.tasks.topological_order()?;
        Ok(())
    }

    fn check_unique_ids(&self) -> Result<()> {
        let mut resources = BTreeSet::new();
        for node in &self.resources.nodes {
            if !resources.insert(node.id) {
                return Err(Error::DuplicateResource(node.id.0));
            }
        }

        let mut tasks = BTreeSet::new();
        for node in &self.tasks.nodes {
            if !tasks.insert(node.id) {
                return Err(Error::DuplicateTask(node.id.0));
            }
        }
        Ok(())
    }

    fn check_edges(&self) -> Result<()> {
        let resources: BTreeSet<ResourceId> = self.resources.nodes.iter().map(|n| n.id).collect();
        let tasks: BTreeSet<TaskId> = self.tasks.nodes.iter().map(|n| n.id).collect();

        for edge in &self.tasks.edges {
            if !tasks.contains(&edge.to_task) {
                return Err(Error::UnknownTask(edge.to_task.0));
            }
            if edge.from_task.is_none() && edge.from_resource.is_none() {
                return Err(Error::EmptyEdgeSource(edge.to_task.0));
            }
            if let Some(from) = edge.from_task {
                if !tasks.contains(&from) {
                    return Err(Error::UnknownTask(from.0));
                }
            }
            if let Some(resource) = edge.from_resource {
                if !resources.contains(&resource) {
                    return Err(Error::UnknownResource(resource.0));
                }
            }
        }
        Ok(())
    }
}

impl TaskGraph {
    /// Returns tasks in dependency order. Fails on cycles.
    ///
    /// Only `from_task` edges contribute to ordering. Resource-only edges are
    /// data/memory annotations validated by [`Graph::validate`].
    ///
    /// The order is produced by the shared graph layer: among ready tasks the
    /// smallest [`TaskId`] goes first, preserving the scheduler contract that
    /// this used to implement locally.
    pub fn topological_order(&self) -> Result<Vec<TaskId>> {
        let task_ids: BTreeSet<TaskId> = self.nodes.iter().map(|n| n.id).collect();
        for edge in &self.edges {
            let Some(from) = edge.from_task
            else {
                continue;
            };
            if !task_ids.contains(&from) {
                return Err(Error::UnknownTask(from.0));
            }
            if !task_ids.contains(&edge.to_task) {
                return Err(Error::UnknownTask(edge.to_task.0));
            }
        }

        // 顶点与端点已在上面核对，共享算法只剩成环这一种失败。
        boost_math::graph_theory::topological_order_by(self, |task| *task).map_err(|_| Error::Cycle)
    }
}

/// Task nodes are the vertices; only task-to-task edges are exported here.
impl GraphRead for TaskGraph {
    type Vertex = TaskId;
    type Edge = usize;

    fn is_directed(&self) -> bool {
        true
    }

    fn vertex_count(&self) -> usize {
        self.nodes.len()
    }

    fn edge_count(&self) -> usize {
        self.edges.iter().filter(|edge| edge.from_task.is_some()).count()
    }

    fn vertices(&self) -> impl Iterator<Item = &Self::Vertex> {
        self.nodes.iter().map(|node| &node.id)
    }

    fn edges(&self) -> impl Iterator<Item = Self::Edge> {
        self.edges.iter().enumerate().filter(|(_, edge)| edge.from_task.is_some()).map(|(position, _)| position)
    }

    fn endpoints(&self, edge: Self::Edge) -> Option<(&Self::Vertex, &Self::Vertex)> {
        let edge = self.edges.get(edge)?;
        Some((edge.from_task.as_ref()?, &edge.to_task))
    }
}

impl OutgoingEdges for TaskGraph {
    fn outgoing(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge> {
        self.edges.iter().enumerate().filter(move |(_, edge)| edge.from_task.as_ref() == Some(vertex)).map(|(position, _)| position)
    }
}
