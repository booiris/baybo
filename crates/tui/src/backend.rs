//! Terminal backend that answers ratatui's cursor-position query from its own
//! bookkeeping instead of asking the terminal.
//!
//! ratatui anchors an inline viewport by asking the backend where the cursor
//! is, which [`CrosstermBackend`] answers with a DSR round-trip: it writes
//! `ESC[6n` and reads the reply off stdin. That read contends with the
//! [`crossterm::event::EventStream`] reader thread for crossterm's single
//! global input reader, and losing is fatal rather than cosmetic — the query
//! fails, the error unwinds the event loop, and the terminal's reply arrives
//! after the process is gone, landing in the shell's input buffer as a stray
//! `;1R`.
//!
//! Nothing here needs to ask. Every inline terminal is built immediately after
//! the cursor has been put somewhere known (a screen home, a viewport clear),
//! so the caller passes that anchor in and this backend reports it. No query is
//! ever emitted, which leaves `EventStream` as the process's only stdin reader.
//!
//! The tracked position is exact after [`Backend::set_cursor_position`] and
//! [`Backend::append_lines`] — the two operations whose result ratatui feeds
//! back into inline geometry, and the only ones that move the cursor by a
//! knowable amount. [`Backend::draw`] also leaves the cursor after the last
//! painted cell, but chat mode sets the frame cursor on every draw
//! (`chat::render_input`), so ratatui immediately follows with an absolute
//! `set_cursor_position` and the tracked value stays exact.

use std::io::Write;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub(crate) struct AnchoredBackend<B> {
    inner: B,
    cursor: Position,
}

impl<W: Write> AnchoredBackend<CrosstermBackend<W>> {
    pub(crate) fn new(writer: W, anchor: Position) -> Self {
        Self::wrap(CrosstermBackend::new(writer), anchor)
    }
}

impl<B> AnchoredBackend<B> {
    pub(crate) fn wrap(inner: B, anchor: Position) -> Self {
        Self {
            inner,
            cursor: anchor,
        }
    }
}

impl<B: Backend> AnchoredBackend<B> {
    /// The tracked cursor position, clamped to the live screen, for callers
    /// that need to carry an anchor across a terminal rebuild.
    pub(crate) fn cursor(&self) -> Result<Position, B::Error> {
        self.clamped(self.cursor)
    }

    /// A real terminal's DSR reply can never name a cell outside the screen, so
    /// neither may the bookkeeping standing in for it. An anchor can go stale —
    /// the row carried across the dashboard's alt-screen round trip survives a
    /// window shrink — and `compute_inline_size` reduces an out-of-range row by
    /// at most the screen height without ever clamping it, so an unclamped
    /// report anchors the inline viewport off-screen with no way back.
    fn clamped(&self, position: Position) -> Result<Position, B::Error> {
        let size = self.inner.size()?;
        Ok(Position {
            x: position.x.min(size.width.saturating_sub(1)),
            y: position.y.min(size.height.saturating_sub(1)),
        })
    }
}

