//! The terminal edge: progress bars on stderr, and the log writer that
//! keeps log lines and bar redraws from splicing together. Tests swap the
//! real stderr for a hidden one with `Terminal::create_null` and track
//! the status lines the bars would show.

use std::{
    fmt::Display,
    io::{self, Write},
};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use yansi::Paint;

use crate::nullable::OutputListener;

/// Whether a status line reports success, a problem the run worked around,
/// or a failure. It picks the color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Good,
    Warn,
    Bad,
}

/// One status line a bar printed, as a test sees it: the right-aligned word,
/// its tone, and the text after it, without colors. A line is recorded even
/// when the bar is hidden and nobody sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) word: &'static str,
    pub(crate) tone: Tone,
    pub(crate) detail: String,
}

/// The shared progress area on stderr. Clones share it.
#[derive(Debug, Clone)]
pub struct Terminal {
    multi: MultiProgress,
    visible: bool,
    lines: OutputListener<Line>,
}

impl Terminal {
    /// The real stderr. Bars draw only when stderr is a terminal.
    #[must_use]
    pub fn create() -> Self {
        let multi = MultiProgress::new();
        Self {
            visible: !multi.is_hidden(),
            multi,
            lines: OutputListener::default(),
        }
    }

    /// A writer for log lines. It clears the bars, writes the line to
    /// stderr, and lets the bars redraw, so the two never splice mid-line.
    /// It always writes to the process's real stderr, even for a nulled
    /// terminal, so tests capture logs through tracing instead.
    #[must_use]
    pub fn log_writer(&self) -> SuspendingStderr {
        SuspendingStderr(self.multi.clone())
    }

    /// Whether a person can see the bars. When they cannot, the bars and
    /// their status lines vanish, so failures must go to the log instead.
    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    /// A new bar of `len` steps, labeled `prefix`.
    pub(crate) fn bar(&self, prefix: &str, len: usize) -> Bar {
        let pb = self.multi.add(
            ProgressBar::new(len as u64).with_style(
                ProgressStyle::with_template(
                    "{prefix:>8} [{bar}] {human_pos}/{human_len}: {wide_msg}",
                )
                .expect("the template is valid"),
            ),
        );
        pb.set_prefix(prefix.bold().to_string());
        Bar {
            pb,
            lines: self.lines.clone(),
        }
    }
}

/// One progress bar in the shared area.
#[derive(Debug)]
pub(crate) struct Bar {
    pb: ProgressBar,
    lines: OutputListener<Line>,
}

impl Bar {
    /// Print a status line above the bar: `word` right-aligned and colored
    /// by `tone`, then `detail`.
    pub(crate) fn status(&self, word: &'static str, tone: Tone, detail: impl Display) {
        let detail = detail.to_string();
        let styled = match tone {
            Tone::Good => word.bold().green(),
            Tone::Warn => word.bold().yellow(),
            Tone::Bad => word.bold().red(),
        };
        self.pb.println(format!("{styled:>8} {detail}"));
        self.lines.emit(|| Line { word, tone, detail });
    }

    /// Show what is still in flight beside the bar.
    pub(crate) fn set_message(&self, message: String) {
        self.pb.set_message(message);
    }

    pub(crate) fn inc(&self) {
        self.pb.inc(1);
    }

    pub(crate) fn finish_and_clear(&self) {
        self.pb.finish_and_clear();
    }
}

/// An [`io::Write`] for tracing that clears the active progress bars, writes
/// the log line to stderr, and lets the bars redraw.
pub struct SuspendingStderr(MultiProgress);

impl Write for SuspendingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.suspend(|| io::stderr().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.suspend(|| io::stderr().flush())
    }
}

#[cfg(test)]
impl Terminal {
    /// A terminal that draws nothing. `visible` says whether the code under
    /// test should believe a person is watching.
    pub(crate) fn create_null(visible: bool) -> Self {
        Self {
            multi: MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden()),
            visible,
            lines: OutputListener::default(),
        }
    }

    /// Record every status line printed from now on, by any bar.
    pub(crate) fn track_lines(&self) -> crate::nullable::OutputTracker<Line> {
        self.lines.track()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{Line, Terminal, Tone};

    // The suspend wrapper must hand every byte through to stderr, with or
    // without an active bar; swallowed log lines would be far worse than the
    // splicing it prevents.
    #[test]
    fn the_log_writer_passes_bytes_through() {
        let terminal = Terminal::create();
        let mut w = terminal.log_writer();
        assert_eq!(w.write(b"test line\n").unwrap(), 10);
        w.flush().unwrap();

        let _bar = terminal.bar("Testing", 1);
        assert_eq!(w.write(b"with a bar\n").unwrap(), 11);
    }

    #[test]
    fn tracks_status_lines_from_every_bar_without_colors() {
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();
        terminal
            .bar("Fetching", 2)
            .status("Fetched", Tone::Good, "https://a.example/");
        terminal
            .bar("Summarizing", 1)
            .status("Error", Tone::Bad, "boom");
        assert_eq!(
            lines.data(),
            [
                Line {
                    word: "Fetched",
                    tone: Tone::Good,
                    detail: "https://a.example/".to_string()
                },
                Line {
                    word: "Error",
                    tone: Tone::Bad,
                    detail: "boom".to_string()
                },
            ]
        );
    }
}
