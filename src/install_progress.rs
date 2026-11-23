//! Staged media-tool installation progress, shown as a ring that wraps the
//! button (the pattern Bazaar uses for its install button) plus a caption
//! underneath for the current stage. No byte progress is available from the
//! installers yet, so the ring sweeps indeterminately.

use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;
use std::time::Duration;

use gettextrs::gettext;
use gtk4::glib::{self, ControlFlow};
use gtk4::prelude::*;

// One install in flight; later clicks cannot start a second one.
thread_local! {
    static RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const INSET: f64 = 3.0;
const STROKE: f64 = 4.0;
const ARC_STEPS: usize = 12;
const SAMPLE_STEPS: usize = 64;

/// End of the drawn segment for a known fraction. Mirrors Bazaar: a fraction
/// of 0 still shows a short segment, so "started" is visible.
fn determinate_end(fraction: f64) -> f64 {
    0.05 + 0.95 * fraction.clamp(0.0, 1.0)
}

/// End of the moving segment while progress is unknown (a 20% sweep).
/// This may exceed 1.0; the drawing code splits the segment at the outline's
/// closing point so the animation wraps without drawing a chord across it.
fn indeterminate_end(phase: f64) -> f64 {
    phase + 0.2
}

fn installed_status(version: Option<String>) -> String {
    match version {
        Some(version) => gettext("Installed • {version}").replace("{version}", &version),
        None => gettext("Installed"),
    }
}

/// Pill outline as a closed polyline, clockwise from the top-left tangent.
fn pill_outline(width: f64, height: f64) -> Vec<(f64, f64)> {
    let radius = (height / 2.0 - INSET).min(width / 2.0 - INSET).max(1.0);
    let left = INSET + radius;
    let right = (width - INSET - radius).max(left);
    let center_y = height / 2.0;
    let top = INSET;
    let bottom = height - INSET;
    let mut points = Vec::with_capacity(2 * ARC_STEPS + 4);
    points.push((left, top));
    points.push((right, top));
    for step in 1..=ARC_STEPS {
        let angle = -PI / 2.0 + PI * step as f64 / ARC_STEPS as f64;
        points.push((
            right + radius * angle.cos(),
            center_y + radius * angle.sin(),
        ));
    }
    points.push((left, bottom));
    for step in 1..=ARC_STEPS {
        let angle = PI / 2.0 + PI * step as f64 / ARC_STEPS as f64;
        points.push((left + radius * angle.cos(), center_y + radius * angle.sin()));
    }
    points.push((left, top));
    points
}

/// Sample the part of the outline between two fractional distances. Both
/// bounds stay inside 0..=1 so the segment never crosses the closing edge.
fn outline_segment(width: f64, height: f64, start: f64, end: f64) -> Vec<(f64, f64)> {
    let outline = pill_outline(width, height);
    if outline.len() < 2 {
        return outline;
    }
    let mut cumulative = Vec::with_capacity(outline.len());
    cumulative.push(0.0);
    for pair in outline.windows(2) {
        let length = ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt();
        cumulative.push(cumulative.last().copied().unwrap_or(0.0) + length);
    }
    let total = cumulative.last().copied().unwrap_or(0.0);
    if total <= 0.0 {
        return outline;
    }
    let last_segment = outline.len() - 2;
    let mut points = Vec::with_capacity(SAMPLE_STEPS + 1);
    for sample in 0..=SAMPLE_STEPS {
        let fraction = start + (end - start) * sample as f64 / SAMPLE_STEPS as f64;
        let distance = fraction.clamp(0.0, 1.0) * total;
        let mut index = 0;
        while index < last_segment && cumulative[index + 1] < distance {
            index += 1;
        }
        let segment_length = cumulative[index + 1] - cumulative[index];
        let ratio = if segment_length <= 0.0 {
            0.0
        } else {
            (distance - cumulative[index]) / segment_length
        };
        let from = outline[index];
        let to = outline[index + 1];
        points.push((
            from.0 + (to.0 - from.0) * ratio,
            from.1 + (to.1 - from.1) * ratio,
        ));
    }
    points
}

/// Split a segment that crosses the outline's closing point into two
/// contiguous subpaths. A single polyline would draw a chord across the pill.
fn outline_segments(width: f64, height: f64, start: f64, end: f64) -> Vec<Vec<(f64, f64)>> {
    if end <= 1.0 {
        vec![outline_segment(width, height, start, end)]
    } else {
        vec![
            outline_segment(width, height, start, 1.0),
            outline_segment(width, height, 0.0, end - 1.0),
        ]
    }
}

fn ensure_ring_css() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_ok() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(
            ".grab-install-ring { color: var(--accent-bg-color, currentColor); }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("install ring CSS: no display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[derive(Default)]
struct RingState {
    active: bool,
    fraction: Option<f64>,
    phase: f64,
    tick: Option<glib::source::SourceId>,
}

/// The install button, a non-interactive ring drawn around it, and a caption
/// below it for the active stage or a failure message.
pub struct ToolProgressButton {
    pub root: gtk4::Box,
    pub button: gtk4::Button,
    ring: gtk4::DrawingArea,
    status: gtk4::Label,
    state: Rc<RefCell<RingState>>,
    restore_label: RefCell<String>,
}

impl ToolProgressButton {
    pub fn new(label: &str) -> Self {
        ensure_ring_css();
        let button = gtk4::Button::builder()
            .label(label)
            .valign(gtk4::Align::Center)
            .build();
        button.set_margin_start(6);
        button.set_margin_end(6);
        button.set_margin_top(6);
        button.set_margin_bottom(6);

        let ring = gtk4::DrawingArea::new();
        ring.add_css_class("grab-install-ring");
        let ring_label = gettext("Media support tool install progress");
        ring.update_property(&[gtk4::accessible::Property::Label(&ring_label)]);
        ring.set_can_target(false);
        ring.set_hexpand(true);
        ring.set_vexpand(true);
        ring.set_visible(false);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&button));
        overlay.add_overlay(&ring);

        let status = gtk4::Label::new(None);
        status.add_css_class("caption");
        status.add_css_class("dimmed");
        status.set_halign(gtk4::Align::Center);
        status.set_wrap(true);
        status.set_max_width_chars(32);
        status.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        status.set_visible(false);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        root.append(&overlay);
        root.append(&status);

        let state = Rc::new(RefCell::new(RingState::default()));
        let cleanup = state.clone();
        root.connect_destroy(move |_| {
            if let Some(id) = cleanup.borrow_mut().tick.take() {
                id.remove();
            }
        });
        let draw_state = state.clone();
        ring.set_draw_func(move |area, cr, width, height| {
            let state = draw_state.borrow();
            if !state.active || width <= 0 || height <= 0 {
                return;
            }
            let segments = match state.fraction {
                Some(fraction) => vec![outline_segment(
                    width as f64,
                    height as f64,
                    0.0,
                    determinate_end(fraction),
                )],
                None => outline_segments(
                    width as f64,
                    height as f64,
                    state.phase,
                    indeterminate_end(state.phase),
                ),
            };
            let color = area.color();
            cr.set_source_rgba(
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
                color.alpha() as f64,
            );
            cr.set_line_width(STROKE);
            cr.set_line_cap(gtk4::cairo::LineCap::Butt);
            cr.set_line_join(gtk4::cairo::LineJoin::Round);
            for points in segments {
                let Some((first, rest)) = points.split_first() else {
                    continue;
                };
                cr.move_to(first.0, first.1);
                for point in rest {
                    cr.line_to(point.0, point.1);
                }
            }
            let _ = cr.stroke();
        });

        Self {
            root,
            button,
            ring,
            status,
            state,
            restore_label: RefCell::new(label.to_string()),
        }
    }

    fn begin(&self) {
        *self.restore_label.borrow_mut() = self
            .button
            .label()
            .map(|label| label.to_string())
            .unwrap_or_default();
        self.button.set_label(&gettext("Installing…"));
        self.status.set_visible(false);
        {
            let mut state = self.state.borrow_mut();
            state.active = true;
            state.fraction = None;
            state.phase = 0.0;
        }
        self.ring.set_visible(true);
        self.button.set_sensitive(false);
        self.start_tick();
        self.ring.queue_draw();
    }

    fn set_progress(&self, fraction: f64) {
        self.state.borrow_mut().fraction = Some(fraction.clamp(0.0, 1.0));
        self.ring.queue_draw();
    }

    fn set_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(true);
    }

    /// Another widget owns the in-flight install: tell this one's user why
    /// nothing started, then clear the hint a few seconds later.
    fn note_busy(&self) {
        if self.state.borrow().active {
            return;
        }
        self.set_status(&gettext("An install is already in progress…"));
        let status = self.status.downgrade();
        let state = self.state.clone();
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            if !state.borrow().active
                && let Some(status) = status.upgrade()
            {
                status.set_visible(false);
            }
        });
    }

    fn finish(&self) {
        self.stop_tick();
        self.state.borrow_mut().active = false;
        self.ring.set_visible(false);
        self.status.set_visible(false);
        self.button.set_label(&self.restore_label.borrow());
        self.button.set_sensitive(true);
    }

    fn fail(&self) {
        // The caller owns the error surface (row subtitle); don't duplicate it
        // in the caption or overwrite the button's stateful label.
        self.finish();
    }

    fn start_tick(&self) {
        let mut state = self.state.borrow_mut();
        if state.tick.is_some() {
            return;
        }
        let ring = self.ring.downgrade();
        let shared = self.state.clone();
        let id = glib::timeout_add_local(Duration::from_millis(33), move || {
            let Some(ring) = ring.upgrade() else {
                shared.borrow_mut().tick = None;
                return ControlFlow::Break;
            };
            let mut state = shared.borrow_mut();
            if !state.active {
                state.tick = None;
                return ControlFlow::Break;
            }
            state.phase = (state.phase + 1.0 / 90.0) % 1.0;
            drop(state);
            ring.queue_draw();
            ControlFlow::Continue
        });
        state.tick = Some(id);
    }

    fn stop_tick(&self) {
        if let Some(id) = self.state.borrow_mut().tick.take() {
            id.remove();
        }
    }
}

