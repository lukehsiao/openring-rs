//! Helpers shared by the test modules: log capture, and running a future
//! to completion from a plain `#[test]`.

use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// Run `future` to completion on a fresh single-threaded runtime. For
/// tests that must stay sync, such as hegel properties and tests that wrap
/// a run in [`warnings_logged`].
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

/// Capture everything openring logs at warn level and above while `f` runs,
/// on this thread, through tracing's own subscriber machinery.
pub(crate) fn warnings_logged(f: impl FnOnce()) -> String {
    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(buffer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes).expect("log output is UTF-8")
}
