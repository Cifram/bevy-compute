use bevy::{
	ecs::system::SystemState,
	prelude::*,
	render::{
		graph::CameraDriverLabel,
		render_graph::{RenderGraph, RenderLabel},
	},
};

use super::{compute_node::ComputeNode, compute_sequence::ComputeSequence};

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct ComputeLabel;

pub fn compute_render_setup(world: &mut World) {
	let mut system_state: SystemState<(ResMut<RenderGraph>, Res<ComputeSequence>)> = SystemState::new(world);
	let (mut render_graph, sequence) = system_state.get_mut(world);

	render_graph.add_node(ComputeLabel, ComputeNode::new(&sequence));
	render_graph.add_node_edge(ComputeLabel, CameraDriverLabel);
}
