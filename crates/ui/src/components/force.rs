//! A force-directed layout, written in Rust.
//!
//! This is the piece that most justifies compiling the frontend to WebAssembly.
//! A graph layout is a tight numeric loop run sixty times a second over every
//! node and every edge — exactly the shape of work where WASM earns its keep,
//! and it lets the graph view exist without pulling in d3.
//!
//! Three forces, which between them produce the layout people expect from a
//! linked notes app:
//!
//! * **repulsion** pushes every node away from every other, so labels do not pile
//!   up. Computed through a Barnes-Hut quadtree above a threshold, because the
//!   naive form is O(n²) and a vault of a few thousand notes would stutter.
//! * **springs** pull linked notes together, so clusters form around topics.
//! * **centring** drifts everything gently toward the middle, so a disconnected
//!   note cannot be flung off-screen and lost.
//!
//! The simulation is deterministic given the same input: initial positions come
//! from a fixed hash of each node's index rather than from a random number
//! generator, so reopening the graph gives the same picture rather than
//! reshuffling the user's mental map.

/// Below this many nodes the direct O(n²) loop is faster than building a tree.
const BARNES_HUT_THRESHOLD: usize = 400;

/// Opening angle for the Barnes-Hut approximation. Larger is faster and cruder;
/// 0.9 keeps the layout visually indistinguishable from the exact computation.
const THETA: f32 = 0.9;

/// Stops the simulation once motion falls below this, so an settled graph costs
/// nothing rather than burning a core forever.
const SETTLE_THRESHOLD: f32 = 0.012;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };
}

#[derive(Debug, Clone)]
pub struct Node {
    pub position: Vec2,
    pub velocity: Vec2,
    /// Heavier nodes move less, so a hub stays put while its leaves arrange
    /// themselves around it.
    pub mass: f32,
    /// Set while the user is dragging this node.
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub source: usize,
    pub target: usize,
    /// Scales this edge's pull. 1.0 for a link somebody wrote; a suggestion
    /// carries its similarity score, so it tugs related notes closer without
    /// rearranging a layout the real links already determined.
    pub weight: f32,
}

pub struct Simulation {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Falls toward zero, so the layout settles instead of jittering forever.
    alpha: f32,
    repulsion: f32,
    spring: f32,
    spring_length: f32,
    centring: f32,
    damping: f32,
}

impl Simulation {
    pub fn new(degrees: &[u32], edges: Vec<Edge>) -> Simulation {
        let count = degrees.len();
        let nodes = degrees
            .iter()
            .enumerate()
            .map(|(index, degree)| Node {
                position: initial_position(index, count),
                velocity: Vec2::ZERO,
                // Square root rather than linear: a note with fifty links should
                // be weightier than one with five, but not ten times as immovable.
                mass: 1.0 + (*degree as f32).sqrt(),
                pinned: false,
            })
            .collect();

        Simulation {
            nodes,
            edges,
            alpha: 1.0,
            repulsion: 3000.0,
            spring: 0.045,
            spring_length: 62.0,
            centring: 0.014,
            damping: 0.82,
        }
    }

    /// True once the graph has stopped moving meaningfully.
    pub fn settled(&self) -> bool {
        self.alpha < SETTLE_THRESHOLD
    }

    /// Restarts the motion, after a drag or a change of data.
    pub fn reheat(&mut self) {
        self.alpha = 1.0;
    }

    pub fn set_pinned(&mut self, index: usize, pinned: bool) {
        if let Some(node) = self.nodes.get_mut(index) {
            node.pinned = pinned;
            node.velocity = Vec2::ZERO;
        }
    }

    pub fn set_position(&mut self, index: usize, position: Vec2) {
        if let Some(node) = self.nodes.get_mut(index) {
            node.position = position;
            node.velocity = Vec2::ZERO;
        }
    }