/// Staged media-tool installation under the wrapped-ring progress button.
/// `on_error`/`on_success` report to the caller; `tools` limits an update to
/// the tools actually being downloaded (None = fresh install, show all).
pub fn run(
    progress: &Rc<ToolProgressButton>,
    on_error: impl Fn(String) + 'static,
    on_success: impl Fn() + 'static,
    tools: Option<Vec<String>>,
) {
    if RUNNING.with(|running| running.get()) {
        progress.note_busy();
        return;
    }
    RUNNING.with(|running| running.set(true));
    let progress = progress.clone();
    progress.begin();

    let include = |name: &str| {
        tools
            .as_ref()
            .map(|list| list.iter().any(|item| item == name))
            .unwrap_or(true)
    };
    let do_yt = include("yt-dlp");
    let do_gdl = include("gallery-dl");
    let do_deno = include("deno");

    glib::spawn_future_local(async move {
        if do_yt {
            progress.set_status(&gettext("Downloading yt-dlp…"));
            let yt_path = match crate::video::install_ytdlp().await {
                Ok(path) => path,
                Err(error) => {
                    let message = error.to_string();
                    progress.fail();
                    RUNNING.with(|running| running.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(yt_path, "--version").await;
            progress.set_status(&installed_status(version));
        }
        if do_gdl {
            progress.set_status(&gettext("Downloading gallery-dl…"));
            let gdl_path = match crate::runtime::tokio_rt()
                .spawn(async move { crate::gallery_tools::install_gallerydl().await })
                .await
                .ok()
                .unwrap_or_else(|| Err(crate::gallery_tools::GalleryError::install("spawn failed")))
            {
                Ok(path) => path,
                Err(error) => {
                    let message = error.to_string();
                    progress.fail();
                    RUNNING.with(|running| running.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::runtime::tokio_rt()
                .spawn(async move {
                    crate::gallery_tools::gallerydl_display_version(&gdl_path)
                        .await
                        .ok()
                })
                .await
                .ok()
                .flatten();
            progress.set_status(&installed_status(version));
        }
        if do_deno {
            progress.set_status(&gettext("Downloading Deno…"));
            let deno_path = match crate::video::install_deno().await {
                Ok(path) => path,
                Err(error) => {
                    let message = error.to_string();
                    progress.fail();
                    RUNNING.with(|running| running.set(false));
                    on_error(message);
                    return;
                }
            };
            let version = crate::video_tools::tool_display_version(deno_path, "--version").await;
            progress.set_status(&installed_status(version));
        }

        progress.set_progress(1.0);
        glib::timeout_future(Duration::from_millis(1200)).await;
        progress.finish();
        RUNNING.with(|running| running.set(false));
        on_success();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinate_progress_starts_small_and_clamps() {
        assert_eq!(determinate_end(0.0), 0.05);
        assert_eq!(determinate_end(1.0), 1.0);
        assert_eq!(determinate_end(2.0), 1.0);
        assert_eq!(determinate_end(-1.0), 0.05);
    }

    #[test]
    fn indeterminate_sweep_stays_inside_the_outline() {
        assert_eq!(indeterminate_end(0.0), 0.2);
        assert!((indeterminate_end(0.7) - 0.9).abs() < 1e-9);
        assert!(indeterminate_end(0.775) <= 1.0);
        assert!((indeterminate_end(0.9) - 1.1).abs() < 1e-9);
    }

    #[test]
    fn indeterminate_sweep_wraps_without_a_chord_across_the_pill() {
        let segments = outline_segments(120.0, 40.0, 0.9, indeterminate_end(0.9));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].last(), segments[1].first());
    }

    #[test]
    fn outline_fits_a_narrow_button() {
        let points = outline_segment(40.0, 46.0, 0.0, 1.0);
        for (x, y) in &points {
            assert!((0.0..=40.0).contains(x), "x outside narrow area: {x}");
            assert!((0.0..=46.0).contains(y), "y outside narrow area: {y}");
        }
    }

    #[test]
    fn full_outline_is_closed_and_fits_the_area() {
        let points = outline_segment(120.0, 40.0, 0.0, 1.0);
        assert!(points.len() > 2);
        let first = points.first().unwrap();
        let last = points.last().unwrap();
        assert!((first.0 - last.0).abs() < 0.001);
        assert!((first.1 - last.1).abs() < 0.001);
        for (x, y) in &points {
            assert!((0.0..=120.0).contains(x), "x outside area: {x}");
            assert!((0.0..=40.0).contains(y), "y outside area: {y}");
        }
    }

    #[test]
    fn partial_segment_covers_only_its_slice() {
        let full = outline_segment(120.0, 40.0, 0.0, 1.0);
        let half = outline_segment(120.0, 40.0, 0.0, 0.5);
        assert_eq!(half.first(), full.first());
        assert_ne!(half.last(), full.last());
    }
}
