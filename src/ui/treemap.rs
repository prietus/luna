//! Treemap of what takes space inside an archive (BorgMac's `TreemapView`):
//! one level of the tree at a time, squarified, click to drill down.
use super::archive_browser::{NavHeader, load_tree, loading_page, nav_header};
use super::kit;
use super::state::AppState;
use crate::core::archive_tree::{ArchiveTree, NodeId};
use crate::core::format;
use crate::core::models::{Archive, Repository};
use adw::prelude::*;
use gtk::{cairo, glib};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

/// Children smaller than this share of their parent are grouped into "other".
const THRESHOLD: f64 = 0.002;

// ------------------------------------------------- per-level snapshot items

#[derive(Debug, Clone, PartialEq)]
pub struct TreemapItem {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub is_aggregated: bool,
    pub node: Option<NodeId>,
}

/// Produces renderable items for one level of a tree, dropping items smaller
/// than `threshold * parent_total` into an aggregated "other" bucket.
pub fn items_for(tree: &ArchiveTree, parent: NodeId, threshold: f64) -> Vec<TreemapItem> {
    let all: Vec<(NodeId, u64)> =
        tree.children(parent).filter(|(_, n)| n.total_size > 0).map(|(id, n)| (id, n.total_size)).collect();
    let total: u64 = all.iter().map(|(_, size)| size).sum();
    if total == 0 {
        return vec![];
    }
    let cutoff = (total as f64 * threshold) as u64;
    let mut result = vec![];
    let (mut hidden_count, mut hidden_sum) = (0usize, 0u64);
    for (id, size) in all {
        if size >= cutoff {
            let node = tree.get(id);
            result.push(TreemapItem {
                id: node.path.clone(),
                name: node.name.clone(),
                size,
                is_aggregated: false,
                node: Some(id),
            });
        } else {
            hidden_count += 1;
            hidden_sum += size;
        }
    }
    if hidden_count > 0 {
        result.push(TreemapItem {
            id: format!("__rest__:{}", tree.get(parent).path),
            name: format!("other ({hidden_count})"),
            size: hidden_sum,
            is_aggregated: true,
            node: None,
        });
    }
    result
}

// ------------------------------------------------ squarified treemap layout

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

#[derive(Debug, Clone)]
pub struct PlacedRect {
    pub item: TreemapItem,
    pub rect: Rect,
}

pub fn squarify(items: &[TreemapItem], rect: Rect) -> Vec<PlacedRect> {
    if items.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
        return vec![];
    }
    let mut sorted: Vec<&TreemapItem> = items.iter().filter(|i| i.size > 0).collect();
    sorted.sort_by(|a, b| b.size.cmp(&a.size));
    if sorted.is_empty() {
        return vec![];
    }

    let total_size: f64 = sorted.iter().map(|i| i.size as f64).sum();
    let scale = rect.width * rect.height / total_size; // units: px² per byte
    // Pre-scale each item to area (in px²), keep in parallel array.
    let areas: Vec<f64> = sorted.iter().map(|i| i.size as f64 * scale).collect();

    let worst = |row: &[usize], side: f64| -> f64 {
        let (Some(&first), Some(&last)) = (row.first(), row.last()) else { return f64::INFINITY };
        let sum: f64 = row.iter().map(|&i| areas[i]).sum();
        if sum <= 0.0 {
            return f64::INFINITY;
        }
        let (s2, sum2) = (side * side, sum * sum);
        ((s2 * areas[first]) / sum2).max(sum2 / (s2 * areas[last]))
    };

    let mut result: Vec<PlacedRect> = Vec::with_capacity(sorted.len());
    let mut layout_row = |row: &[usize], rect: &mut Rect| {
        let row_sum: f64 = row.iter().map(|&i| areas[i]).sum();
        if row_sum <= 0.0 {
            return;
        }
        if rect.width >= rect.height {
            let strip_width = row_sum / rect.height;
            let mut y = rect.y;
            for &i in row {
                let h = areas[i] / strip_width;
                result.push(PlacedRect {
                    item: sorted[i].clone(),
                    rect: Rect { x: rect.x, y, width: strip_width, height: h },
                });
                y += h;
            }
            rect.x += strip_width;
            rect.width = (rect.width - strip_width).max(0.0);
        } else {
            let strip_height = row_sum / rect.width;
            let mut x = rect.x;
            for &i in row {
                let w = areas[i] / strip_height;
                result.push(PlacedRect {
                    item: sorted[i].clone(),
                    rect: Rect { x, y: rect.y, width: w, height: strip_height },
                });
                x += w;
            }
            rect.y += strip_height;
            rect.height = (rect.height - strip_height).max(0.0);
        }
    };

    let mut remaining = rect;
    let mut row: Vec<usize> = vec![]; // indexes into sorted/areas
    let mut i = 0;
    while i < sorted.len() {
        let side = remaining.width.min(remaining.height);
        let mut with_new = row.clone();
        with_new.push(i);
        if row.is_empty() || worst(&with_new, side) <= worst(&row, side) {
            row = with_new;
            i += 1;
        } else {
            layout_row(&row, &mut remaining);
            row.clear();
            if remaining.width <= 0.0 || remaining.height <= 0.0 {
                break;
            }
        }
    }
    if !row.is_empty() {
        layout_row(&row, &mut remaining);
    }
    result
}

