use crate::config::Config;
use crate::config::TextStyle;
use crate::font::{FontConfiguration, FontSystemSelection, GlyphInfo};
use crate::frontend::guicommon::clipboard::SystemClipboard;
use crate::frontend::guicommon::host::{KeyAssignment, KeyMap};
use crate::frontend::guicommon::window::SpawnTabDomain;
use crate::frontend::{front_end, gui_executor};
use crate::mux::renderable::Renderable;
use crate::mux::tab::{Tab, TabId};
use crate::mux::window::WindowId as MuxWindowId;
use crate::mux::Mux;
use ::window::bitmaps::atlas::{Atlas, OutOfTextureSpace, Sprite, SpriteSlice};
use ::window::bitmaps::{Image, ImageTexture};
use ::window::*;
use failure::Fallible;
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use term::color::ColorPalette;
use term::{CursorPosition, Line, Underline};
use termwiz::color::RgbColor;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GlyphKey {
    font_idx: usize,
    glyph_pos: u32,
    style: TextStyle,
}

/// Caches a rendered glyph.
/// The image data may be None for whitespace glyphs.
struct CachedGlyph {
    has_color: bool,
    x_offset: f64,
    y_offset: f64,
    bearing_x: f64,
    bearing_y: f64,
    texture: Option<Sprite<ImageTexture>>,
    scale: f64,
}

pub struct TermWindow {
    window: Option<Window>,
    fonts: Rc<FontConfiguration>,
    _config: Arc<Config>,
    cell_size: Size,
    dimensions: Dimensions,
    mux_window_id: MuxWindowId,
    descender: f64,
    descender_row: isize,
    descender_plus_one: isize,
    descender_plus_two: isize,
    strike_row: isize,
    glyph_cache: RefCell<HashMap<GlyphKey, Rc<CachedGlyph>>>,
    atlas: RefCell<Atlas<ImageTexture>>,
    clipboard: Arc<dyn term::Clipboard>,
    keys: KeyMap,
}

struct Host<'a> {
    writer: &'a mut dyn std::io::Write,
    context: &'a dyn WindowOps,
    clipboard: &'a Arc<dyn term::Clipboard>,
}

impl<'a> term::TerminalHost for Host<'a> {
    fn writer(&mut self) -> &mut dyn std::io::Write {
        self.writer
    }

    fn get_clipboard(&mut self) -> Fallible<Arc<dyn term::Clipboard>> {
        Ok(Arc::clone(self.clipboard))
    }

    fn set_title(&mut self, title: &str) {
        self.context.set_title(title);
    }

    fn click_link(&mut self, link: &Arc<term::cell::Hyperlink>) {
        log::error!("clicking {}", link.uri());
        if let Err(err) = open::that(link.uri()) {
            log::error!("failed to open {}: {:?}", link.uri(), err);
        }
    }
}

impl WindowCallbacks for TermWindow {
    fn created(&mut self, window: &Window) {
        self.window.replace(window.clone());
    }

