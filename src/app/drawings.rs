//! Drawings as something an agent can *read* (follow-up to Milestone 7.5):
//! a terminal connected to a Drawing (the connection grants
//! `ShareContext`) can list it and get it rendered to a PNG plus a
//! pixel-space summary of its strokes — `duetctl drawing list|read`. Agents
//! never draw; the drawing stays the user's sketch.
//!
//! Rendering uses cairo off-screen (no window, no widget), so it works for
//! a drawing in a background workspace's view too, as long as the node is
//! live in the active workspace.

use super::App;
use crate::message::{DrawingExport, DrawingStrokeSummary, DrawingSummary};
use crate::model::{EdgeCapability, NodeKind, Stroke};
use crate::orchestration::permissions::authorize;
use gtk4::cairo;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Height of a Drawing card's toolbar; the drawing surface is the card's
/// body minus this (`node_drawing.rs`).
const TOOLBAR_HEIGHT: f64 = 32.0;

/// Exports are at least this wide, so a small card still reads clearly.
const MIN_EXPORT_WIDTH: f64 = 800.0;

impl App {
    fn may_read_drawing(&self, requested_by: Option<Uuid>, id: Uuid) -> Result<(), String> {
        match requested_by {
            // The human operator (GUI / bare CLI) is trusted.
            None => Ok(()),
            Some(agent) => {
                authorize(&self.edges, agent, id, EdgeCapability::ShareContext).map_err(|_| {
                    "not authorized: connect this drawing to your terminal on the canvas \
                     (it grants ShareContext) to read it"
                        .to_string()
                })
            }
        }
    }

    /// Every drawing in the active workspace the requester may read: all of
    /// them for the human operator, the connected ones for an agent.
    pub fn list_drawings(&self, requested_by: Option<Uuid>) -> Vec<DrawingSummary> {
        let mut drawings: Vec<DrawingSummary> = self
            .nodes
            .iter()
            .filter_map(|(id, entry)| match &entry.record.kind {
                NodeKind::Drawing(drawing) => Some(DrawingSummary {
                    id: *id,
                    strokes: drawing.strokes.len(),
                    readable: self.may_read_drawing(requested_by, *id).is_ok(),
                }),
                _ => None,
            })
            .filter(|summary| summary.readable)
            .collect();
        drawings.sort_by_key(|d| d.id);
        drawings
    }

    /// Renders drawing `id` to `<data>/duet/drawing-exports/<id>.png` and
    /// returns its path with the strokes in pixel coordinates of that image.
    pub fn read_drawing(
        &self,
        requested_by: Option<Uuid>,
        id: Uuid,
    ) -> Result<DrawingExport, String> {
        let entry = self
            .nodes
            .get(&id)
            .ok_or_else(|| format!("no drawing with id {id} in this workspace"))?;
        let NodeKind::Drawing(drawing) = &entry.record.kind else {
            return Err(format!("{id} is not a drawing"));
        };
        self.may_read_drawing(requested_by, id)?;
        let surface = (
            entry.record.size.0.max(1.0),
            (entry.record.size.1 - TOOLBAR_HEIGHT).max(1.0),
        );
        let scale = (MIN_EXPORT_WIDTH / surface.0).max(1.0);
        let size = ((surface.0 * scale).round(), (surface.1 * scale).round());
        let dir = self
            .store_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join("drawing-exports");
        let path = dir.join(format!("{id}.png"));
        render_png(&drawing.strokes, size, scale, &dir, &path)?;
        let strokes = drawing
            .strokes
            .iter()
            .filter(|s| crate::drawing::is_valid(s))
            .map(|stroke| DrawingStrokeSummary {
                color: stroke.color.clone(),
                width: stroke.width * scale,
                points: stroke
                    .points
                    .iter()
                    .map(|p| {
                        let (x, y) = crate::drawing::denormalize(*p, size);
                        (x.round() as i32, y.round() as i32)
                    })
                    .collect(),
            })
            .collect();
        Ok(DrawingExport {
            id,
            path,
            width: size.0 as u32,
            height: size.1 as u32,
            strokes,
        })
    }
}

/// White "paper", every valid stroke on top, written atomically to `path`
/// inside an owner-only `dir`.
fn render_png(
    strokes: &[Stroke],
    size: (f64, f64),
    scale: f64,
    dir: &Path,
    path: &Path,
) -> Result<(), String> {
    let mut image =
        cairo::ImageSurface::create(cairo::Format::ARgb32, size.0 as i32, size.1 as i32)
            .map_err(|error| format!("couldn't render the drawing: {error}"))?;
    {
        let cr = cairo::Context::new(&image)
            .map_err(|error| format!("couldn't render the drawing: {error}"))?;
        cr.set_source_rgb(0.984, 0.984, 0.976);
        let _ = cr.paint();
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        for stroke in strokes.iter().filter(|s| crate::drawing::is_valid(s)) {
            let scaled = Stroke {
                width: stroke.width * scale,
                ..stroke.clone()
            };
            crate::node_drawing::paint_stroke(&cr, &scaled, size);
        }
    }
    image.flush();
    let (width, height, stride) = (image.width(), image.height(), image.stride());
    let pixels = image
        .data()
        .map_err(|error| format!("couldn't read the rendered drawing: {error}"))?
        .to_vec();
    // cairo's ARGB32 is native-endian premultiplied: BGRA bytes on the
    // little-endian machines Duet runs on.
    let texture = gtk4::gdk::MemoryTexture::new(
        width,
        height,
        if cfg!(target_endian = "little") {
            gtk4::gdk::MemoryFormat::B8g8r8a8Premultiplied
        } else {
            gtk4::gdk::MemoryFormat::A8r8g8b8Premultiplied
        },
        &gtk4::glib::Bytes::from_owned(pixels),
        stride as usize,
    );
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let temporary: PathBuf = path.with_extension("png.tmp");
    use gtk4::prelude::TextureExt;
    texture
        .save_to_png(&temporary)
        .map_err(|error| format!("couldn't write the drawing image: {error}"))?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}
