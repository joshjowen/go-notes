//! The graph view: a canvas driven by the Rust force simulation.
//!
//! Rendering is immediate-mode on a 2D canvas rather than a DOM tree of SVG
//! elements. A vault of a few thousand notes means a few thousand nodes and
//! rather more edges, and asking the browser to lay out that many elements sixty
//! times a second is the difference between a graph that glides and one that
//! stutters.

use std::cell::RefCell;
use std::rc::Rc;

use go_notes_shared::GraphResponse;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::force::{Edge, Simulation, Vec2};
use crate::state::{use_app, MainView};

/// How the camera maps simulation coordinates to the canvas.
#[derive(Clone, Copy)]
struct Camera {
    offset: Vec2,
    scale: f32,
}

impl Camera {
    fn to_screen(&self, world: Vec2, width: f32, height: f32) -> Vec2 {
        Vec2 {
            x: world.x * self.scale + self.offset.x + width / 2.0,
            y: world.y * self.scale + self.offset.y + height / 2.0,
        }
    }

    fn to_world(&self, screen: Vec2, width: f32, height: f32) -> Vec2 {
        Vec2 {
            x: (screen.x - width / 2.0 - self.offset.x) / self.scale,
            y: (screen.y - height / 2.0 - self.offset.y) / self.scale,
        }
    }
}

/// Everything the animation loop needs, shared with the event handlers.
struct GraphScene {
    simulation: Simulation,
    data: GraphResponse,
    camera: Camera,
    hovered: Option<usize>,
    dragging: Option<usize>,
    panning: bool,
    last_pointer: Vec2,
    /// Set once after the first layout settles, to frame the whole graph.
    fitted: bool,
}

impl GraphScene {
    fn empty() -> GraphScene {
        GraphScene {
            simulation: Simulation::new(&[], vec![]),
            data: GraphResponse {
                nodes: Vec::new(),
                edges: Vec::new(),
            },
            camera: Camera {
                offset: Vec2::ZERO,
                scale: 1.0,
            },
            hovered: None,
            dragging: None,
            panning: false,
            last_pointer: Vec2::ZERO,
            fitted: false,
        }
    }

    fn load(&mut self, data: GraphResponse) {
        let degrees: Vec<u32> = data.nodes.iter().map(|node| node.degree).collect();
        let edges = data
            .edges
            .iter()
            .map(|edge| Edge {
                source: edge.source as usize,
                target: edge.target as usize,
            })
            .collect();

        self.simulation = Simulation::new(&degrees, edges);
        self.data = data;
        self.fitted = false;
        self.hovered = None;
        self.dragging = None;
    }

    /// Scales and centres the camera so the whole graph is visible.
    fn fit(&mut self, width: f32, height: f32) {
        if self.data.nodes.is_empty() {
            return;
        }
        let (min, max) = self.simulation.bounds();
        let span_x = (max.x - min.x).max(1.0);
        let span_y = (max.y - min.y).max(1.0);

        // A margin so nodes at the edge are not clipped by their own radius
        // or their label.
        let scale = ((width - 120.0) / span_x)
            .min((height - 120.0) / span_y)
            .clamp(0.08, 2.5);

        let centre = Vec2 {
            x: (min.x + max.x) / 2.0,
            y: (min.y + max.y) / 2.0,
        };
        self.camera = Camera {
            offset: Vec2 {
                x: -centre.x * scale,
                y: -centre.y * scale,
            },
            scale,
        };
        self.fitted = true;
    }
}