    /// Advances the simulation one frame.
    pub fn step(&mut self) {
        if self.settled() {
            return;
        }

        let count = self.nodes.len();
        let mut forces = vec![Vec2::ZERO; count];

        if count > BARNES_HUT_THRESHOLD {
            self.apply_repulsion_approx(&mut forces);
        } else {
            self.apply_repulsion_exact(&mut forces);
        }
        self.apply_springs(&mut forces);
        self.apply_centring(&mut forces);
        self.integrate(&forces);

        // Cooling schedule: 2% per frame settles a typical graph in a couple of
        // seconds, slow enough that the motion reads as settling rather than
        // snapping into place.
        self.alpha *= 0.98;
    }

    fn apply_repulsion_exact(&self, forces: &mut [Vec2]) {
        let count = self.nodes.len();
        for i in 0..count {
            for j in (i + 1)..count {
                let dx = self.nodes[i].position.x - self.nodes[j].position.x;
                let dy = self.nodes[i].position.y - self.nodes[j].position.y;
                let distance_squared = (dx * dx + dy * dy).max(0.01);
                let distance = distance_squared.sqrt();

                // Repulsion scales with the mass of the *other* node, so a
                // heavily-linked hub clears more space around itself than a
                // leaf does. The pair is therefore not symmetric, and each side
                // gets its own magnitude — which is also exactly what the
                // Barnes-Hut path computes when it treats a cell as one body of
                // the cell's total mass. The two must agree, or crossing the
                // node-count threshold would visibly change the layout.
                let base = self.repulsion / distance_squared;
                let unit_x = dx / distance;
                let unit_y = dy / distance;

                forces[i].x += unit_x * base * self.nodes[j].mass;
                forces[i].y += unit_y * base * self.nodes[j].mass;
                forces[j].x -= unit_x * base * self.nodes[i].mass;
                forces[j].y -= unit_y * base * self.nodes[i].mass;
            }
        }
    }

    fn apply_repulsion_approx(&self, forces: &mut [Vec2]) {
        let tree = QuadTree::build(&self.nodes);
        for (index, node) in self.nodes.iter().enumerate() {
            let force = tree.force_on(index, node.position, self.repulsion);
            forces[index].x += force.x;
            forces[index].y += force.y;
        }
    }

    fn apply_springs(&self, forces: &mut [Vec2]) {
        for edge in &self.edges {
            if edge.source >= self.nodes.len() || edge.target >= self.nodes.len() {
                continue;
            }
            let dx = self.nodes[edge.target].position.x - self.nodes[edge.source].position.x;
            let dy = self.nodes[edge.target].position.y - self.nodes[edge.source].position.y;
            let distance = (dx * dx + dy * dy).sqrt().max(0.01);

            let displacement = distance - self.spring_length;
            let magnitude = self.spring * edge.weight * displacement;
            let fx = dx / distance * magnitude;
            let fy = dy / distance * magnitude;

            forces[edge.source].x += fx;
            forces[edge.source].y += fy;
            forces[edge.target].x -= fx;
            forces[edge.target].y -= fy;
        }
    }

    fn apply_centring(&self, forces: &mut [Vec2]) {
        for (index, node) in self.nodes.iter().enumerate() {
            forces[index].x -= node.position.x * self.centring;
            forces[index].y -= node.position.y * self.centring;
        }
    }

    fn integrate(&mut self, forces: &[Vec2]) {
        for (index, node) in self.nodes.iter_mut().enumerate() {
            if node.pinned {
                continue;
            }
            node.velocity.x = (node.velocity.x + forces[index].x / node.mass * self.alpha)
                * self.damping;
            node.velocity.y = (node.velocity.y + forces[index].y / node.mass * self.alpha)
                * self.damping;

            // A speed cap stops a pathological configuration — two nodes almost
            // exactly on top of each other — from launching them off-screen.
            let speed = (node.velocity.x * node.velocity.x
                + node.velocity.y * node.velocity.y)
                .sqrt();
            const MAX_SPEED: f32 = 34.0;
            if speed > MAX_SPEED {
                node.velocity.x = node.velocity.x / speed * MAX_SPEED;
                node.velocity.y = node.velocity.y / speed * MAX_SPEED;
            }

            node.position.x += node.velocity.x;
            node.position.y += node.velocity.y;
        }
    }

