use std::collections::HashMap;
use std::ops::Deref;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use ratatui::DefaultTerminal;

use kronark_node_parser::kronarknode::{
	instance::Instance, nodes::NodeEntry, roots::Roots, socket::DataType, types::TypeEntry, Node,
};

// Take ownership of a `Node` and parse out its contents
// The data will be taken out of the node and restructured to make rendering easier
// Create a HashMap allowing access of instances by ID, types and node types by string
// Input Root always goes in the far left, Output Root always in the far right
//
// Instances should be separated into columns based on their connection depth
// Connection depth is defined to be the max of the connection depths of all instances connecting
// TO the instance in question, plus one. If an instance has no connections on its input side, it
// has a connection depth of zero. This means we'll be ignoring the stored x values of the instance
//
// The vertical placement of an instance is compressed with all instances in its column, with a
// padding of one. The order of vertical placement will respect the y values stored in the
// instances, but exact positioning will not
//
// Padding between columns is based on how many incoming connections the right column has plus how
// many outgoing connections the previous column has that do NOT link to the right column
// Columns will have a single column of space for each outgoing connection of the previous column,
// with one column of padding on both sides
//
// The intention with this is to draw connections horizontally outwards until they have their own
// unique column to bend, then it will bend up or down as needed to get to the row of its
// connection if the connection exists in the next column of instances, or to clear the bottom of
// the next column of instances if it does not connect immediately to the next depth.
// This requires at minimum one column of spacing per connected input of the right instance column,
// plus an additional column of spacing for each output that needs to leave the region rather than
// connecting to the right column
//
// When a connection bends back horizontally, it's possible that two lines may overlap if we
// compress the vertical space as much as possible, as shown below:
// ********************************
// ───┐ ┌────
//    │ │
// ─────┘────
// ********************************
// In this situation, the line starting at the top left was drawn first, extended out to its target
// column, then went back horizontal at the target row. The bottom line did the same and damaged
// the drawing of the top line. I haven't thought up a good way to avoid this, so the simple method
// which will get us close to a functional renderer as quickly as possible is to simply alternate
// the socket positions from column to column, so that inputs and outputs never lie on the same
// row. Additionally, we will have to detect intersections to replace them with the character '┼'
// (or we ignore that because it's not that important and we can still make sense of it)
//
// I am *very* open to ideas for this. Remember, we're not trying to make it pretty, just good
// enough so we can document the nodes. Pretty comes later.
//
// Additionally, out of necessity I believe it's a good idea to allow scrolling of the view window
// with arrow keys, to browse larger node graphs. `ratatui` does not inherently support having its
// widgets overdraw, but we can implement our own widgets and draw to the buffer provided,
// performing our own overdraw culling. See the video and main.rs file sent in the Kronark Discord
// under the forum thread for this project. I apologize in advance for the shitty code in that
// file, it was put together as hastily as I could to get a demonstration.
// Alternatively, an idea I had while writing this, we could instead only scroll by column and not
// worry about culling overdraw. We generate a simple widget for each instance, do some simple
// calculations to determine the column widths, then render only as many columns would 100% fit on
// screen. Pressing right arrow would shift the leftmost visible column over once. Lines connecting
// to offscreen instances will draw as much of their route as they can, then terminate in an angle
// bracket indicating they go offscreen. This might be simpler. Same logic can be applied to
// vertical scrolling, instead you go by instance within a column.
//
// I've tried to outline what the structure of this renderer could look like below, but this is
// certainly not final. If someone begins to implement this or components of this, do let me know
// so we can coordinate our work and discuss the structure of this.

// Buffer intermediary that will ignore draws entirely offscreen and handle discarding of draws
// partially offscreen
struct OverdrawBuffer {}

struct Size {
	width: i32,
	height: i32,
}
trait WidgetSize {
	fn get_size_estimate(&self) -> Size;
}
// Uses `OverdrawBuffer` instead of `Buffer` and takes a shift value
// Should auto-implement on `Widget`s
trait OverdrawWidget {
	fn render(&self, area: Rect, x_shift: i32, y_shift: i32, buf: &mut OverdrawBuffer);
}

// Thin wrapper for type-safety
#[derive(Debug, PartialEq, Eq, Hash)]
struct InstanceID(usize);
impl Deref for InstanceID {
	type Target = usize;
	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

// TODO: We need a way to store the padding. Should it be here or elsewhere?
#[derive(Debug, Default)]
struct Column {
	instances: Vec<InstanceID>,
}

#[derive(Debug)]
struct InstanceRenderer {
	id: InstanceID,
	x_pos: i32,
	y_pos: i32,
	width: i32,
	height: i32,
}
impl InstanceRenderer {
	fn from_instance(instance: &Instance, x_pos: i32, y_pos: i32) -> InstanceRenderer {
		// TODO: Replace dummy values with correctly computed values
		InstanceRenderer {
			id: InstanceID(instance.key),
			x_pos,
			y_pos,
			width: 20, // TEMP
			height: 5, // TEMP
		}
	}
}
impl WidgetSize for InstanceRenderer {
	fn get_size_estimate(&self) -> Size {
		Size {
			width: self.width,
			height: self.height,
		}
	}
}
impl Widget for &InstanceRenderer {
	fn render(self, area: Rect, buf: &mut Buffer) {
		// TODO: Make this render the node properly
		buf.set_style(area, Style::new().bg(Color::Rgb(30, 30, 30)));
	}
}

#[derive(Debug)]
struct NodeDefRenderer {
	roots: Roots,
	// We aren't guaranteed to have consecutive instance IDs, so a `HashMap` it is
	instance_table: HashMap<InstanceID, Instance>,
	node_table: Vec<NodeEntry>,
	type_table: Vec<TypeEntry>,