#[component]
pub fn GraphView() -> impl IntoView {
    let state = use_app();
    let canvas_ref: NodeRef<html::Canvas> = NodeRef::new();

    let scene = Rc::new(RefCell::new(GraphScene::empty()));
    let local_only = RwSignal::new(false);
    let depth = RwSignal::new(1u32);
    let loading = RwSignal::new(true);
    let node_count = RwSignal::new(0usize);

    // Fetch, and refetch when the vault changes or the scope is adjusted.
    Effect::new({
        let scene = scene.clone();
        move |_| {
            if state.main_view.get() != MainView::Graph {
                return;
            }
            let _ = state.graph_epoch.get();
            let is_local = local_only.get();
            let depth_value = depth.get();
            let focus = state.active_path();

            let scene = scene.clone();
            loading.set(true);
            spawn_local(async move {
                let scope = if is_local && focus.is_some() {
                    "local"
                } else {
                    "all"
                };
                match api::graph(scope, focus.as_deref(), depth_value).await {
                    Ok(data) => {
                        crate::offline::net::report_reachable(state);
                        node_count.set(data.nodes.len());
                        scene.borrow_mut().load(data);
                    }
                    // The graph is the one view with no offline answer: it is
                    // built from the link table for the *whole* vault, and this
                    // device only holds the notes it has opened. A partial graph
                    // would not be a smaller truth, it would be a wrong one —
                    // notes shown as unlinked because their neighbours are
                    // simply not here.
                    Err(err) if err.is_offline() => {
                        crate::offline::net::report_unreachable(state);
                        node_count.set(0);
                        scene.borrow_mut().load(go_notes_shared::GraphResponse {
                            nodes: Vec::new(),
                            edges: Vec::new(),
                        });
                    }
                    Err(err) => state.error(err.user_message()),
                }
                loading.set(false);
            });
        }
    });

    // The animation loop. Started once the canvas exists, and it keeps itself
    // alive by rescheduling; the `running` flag stops it when the view closes.
    let running = Rc::new(RefCell::new(false));
    Effect::new({
        let scene = scene.clone();
        let running = running.clone();
        move |_| {
            let showing = state.main_view.get() == MainView::Graph;
            if !showing {
                *running.borrow_mut() = false;
                return;
            }
            if *running.borrow() {
                return;
            }
            let Some(canvas) = canvas_ref.get() else {
                return;
            };
            *running.borrow_mut() = true;

            let canvas: web_sys::HtmlCanvasElement = canvas.unchecked_into();
            let scene = scene.clone();
            let running = running.clone();

            // The classic self-rescheduling rAF closure: it has to hold a
            // reference to itself, so it is created in two steps.
            let callback = Rc::new(RefCell::new(None::<Closure<dyn FnMut()>>));
            let scheduler = callback.clone();

            *callback.borrow_mut() = Some(Closure::new(move || {
                if !*running.borrow() {
                    return;
                }
                {
                    let mut scene = scene.borrow_mut();
                    scene.simulation.step();
                    render(&canvas, &mut scene);
                }
                if let Some(next) = scheduler.borrow().as_ref() {
                    request_frame(next);
                }
            }));

            // The borrow is bound to a name rather than left as a temporary
            // inside the `if let`. A temporary would be dropped after `callback`
            // itself, which is the wrong order and does not compile.
            let borrowed = callback.borrow();
            if let Some(closure) = borrowed.as_ref() {
                request_frame(closure);
            }
        }
    });

    // --- pointer interaction ------------------------------------------------
    //
    // Pointer events rather than mouse events, so a finger drags a node and
    // pans the canvas exactly as a mouse does — one set of handlers for both,
    // which is the whole reason the API exists. The canvas carries
    // `touch-action: none` so the browser stops trying to scroll the page out
    // from under the gesture.

    let pointer_position = |ev: &web_sys::MouseEvent| -> (Vec2, f32, f32) {
        let target: web_sys::Element = ev.target().unwrap().unchecked_into();
        let rect = target.get_bounding_client_rect();
        (
            Vec2 {
                x: ev.client_x() as f32 - rect.left() as f32,
                y: ev.client_y() as f32 - rect.top() as f32,
            },
            rect.width() as f32,
            rect.height() as f32,
        )
    };

    let on_move = {
        let scene = scene.clone();
        move |ev: web_sys::MouseEvent| {
            let (pointer, width, height) = pointer_position(&ev);
            let mut scene = scene.borrow_mut();
            let world = scene.camera.to_world(pointer, width, height);

            if let Some(index) = scene.dragging {
                scene.simulation.set_position(index, world);
                scene.simulation.reheat();
                scene.last_pointer = pointer;
                return;
            }

            if scene.panning {
                let dx = pointer.x - scene.last_pointer.x;
                let dy = pointer.y - scene.last_pointer.y;
                scene.camera.offset.x += dx;
                scene.camera.offset.y += dy;
                scene.last_pointer = pointer;
                return;
            }

            // Hit radius in world units, so hovering stays comfortable at any zoom.
            let radius = 14.0 / scene.camera.scale;
            scene.hovered = scene.simulation.node_at(world, radius);
        }
    };

    let on_down = {
        let scene = scene.clone();
        move |ev: web_sys::MouseEvent| {
            let (pointer, width, height) = pointer_position(&ev);
            let mut scene = scene.borrow_mut();
            let world = scene.camera.to_world(pointer, width, height);
            let radius = 14.0 / scene.camera.scale;

            scene.last_pointer = pointer;
            match scene.simulation.node_at(world, radius) {
                Some(index) => {
                    scene.dragging = Some(index);
                    scene.simulation.set_pinned(index, true);
                }
                None => scene.panning = true,
            }
        }
    };

    let on_up = {
        let scene = scene.clone();
        move |_ev: web_sys::MouseEvent| {
            let mut scene = scene.borrow_mut();
            if let Some(index) = scene.dragging.take() {
                // Releasing unpins, so the node settles back into the layout
                // rather than staying frozen where it was dropped.
                scene.simulation.set_pinned(index, false);
                scene.simulation.reheat();
            }
            scene.panning = false;
        }
    };

    let on_click = {
        let scene = scene.clone();
        move |ev: web_sys::MouseEvent| {
            let (pointer, width, height) = pointer_position(&ev);
            let (path, title, unresolved) = {
                let scene = scene.borrow();
                let world = scene.camera.to_world(pointer, width, height);
                let radius = 14.0 / scene.camera.scale;
                match scene.simulation.node_at(world, radius) {
                    Some(index) => match scene.data.nodes.get(index) {
                        Some(node) => (
                            node.path.clone(),
                            node.title.clone(),
                            node.unresolved,
                        ),
                        None => return,
                    },
                    None => return,
                }
            };

            if unresolved {
                state.notify(format!("“{title}” is linked but has not been written yet."));
                return;
            }
            state.open_tab(path, title);
        }
    };

    let on_wheel = {
        let scene = scene.clone();
        move |ev: web_sys::WheelEvent| {
            ev.prevent_default();
            let mut scene = scene.borrow_mut();
            let factor = if ev.delta_y() < 0.0 { 1.12 } else { 1.0 / 1.12 };
            scene.camera.scale = (scene.camera.scale * factor).clamp(0.05, 6.0);
        }
    };

    // Zoom, for anyone without a scroll wheel. A pinch would be nicer and needs
    // two tracked pointers and a gesture state machine; two buttons work today
    // and are also the only way to zoom from a keyboard.
    let zoom_by = {
        let scene = scene.clone();
        move |factor: f32| {
            let mut scene = scene.borrow_mut();
            scene.camera.scale = (scene.camera.scale * factor).clamp(0.05, 6.0);
        }
    };
    let zoom_in = {
        let zoom_by = zoom_by.clone();
        move |_| zoom_by(1.25)
    };
    let zoom_out = move |_| zoom_by(1.0 / 1.25);

    let fit_now = {
        let scene = scene.clone();
        move |_| {
            if let Some(canvas) = canvas_ref.get() {
                let element: web_sys::Element = canvas.unchecked_into();
                let rect = element.get_bounding_client_rect();
                scene
                    .borrow_mut()
                    .fit(rect.width() as f32, rect.height() as f32);
            }
        }
    };

    view! {
        <div class="gn-graph">
            <canvas
                node_ref=canvas_ref
                on:pointermove=move |ev: web_sys::PointerEvent| on_move(ev.unchecked_into())
                on:pointerdown=move |ev: web_sys::PointerEvent| on_down(ev.unchecked_into())
                on:pointerup={
                    let on_up = on_up.clone();
                    move |ev: web_sys::PointerEvent| on_up(ev.unchecked_into())
                }
                on:pointercancel={
                    let on_up = on_up.clone();
                    move |ev: web_sys::PointerEvent| on_up(ev.unchecked_into())
                }
                on:pointerleave=move |ev: web_sys::PointerEvent| on_up(ev.unchecked_into())
                on:click=on_click
                on:wheel=on_wheel
            ></canvas>

            <div class="gn-graph-controls">
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || local_only.get()
                        on:change=move |ev| local_only.set(event_target_checked(&ev))
                    />
                    "Around this note only"
                </label>

                <Show when=move || local_only.get()>
                    <label>
                        "Depth"
                        <input
                            type="range"
                            min="1"
                            max="4"
                            prop:value=move || depth.get().to_string()
                            on:input=move |ev| {
                                if let Ok(value) = event_target_value(&ev).parse::<u32>() {
                                    depth.set(value);
                                }
                            }
                        />
                        {move || depth.get()}
                    </label>
                </Show>

                <button class="gn-graph-fit" on:click=fit_now>
                    "Fit to view"
                </button>

                <span class="gn-graph-zoom">
                    <button title="Zoom out" aria-label="Zoom out" on:click=zoom_out>"−"</button>
                    <button title="Zoom in" aria-label="Zoom in" on:click=zoom_in>"+"</button>
                </span>

                <span class="gn-graph-count">
                    {move || {
                        if loading.get() {
                            "Loading…".to_string()
                        } else if state.local_only() {
                            "Needs the server".to_string()
                        } else {
                            let count = node_count.get();
                            format!("{count} note{}", if count == 1 { "" } else { "s" })
                        }
                    }}
                </span>

                // Canvas interactions are invisible until someone tries them, so
                // say what they are rather than leaving people to guess.
                <span class="gn-graph-hint">
                    {move || {
                        if state.local_only() {
                            "The link graph is built by the server from the whole vault, so it is \
                             unavailable offline. Editing, search and backlinks still work."
                        } else {
                            "Drag to pan · Scroll to zoom · Click a note to open it"
                        }
                    }}
                </span>
            </div>
        </div>
    }
}