    /// The node nearest a point, if it is within `radius`.
    pub fn node_at(&self, point: Vec2, radius: f32) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (index, node) in self.nodes.iter().enumerate() {
            let dx = node.position.x - point.x;
            let dy = node.position.y - point.y;
            let distance_squared = dx * dx + dy * dy;
            if distance_squared <= radius * radius
                && best.is_none_or(|(_, best_distance)| distance_squared < best_distance)
            {
                best = Some((index, distance_squared));
            }
        }
        best.map(|(index, _)| index)
    }

    /// The bounding box of the laid-out graph, for fitting it to the viewport.
    pub fn bounds(&self) -> (Vec2, Vec2) {
        if self.nodes.is_empty() {
            return (Vec2::ZERO, Vec2::ZERO);
        }
        let mut min = Vec2 {
            x: f32::MAX,
            y: f32::MAX,
        };
        let mut max = Vec2 {
            x: f32::MIN,
            y: f32::MIN,
        };
        for node in &self.nodes {
            min.x = min.x.min(node.position.x);
            min.y = min.y.min(node.position.y);
            max.x = max.x.max(node.position.x);
            max.y = max.y.max(node.position.y);
        }
        (min, max)
    }
}

/// Deterministic starting positions on a phyllotactic spiral.
///
/// Deterministic matters: the graph should look the same each time it is opened,
/// because users navigate by remembering where things were. The spiral spreads
/// nodes evenly, which converges faster than clustering them near the origin.
fn initial_position(index: usize, count: usize) -> Vec2 {
    // The golden angle, which is what makes a phyllotactic spiral even.
    const GOLDEN_ANGLE: f32 = 2.399_963_2;
    let radius = 26.0 * (index as f32 + 0.5).sqrt() * (count as f32).max(1.0).log2().max(1.0) / 3.0;
    let angle = index as f32 * GOLDEN_ANGLE;
    Vec2 {
        x: radius * angle.cos(),
        y: radius * angle.sin(),
    }
}

// ---------------------------------------------------------------------------
// Barnes-Hut
// ---------------------------------------------------------------------------

/// A quadtree of node positions, used to approximate long-range repulsion.
///
/// A distant cluster of nodes is treated as one node at its centre of mass,
/// which turns the O(n²) all-pairs loop into O(n log n).
///
/// Each leaf remembers *which* body it holds. That is not bookkeeping for its
/// own sake: without it, a node walking the tree finds itself in its own leaf,
/// computes a zero distance, and gets an enormous force in an arbitrary
/// direction. Every node would repel itself, which is both wrong and unstable.
struct QuadTree {
    nodes: Vec<TreeNode>,
}

struct TreeNode {
    centre: Vec2,
    half_size: f32,
    mass: f32,
    centre_of_mass: Vec2,
    /// Indices into `QuadTree::nodes`; `NO_CHILD` for an absent child.
    children: [usize; 4],
    /// The single body in this leaf, if any. `None` for internal nodes.
    body: Option<usize>,
    is_leaf: bool,
}

const NO_CHILD: usize = usize::MAX;

/// Depth cap, so two coincident points cannot recurse forever. At this depth the
/// cells are far smaller than any distance the layout can resolve.
const MAX_DEPTH: u32 = 24;

impl QuadTree {
    fn build(bodies: &[Node]) -> QuadTree {
        let mut min = Vec2 {
            x: f32::MAX,
            y: f32::MAX,
        };
        let mut max = Vec2 {
            x: f32::MIN,
            y: f32::MIN,
        };
        for body in bodies {
            min.x = min.x.min(body.position.x);
            min.y = min.y.min(body.position.y);
            max.x = max.x.max(body.position.x);
            max.y = max.y.max(body.position.y);
        }

        let centre = Vec2 {
            x: (min.x + max.x) / 2.0,
            y: (min.y + max.y) / 2.0,
        };
        let half_size = ((max.x - min.x).max(max.y - min.y) / 2.0).max(1.0) + 1.0;

        let mut tree = QuadTree {
            nodes: vec![TreeNode {
                centre,
                half_size,
                mass: 0.0,
                centre_of_mass: Vec2::ZERO,
                children: [NO_CHILD; 4],
                body: None,
                is_leaf: true,
            }],
        };

        for (index, body) in bodies.iter().enumerate() {
            tree.insert(0, index, body.position, body.mass, bodies, 0);
        }
        tree
    }