// ------------------------------------------------------------------ colours

fn hsb_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let h6 = (h.fract() + 1.0).fract() * 6.0;
    let sector = h6.floor();
    let f = h6 - sector;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match sector as u8 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Hue derived from a hash of the item's path, so a folder keeps its colour
/// while drilling in and out (FNV-1a: stable across runs, unlike Swift's
/// per-process seeded `Hasher`).
fn color_for(item: &TreemapItem) -> (f64, f64, f64) {
    if item.is_aggregated {
        return (0.35, 0.35, 0.35);
    }
    let hash = item.id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    let hue = (hash % 997) as f64 / 997.0;
    hsb_to_rgb(hue, 0.55, 0.62)
}

// --------------------------------------------------------------------- view

struct Treemap {
    dialog: adw::Dialog,
    tree: RefCell<Option<Arc<ArchiveTree>>>,
    /// Path segments to the current focus.
    drill: RefCell<Vec<String>>,
    loading: Cell<bool>,
    /// Items of the current level.
    items: RefCell<Vec<TreemapItem>>,
    /// Their rectangles as last drawn (hit-testing for hover/click/tooltip).
    placed: RefCell<Vec<PlacedRect>>,
    hovered: RefCell<Option<TreemapItem>>,
    header: NavHeader,
    total: gtk::Label,
    stack: gtk::Stack,
    area: gtk::DrawingArea,
    hint: gtk::Label,
    hover_row: gtk::Box,
    hover_dot: gtk::DrawingArea,
    hover_name: gtk::Label,
    hover_size: gtk::Label,
    hover_children_dot: gtk::Label,
    hover_children: gtk::Label,
}

/// Lets other code (the debug hooks) drive an open treemap.
pub struct Handle(Weak<Treemap>);

impl Handle {
    /// Focuses the folder at `path` ("a/b/c").
    pub fn drill(&self, path: &str) {
        if let Some(t) = self.0.upgrade() {
            t.drill.replace(path.split('/').filter(|s| !s.is_empty()).map(String::from).collect());
            refresh(&t);
        }
    }

    /// Highlights the rectangle called `name`, as if the pointer were on it.
    pub fn hover(&self, name: &str) {
        if let Some(t) = self.0.upgrade() {
            let item = t.items.borrow().iter().find(|i| i.name == name).cloned();
            set_hovered(&t, item);
        }
    }
}

pub fn present(parent: &impl IsA<gtk::Widget>, app: &Rc<AppState>, repo: &Repository, archive: &Archive) {
    open(parent, app, repo, archive);
}

