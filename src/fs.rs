//! The file system edge. `run` reads the template and the urls file, and
//! the cache module loads and saves the cache file, through [`Fs`]. Tests
//! swap the disk for files held in memory with `Fs::create_null`.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::Path,
};

#[derive(Debug, Clone)]
pub(crate) struct Fs {
    disk: Disk,
}

#[derive(Debug, Clone)]
enum Disk {
    Real,
    #[cfg(test)]
    Stubbed(stub::Files),
}

impl Fs {
    /// The real file system.
    pub(crate) fn create() -> Self {
        Self { disk: Disk::Real }
    }

    /// Read a whole file as UTF-8 text. A file in another encoding is an
    /// [`io::ErrorKind::InvalidData`] error.
    pub(crate) fn read_to_string(&self, path: &Path) -> io::Result<String> {
        match &self.disk {
            Disk::Real => std::fs::read_to_string(path),
            #[cfg(test)]
            Disk::Stubbed(files) => files.read(path),
        }
    }

    /// Read a whole file as UTF-8 text while holding a shared lock on it, so
    /// another openring process cannot replace it halfway through the read.
    pub(crate) fn read_locked(&self, path: &Path) -> io::Result<String> {
        match &self.disk {
            Disk::Real => {
                let mut file = File::open(path)?;
                file.lock_shared()?;
                let mut text = String::new();
                file.read_to_string(&mut text)?;
                Ok(text)
            }
            #[cfg(test)]
            Disk::Stubbed(files) => files.read(path),
        }
    }

    /// Replace a file's contents with what `write` produces, creating any
    /// missing parent directories. An exclusive lock covers the whole
    /// replacement, so readers holding the shared lock never see a half
    /// written or emptied file.
    pub(crate) fn write_locked(
        &self,
        path: &Path,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        match &self.disk {
            Disk::Real => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Open without truncating: the file may only be emptied after
                // the exclusive lock is held.
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)?;
                file.lock()?;
                file.set_len(0)?;
                let mut w = BufWriter::new(file);
                write(&mut w)?;
                // BufWriter's Drop swallows write errors; flush so they surface.
                w.flush()
            }
            #[cfg(test)]
            Disk::Stubbed(files) => files.write(path, write),
        }
    }
}

#[cfg(test)]
impl Fs {
    /// A file system holding only `files`, as path and text. Every other
    /// path is missing. Writes land in memory, so a later read sees them.
    pub(crate) fn create_null<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            disk: Disk::Stubbed(stub::Files::new(files)),
        }
    }
}

#[cfg(test)]
mod stub {
    use std::{
        collections::HashMap,
        io::{self, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex, PoisonError},
    };

    /// Files held in memory. Clones share them.
    #[derive(Debug, Clone)]
    pub(super) struct Files(Arc<Mutex<HashMap<PathBuf, String>>>);

    impl Files {
        pub(super) fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
            let mut by_path = HashMap::new();
            for (path, text) in files {
                // `/a//b` and `/a/b` name the same file, so a second entry
                // would silently replace the first.
                assert!(
                    by_path
                        .insert(PathBuf::from(path), text.to_string())
                        .is_none(),
                    "Nulled Fs: {path} is configured twice"
                );
            }
            Self(Arc::new(Mutex::new(by_path)))
        }

        pub(super) fn read(&self, path: &Path) -> io::Result<String> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(path)
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Nulled Fs: no file configured at {}", path.display()),
                    )
                })
        }

        pub(super) fn write(
            &self,
            path: &Path,
            write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
        ) -> io::Result<()> {
            let mut bytes = Vec::new();
            write(&mut bytes)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.to_path_buf(), text);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{self, Write},
        path::Path,
        time::Duration,
    };

    use super::Fs;

    #[test]
    fn reads_a_real_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "text").unwrap();
        assert_eq!(Fs::create().read_to_string(file.path()).unwrap(), "text");
    }

    #[test]
    fn a_missing_real_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Fs::create()
            .read_to_string(&dir.path().join("missing"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_real_file_that_is_not_utf8_is_invalid_data() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[0xFF, 0xFE, b'\n']).unwrap();
        let err = Fs::create().read_to_string(file.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_locked_write_creates_parent_directories_and_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("cache.json");
        let fs = Fs::create();
        fs.write_locked(&path, |w| w.write_all(b"a longer first version"))
            .unwrap();
        fs.write_locked(&path, |w| w.write_all(b"second")).unwrap();
        assert_eq!(fs.read_locked(&path).unwrap(), "second");
    }

    #[test]
    fn a_locked_write_waits_for_readers_instead_of_truncating_under_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let fs = Fs::create();
        fs.write_locked(&path, |w| w.write_all(b"first")).unwrap();

        // A concurrent reader mid-read: holds the shared lock.
        let reader = File::open(&path).unwrap();
        reader.lock_shared().unwrap();

        // A second write must block on the lock without having touched the
        // file; truncating first would yank the data out from under readers.
        let writer = {
            let (fs, path) = (fs.clone(), path.clone());
            std::thread::spawn(move || fs.write_locked(&path, |w| w.write_all(b"second")))
        };
        // Give the writer ample time to reach its truncation.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");

        drop(reader);
        writer.join().unwrap().unwrap();
        assert_eq!(fs.read_locked(&path).unwrap(), "second");
    }

    #[test]
    fn a_nulled_fs_reads_its_configured_files_only() {
        let fs = Fs::create_null([("/etc/urls", "https://a.example/")]);
        assert_eq!(
            fs.read_to_string(Path::new("/etc/urls")).unwrap(),
            "https://a.example/"
        );
        let err = fs.read_locked(Path::new("/etc/other")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            err.to_string(),
            "Nulled Fs: no file configured at /etc/other"
        );
    }

    #[test]
    fn a_nulled_write_is_read_back_by_any_clone() {
        let fs = Fs::create_null([]);
        fs.clone()
            .write_locked(Path::new("/cache.json"), |w| w.write_all(b"{}"))
            .unwrap();
        assert_eq!(fs.read_locked(Path::new("/cache.json")).unwrap(), "{}");
    }
}
