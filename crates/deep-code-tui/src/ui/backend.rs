//! Buffers each ANSI frame and keeps the native cursor hidden until it is positioned.

use std::io::{self, BufWriter, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::{Frame, Terminal};

use crate::interruptible_output::MAX_REQUEST_BYTES;

pub(super) struct FrameBackend<W: Write> {
    output: BufWriter<W>,
    frame: Vec<u8>,
    ansi: bool,
    // Some while drawing. Ratatui requests Show before Move, so defer
    // visibility until the end of the frame.
    show_after_draw: Option<bool>,
}

impl<W: Write> FrameBackend<W> {
    pub(super) fn new(output: BufWriter<W>, ansi: bool) -> Self {
        Self {
            output,
            frame: Vec::new(),
            ansi,
            show_after_draw: None,
        }
    }

    fn draw_writer(&mut self) -> &mut dyn Write {
        if self.ansi && self.show_after_draw.is_some() {
            &mut self.frame
        } else {
            // WinAPI cursor operations can take effect immediately, so the
            // legacy console path must honor Crossterm's intervening flushes.
            &mut self.output
        }
    }

    fn begin_frame(&mut self) -> io::Result<()> {
        if self.ansi {
            self.frame.clear();
            queue!(self.frame, BeginSynchronizedUpdate, Hide)?;
        } else {
            execute!(self.output, Hide)?;
        }
        self.show_after_draw = Some(false);
        Ok(())
    }

    fn finish_frame(&mut self, drawn: io::Result<()>) -> io::Result<()> {
        let show = self.show_after_draw.take() == Some(true);
        // At this point ANSI bytes are still local. A failed draw can be
        // discarded without entering synchronized mode on the terminal.
        drawn?;
        if !self.ansi {
            if show {
                execute!(self.output, Show)?;
            }
            return Ok(());
        }

        if show {
            queue!(self.frame, Show)?;
        }
        queue!(self.frame, EndSynchronizedUpdate)?;

        // Cancellation can stop between output requests. End each request
        // at a complete UTF-8 scalar so the Windows console adapter can
        // accept the next write even when it comes from terminal cleanup.
        let mut frame = std::str::from_utf8(&self.frame)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !self.output.buffer().is_empty() {
            self.output.flush()?;
        }
        // Bypass the buffer so it can't recombine these request boundaries.
        let output = self.output.get_mut();
        while !frame.is_empty() {
            let (chunk, remaining) = frame.split_at(frame.floor_char_boundary(MAX_REQUEST_BYTES));
            output.write_all(chunk.as_bytes())?;
            frame = remaining;
        }
        output.flush()
    }
}

impl<W: Write> Write for FrameBackend<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.draw_writer().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.draw_writer().flush()
    }
}

impl<W: Write> Backend for FrameBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        CrosstermBackend::new(self.draw_writer()).draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        CrosstermBackend::new(self.draw_writer()).append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        if let Some(show) = &mut self.show_after_draw {
            *show = false;
            return Ok(());
        }
        CrosstermBackend::new(&mut self.output).hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if let Some(show) = &mut self.show_after_draw {
            *show = true;
            return Ok(());
        }
        CrosstermBackend::new(&mut self.output).show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        CrosstermBackend::new(io::sink()).get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        CrosstermBackend::new(self.draw_writer()).set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        CrosstermBackend::new(self.draw_writer()).clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        CrosstermBackend::new(self.draw_writer()).clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        CrosstermBackend::new(io::sink()).size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        CrosstermBackend::new(io::sink()).window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }
}