    fn can_close(&mut self) -> bool {
        // can_close triggers the current tab to be closed.
        // If we have no tabs left then we can close the whole window.
        // If we're in a weird state, then we allow the window to close too.
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return true,
        };
        mux.remove_tab(tab.tab_id());
        if let Some(mut win) = mux.get_window_mut(self.mux_window_id) {
            win.remove_by_id(tab.tab_id());
            return win.is_empty();
        };
        true
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn mouse_event(&mut self, event: &MouseEvent, context: &dyn WindowOps) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        use ::term::input::MouseButton as TMB;
        use ::term::input::MouseEventKind as TMEK;
        use ::window::MouseButtons as WMB;
        use ::window::MouseEventKind as WMEK;
        tab.mouse_event(
            term::MouseEvent {
                kind: match event.kind {
                    WMEK::Move => TMEK::Move,
                    WMEK::VertWheel(_)
                    | WMEK::HorzWheel(_)
                    | WMEK::DoubleClick(_)
                    | WMEK::Press(_) => TMEK::Press,
                    WMEK::Release(_) => TMEK::Release,
                },
                button: match event.kind {
                    WMEK::Release(ref press)
                    | WMEK::Press(ref press)
                    | WMEK::DoubleClick(ref press) => match press {
                        MousePress::Left => TMB::Left,
                        MousePress::Middle => TMB::Middle,
                        MousePress::Right => TMB::Right,
                    },
                    WMEK::Move => {
                        if event.mouse_buttons == WMB::LEFT {
                            TMB::Left
                        } else if event.mouse_buttons == WMB::RIGHT {
                            TMB::Right
                        } else if event.mouse_buttons == WMB::MIDDLE {
                            TMB::Middle
                        } else {
                            TMB::None
                        }
                    }
                    WMEK::VertWheel(amount) => {
                        if amount > 0 {
                            TMB::WheelUp(amount as usize)
                        } else {
                            TMB::WheelDown((-amount) as usize)
                        }
                    }
                    WMEK::HorzWheel(_) => TMB::None,
                },
                x: (event.x as isize / self.cell_size.width) as usize,
                y: (event.y as isize / self.cell_size.height) as i64,
                modifiers: window_mods_to_termwiz_mods(event.modifiers),
            },
            &mut Host {
                writer: &mut *tab.writer(),
                context,
                clipboard: &self.clipboard,
            },
        )
        .ok();

        match event.kind {
            WMEK::Move => {}
            _ => context.invalidate(),
        }

        // When hovering over a hyperlink, show an appropriate
        // mouse cursor to give the cue that it is clickable
        context.set_cursor(Some(if tab.renderer().current_highlight().is_some() {
            MouseCursor::Hand
        } else {
            MouseCursor::Text
        }));
    }

    fn resize(&mut self, dimensions: Dimensions) {
        self.scaling_changed(dimensions, self.fonts.get_font_scale());
    }

    fn key_event(&mut self, key: &KeyEvent, _context: &dyn WindowOps) -> bool {
        if !key.key_is_down {
            return false;
        }

        let mux = Mux::get().unwrap();
        if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
            let modifiers = window_mods_to_termwiz_mods(key.modifiers);

            use ::termwiz::input::KeyCode as KC;
            use ::window::KeyCode as WK;

            let key_down = match key.key {
                WK::Char(c) => Some(KC::Char(c)),
                WK::Composed(ref s) => {
                    tab.writer().write_all(s.as_bytes()).ok();
                    return true;
                }
                WK::Function(f) => Some(KC::Function(f)),
                WK::LeftArrow => Some(KC::LeftArrow),
                WK::RightArrow => Some(KC::RightArrow),
                WK::UpArrow => Some(KC::UpArrow),
                WK::DownArrow => Some(KC::DownArrow),
                WK::Home => Some(KC::Home),
                WK::End => Some(KC::End),
                WK::PageUp => Some(KC::PageUp),
                WK::PageDown => Some(KC::PageDown),
                // TODO: more keys (eg: numpad!)
                _ => None,
            };

            if let Some(key) = key_down {
                if let Some(assignment) = self.keys.lookup(key, modifiers) {
                    self.perform_key_assignment(&tab, &assignment).ok();
                    return true;
                } else if tab.key_down(key, modifiers).is_ok() {
                    return true;
                }
            }
        }

        false
    }

    fn paint(&mut self, ctx: &mut dyn PaintContext) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => {
                ctx.clear(Color::rgb(0, 0, 0));
                return;
            }
        };
        let start = std::time::Instant::now();
        if let Err(err) = self.paint_tab(&tab, ctx) {
            if let Some(&OutOfTextureSpace { size }) = err.downcast_ref::<OutOfTextureSpace>() {
                log::error!("out of texture space, allocating {}", size);
                match self.recreate_texture_atlas(size) {
                    Ok(_) => {
                        tab.renderer().make_all_lines_dirty();
                        // Recursively initiate a new paint
                        return self.paint(ctx);
                    }
                    Err(err) => log::error!("failed recreate atlas: {}", err),
                };
            }
            log::error!("paint failed: {}", err);
        }
        log::error!("paint_tab elapsed={:?}", start.elapsed());
        self.update_title();
    }
}