	instance_layout: Vec<Column>,
	x_shift: i32,
	y_shift: i32,
}
impl NodeDefRenderer {
	fn init_layout(&mut self) {
		// Notate a depth for each instance, keeping track of the largest depth
		// NOTE: In this situation, depth is labelled from right to left with right being depth 0.
		// This is done because it is easier. Instances are only aware of their connections from
		// their inputs, so it is easiest to travel backwards through the graph

		let mut depths: HashMap<InstanceID, usize> = HashMap::new();
		let mut to_process = vec![];
		for (instance_id, _) in self.roots.output_connections.iter() {
			if *instance_id == 255 {
				continue;
			}
			depths.insert(InstanceID(*instance_id as usize), 0);
			to_process.push(InstanceID(*instance_id as usize));
		}

		let mut max_depth = 0;
		while to_process.len() > 0 {
			// `.unwrap()` acceptable since if there's no value, something has gone *very wrong*
			let instance_id = to_process.pop().unwrap();
			let instance_depth = *depths.get(&instance_id).unwrap();
			max_depth = max_depth.max(instance_depth);
			let instance = self.instance_table.get(&instance_id).unwrap();
			for socket in instance.sockets.iter() {
				if let Some(DataType::Connection(connection_id, _)) = socket.data {
					if connection_id == 255 {
						continue;
					}
					depths.insert(InstanceID(connection_id as usize), instance_depth + 1);
					to_process.push(InstanceID(connection_id as usize));
				}
			}
		}

		// Reorganize into columns
		let mut columns = vec![];
		columns.resize_with(max_depth + 1, Column::default);

		for (instance_id, depth) in depths {
			columns[depth].instances.push(instance_id);
		}

		self.instance_layout = columns;
	}

	fn from_node(node: Node) -> Self {
		match node {
			Node::V1(node_def) => {
				let roots = node_def.roots;
				let node_table = node_def.nodes;
				let type_table = node_def.types;

				let mut instance_table = HashMap::new();
				for instance in node_def.instances {
					instance_table.insert(InstanceID(instance.key), instance);
				}

				let mut renderer = NodeDefRenderer {
					roots,
					instance_table,
					node_table,
					type_table,
					instance_layout: vec![],
					x_shift: 0,
					y_shift: 0,
				};
				renderer.init_layout();

				renderer
			}
			#[allow(unreachable_patterns)]
			_ => panic!("unsupported version"),
		}
	}
}
impl Widget for &NodeDefRenderer {
	fn render(self, area: Rect, buffer: &mut Buffer) {
		// Render input root, then columns from last to first, then output root
		let mut cur_x = 0;
		let mut cur_y = 0;

		// temp paddings
		let pad_x = 3;
		let pad_y = 1;

		// TODO: Render a proper input root
		buffer.set_style(
			Rect::new(cur_x as u16, cur_y as u16, 20, 5),
			Style::new().bg(Color::Rgb(100, 100, 100)),
		);

		cur_x += 20 + pad_x;

		for column in self.instance_layout.iter().rev() {
			cur_y = 0;
			let mut max_width = 0;

			for instance_id in column.instances.iter() {
				let cur_instance = self.instance_table.get(instance_id).unwrap();
				let renderer = InstanceRenderer::from_instance(cur_instance, cur_x, cur_y);
				let cur_width = renderer.width;
				let cur_height = renderer.height;
				let draw_area = Rect::new(
					cur_x as u16,
					cur_y as u16,
					cur_width as u16,
					cur_height as u16,
				);
				renderer.render(draw_area, buffer);

				cur_y += cur_height + pad_y;
				max_width = max_width.max(cur_width);
			}

			cur_x += max_width + pad_x;
		}
	}
}

fn run(mut terminal: DefaultTerminal, mut renderer: NodeDefRenderer) -> std::io::Result<()> {
	loop {
		terminal.draw(|frame| frame.render_widget(&renderer, frame.area()))?;
		if let Event::Key(ke) = event::read()? {
			if ke.kind != KeyEventKind::Press {
				continue;
			}
			match ke.code {
				KeyCode::Char('q') => break,
				_ => (),
			}
		}
	}

	Ok(())
}

// This will setup the terminal and the renderer struct, then enter another function to loop
// drawing and event processing
pub fn enter_node_view(node: Node) -> std::io::Result<()> {
	let renderer = NodeDefRenderer::from_node(node);
	let terminal = ratatui::init();
	let result = run(terminal, renderer);
	ratatui::restore();
	result
}