impl<B: Backend> Backend for AnchoredBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)?;
        // `CrosstermBackend::append_lines` prints bare LFs and raw mode has
        // already disabled output post-processing, so the column survives; the
        // row advances until the screen starts scrolling under the cursor.
        let bottom = self.inner.size()?.height.saturating_sub(1);
        self.cursor.y = self.cursor.y.saturating_add(n).min(bottom);
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.clamped(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.cursor = position;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    const CURSOR_QUERY_REACHED_TERMINAL: &str = "a cursor query reached the terminal";

    /// Backend whose cursor query always fails, standing in for the real DSR
    /// round-trip losing its race against `EventStream`. Every other operation
    /// delegates to a [`TestBackend`], so an inline viewport driven through it
    /// behaves normally right up until something asks the terminal where the
    /// cursor is.
    struct Poisoned(TestBackend);

    impl Backend for Poisoned {
        type Error = std::io::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.0.draw(content).map_err(|e| match e {})
        }
        fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
            self.0.append_lines(n).map_err(|e| match e {})
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.hide_cursor().map_err(|e| match e {})
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.show_cursor().map_err(|e| match e {})
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            Err(std::io::Error::other(CURSOR_QUERY_REACHED_TERMINAL))
        }
        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.0.set_cursor_position(position).map_err(|e| match e {})
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            self.0.clear().map_err(|e| match e {})
        }
        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.0.clear_region(clear_type).map_err(|e| match e {})
        }
        fn size(&self) -> Result<Size, Self::Error> {
            self.0.size().map_err(|e| match e {})
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.0.window_size().map_err(|e| match e {})
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            self.0.flush().map_err(|e| match e {})
        }
    }

    fn poisoned_terminal(viewport_h: u16, anchor: Position) -> Terminal<AnchoredBackend<Poisoned>> {
        let backend = AnchoredBackend::wrap(Poisoned(TestBackend::new(40, 10)), anchor);
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_h),
            },
        )
        .expect("building an inline terminal must not query the cursor")
    }

    /// The regression guard for the DSR race: drive a full inline-viewport
    /// lifecycle over a backend that errors on any cursor query. Every step
    /// must succeed, which is only possible if `AnchoredBackend` answered all
    /// of them itself.
    #[test]
    fn the_inline_lifecycle_never_queries_the_terminal() {
        let mut terminal = poisoned_terminal(3, Position::ORIGIN);

        // Enough scrollback commits to push the viewport to the bottom and
        // start scrolling under it — the path that grew a cursor query in
        // ratatui-core 0.1.2.
        for _ in 0..12 {
            terminal
                .insert_before(1, |_| {})
                .expect("insert_before must not query the cursor");
        }
        // Not just "no error": the viewport has to end up pinned to the bottom
        // of the screen, with the tracked cursor riding the last row the
        // scrollback pushed it to.
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 7, 40, 3));
        assert_eq!(
            terminal.backend().cursor().expect("cursor"),
            Position { x: 0, y: 9 }
        );
        terminal
            .draw(|_| {})
            .expect("draw must not query the cursor");
        terminal.clear().expect("clear must not query the cursor");
        let area = terminal.get_frame().area();
        terminal
            .resize(area)
            .expect("resize must not query the cursor");
        terminal
            .draw(|_| {})
            .expect("the post-resize draw must not query the cursor");
    }

    /// The anchor the caller passes in is what ratatui uses to place the
    /// viewport, so an inline terminal built after a screen home lands at the
    /// top rather than wherever the previous viewport was.
    #[test]
    fn the_supplied_anchor_places_the_viewport() {
        let mut top = poisoned_terminal(3, Position::ORIGIN);
        assert_eq!(top.get_frame().area().y, 0);

        let mut lower = poisoned_terminal(3, Position { x: 0, y: 4 });
        assert_eq!(lower.get_frame().area().y, 4);
    }

    /// Appending scrollback lines walks the tracked cursor down and pins it at
    /// the last row once the screen scrolls, so a later rebuild anchors at the
    /// bottom instead of running off the end of the screen.
    #[test]
    fn appending_lines_tracks_the_cursor_and_stops_at_the_bottom() {
        let mut backend =
            AnchoredBackend::wrap(Poisoned(TestBackend::new(40, 10)), Position::ORIGIN);

        backend.append_lines(3).expect("append");
        assert_eq!(backend.cursor().expect("cursor"), Position { x: 0, y: 3 });

        backend.append_lines(100).expect("append past the bottom");
        assert_eq!(backend.cursor().expect("cursor"), Position { x: 0, y: 9 });
    }

    /// Chat sets a frame cursor on every draw, so ratatui follows each frame
    /// with an absolute `set_cursor_position` — the tracked value has to follow
    /// it, or the anchor carried across a rebuild is whatever it was seeded
    /// with.
    #[test]
    fn a_frame_cursor_moves_the_tracked_position() {
        let mut backend =
            AnchoredBackend::wrap(Poisoned(TestBackend::new(40, 10)), Position::ORIGIN);

        backend
            .set_cursor_position(Position { x: 3, y: 4 })
            .expect("set cursor");

        assert_eq!(backend.cursor().expect("cursor"), Position { x: 3, y: 4 });
        assert_eq!(
            backend.get_cursor_position().expect("query"),
            Position { x: 3, y: 4 }
        );
    }

    /// A carried anchor can outlive the screen it was measured on — the
    /// dashboard's alt-screen round trip spans a window resize. A real DSR
    /// reply could never name a row off the screen, and neither may this, or
    /// `compute_inline_size` anchors the viewport where nothing is visible.
    #[test]
    fn a_stale_anchor_is_clamped_to_the_screen() {
        let stale = Position { x: 99, y: 56 };
        let backend = AnchoredBackend::wrap(Poisoned(TestBackend::new(40, 10)), stale);
        assert_eq!(
            backend.cursor().expect("cursor"),
            Position { x: 39, y: 9 },
            "the carried anchor must be clamped to the live screen"
        );

        let mut terminal = poisoned_terminal(3, stale);
        assert_eq!(
            terminal.get_frame().area(),
            Rect::new(0, 7, 40, 3),
            "an inline viewport built from a stale anchor must stay on screen"
        );
    }
}