impl TermWindow {
    pub fn new_window(
        config: &Arc<Config>,
        fontconfig: &Rc<FontConfiguration>,
        tab: &Rc<dyn Tab>,
        mux_window_id: MuxWindowId,
    ) -> Fallible<()> {
        log::error!(
            "TermWindow::new_window called with mux_window_id {}",
            mux_window_id
        );
        let (physical_rows, physical_cols) = tab.renderer().physical_dimensions();

        let metrics = fontconfig.default_font_metrics()?;
        let (cell_height, cell_width) = (
            metrics.cell_height.ceil() as usize,
            metrics.cell_width.ceil() as usize,
        );

        let width = cell_width * physical_cols;
        let height = cell_height * physical_rows;

        let surface = Rc::new(ImageTexture::new(4096, 4096));
        let atlas = RefCell::new(Atlas::new(&surface)?);

        let descender_row = (cell_height as f64 + metrics.descender) as isize;
        let descender_plus_one = (1 + descender_row).min(cell_height as isize - 1);
        let descender_plus_two = (2 + descender_row).min(cell_height as isize - 1);
        let strike_row = descender_row / 2;

        let window = Window::new_window(
            "wezterm",
            "wezterm",
            width,
            height,
            Box::new(Self {
                window: None,
                cell_size: Size::new(cell_width as isize, cell_height as isize),
                mux_window_id,
                _config: Arc::clone(config),
                fonts: Rc::clone(fontconfig),
                descender: metrics.descender,
                descender_row,
                descender_plus_one,
                descender_plus_two,
                strike_row,
                dimensions: Dimensions {
                    pixel_width: width,
                    pixel_height: height,
                    // This is the default dpi; we'll get a resize
                    // event to inform us of the true dpi if it is
                    // different from this value
                    dpi: 96,
                },
                glyph_cache: RefCell::new(HashMap::new()),
                atlas,
                clipboard: Arc::new(SystemClipboard::new()),
                keys: KeyMap::new(),
            }),
        )?;

        let cloned_window = window.clone();

        Connection::get().unwrap().schedule_timer(
            std::time::Duration::from_millis(35),
            move || {
                let mux = Mux::get().unwrap();
                if let Some(tab) = mux.get_active_tab_for_window(mux_window_id) {
                    if tab.renderer().has_dirty_lines() {
                        cloned_window.invalidate();
                    }
                } else {
                    cloned_window.close();
                }
            },
        );

        window.show();
        Ok(())
    }

    fn recreate_texture_atlas(&mut self, size: usize) -> Fallible<()> {
        let surface = Rc::new(ImageTexture::new(size, size));
        let atlas = RefCell::new(Atlas::new(&surface).expect("failed to create new texture atlas"));
        self.glyph_cache.borrow_mut().clear();
        self.atlas = atlas;
        Ok(())
    }

    fn update_title(&mut self) {
        let mux = Mux::get().unwrap();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        let num_tabs = window.len();

        if num_tabs == 0 {
            return;
        }
        let tab_no = window.get_active_idx();

        let title = match window.get_active() {
            Some(tab) => tab.get_title(),
            None => return,
        };

        drop(window);

        if let Some(window) = self.window.as_ref() {
            if num_tabs == 1 {
                window.set_title(&title);
            } else {
                window.set_title(&format!("[{}/{}] {}", tab_no + 1, num_tabs, title));
            }
        }
    }