    fn insert(
        &mut self,
        index: usize,
        body: usize,
        position: Vec2,
        mass: f32,
        bodies: &[Node],
        depth: u32,
    ) {
        self.accumulate_mass(index, position, mass);

        if self.nodes[index].is_leaf {
            // An empty leaf simply takes the body.
            if self.nodes[index].body.is_none() {
                self.nodes[index].body = Some(body);
                return;
            }
            // Coincident points at maximum depth share a leaf; the mass is
            // already accounted for, so there is nothing more to do.
            if depth >= MAX_DEPTH {
                return;
            }

            // An occupied leaf has to split, pushing its existing body down
            // before the new one joins it.
            let existing = self.nodes[index].body.take().expect("checked above");
            self.nodes[index].is_leaf = false;
            let existing_position = bodies[existing].position;
            let existing_mass = bodies[existing].mass;
            self.insert_into_child(index, existing, existing_position, existing_mass, bodies, depth);
            self.insert_into_child(index, body, position, mass, bodies, depth);
            return;
        }

        self.insert_into_child(index, body, position, mass, bodies, depth);
    }

    fn insert_into_child(
        &mut self,
        parent: usize,
        body: usize,
        position: Vec2,
        mass: f32,
        bodies: &[Node],
        depth: u32,
    ) {
        let quadrant = self.quadrant_of(parent, position);
        if self.nodes[parent].children[quadrant] == NO_CHILD {
            let child = self.make_child(parent, quadrant);
            self.nodes[parent].children[quadrant] = child;
        }
        let child = self.nodes[parent].children[quadrant];
        self.insert(child, body, position, mass, bodies, depth + 1);
    }

    fn accumulate_mass(&mut self, index: usize, position: Vec2, mass: f32) {
        let total = self.nodes[index].mass + mass;
        if total <= 0.0 {
            return;
        }
        self.nodes[index].centre_of_mass = Vec2 {
            x: (self.nodes[index].centre_of_mass.x * self.nodes[index].mass + position.x * mass)
                / total,
            y: (self.nodes[index].centre_of_mass.y * self.nodes[index].mass + position.y * mass)
                / total,
        };
        self.nodes[index].mass = total;
    }

    fn quadrant_of(&self, index: usize, position: Vec2) -> usize {
        let centre = self.nodes[index].centre;
        match (position.x >= centre.x, position.y >= centre.y) {
            (false, false) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (true, true) => 3,
        }
    }

    fn make_child(&mut self, parent: usize, quadrant: usize) -> usize {
        let half = self.nodes[parent].half_size / 2.0;
        let centre = self.nodes[parent].centre;
        let offset = match quadrant {
            0 => (-half, -half),
            1 => (half, -half),
            2 => (-half, half),
            _ => (half, half),
        };

        self.nodes.push(TreeNode {
            centre: Vec2 {
                x: centre.x + offset.0,
                y: centre.y + offset.1,
            },
            half_size: half,
            mass: 0.0,
            centre_of_mass: Vec2::ZERO,
            children: [NO_CHILD; 4],
            body: None,
            is_leaf: true,
        });
        self.nodes.len() - 1
    }

    /// The repulsion felt by body `query` at `position`.
    fn force_on(&self, query: usize, position: Vec2, strength: f32) -> Vec2 {
        let mut force = Vec2::ZERO;
        self.accumulate(0, query, position, strength, &mut force);
        force
    }