pub(super) fn draw_frame<W: Write>(
    terminal: &mut Terminal<FrameBackend<W>>,
    render: impl FnOnce(&mut Frame<'_>),
) -> io::Result<()> {
    terminal.backend_mut().begin_frame()?;
    let drawn = terminal.draw(render).map(|_| ());
    terminal.backend_mut().finish_frame(drawn)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ratatui::{TerminalOptions, Viewport, layout::Rect, style::Style};

    const BEGIN: &str = "\x1b[?2026h";
    const END: &str = "\x1b[?2026l";
    const HIDE: &str = "\x1b[?25l";
    const SHOW: &str = "\x1b[?25h";

    #[derive(Default)]
    struct RecordingOutput {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for RecordingOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn terminal(ansi: bool) -> Terminal<FrameBackend<RecordingOutput>> {
        let backend = FrameBackend::new(BufWriter::new(RecordingOutput::default()), ansi);
        let viewport = Viewport::Fixed(Rect::new(0, 0, 8, 3));
        Terminal::with_options(backend, TerminalOptions { viewport }).unwrap()
    }

    fn render_text_and_cursor(frame: &mut Frame<'_>) {
        frame.buffer_mut().set_string(0, 0, "x", Style::default());
        frame.set_cursor_position((2, 1));
    }

    fn assert_cursor_is_hidden_until_positioned(frame: &str) {
        let hide = frame.find(HIDE).unwrap();
        let text = frame.find('x').unwrap();
        let move_to_cursor = frame.find("\x1b[2;3H").unwrap();
        let show = frame.find(SHOW).unwrap();
        assert!(hide < text && text < move_to_cursor && move_to_cursor < show);
    }

    #[test]
    fn ansi_frame_submits_text_cursor_and_sync_end_in_one_write() {
        let mut terminal = terminal(true);
        draw_frame(&mut terminal, render_text_and_cursor).unwrap();

        let output = terminal.backend().output.get_ref();
        assert_eq!(output.writes.len(), 1);
        assert_eq!(output.flushes, 1);
        let frame = std::str::from_utf8(&output.writes[0]).unwrap();
        assert!(frame.starts_with(BEGIN));
        assert!(frame.ends_with(END));
        assert_cursor_is_hidden_until_positioned(frame);

        draw_frame(&mut terminal, |_| {}).unwrap();
        let output = terminal.backend().output.get_ref();
        let frame = std::str::from_utf8(&output.writes[1]).unwrap();
        assert!(frame.contains(HIDE));
        assert!(!frame.contains(SHOW));
    }

    #[test]
    fn large_ansi_frame_requests_leave_no_partial_utf8_for_cleanup() {
        use std::sync::{Arc, Mutex};

        use deep_code_agent::CancellationToken;

        use crate::interruptible_output::Output;

        struct RecordingRequests {
            writes: Arc<Mutex<Vec<Vec<u8>>>>,
            cancel_after_first: Option<CancellationToken>,
        }

        impl Write for RecordingRequests {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes.lock().unwrap().push(bytes.to_vec());
                if let Some(token) = self.cancel_after_first.take() {
                    token.cancel();
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        for cancelled in [false, true] {
            let token = CancellationToken::new();
            let writes = Arc::new(Mutex::new(Vec::new()));
            let output = Output::new(
                RecordingRequests {
                    writes: writes.clone(),
                    cancel_after_first: cancelled.then(|| token.clone()),
                },
                &token,
            )
            .unwrap();
            let buffered = BufWriter::with_capacity(MAX_REQUEST_BYTES * 2, output.clone());
            let mut backend = FrameBackend::new(buffered, true);
            backend.begin_frame().unwrap();
            let padding = MAX_REQUEST_BYTES - backend.frame.len() - 1;
            backend.write_all(&vec![b'x'; padding]).unwrap();
            backend.write_all("界🙂".as_bytes()).unwrap();

            let written = backend.finish_frame(Ok(()));
            if cancelled {
                assert_eq!(written.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
                let mut cleanup = BufWriter::new(output.cleanup());
                execute!(cleanup, EndSynchronizedUpdate).unwrap();
            } else {
                written.unwrap();
            }

            let requests = writes.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].len(), MAX_REQUEST_BYTES - 1);
            assert!(
                std::str::from_utf8(&requests[0])
                    .unwrap()
                    .starts_with(BEGIN)
            );
            let last = std::str::from_utf8(&requests[1]).unwrap();
            if cancelled {
                assert_eq!(last, END);
            } else {
                assert_eq!(last, format!("界🙂{END}"));
            }
        }
    }

    #[test]
    fn fallback_paints_with_the_cursor_hidden_until_positioned() {
        let mut terminal = terminal(false);
        draw_frame(&mut terminal, render_text_and_cursor).unwrap();

        let frame = String::from_utf8(terminal.backend().output.get_ref().writes.concat()).unwrap();
        assert!(!frame.contains(BEGIN));
        assert!(!frame.contains(END));
        assert_cursor_is_hidden_until_positioned(&frame);
    }

    #[test]
    fn failed_draw_does_not_publish_any_of_its_staged_ansi_bytes() {
        let mut terminal = terminal(true);
        let backend = terminal.backend_mut();
        backend.begin_frame().unwrap();
        backend.write_all(b"partial frame").unwrap();
        let error = backend
            .finish_frame(Err(io::Error::other("draw failed")))
            .unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
        assert!(backend.output.get_ref().writes.is_empty());

        draw_frame(&mut terminal, render_text_and_cursor).unwrap();
        let frame = std::str::from_utf8(&terminal.backend().output.get_ref().writes[0]).unwrap();
        assert!(!frame.contains("partial frame"));
    }
}