pub fn open(parent: &impl IsA<gtk::Widget>, _app: &Rc<AppState>, repo: &Repository, archive: &Archive) -> Handle {
    let (dialog, root) = kit::bare_sheet(&archive.name, 880, 620);

    let header = nav_header(&archive.name, "", "");
    let total = kit::label("", &["heading", "numeric"]);
    header.trailing.append(&total);
    root.append(&header.root);
    root.append(&kit::divider());

    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_has_tooltip(true);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(
        &loading_page("Loading index…", "Cached to disk afterwards, just like the browser and history views."),
        Some("loading"),
    );
    stack.add_named(&area, Some("canvas"));
    stack.add_named(
        &kit::empty_state("view-grid-symbolic", "Nothing to show", "This folder has no measurable files."),
        Some("empty"),
    );
    stack.add_named(&kit::vbox(0), Some("blank"));
    root.append(&stack);
    root.append(&kit::divider());

    // ---------------------------------------------------------------- footer
    let footer = kit::hbox(0);
    footer.add_css_class("browser-footer");
    let hint = kit::label(
        "Click a rectangle to enter that folder. Items < 0.2% are grouped into 'other'.",
        &["caption", "secondary"],
    );
    let hover_row = kit::hbox(6);
    hover_row.set_visible(false);
    let hover_dot = gtk::DrawingArea::new();
    hover_dot.set_content_width(10);
    hover_dot.set_content_height(10);
    hover_dot.set_valign(gtk::Align::Center);
    let hover_name = kit::label("", &["caption", "mono"]);
    let hover_size = kit::label("", &["caption", "secondary", "numeric"]);
    let hover_children_dot = kit::label("·", &["tertiary"]);
    let hover_children = kit::label("", &["caption", "secondary"]);
    hover_row.append(&hover_dot);
    hover_row.append(&hover_name);
    hover_row.append(&kit::label("·", &["tertiary"]));
    hover_row.append(&hover_size);
    hover_row.append(&hover_children_dot);
    hover_row.append(&hover_children);
    footer.append(&hint);
    footer.append(&hover_row);
    footer.append(&kit::spacer());
    root.append(&footer);

    let t = Rc::new(Treemap {
        dialog: dialog.clone(),
        tree: RefCell::new(None),
        drill: RefCell::new(vec![]),
        loading: Cell::new(true),
        items: RefCell::new(vec![]),
        placed: RefCell::new(vec![]),
        hovered: RefCell::new(None),
        header,
        total,
        stack,
        area,
        hint,
        hover_row,
        hover_dot,
        hover_name,
        hover_size,
        hover_children_dot,
        hover_children,
    });

    t.header.close.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    t.header.back.connect_clicked(glib::clone!(
        #[weak]
        t,
        move |_| {
            t.drill.borrow_mut().pop();
            refresh(&t);
        }
    ));
    t.header.home.connect_clicked(glib::clone!(
        #[weak]
        t,
        move |_| {
            t.drill.borrow_mut().clear();
            refresh(&t);
        }
    ));

    t.area.set_draw_func(glib::clone!(
        #[weak]
        t,
        move |_, cr, width, height| draw(&t, cr, width as f64, height as f64)
    ));
    t.hover_dot.set_draw_func(glib::clone!(
        #[weak]
        t,
        move |_, cr, width, height| {
            if let Some(item) = t.hovered.borrow().as_ref() {
                let (r, g, b) = color_for(item);
                let (w, h) = (width as f64, height as f64);
                cr.set_source_rgb(r, g, b);
                cr.arc(w / 2.0, h / 2.0, w.min(h) / 2.0, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            }
        }
    ));

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(glib::clone!(
        #[weak]
        t,
        move |_, x, y| {
            let item = item_at(&t, x, y);
            set_hovered(&t, item);
        }
    ));
    motion.connect_leave(glib::clone!(
        #[weak]
        t,
        move |_| set_hovered(&t, None)
    ));
    t.area.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.connect_released(glib::clone!(
        #[weak]
        t,
        move |_, _, x, y| {
            let Some(item) = item_at(&t, x, y) else { return };
            let Some(tree) = t.tree.borrow().clone() else { return };
            // Only real folders with something inside can be entered.
            let Some(node) = item.node.filter(|_| !item.is_aggregated).map(|id| tree.get(id)) else { return };
            if node.children.is_empty() {
                return;
            }
            t.drill.borrow_mut().push(node.name.clone());
            refresh(&t);
        }
    ));
    t.area.add_controller(click);

    t.area.connect_query_tooltip(glib::clone!(
        #[weak]
        t,
        #[upgrade_or]
        false,
        move |_, x, y, _, tooltip| {
            match item_at(&t, x as f64, y as f64) {
                Some(item) => {
                    tooltip.set_text(Some(&format!("{} — {}", item.name, format::bytes_file(item.size as i64))));
                    true
                }
                None => false,
            }
        }
    ));

    // The controller lives as long as the sheet is open.
    let keep = RefCell::new(Some(t.clone()));
    dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });

    dialog.present(Some(parent));
    refresh(&t);
    load_tree(
        repo,
        archive,
        glib::clone!(
            #[weak]
            t,
            move |result: Result<Arc<ArchiveTree>, String>| {
                t.loading.set(false);
                match result {
                    Ok(tree) => {
                        t.tree.replace(Some(tree));
                        refresh(&t);
                    }
                    Err(e) => {
                        refresh(&t);
                        kit::alert(&t.dialog, "Error", &e);
                    }
                }
            }
        ),
    );
    Handle(Rc::downgrade(&t))
}