    fn accumulate(
        &self,
        index: usize,
        query: usize,
        position: Vec2,
        strength: f32,
        force: &mut Vec2,
    ) {
        let node = &self.nodes[index];
        if node.mass == 0.0 {
            return;
        }
        // A body must not repel itself.
        if node.is_leaf && node.body == Some(query) {
            return;
        }

        let dx = position.x - node.centre_of_mass.x;
        let dy = position.y - node.centre_of_mass.y;
        let distance_squared = (dx * dx + dy * dy).max(0.01);
        let distance = distance_squared.sqrt();

        // The Barnes-Hut test: a cell is far enough away to treat as a single
        // point when its width divided by the distance is below theta.
        if node.is_leaf || (node.half_size * 2.0) / distance < THETA {
            let magnitude = strength * node.mass / distance_squared;
            force.x += dx / distance * magnitude;
            force.y += dy / distance * magnitude;
            return;
        }

        for child in node.children {
            if child != NO_CHILD {
                self.accumulate(child, query, position, strength, force);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(simulation: &mut Simulation, frames: usize) {
        for _ in 0..frames {
            simulation.step();
        }
    }

    fn distance(a: Vec2, b: Vec2) -> f32 {
        ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
    }

    #[test]
    fn initial_positions_are_deterministic() {
        // Reopening the graph must not reshuffle it — people navigate by
        // remembering where things were.
        let first = Simulation::new(&[1, 1, 1], vec![]);
        let second = Simulation::new(&[1, 1, 1], vec![]);
        for (a, b) in first.nodes.iter().zip(second.nodes.iter()) {
            assert_eq!(a.position, b.position);
        }
    }

    #[test]
    fn nodes_start_apart_rather_than_stacked() {
        let simulation = Simulation::new(&[0; 40], vec![]);
        for i in 0..simulation.nodes.len() {
            for j in (i + 1)..simulation.nodes.len() {
                assert!(
                    distance(simulation.nodes[i].position, simulation.nodes[j].position) > 0.5,
                    "nodes {i} and {j} started on top of each other"
                );
            }
        }
    }

    #[test]
    fn unlinked_nodes_push_apart() {
        let mut simulation = Simulation::new(&[0, 0], vec![]);
        simulation.set_position(0, Vec2 { x: 1.0, y: 0.0 });
        simulation.set_position(1, Vec2 { x: -1.0, y: 0.0 });
        let before = distance(simulation.nodes[0].position, simulation.nodes[1].position);

        run(&mut simulation, 40);

        let after = distance(simulation.nodes[0].position, simulation.nodes[1].position);
        assert!(after > before, "{after} should exceed {before}");
    }

    #[test]
    fn linked_nodes_settle_near_the_spring_length() {
        let mut simulation = Simulation::new(&[1, 1], vec![Edge { source: 0, target: 1, weight: 1.0 }]);
        simulation.set_position(0, Vec2 { x: -400.0, y: 0.0 });
        simulation.set_position(1, Vec2 { x: 400.0, y: 0.0 });

        run(&mut simulation, 900);

        let gap = distance(simulation.nodes[0].position, simulation.nodes[1].position);
        assert!(
            (20.0..200.0).contains(&gap),
            "linked nodes settled {gap} apart, which is not a sensible resting distance"
        );
    }

    #[test]
    fn the_simulation_settles() {
        let mut simulation = Simulation::new(&[2, 2, 1, 1], vec![
            Edge { source: 0, target: 1, weight: 1.0 },
            Edge { source: 1, target: 2, weight: 1.0 },
            Edge { source: 0, target: 3, weight: 1.0 },
        ]);
        assert!(!simulation.settled());
        run(&mut simulation, 800);
        assert!(simulation.settled(), "the layout never stopped moving");
    }

    /// Everything must stay finite. A NaN anywhere propagates through the whole
    /// layout in one frame and the graph vanishes.
    #[test]
    fn positions_stay_finite_even_when_nodes_coincide() {
        let mut simulation = Simulation::new(&[1; 12], vec![]);
        for index in 0..12 {
            simulation.set_position(index, Vec2 { x: 0.0, y: 0.0 });
        }
        run(&mut simulation, 200);

        for node in &simulation.nodes {
            assert!(
                node.position.x.is_finite() && node.position.y.is_finite(),
                "position went non-finite: {:?}",
                node.position
            );
        }
    }

    #[test]
    fn pinned_nodes_do_not_move() {
        let mut simulation = Simulation::new(&[1, 1, 1], vec![]);
        let anchor = Vec2 { x: 40.0, y: -25.0 };
        simulation.set_position(0, anchor);
        simulation.set_pinned(0, true);

        run(&mut simulation, 120);

        assert_eq!(simulation.nodes[0].position, anchor);
    }

    #[test]
    fn hit_testing_finds_the_nearest_node_within_range() {
        let mut simulation = Simulation::new(&[1, 1], vec![]);
        simulation.set_position(0, Vec2 { x: 0.0, y: 0.0 });
        simulation.set_position(1, Vec2 { x: 100.0, y: 0.0 });

        assert_eq!(simulation.node_at(Vec2 { x: 4.0, y: 3.0 }, 12.0), Some(0));
        assert_eq!(simulation.node_at(Vec2 { x: 98.0, y: 0.0 }, 12.0), Some(1));
        assert_eq!(simulation.node_at(Vec2 { x: 50.0, y: 0.0 }, 12.0), None);
    }

    /// The approximation must agree with the exact computation closely enough
    /// that crossing the node-count threshold does not visibly change a layout.
    ///
    /// The error is measured against the *mean* force magnitude rather than each
    /// node's own. A node near the centre of a symmetric arrangement has its
    /// repulsions very nearly cancel, so its net force is close to zero and any
    /// per-node relative error explodes while the absolute error stays tiny —
    /// which would make this test fail for a layout that is visually perfect.
    #[test]
    fn barnes_hut_approximates_the_exact_repulsion() {
        let count = 60;
        let simulation = Simulation::new(&vec![1; count], vec![]);

        let mut exact = vec![Vec2::ZERO; count];
        simulation.apply_repulsion_exact(&mut exact);

        let mut approx = vec![Vec2::ZERO; count];
        simulation.apply_repulsion_approx(&mut approx);

        let magnitude = |v: Vec2| (v.x * v.x + v.y * v.y).sqrt();
        let mean: f32 = exact.iter().map(|v| magnitude(*v)).sum::<f32>() / count as f32;
        assert!(mean > 0.0, "the exact computation produced no forces at all");

        for index in 0..count {
            let difference = magnitude(Vec2 {
                x: exact[index].x - approx[index].x,
                y: exact[index].y - approx[index].y,
            });
            assert!(
                difference / mean < 0.25,
                "node {index}: approximation was off by {:.1}% of the mean force",
                100.0 * difference / mean
            );
        }
    }

    /// Both paths must weight repulsion by the other node's mass, or a graph
    /// would rearrange itself the moment it grew past the threshold.
    #[test]
    fn heavier_nodes_push_harder() {
        let simulation = Simulation::new(&[0, 16], vec![]);
        let mut forces = vec![Vec2::ZERO; 2];
        simulation.apply_repulsion_exact(&mut forces);

        let magnitude = |v: Vec2| (v.x * v.x + v.y * v.y).sqrt();
        assert!(
            magnitude(forces[0]) > magnitude(forces[1]),
            "the light node should be pushed harder by the heavy one than the reverse"
        );
    }

    #[test]
    fn a_large_graph_steps_without_panicking() {
        // Exercises the Barnes-Hut path, including the tree rebuild each frame.
        let count = BARNES_HUT_THRESHOLD + 120;
        let edges = (0..count - 1)
            .map(|index| Edge {
                source: index,
                target: index + 1,
                weight: 1.0,
            })
            .collect();
        let mut simulation = Simulation::new(&vec![2; count], edges);
        run(&mut simulation, 30);

        assert!(simulation.nodes.iter().all(|node| node.position.x.is_finite()));
    }

    #[test]
    fn an_empty_graph_is_harmless() {
        let mut simulation = Simulation::new(&[], vec![]);
        run(&mut simulation, 10);
        assert_eq!(simulation.bounds(), (Vec2::ZERO, Vec2::ZERO));
        assert_eq!(simulation.node_at(Vec2::ZERO, 10.0), None);
    }

    /// Edges referring to nodes that do not exist must be skipped rather than
    /// panicking — a malformed response should degrade, not crash the tab.
    #[test]
    fn out_of_range_edges_are_ignored() {
        let mut simulation = Simulation::new(&[1, 1], vec![Edge { source: 0, target: 99, weight: 1.0 }]);
        run(&mut simulation, 10);
        assert!(simulation.nodes.iter().all(|node| node.position.x.is_finite()));
    }
}