fn request_frame(callback: &Closure<dyn FnMut()>) {
    if let Some(window) = web_sys::window() {
        let _ = window.request_animation_frame(callback.as_ref().unchecked_ref());
    }
}

/// Reads a CSS custom property off the document root.
///
/// Colours come from the stylesheet rather than being hard-coded here, so the
/// graph follows the light/dark theme along with everything else.
fn theme_colour(name: &str, fallback: &str) -> String {
    web_sys::window()
        .and_then(|window| {
            let document = window.document()?;
            let root = document.document_element()?;
            let style = window.get_computed_style(&root).ok()??;
            style.get_property_value(name).ok()
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn render(canvas: &web_sys::HtmlCanvasElement, scene: &mut GraphScene) {
    let Some(context) = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|ctx| ctx.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
    else {
        return;
    };

    // Match the backing store to the element's CSS size and the display's pixel
    // ratio, or the whole graph renders blurry on a HiDPI screen.
    let ratio = web_sys::window()
        .map(|window| window.device_pixel_ratio())
        .unwrap_or(1.0) as f32;
    let css_width = canvas.client_width() as f32;
    let css_height = canvas.client_height() as f32;
    if css_width <= 0.0 || css_height <= 0.0 {
        return;
    }

    let target_width = (css_width * ratio) as u32;
    let target_height = (css_height * ratio) as u32;
    if canvas.width() != target_width || canvas.height() != target_height {
        canvas.set_width(target_width);
        canvas.set_height(target_height);
    }
    let _ = context.reset_transform();
    let _ = context.scale(ratio as f64, ratio as f64);
    context.clear_rect(0.0, 0.0, css_width as f64, css_height as f64);

    // Frame the graph once it has settled, so the fit reflects the final layout.
    if !scene.fitted && scene.simulation.settled() {
        scene.fit(css_width, css_height);
    }

    let text_colour = theme_colour("--gn-text-muted", "#b3b3b3");
    let accent = theme_colour("--gn-accent", "#7f6df2");
    let unresolved_colour = theme_colour("--gn-unresolved", "#d9707a");
    let edge_colour = theme_colour("--gn-border-strong", "#4a4a4a");

    let camera = scene.camera;

    // Neighbours of the hovered node, so hovering highlights a neighbourhood
    // rather than a single dot.
    let highlighted: Option<std::collections::HashSet<usize>> = scene.hovered.map(|hovered| {
        let mut set = std::collections::HashSet::new();
        set.insert(hovered);
        for edge in &scene.simulation.edges {
            if edge.source == hovered {
                set.insert(edge.target);
            } else if edge.target == hovered {
                set.insert(edge.source);
            }
        }
        set
    });

    // --- edges --------------------------------------------------------------
    context.set_line_width(1.0);
    for edge in &scene.simulation.edges {
        let (Some(source), Some(target)) = (
            scene.simulation.nodes.get(edge.source),
            scene.simulation.nodes.get(edge.target),
        ) else {
            continue;
        };

        let touches_hover = highlighted
            .as_ref()
            .is_some_and(|set| set.contains(&edge.source) && set.contains(&edge.target));

        context.set_global_alpha(if highlighted.is_none() {
            0.45
        } else if touches_hover {
            0.9
        } else {
            0.08
        });
        context.set_stroke_style_str(if touches_hover { &accent } else { &edge_colour });

        let from = camera.to_screen(source.position, css_width, css_height);
        let to = camera.to_screen(target.position, css_width, css_height);

        context.begin_path();
        context.move_to(from.x as f64, from.y as f64);
        context.line_to(to.x as f64, to.y as f64);
        context.stroke();
    }

    // --- nodes --------------------------------------------------------------
    let show_labels = camera.scale > 0.45 || scene.data.nodes.len() < 120;
    context.set_font("12px -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif");
    context.set_text_align("center");

    for (index, node) in scene.data.nodes.iter().enumerate() {
        let Some(body) = scene.simulation.nodes.get(index) else {
            continue;
        };
        let screen = camera.to_screen(body.position, css_width, css_height);

        // Cull anything off-screen; a large graph spends most of its time with
        // most nodes outside the viewport.
        if screen.x < -60.0
            || screen.y < -60.0
            || screen.x > css_width + 60.0
            || screen.y > css_height + 60.0
        {
            continue;
        }

        let dimmed = highlighted
            .as_ref()
            .is_some_and(|set| !set.contains(&index));
        context.set_global_alpha(if dimmed { 0.18 } else { 1.0 });

        // Radius grows with degree, so hubs read as hubs at a glance.
        let radius = (3.5 + (node.degree as f32).sqrt() * 1.9).min(18.0);

        context.begin_path();
        let _ = context.arc(
            screen.x as f64,
            screen.y as f64,
            radius as f64,
            0.0,
            std::f64::consts::PI * 2.0,
        );
        context.set_fill_style_str(if node.unresolved {
            &unresolved_colour
        } else {
            &accent
        });
        context.fill();

        if show_labels && !dimmed {
            context.set_fill_style_str(&text_colour);
            let label = if node.title.chars().count() > 28 {
                format!("{}…", node.title.chars().take(27).collect::<String>())
            } else {
                node.title.clone()
            };
            let _ = context.fill_text(
                &label,
                screen.x as f64,
                (screen.y + radius + 13.0) as f64,
            );
        }
    }

    context.set_global_alpha(1.0);

    if scene.data.nodes.is_empty() {
        context.set_fill_style_str(&text_colour);
        let _ = context.fill_text(
            "No notes to draw yet.",
            (css_width / 2.0) as f64,
            (css_height / 2.0) as f64,
        );
    }
}