/// Recomputes the current level and brings header, canvas and footer in line.
fn refresh(t: &Rc<Treemap>) {
    let segments = t.drill.borrow().clone();
    t.header.set_path(&segments);
    set_hovered(t, None);
    let tree = t.tree.borrow().clone();
    let Some(tree) = tree else {
        t.total.set_text("");
        t.items.borrow_mut().clear();
        t.stack.set_visible_child_name(if t.loading.get() { "loading" } else { "blank" });
        return;
    };
    let current = tree.node_at(&segments);
    t.total.set_text(&format::bytes_file(tree.get(current).total_size as i64));
    let items = items_for(&tree, current, THRESHOLD);
    t.stack.set_visible_child_name(if items.is_empty() { "empty" } else { "canvas" });
    t.items.replace(items);
    t.placed.borrow_mut().clear();
    t.area.queue_draw();
}

fn item_at(t: &Treemap, x: f64, y: f64) -> Option<TreemapItem> {
    t.placed.borrow().iter().find(|p| p.rect.contains(x, y)).map(|p| p.item.clone())
}

fn set_hovered(t: &Treemap, item: Option<TreemapItem>) {
    if *t.hovered.borrow() == item {
        return;
    }
    t.hint.set_visible(item.is_none());
    t.hover_row.set_visible(item.is_some());
    if let Some(item) = &item {
        t.hover_name.set_text(&item.name);
        t.hover_size.set_text(&format::bytes_file(item.size as i64));
        let tree = t.tree.borrow();
        let folder = item.node.and_then(|id| tree.as_ref().map(|tr| tr.get(id))).filter(|n| n.is_directory());
        t.hover_children_dot.set_visible(folder.is_some());
        t.hover_children.set_visible(folder.is_some());
        if let Some(node) = folder {
            t.hover_children.set_text(&format!("{} children", node.children.len()));
        }
    }
    t.hovered.replace(item);
    t.hover_dot.queue_draw();
    t.area.queue_draw();
}

// ------------------------------------------------------------------ drawing

const NAME_FONT_SIZE: f64 = 11.5; // .caption.bold()
const SIZE_FONT_SIZE: f64 = 10.5; // .caption2

