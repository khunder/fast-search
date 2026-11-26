//! A reusable dialog window: a titled, modal window over the main one,
//! made of content rows (text, label/value pairs, checkboxes, buttons)
//! and a button bar. It comes in two shapes: a plain one sized to its
//! content (confirmations, help sheets), and one with a list of pages down
//! the left side (the Options window).

use super::*;

/// One line of dialog content.
pub enum Row {
    Text(String),
    /// Secondary text, also used for section headings.
    Dim(String),
    /// A label and its value.
    Pair(&'static str, String),
    /// A checkbox; ticking it runs the action, and its state is whatever
    /// `App::checked` reports for that action.
    Check(&'static str, Action),
    /// A button inside the content; pressing it runs the action and
    /// leaves the dialog open.
    Button(&'static str, Action),
    /// An entry of a list box (consecutive ones form the box); choosing
    /// it runs the action, and the chosen one is whichever `App::checked`
    /// reports for its action.
    ListItem(String, Action),
    /// A one-line text field (one per dialog; its text is
    /// `App::dialog_input`), with a hint shown while it is empty. Enter
    /// in the field runs the action.
    Input(&'static str, Action),
    /// Half a line of space.
    Gap,
}

/// A button of the bar at the bottom: its label and what it does (`None`
/// just closes the dialog). Pressing one closes the dialog.
pub type Button = (&'static str, Option<Action>);

/// An entry of the page list: a page, or a group heading that opens its
/// first page.
pub struct NavItem {
    pub label: &'static str,
    /// Pages are indented under their group.
    pub child: bool,
    pub action: Action,
}

/// Something in the dialog the user can activate.
#[derive(Clone, Copy, PartialEq)]
enum Item {
    Nav(usize),
    Check(Action),
    RowButton(Action),
    Input(Action),
    Button(usize),
}

type Rect = (i32, i32, i32, i32);

/// Puts the texts of dialog rows into the current language.
fn localize(rows: Vec<Row>) -> Vec<Row> {
    rows.into_iter()
        .map(|row| match row {
            Row::Text(t) => Row::Text(tr(&t).to_string()),
            Row::Dim(t) => Row::Dim(tr(&t).to_string()),
            Row::Pair(label, value) => Row::Pair(tr(label), tr(&value).to_string()),
            Row::Check(label, action) => Row::Check(tr(label), action),
            Row::Button(label, action) => Row::Button(tr(label), action),
            Row::Input(hint, action) => Row::Input(tr(hint), action),
            other => other,
        })
        .collect()
}

fn localize_nav(nav: Vec<NavItem>) -> Vec<NavItem> {
    nav.into_iter().map(|item| NavItem { label: tr(item.label), ..item }).collect()
}

pub struct Dialog {
    pub cv: Canvas,
    rows: Vec<Row>,
    buttons: Vec<Button>,
    /// The last button is the main one; draw it as dangerous (red).
    danger: bool,
    /// A reference sheet: pair labels are examples, shown prominently.
    pub sheet: bool,
    /// The page list; empty for a plain dialog.
    nav: Vec<NavItem>,
    nav_selected: usize,
    /// Title of the page shown beside the list.
    heading: String,
    /// Index into the activatable items.
    focus: usize,
    hover: Option<usize>,
    /// Height of the title bar we draw ourselves, on a desktop that gives
    /// the window none (0 otherwise), and what the window then shows: the
    /// bar above the content.
    bar: i32,
    title: String,
    framed: Canvas,
    close_hover: bool,
}

impl Dialog {
    /// What the dialog window shows: its content, under our own title
    /// bar if we draw one.
    pub fn shown(&self) -> &Canvas {
        if self.bar > 0 { &self.framed } else { &self.cv }
    }
}

/// Where everything in a dialog goes.
struct Layout {
    w: i32,
    h: i32,
    /// Left edge and width of the content rows.
    content: (i32, i32),
    /// Top of the first content row.
    top: i32,
    /// The page list box and the page panel, when there is a page list.
    panes: Option<(Rect, Rect)>,
    items: Vec<(i32, i32, i32, i32, Item)>,
}

impl App {
    /// Opens a plain dialog. `focus_button` is the button Enter triggers at first.
    pub fn open_dialog(
        &mut self,
        title: &str,
        rows: Vec<Row>,
        buttons: Vec<Button>,
        focus_button: usize,
        danger: bool,
    ) -> io::Result<()> {
        self.open_dialog_with(title, rows, buttons, focus_button, danger, Vec::new(), 0, "")
    }

    /// Opens a plain dialog whose text field starts with `text` in it and
    /// has the focus. The last button is what the dialog is for.
    pub fn open_input_dialog(&mut self, title: &str, rows: Vec<Row>, buttons: Vec<Button>, text: String) {
        let last = buttons.len().saturating_sub(1);
        if let Err(e) = self.open_dialog(title, rows, buttons, last, false) {
            self.status = e.to_string();
            return;
        }
        self.dialog_input = text;
        if let Some(d) = self.dialog.take() {
            let field = self.dialog_layout(&d).items.iter().position(|item| matches!(item.4, Item::Input(_)));
            self.dialog = Some(Dialog { focus: field.unwrap_or(d.focus), ..d });
        }
        let _ = self.render_dialog();
    }

    /// Opens a dialog with a page list down its left side, or, if such a
    /// dialog is already open, shows another page in it.
    pub fn open_paged_dialog(
        &mut self,
        title: &str,
        nav: Vec<NavItem>,
        nav_selected: usize,
        heading: &str,
        rows: Vec<Row>,
    ) -> io::Result<()> {
        let (rows, nav) = (localize(rows), localize_nav(nav));
        let heading = tr(heading);
        if let Some(dialog) = self.dialog.as_mut().filter(|d| !d.nav.is_empty()) {
            // Same window, new page.
            // Redrawing the same page (a value changed) keeps the focus.
            if dialog.nav_selected != nav_selected {
                dialog.focus = nav_selected;
                dialog.hover = None;
            }
            dialog.rows = rows;
            dialog.nav = nav; // its labels follow a language change
            dialog.nav_selected = nav_selected;
            dialog.heading = heading.to_string();
            return self.render_dialog();
        }
        // OK saves and closes, Cancel closes, Apply saves and stays.
        let buttons = vec![("OK", Some(Action::OptionsOk)), ("Cancel", None), ("Apply", Some(Action::OptionsApply))];
        self.open_dialog_with(title, rows, buttons, 0, false, nav, nav_selected, heading)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_dialog_with(
        &mut self,
        title: &str,
        rows: Vec<Row>,
        buttons: Vec<Button>,
        focus_button: usize,
        danger: bool,
        nav: Vec<NavItem>,
        nav_selected: usize,
        heading: &str,
    ) -> io::Result<()> {
        self.close_dialog();
        self.menu = None;
        let title = tr(title);
        let mut dialog = Dialog {
            cv: Canvas::new(),
            rows: localize(rows),
            buttons: buttons.into_iter().map(|(label, action)| (tr(label), action)).collect(),
            danger,
            sheet: false,
            nav: localize_nav(nav),
            nav_selected,
            heading: tr(heading).to_string(),
            focus: 0,
            hover: None,
            bar: if self.b.dialog_needs_title_bar() { self.row_h() + 10 * self.u } else { 0 },
            title: title.to_string(),
            framed: Canvas::new(),
            close_hover: false,
        };
        let layout = self.dialog_layout(&dialog);
        let (w, h) = (layout.w, layout.h);
        dialog.focus = layout.items.iter().position(|i| i.4 == Item::Button(focus_button)).unwrap_or(0);
        dialog.cv.resize(w as usize, h as usize);

        self.b.open_dialog(w, h + dialog.bar, title)?;
        self.dialog = Some(dialog);
        self.render_dialog()
    }

    pub fn close_dialog(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            drop(dialog);
            self.b.close_dialog();
            // The next time the Options window opens, its pages start fresh.
            self.options_page = None;
            self.index_settings_loaded = false;
            self.dialog_input.clear();
        }
    }

    /// True while the dialog with a page list is open.
    pub fn paged_dialog_open(&self) -> bool {
        self.dialog.as_ref().is_some_and(|d| !d.nav.is_empty())
    }

    /// Width of the label column of `Row::Pair` rows: fits the widest label.
    fn dialog_label_w(&mut self, d: &Dialog) -> i32 {
        let widest = d.rows.iter().map(|r| if let Row::Pair(label, _) = r { self.text.width(label) } else { 0 }).max();
        (widest.unwrap_or(0) + 22 * self.u).max(110 * self.u)
    }

    fn dialog_layout(&mut self, d: &Dialog) -> Layout {
        let (u, row_h) = (self.u, self.row_h());
        let pad = 18 * u;
        let paged = !d.nav.is_empty();
        let button_h = row_h + 8 * u;
        let button_w: Vec<i32> = d.buttons.iter().map(|(label, _)| self.text.width(label) + 36 * u).collect();
        let buttons_total = button_w.iter().sum::<i32>() + 10 * u * (button_w.len() as i32 - 1).max(0);
        let mut items = Vec::new();

        // A paged dialog has one fixed size, so pages can change in place;
        // a plain one fits its content.
        let (w, h, content, top, panes) = if paged {
            let (w, h) = (860 * u, 620 * u);
            let bottom = h - pad - button_h - 14 * u;
            let list = (pad, pad, 190 * u, bottom - pad);
            let panel = (pad + 200 * u, pad, w - 2 * pad - 200 * u, bottom - pad);
            let nav_h = self.text.height + 6 * u;
            for (i, item) in d.nav.iter().enumerate() {
                let indent = if item.child { 18 * u } else { 4 * u };
                let iw = self.text.width(item.label) + 10 * u;
                items.push((list.0 + indent, list.1 + 6 * u + i as i32 * nav_h, iw, nav_h, Item::Nav(i)));
            }
            let content = (panel.0 + 16 * u, panel.2 - 32 * u);
            (w, h, content, panel.1 + row_h + 22 * u, Some((list, panel)))
        } else {
            let label_w = self.dialog_label_w(d);
            let widest = d
                .rows
                .iter()
                .map(|row| match row {
                    Row::Text(t) | Row::Dim(t) => self.text.width(t),
                    Row::Pair(_, value) => label_w + self.text.width(value),
                    Row::Check(label, _) => 28 * u + self.text.width(label),
                    Row::Button(label, _) => 30 * u + self.text.width(label),
                    Row::ListItem(label, _) => 20 * u + self.text.width(label),
                    Row::Input(..) => 420 * u,
                    Row::Gap => 0,
                })
                .max()
                .unwrap_or(0);
            let w = (widest.max(buttons_total) + 2 * pad).clamp(380 * u, 980 * u);
            (w, 0, (pad, w - 2 * pad), pad, None)
        };

        let mut y = top;
        for row in &d.rows {
            match row {
                Row::Check(_, action) => items.push((content.0 - 4 * u, y, content.1 + 8 * u, row_h, Item::Check(*action))),
                Row::ListItem(_, action) => items.push((content.0, y, content.1, row_h, Item::Check(*action))),
                Row::Input(_, action) => items.push((content.0, y, content.1, row_h, Item::Input(*action))),
                Row::Button(label, action) => {
                    let bw = self.text.width(label) + 28 * u;
                    items.push((content.0, y + u, bw, row_h - 2 * u, Item::RowButton(*action)));
                }
                _ => {}
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }
        let h = if paged { h } else { y + 14 * u + button_h + pad };
        let by = h - pad - button_h;
        let mut x = w - pad - buttons_total;
        for (i, bw) in button_w.into_iter().enumerate() {
            items.push((x, by, bw, button_h, Item::Button(i)));
            x += bw + 10 * u;
        }
        Layout { w, h, content, top, panes, items }
    }

    pub fn render_dialog(&mut self) -> io::Result<()> {
        let Some(mut d) = self.dialog.take() else { return Ok(()) };
        let (u, row_h) = (self.u, self.row_h());
        let layout = self.dialog_layout(&d);
        let (w, h) = (layout.w, layout.h);
        let label_w = self.dialog_label_w(&d);
        let line = u.max(1);
        let item_at = |item: Item| layout.items.iter().position(|i| i.4 == item);
        d.cv.fill(0, 0, w, h, if layout.panes.is_some() { PANEL } else { WHITE });

        if let Some((list, panel)) = layout.panes {
            // The page list: a white box, the open page highlighted.
            d.cv.fill(list.0, list.1, list.2, list.3, WHITE);
            d.cv.frame(list.0, list.1, list.2, list.3, line, 0x9a9a9a);
            for (index, &(x, y, iw, ih, item)) in layout.items.iter().enumerate() {
                let Item::Nav(i) = item else { continue };
                let selected = i == d.nav_selected;
                if selected {
                    d.cv.fill(x - 2 * u, y, iw + 2 * u, ih, SELECT);
                } else if d.hover == Some(index) {
                    d.cv.fill(x - 2 * u, y, iw + 2 * u, ih, HOVER);
                }
                if d.nav[i].child {
                    // Tree lines: a trunk running down past every page of
                    // the group (stopping at the last one) and a branch
                    // out to each page.
                    let last_of_group = d.nav.get(i + 1).is_none_or(|next| !next.child);
                    let trunk = if last_of_group { ih / 2 + line } else { ih };
                    d.cv.fill(x - 10 * u, y, line, trunk, 0xa0a0a0);
                    d.cv.fill(x - 10 * u, y + ih / 2, 7 * u, line, 0xa0a0a0);
                }
                self.text.draw(&mut d.cv, x + 3 * u, y + 3 * u, d.nav[i].label, iw, if selected { ON_ACCENT } else { TEXT });
                if d.focus == index && !selected {
                    d.cv.frame(x - 2 * u, y, iw + 2 * u, ih, line, 0x5b9bea);
                }
            }
            // The page: a tab with its title on a bordered panel.
            let tab_w = self.text.width(&d.heading) + 24 * u;
            d.cv.fill(panel.0, panel.1 + row_h + 6 * u, panel.2, panel.3 - row_h - 6 * u, 0xf9f9f9);
            d.cv.frame(panel.0, panel.1 + row_h + 6 * u, panel.2, panel.3 - row_h - 6 * u, line, LINE);
            d.cv.fill(panel.0, panel.1, tab_w, row_h + 6 * u + line, 0xf9f9f9);
            d.cv.fill(panel.0, panel.1, tab_w, line, LINE);
            d.cv.fill(panel.0, panel.1, line, row_h + 6 * u, LINE);
            d.cv.fill(panel.0 + tab_w - line, panel.1, line, row_h + 6 * u, LINE);
            self.text.draw(&mut d.cv, panel.0 + 12 * u, panel.1 + 6 * u, &d.heading, tab_w, TEXT);
        }

        let (cx, cw) = layout.content;
        let mut y = layout.top;
        for row in &d.rows {
            let ty = y + 3 * u;
            match row {
                Row::Text(t) => self.text.draw(&mut d.cv, cx, ty, t, cw, TEXT),
                Row::Dim(t) => self.text.draw(&mut d.cv, cx, ty, t, cw, DIM),
                Row::Pair(label, value) => {
                    // On sheets of examples the label is what you type: dark, value dim.
                    let (label_ink, value_ink) = if d.sheet { (TEXT, DIM) } else { (DIM, TEXT) };
                    self.text.draw(&mut d.cv, cx, ty, label, label_w - 10 * u, label_ink);
                    self.text.draw(&mut d.cv, cx + label_w, ty, value, cw - label_w, value_ink);
                }
                Row::Check(label, action) => {
                    let index = item_at(Item::Check(*action));
                    if index.is_some() && (d.hover == index || Some(d.focus) == index) {
                        d.cv.fill(cx - 4 * u, y, cw + 8 * u, row_h, if d.hover == index { HOVER } else { 0xeef4fc });
                    }
                    let (box_size, by) = (14 * u, y + (row_h - 14 * u) / 2);
                    let on = self.checked(*action);
                    d.cv.fill(cx, by, box_size, box_size, if on { SELECT } else { WHITE });
                    d.cv.frame(cx, by, box_size, box_size, line, if on { SELECT } else { 0x8a8a8a });
                    if on {
                        // A tick: a short stroke down-right, a long one up-right.
                        for i in 0..4 * u {
                            d.cv.fill(cx + 3 * u + i, by + 7 * u + i, 2 * line, 2 * line, ON_ACCENT);
                        }
                        for i in 0..6 * u {
                            d.cv.fill(cx + 6 * u + i, by + 10 * u - i, 2 * line, 2 * line, ON_ACCENT);
                        }
                    }
                    self.text.draw(&mut d.cv, cx + 24 * u, ty, label, cw - 24 * u, TEXT);
                }
                Row::Button(label, action) => {
                    let Some(index) = item_at(Item::RowButton(*action)) else { continue };
                    let (x, by, bw, bh, _) = layout.items[index];
                    let fill = if d.hover == Some(index) { HOVER } else { WHITE };
                    let edge = if d.focus == index { SELECT } else { LINE };
                    d.cv.round_frame(x, by, bw, bh, 4 * u, line, edge, fill);
                    self.text.draw(&mut d.cv, x + 14 * u, by + (bh - self.text.height) / 2, label, bw, TEXT);
                }
                Row::ListItem(label, action) => {
                    // One line of a white list box; the chosen line is filled.
                    let index = item_at(Item::Check(*action));
                    let chosen = self.checked(*action);
                    let fill = if chosen { SELECT } else if d.hover == index { HOVER } else { WHITE };
                    d.cv.fill(cx, y, cw, row_h, fill);
                    d.cv.fill(cx, y, line, row_h, 0x9a9a9a);
                    d.cv.fill(cx + cw - line, y, line, row_h, 0x9a9a9a);
                    self.text.draw(&mut d.cv, cx + 8 * u, ty, label, cw - 16 * u, if chosen { ON_ACCENT } else { TEXT });
                }
                Row::Input(hint, action) => {
                    let focused = item_at(Item::Input(*action)) == Some(d.focus);
                    d.cv.fill(cx, y, cw, row_h, WHITE);
                    d.cv.frame(cx, y, cw, row_h, line, if focused { 0x5b9bea } else { 0x9a9a9a });
                    // A long text shows its end, where typing happens.
                    let room = cw - 16 * u;
                    let mut shown = self.dialog_input.as_str();
                    while self.text.width(shown) > room {
                        shown = &shown[shown.chars().next().map_or(1, char::len_utf8)..];
                    }
                    if self.dialog_input.is_empty() {
                        self.text.draw(&mut d.cv, cx + 8 * u, ty, hint, room, 0x9a9a9a);
                    } else {
                        self.text.draw(&mut d.cv, cx + 8 * u, ty, shown, room + 4 * u, TEXT);
                    }
                    if focused {
                        let caret = cx + 8 * u + if self.dialog_input.is_empty() { 0 } else { self.text.width(shown) + u };
                        d.cv.fill(caret, ty, line, self.text.height, TEXT);
                    }
                }
                Row::Gap => {}
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }
        // Close each list box with a top and a bottom edge.
        let mut y = layout.top;
        for (i, row) in d.rows.iter().enumerate() {
            let is_item = |r: Option<&Row>| matches!(r, Some(Row::ListItem(..)));
            if is_item(Some(row)) {
                if i == 0 || !is_item(d.rows.get(i - 1)) {
                    d.cv.fill(cx, y, cw, line, 0x9a9a9a);
                }
                if !is_item(d.rows.get(i + 1)) {
                    d.cv.fill(cx, y + row_h - line, cw, line, 0x9a9a9a);
                }
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }

        let last = d.buttons.len().saturating_sub(1);
        for (index, &(x, by, bw, bh, item)) in layout.items.iter().enumerate() {
            let Item::Button(i) = item else { continue };
            let label = d.buttons[i].0;
            // The last button is the main action, filled; the rest are plain.
            let main = i == last && d.buttons[i].1.is_some();
            let (fill, ink) = match (main, d.danger, d.hover == Some(index)) {
                (true, true, hovered) => (if hovered { 0xb3261e } else { 0xd93025 }, ON_ACCENT),
                (true, false, _) => (SELECT, ON_ACCENT),
                (false, _, true) => (HOVER, TEXT),
                (false, _, false) => (WHITE, TEXT),
            };
            // Windows 11 buttons: rounded; the focus ring follows the shape.
            if d.focus == index {
                d.cv.round_frame(x - 3 * u, by - 3 * u, bw + 6 * u, bh + 6 * u, 7 * u, 2 * line, TEXT, palette::BASE);
            }
            d.cv.round_frame(x, by, bw, bh, 4 * u, line, if main { fill } else { LINE }, fill);
            let lx = x + (bw - self.text.width(label)) / 2;
            self.text.draw(&mut d.cv, lx, by + (bh - self.text.height) / 2, label, bw, ink);
        }

        if d.bar == 0 {
            self.b.present(Win::Dialog, &d.cv)?;
        } else {
            // Our own title bar: the name, a close button, and a thin
            // frame so the window stands out from what is behind it.
            let (bar, bw) = (d.bar, 44 * u);
            d.framed.resize(d.cv.w, d.cv.h + bar as usize);
            d.framed.px[bar as usize * d.cv.w * 4..].copy_from_slice(&d.cv.px);
            d.framed.fill(0, 0, w, bar, 0xe6e6e6);
            d.framed.fill(0, bar - 1, w, 1, LINE);
            self.text.draw(&mut d.framed, 14 * u, (bar - self.text.height) / 2, &d.title, w - bw - 20 * u, TEXT);
            if d.close_hover {
                d.framed.fill(w - bw, 0, bw, bar - 1, 0xd93025);
            }
            let (gx, gy, g, ink) = (w - bw + (bw - 10 * u) / 2, (bar - 10 * u) / 2, 10 * u, if d.close_hover { ON_ACCENT } else { 0x3a3a3a });
            for i in 0..g {
                d.framed.fill(gx + i, gy + i, line, line, ink);
                d.framed.fill(gx + g - 1 - i, gy + i, line, line, ink);
            }
            d.framed.frame(0, 0, w, h + bar, line, 0x9a9a9a);
            self.b.present(Win::Dialog, &d.framed)?;
        }
        self.dialog = Some(d);
        Ok(())
    }

    /// Runs the item at `index`.
    fn dialog_activate(&mut self, index: usize, time: u32) {
        let Some(d) = self.dialog.take() else { return };
        let item = self.dialog_layout(&d).items.get(index).map(|i| i.4);
        let stay = match item {
            // These act and leave the dialog open.
            Some(Item::Check(action) | Item::RowButton(action) | Item::Input(action)) => Some(action),
            Some(Item::Nav(i)) => Some(d.nav[i].action),
            _ => None,
        };
        let button = match item {
            Some(Item::Button(i)) => Some(d.buttons[i].1),
            _ => None,
        };
        self.dialog = Some(d);
        if let Some(action) = stay {
            self.act(action, time);
            self.refresh_option_states();
            // A page of the Options window is rebuilt: what it shows may
            // depend on what was just changed.
            if self.paged_dialog_open() {
                let _ = self.open_options(self.options_page.unwrap_or(0));
            }
            return;
        }
        let Some(action) = button else { return };
        if self.paged_dialog_open() {
            // The Options window: act while its pages still hold the
            // edits, then close unless this was Apply.
            if let Some(action) = action {
                self.act(action, time);
            }
            if action == Some(Action::OptionsApply) {
                let _ = self.open_options(self.options_page.unwrap_or(0));
            } else {
                self.close_dialog();
            }
            return;
        }
        // Any other dialog: a button closes it, then does its action,
        // which may still want what was typed in the dialog's text field.
        let typed = std::mem::take(&mut self.dialog_input);
        self.close_dialog();
        if let Some(action) = action {
            self.dialog_input = typed;
            self.act(action, time);
            self.dialog_input.clear();
        }
    }

    /// Handles an event addressed to the dialog window.
    pub fn dialog_event(&mut self, event: &Event) -> io::Result<()> {
        let Some(d) = self.dialog.take() else { return Ok(()) };
        let items = self.dialog_layout(&d).items;
        let focus = d.focus;
        let (bar, close_x, close_hover) = (d.bar, d.cv.w as i32 - 44 * self.u, d.close_hover);
        self.dialog = Some(d);
        // The title bar we draw ourselves: drag to move, X to close.
        let moved;
        let event = match *event {
            Event::Motion { win, x, y, mods } if bar > 0 => {
                let on_close = y < bar && x >= close_x;
                if on_close != close_hover {
                    self.dialog.as_mut().unwrap().close_hover = on_close;
                    self.render_dialog()?;
                }
                moved = Event::Motion { win, x, y: y - bar, mods };
                &moved
            }
            Event::Press { win, button, x, y, time, mods } if bar > 0 => {
                if y < bar {
                    match (button, x >= close_x) {
                        (1, true) => self.close_dialog(),
                        (1, false) => self.b.start_move(),
                        _ => {}
                    }
                    return Ok(());
                }
                moved = Event::Press { win, button, x, y: y - bar, time, mods };
                &moved
            }
            _ => event,
        };
        let count = items.len().max(1);
        let hit = |x: i32, y: i32| items.iter().position(|&(ix, iy, iw, ih, _)| x >= ix && x < ix + iw && y >= iy && y < iy + ih);
        match *event {
            Event::Close(_) => {
                self.close_dialog();
                return Ok(());
            }
            Event::Motion { x, y, .. } => {
                let hover = hit(x, y);
                if let Some(d) = self.dialog.as_mut() {
                    if d.hover == hover {
                        return Ok(());
                    }
                    d.hover = hover;
                }
            }
            Event::Press { button: 1, x, y, time, .. } => {
                if let Some(index) = hit(x, y) {
                    self.dialog.as_mut().unwrap().focus = index;
                    self.dialog_activate(index, time);
                }
            }
            Event::Key { key, mods, .. } => {
                let set_focus = |app: &mut App, f: usize| app.dialog.as_mut().unwrap().focus = f;
                // With the text field focused, keys type into it.
                let typing = matches!(items.get(focus), Some((.., Item::Input(_))));
                let shortcut_field = matches!(items.get(focus), Some((.., Item::Input(Action::SetShortcut))));
                match key {
                    // The shortcut field takes the keys pressed, such as
                    // Alt+Q, and sets them at once.
                    _ if shortcut_field && (mods.ctrl || mods.alt) && pressed_keys(key, mods).is_some() => {
                        self.dialog_input = pressed_keys(key, mods).unwrap_or_default();
                        self.act(Action::SetShortcut, 0);
                        self.render_dialog()?;
                        return Ok(());
                    }
                    Key::Char(ch) if typing && mods.ctrl => {
                        if ch.eq_ignore_ascii_case(&'v') {
                            self.b.request_paste(); // the text arrives as an event
                        }
                        return Ok(());
                    }
                    Key::Char(ch) if typing && !ch.is_control() => self.dialog_input.push(ch),
                    Key::Sym(K_BACKSPACE) if typing => drop(self.dialog_input.pop()),
                    Key::Sym(K_ESCAPE) => {
                        self.close_dialog();
                        return Ok(());
                    }
                    Key::Sym(K_RETURN | K_KP_ENTER) | Key::Char(' ') => self.dialog_activate(focus, 0),
                    Key::Sym(K_TAB) if mods.shift => set_focus(self, (focus + count - 1) % count),
                    Key::Sym(K_BACK_TAB | K_LEFT | K_UP) => set_focus(self, (focus + count - 1) % count),
                    Key::Sym(K_TAB | K_RIGHT | K_DOWN) => set_focus(self, (focus + 1) % count),
                    _ => return Ok(()),
                }
            }
            _ => {} // Redraw
        }
        self.render_dialog()
    }
}

/// A key pressed with its modifiers, as people write it ("Ctrl+Alt+F",
/// "Alt+Q", "Ctrl+F12"), for the shortcut field; None for a key a
/// shortcut cannot use.
fn pressed_keys(key: Key, mods: Mods) -> Option<String> {
    let name = match key {
        Key::Char(' ') => "Space".to_string(),
        Key::Char(c) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase().to_string(),
        // F1 to F12 follow each other as key syms.
        Key::Sym(sym) if (K_F1..K_F1 + 12).contains(&sym) => format!("F{}", sym - K_F1 + 1),
        _ => return None,
    };
    let held = [(mods.ctrl, "Ctrl+"), (mods.alt, "Alt+"), (mods.shift, "Shift+")];
    Some(held.iter().filter(|(on, _)| *on).map(|(_, m)| *m).collect::<String>() + &name)
}
