//! Process-wide progress area, shared between the progress bars and the
//! tracing writer so log lines and bar redraws don't splice on a tty.

use std::{
    io::{self, Write},
    sync::OnceLock,
};

use indicatif::{MultiProgress, ProgressBar};

fn multi() -> &'static MultiProgress {
    static MULTI: OnceLock<MultiProgress> = OnceLock::new();
    MULTI.get_or_init(MultiProgress::new)
}

/// Register a progress bar with the shared progress area.
pub(crate) fn add(pb: ProgressBar) -> ProgressBar {
    multi().add(pb)
}

/// An [`io::Write`] for tracing that clears the active progress bars, writes
/// the log line to stderr, and lets the bars redraw, instead of splicing the
/// two streams together mid-line.
pub struct SuspendingStderr;

impl Write for SuspendingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        multi().suspend(|| io::stderr().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        multi().suspend(|| io::stderr().flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The suspend wrapper must hand every byte through to stderr, with or
    // without an active bar; swallowed log lines would be far worse than the
    // splicing it prevents.
    #[test]
    fn suspending_stderr_passes_bytes_through() {
        let mut w = SuspendingStderr;
        assert_eq!(w.write(b"test line\n").unwrap(), 10);
        w.flush().unwrap();

        let _pb = add(ProgressBar::hidden());
        assert_eq!(w.write(b"with a bar\n").unwrap(), 11);
    }
}