/// Shortens `text` with a middle ellipsis until it fits `max_width`.
fn fit_middle(cr: &cairo::Context, text: &str, max_width: f64) -> String {
    let width = |s: &str| cr.text_extents(s).map(|e| e.x_advance()).unwrap_or(f64::MAX);
    if width(text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let shortened = |keep: usize| -> String {
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let mut s: String = chars[..head].iter().collect();
        s.push('…');
        s.extend(&chars[chars.len() - tail..]);
        s
    };
    // Largest number of kept characters that still fits.
    let (mut low, mut high) = (0usize, chars.len().saturating_sub(1));
    while low < high {
        let mid = (low + high).div_ceil(2);
        if width(&shortened(mid)) <= max_width { low = mid } else { high = mid - 1 }
    }
    shortened(low)
}

fn draw(t: &Treemap, cr: &cairo::Context, width: f64, height: f64) {
    let placed = squarify(&t.items.borrow(), Rect { x: 0.0, y: 0.0, width, height });
    let hovered = t.hovered.borrow().as_ref().map(|i| i.id.clone());

    for p in &placed {
        let r = p.rect;
        let is_hovered = hovered.as_deref() == Some(p.item.id.as_str());
        let (red, green, blue) = color_for(&p.item);
        cr.set_source_rgb(red, green, blue);
        cr.rectangle(r.x, r.y, r.width, r.height);
        let _ = cr.fill();

        // Border drawn inside the rectangle (SwiftUI `strokeBorder`).
        let line = if is_hovered { 2.0 } else { 0.5 };
        if r.width > line && r.height > line {
            if is_hovered {
                cr.set_source_rgb(1.0, 1.0, 1.0);
            } else {
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
            }
            cr.set_line_width(line);
            cr.rectangle(r.x + line / 2.0, r.y + line / 2.0, r.width - line, r.height - line);
            let _ = cr.stroke();
        }

        if r.width > 60.0 && r.height > 26.0 {
            let _ = cr.save();
            cr.rectangle(r.x, r.y, r.width, r.height);
            cr.clip();
            let text_width = r.width - 12.0;
            cr.select_font_face("Inter", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            cr.set_font_size(NAME_FONT_SIZE);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.move_to(r.x + 6.0, r.y + 4.0 + NAME_FONT_SIZE);
            let _ = cr.show_text(&fit_middle(cr, &p.item.name, text_width));
            if r.height > 42.0 {
                cr.select_font_face("Inter", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
                cr.set_font_size(SIZE_FONT_SIZE);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
                cr.move_to(r.x + 6.0, r.y + 4.0 + NAME_FONT_SIZE + 4.0 + SIZE_FONT_SIZE);
                let _ = cr.show_text(&format::bytes_file(p.item.size as i64));
            }
            let _ = cr.restore();
        }
    }
    t.placed.replace(placed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, size: u64) -> TreemapItem {
        TreemapItem { id: name.into(), name: name.into(), size, is_aggregated: false, node: None }
    }

    #[test]
    fn squarify_fills_the_rectangle() {
        let items = [item("a", 6), item("b", 6), item("c", 4), item("d", 3), item("e", 2), item("f", 2), item("g", 1)];
        let placed = squarify(&items, Rect { x: 0.0, y: 0.0, width: 600.0, height: 400.0 });
        assert_eq!(placed.len(), items.len());
        let area: f64 = placed.iter().map(|p| p.rect.width * p.rect.height).sum();
        assert!((area - 240_000.0).abs() < 1e-6);
        // The classic example: the two biggest items share the first strip.
        assert!((placed[0].rect.width - 300.0).abs() < 1e-9);
        assert!((placed[0].rect.height - 200.0).abs() < 1e-9);
        for p in &placed {
            assert!(p.rect.x >= -1e-9 && p.rect.x + p.rect.width <= 600.0 + 1e-6);
            assert!(p.rect.y >= -1e-9 && p.rect.y + p.rect.height <= 400.0 + 1e-6);
        }
    }

    #[test]
    fn small_children_are_grouped() {
        use crate::core::models::ArchiveEntry;
        let e = |path: &str, size: u64| ArchiveEntry { kind: "-".into(), path: path.into(), size: Some(size), mtime: None };
        let tree = ArchiveTree::build(&[e("big", 10_000), e("tiny1", 1), e("tiny2", 2), e("empty", 0)]);
        let items = items_for(&tree, ArchiveTree::ROOT, THRESHOLD);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "big");
        assert_eq!(items[1].name, "other (2)");
        assert_eq!(items[1].size, 3);
        assert!(items[1].is_aggregated);
    }
}