    fn activate_tab(&mut self, tab_idx: usize) -> Fallible<()> {
        let mux = Mux::get().unwrap();
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| failure::format_err!("no such window"))?;

        let max = window.len();
        if tab_idx < max {
            window.set_active(tab_idx);

            drop(window);
            self.update_title();
        }
        Ok(())
    }

    fn activate_tab_relative(&mut self, delta: isize) -> Fallible<()> {
        let mux = Mux::get().unwrap();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| failure::format_err!("no such window"))?;

        let max = window.len();
        failure::ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx() as isize;
        let tab = active + delta;
        let tab = if tab < 0 { max as isize + tab } else { tab };
        drop(window);
        self.activate_tab(tab as usize % max)
    }

    fn spawn_tab(&mut self, domain: &SpawnTabDomain) -> Fallible<TabId> {
        let rows = (self.dimensions.pixel_height as usize + 1) / self.cell_size.height as usize;
        let cols = (self.dimensions.pixel_width as usize + 1) / self.cell_size.width as usize;

        let size = portable_pty::PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: self.dimensions.pixel_width as u16,
            pixel_height: self.dimensions.pixel_height as u16,
        };

        let mux = Mux::get().unwrap();

        let domain = match domain {
            SpawnTabDomain::DefaultDomain => mux.default_domain().clone(),
            SpawnTabDomain::CurrentTabDomain => {
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => failure::bail!("window has no tabs?"),
                };
                mux.get_domain(tab.domain_id()).ok_or_else(|| {
                    failure::format_err!("current tab has unresolvable domain id!?")
                })?
            }
            SpawnTabDomain::Domain(id) => mux.get_domain(*id).ok_or_else(|| {
                failure::format_err!("spawn_tab called with unresolvable domain id!?")
            })?,
            SpawnTabDomain::DomainName(name) => mux.get_domain_by_name(&name).ok_or_else(|| {
                failure::format_err!("spawn_tab called with unresolvable domain name {}", name)
            })?,
        };
        let tab = domain.spawn(size, None, self.mux_window_id)?;
        let tab_id = tab.tab_id();

        let len = {
            let window = mux
                .get_window(self.mux_window_id)
                .ok_or_else(|| failure::format_err!("no such window!?"))?;
            window.len()
        };
        self.activate_tab(len - 1)?;
        Ok(tab_id)
    }

    fn perform_key_assignment(
        &mut self,
        tab: &Rc<dyn Tab>,
        assignment: &KeyAssignment,
    ) -> Fallible<()> {
        use KeyAssignment::*;
        match assignment {
            SpawnTab(spawn_where) => {
                self.spawn_tab(spawn_where)?;
            }
            SpawnWindow => {
                self.spawn_new_window();
            }
            ToggleFullScreen => {
                // self.toggle_full_screen(),
            }
            Copy => {
                // Nominally copy, but that is implicit, so NOP
            }
            Paste => {
                tab.trickle_paste(self.clipboard.get_contents()?)?;
            }
            ActivateTabRelative(n) => {
                self.activate_tab_relative(*n)?;
            }
            DecreaseFontSize => self.decrease_font_size(),
            IncreaseFontSize => self.increase_font_size(),
            ResetFontSize => self.reset_font_size(),
            ActivateTab(n) => {
                self.activate_tab(*n)?;
            }
            SendString(s) => tab.writer().write_all(s.as_bytes())?,
            SendByte(b) => tab.writer().write_all(b)?,
            Hide => {
                if let Some(w) = self.window.as_ref() {
                    w.hide();
                }
            }
            Show => {
                if let Some(w) = self.window.as_ref() {
                    w.show();
                }
            }
            CloseCurrentTab => self.close_current_tab(),
            Nop => {}
        };
        Ok(())
    }

    pub fn spawn_new_window(&mut self) {
        promise::Future::with_executor(gui_executor().unwrap(), move || {
            let mux = Mux::get().unwrap();
            let fonts = Rc::new(FontConfiguration::new(
                Arc::clone(mux.config()),
                FontSystemSelection::get_default(),
            ));
            let window_id = mux.new_empty_window();
            let tab =
                mux.default_domain()
                    .spawn(portable_pty::PtySize::default(), None, window_id)?;
            let front_end = front_end().expect("to be called on gui thread");
            front_end.spawn_new_window(mux.config(), &fonts, &tab, window_id)?;
            Ok(())
        });
    }

    #[allow(clippy::float_cmp)]
    fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64) {
        let mux = Mux::get().unwrap();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            if dimensions.dpi != self.dimensions.dpi || font_scale != self.fonts.get_font_scale() {
                self.fonts
                    .change_scaling(font_scale, dimensions.dpi as f64 / 96.);
                let metrics = self
                    .fonts
                    .default_font_metrics()
                    .expect("failed to get font metrics!?");

                let (cell_height, cell_width) = (
                    metrics.cell_height.ceil() as usize,
                    metrics.cell_width.ceil() as usize,
                );

                let atlas_size = self.atlas.borrow().size();
                self.recreate_texture_atlas(atlas_size)
                    .expect("failed to recreate atlas");

                let descender_row = (cell_height as f64 + metrics.descender) as isize;
                let descender_plus_one = (1 + descender_row).min(cell_height as isize - 1);
                let descender_plus_two = (2 + descender_row).min(cell_height as isize - 1);
                let strike_row = descender_row / 2;

                self.descender = metrics.descender;
                self.descender_row = descender_row;
                self.descender_plus_one = descender_plus_one;
                self.descender_plus_two = descender_plus_two;
                self.strike_row = strike_row;

                self.cell_size = Size::new(cell_width as isize, cell_height as isize);
            }

            self.dimensions = dimensions;

            let size = portable_pty::PtySize {
                rows: dimensions.pixel_height as u16 / self.cell_size.height as u16,
                cols: dimensions.pixel_width as u16 / self.cell_size.width as u16,
                pixel_height: dimensions.pixel_height as u16,
                pixel_width: dimensions.pixel_width as u16,
            };
            for tab in window.iter() {
                tab.resize(size).ok();
            }
        };
    }

    fn decrease_font_size(&mut self) {
        self.scaling_changed(self.dimensions, self.fonts.get_font_scale() * 0.9);
    }
    fn increase_font_size(&mut self) {
        self.scaling_changed(self.dimensions, self.fonts.get_font_scale() * 1.1);
    }
    fn reset_font_size(&mut self) {
        self.scaling_changed(self.dimensions, 1.);
    }

    fn close_current_tab(&mut self) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        mux.remove_tab(tab.tab_id());
        if let Some(mut win) = mux.get_window_mut(self.mux_window_id) {
            win.remove_by_id(tab.tab_id());
        }
        self.activate_tab_relative(0).ok();
    }

    fn paint_tab(&mut self, tab: &Rc<dyn Tab>, ctx: &mut dyn PaintContext) -> Fallible<()> {
        let palette = tab.palette();

        let mut term = tab.renderer();
        let cursor = term.get_cursor_position();

        {
            let dirty_lines = term.get_dirty_lines();

            for (line_idx, line, selrange) in dirty_lines {
                self.render_screen_line(ctx, line_idx, &line, selrange, &cursor, &*term, &palette)?;
            }
        }

        term.clean_dirty_lines();

        // Fill any marginal area below the last row
        let (num_rows, _num_cols) = term.physical_dimensions();
        let pixel_height_of_cells = num_rows * self.cell_size.height as usize;
        ctx.clear_rect(
            Rect::new(
                Point::new(0, pixel_height_of_cells as isize),
                Size::new(
                    self.dimensions.pixel_width as isize,
                    (self.dimensions.pixel_height - pixel_height_of_cells) as isize,
                ),
            ),
            rgbcolor_to_window_color(palette.background),
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_screen_line(
        &self,
        ctx: &mut dyn PaintContext,
        line_idx: usize,
        line: &Line,
        selection: Range<usize>,
        cursor: &CursorPosition,
        terminal: &dyn Renderable,
        palette: &ColorPalette,
    ) -> Fallible<()> {
        let (_num_rows, num_cols) = terminal.physical_dimensions();
        let current_highlight = terminal.current_highlight();

        // Break the line into clusters of cells with the same attributes
        let cell_clusters = line.cluster();
        let mut last_cell_idx = 0;
        for cluster in cell_clusters {
            let attrs = &cluster.attrs;
            let is_highlited_hyperlink = match (&attrs.hyperlink, &current_highlight) {
                (&Some(ref this), &Some(ref highlight)) => this == highlight,
                _ => false,
            };
            let style = self.fonts.match_style(attrs);

            let bg_color = palette.resolve_bg(attrs.background);
            let fg_color = match attrs.foreground {
                term::color::ColorAttribute::Default => {
                    if let Some(fg) = style.foreground {
                        fg
                    } else {
                        palette.resolve_fg(attrs.foreground)
                    }
                }
                term::color::ColorAttribute::PaletteIndex(idx) if idx < 8 => {
                    // For compatibility purposes, switch to a brighter version
                    // of one of the standard ANSI colors when Bold is enabled.
                    // This lifts black to dark grey.
                    let idx = if attrs.intensity() == term::Intensity::Bold {
                        idx + 8
                    } else {
                        idx
                    };
                    palette.resolve_fg(term::color::ColorAttribute::PaletteIndex(idx))
                }
                _ => palette.resolve_fg(attrs.foreground),
            };

            let (fg_color, bg_color) = {
                let mut fg = fg_color;
                let mut bg = bg_color;

                if attrs.reverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }

                (fg, bg)
            };

            let glyph_color = rgbcolor_to_window_color(fg_color);
            let bg_color = rgbcolor_to_window_color(bg_color);

            // Shape the printable text from this cluster
            let glyph_info = {
                let font = self.fonts.cached_font(style)?;
                let mut font = font.borrow_mut();
                font.shape(&cluster.text)?
            };

            for info in &glyph_info {
                let cell_idx = cluster.byte_to_cell_idx[info.cluster as usize];
                let glyph = self.cached_glyph(info, style)?;

                let left = (glyph.x_offset + glyph.bearing_x) as f32;
                let top = ((self.cell_size.height as f64 + self.descender)
                    - (glyph.y_offset + glyph.bearing_y)) as f32;

                // underline and strikethrough
                // Figure out what we're going to draw for the underline.
                // If the current cell is part of the current URL highlight
                // then we want to show the underline.
                let underline = match (is_highlited_hyperlink, attrs.underline()) {
                    (true, Underline::None) => Underline::Single,
                    (_, underline) => underline,
                };

                // Iterate each cell that comprises this glyph.  There is usually
                // a single cell per glyph but combining characters, ligatures
                // and emoji can be 2 or more cells wide.
                for glyph_idx in 0..info.num_cells as usize {
                    let cell_idx = cell_idx + glyph_idx;

                    if cell_idx >= num_cols {
                        // terminal line data is wider than the window.
                        // This happens for example while live resizing the window
                        // smaller than the terminal.
                        break;
                    }
                    last_cell_idx = cell_idx;

                    let (glyph_color, bg_color) = self.compute_cell_fg_bg(
                        line_idx,
                        cell_idx,
                        cursor,
                        &selection,
                        glyph_color,
                        bg_color,
                        palette,
                    );

                    let cell_rect = Rect::new(
                        Point::new(
                            cell_idx as isize * self.cell_size.width,
                            self.cell_size.height * line_idx as isize,
                        ),
                        self.cell_size,
                    );
                    ctx.clear_rect(cell_rect, bg_color);

                    match underline {
                        Underline::Single => {
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_plus_one,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_plus_one,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                        }
                        Underline::Double => {
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_row,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_row,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_plus_two,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_plus_two,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                        }
                        Underline::None => {}
                    }
                    if attrs.strikethrough() {
                        ctx.draw_line(
                            Point::new(cell_rect.origin.x, cell_rect.origin.y + self.strike_row),
                            Point::new(
                                cell_rect.origin.x + self.cell_size.width,
                                cell_rect.origin.y + self.strike_row,
                            ),
                            glyph_color,
                            Operator::Over,
                        );
                    }

                    if let Some(ref texture) = glyph.texture {
                        let slice = SpriteSlice {
                            cell_idx: glyph_idx,
                            num_cells: info.num_cells as usize,
                            cell_width: self.cell_size.width as usize,
                            scale: glyph.scale as f32,
                            left_offset: left,
                        };
                        let left = if glyph_idx == 0 { left } else { 0.0 };

                        ctx.draw_image(
                            Point::new(
                                (cell_rect.origin.x as f32 + left) as isize,
                                (cell_rect.origin.y as f32 + top) as isize,
                            ),
                            Some(slice.pixel_rect(texture)),
                            &*texture.texture.image.borrow(),
                            if glyph.has_color {
                                // For full color glyphs, always use their color.
                                // This avoids rendering a black mask when the text
                                // selection moves over the glyph
                                Operator::Over
                            } else {
                                Operator::MultiplyThenOver(glyph_color)
                            },
                        );
                    }
                }
            }
        }

        // Clear any remaining cells to the right of the clusters we
        // found above, otherwise we leave artifacts behind.  The easiest
        // reproduction for the artifacts is to maximize the window and
        // open a vim split horizontally.  Backgrounding vim would leave
        // the right pane with its prior contents instead of showing the
        // cleared lines from the shell in the main screen.

        for cell_idx in last_cell_idx + 1..num_cols {
            // Even though we don't have a cell for these, they still
            // hold the cursor or the selection so we need to compute
            // the colors in the usual way.
            let (_glyph_color, bg_color) = self.compute_cell_fg_bg(
                line_idx,
                cell_idx,
                cursor,
                &selection,
                rgbcolor_to_window_color(palette.foreground),
                rgbcolor_to_window_color(palette.background),
                palette,
            );

            let cell_rect = Rect::new(
                Point::new(
                    cell_idx as isize * self.cell_size.width,
                    self.cell_size.height * line_idx as isize,
                ),
                self.cell_size,
            );
            ctx.clear_rect(cell_rect, bg_color);
        }

        // Fill any marginal area to the right of the last cell
        let pixel_width_of_cells = num_cols * self.cell_size.width as usize;
        ctx.clear_rect(
            Rect::new(
                Point::new(
                    pixel_width_of_cells as isize,
                    self.cell_size.height * line_idx as isize,
                ),
                Size::new(
                    (self.dimensions.pixel_width - pixel_width_of_cells) as isize,
                    self.cell_size.height,
                ),
            ),
            rgbcolor_to_window_color(palette.background),
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_cell_fg_bg(
        &self,
        line_idx: usize,
        cell_idx: usize,
        cursor: &CursorPosition,
        selection: &Range<usize>,
        fg_color: Color,
        bg_color: Color,
        palette: &ColorPalette,
    ) -> (Color, Color) {
        let selected = selection.contains(&cell_idx);
        let is_cursor = line_idx as i64 == cursor.y && cursor.x == cell_idx;

        let (fg_color, bg_color) = match (selected, is_cursor) {
            // Normally, render the cell as configured
            (false, false) => (fg_color, bg_color),
            // Cursor cell overrides colors
            (_, true) => (
                rgbcolor_to_window_color(palette.cursor_fg),
                rgbcolor_to_window_color(palette.cursor_bg),
            ),
            // Selected text overrides colors
            (true, false) => (
                rgbcolor_to_window_color(palette.selection_fg),
                rgbcolor_to_window_color(palette.selection_bg),
            ),
        };

        (fg_color, bg_color)
    }

    /// Resolve a glyph from the cache, rendering the glyph on-demand if
    /// the cache doesn't already hold the desired glyph.
    fn cached_glyph(&self, info: &GlyphInfo, style: &TextStyle) -> Fallible<Rc<CachedGlyph>> {
        let key = GlyphKey {
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            style: style.clone(),
        };

        let mut cache = self.glyph_cache.borrow_mut();

        if let Some(entry) = cache.get(&key) {
            return Ok(Rc::clone(entry));
        }

        let glyph = self.load_glyph(info, style)?;
        cache.insert(key, Rc::clone(&glyph));
        Ok(glyph)
    }

    /// Perform the load and render of a glyph
    #[allow(clippy::float_cmp)]
    fn load_glyph(&self, info: &GlyphInfo, style: &TextStyle) -> Fallible<Rc<CachedGlyph>> {
        let (has_color, glyph, cell_width, cell_height) = {
            let font = self.fonts.cached_font(style)?;
            let mut font = font.borrow_mut();
            let metrics = font.get_fallback(0)?.metrics();
            let active_font = font.get_fallback(info.font_idx)?;
            let has_color = active_font.has_color();
            let glyph = active_font.rasterize_glyph(info.glyph_pos)?;
            (has_color, glyph, metrics.cell_width, metrics.cell_height)
        };

        let scale = if (info.x_advance / f64::from(info.num_cells)).floor() > cell_width {
            f64::from(info.num_cells) * (cell_width / info.x_advance)
        } else if glyph.height as f64 > cell_height {
            cell_height / glyph.height as f64
        } else {
            1.0f64
        };
        let (x_offset, y_offset) = if scale != 1.0 {
            (info.x_offset * scale, info.y_offset * scale)
        } else {
            (info.x_offset, info.y_offset)
        };

        let glyph = if glyph.width == 0 || glyph.height == 0 {
            // a whitespace glyph
            CachedGlyph {
                has_color,
                texture: None,
                x_offset,
                y_offset,
                bearing_x: 0.0,
                bearing_y: 0.0,
                scale,
            }
        } else {
            let raw_im = Image::with_rgba32(
                glyph.width as usize,
                glyph.height as usize,
                4 * glyph.width as usize,
                &glyph.data,
            );

            let bearing_x = glyph.bearing_x * scale;
            let bearing_y = glyph.bearing_y * scale;

            let (scale, raw_im) = if scale != 1.0 {
                (1.0, raw_im.scale_by(scale))
            } else {
                (scale, raw_im)
            };

            let tex = self.atlas.borrow_mut().allocate(&raw_im)?;

            CachedGlyph {
                has_color,
                texture: Some(tex),
                x_offset,
                y_offset,
                bearing_x,
                bearing_y,
                scale,
            }
        };

        Ok(Rc::new(glyph))
    }
}

fn rgbcolor_to_window_color(color: RgbColor) -> Color {
    Color::rgba(color.red, color.green, color.blue, 0xff)
}

fn window_mods_to_termwiz_mods(modifiers: ::window::Modifiers) -> termwiz::input::Modifiers {
    let mut result = termwiz::input::Modifiers::NONE;
    if modifiers.contains(::window::Modifiers::SHIFT) {
        result.insert(termwiz::input::Modifiers::SHIFT);
    }
    if modifiers.contains(::window::Modifiers::ALT) {
        result.insert(termwiz::input::Modifiers::ALT);
    }
    if modifiers.contains(::window::Modifiers::CTRL) {
        result.insert(termwiz::input::Modifiers::CTRL);
    }
    if modifiers.contains(::window::Modifiers::SUPER) {
        result.insert(termwiz::input::Modifiers::SUPER);
    }
    result
}
