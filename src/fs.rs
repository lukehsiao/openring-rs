//! The file system edge. `run` reads the template and the urls file through
//! [`Fs`]. Tests swap the disk for files held in memory with
//! [`Fs::create_null`].

use std::{io, path::Path};

#[derive(Debug, Clone)]
pub(crate) struct Fs {
    disk: Disk,
}

#[derive(Debug, Clone)]
enum Disk {
    Real,
    #[cfg(test)]
    Stubbed(std::sync::Arc<std::collections::HashMap<std::path::PathBuf, String>>),
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
            Disk::Stubbed(files) => files.get(path).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Nulled Fs: no file configured at {}", path.display()),
                )
            }),
        }
    }
}

#[cfg(test)]
impl Fs {
    /// A file system holding only `files`, as path and text. Every other
    /// path is missing.
    pub(crate) fn create_null<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut by_path = std::collections::HashMap::new();
        for (path, text) in files {
            // `/a//b` and `/a/b` name the same file, so a second entry
            // would silently replace the first.
            assert!(
                by_path
                    .insert(std::path::PathBuf::from(path), text.to_string())
                    .is_none(),
                "Nulled Fs: {path} is configured twice"
            );
        }
        Self {
            disk: Disk::Stubbed(std::sync::Arc::new(by_path)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, io::Write, path::Path};

    use super::Fs;

    // Narrow integration tests against real files.

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

    // The nulled file system.

    #[test]
    fn a_nulled_fs_reads_its_configured_files_only() {
        let fs = Fs::create_null([("/etc/urls", "https://a.example/")]);
        assert_eq!(
            fs.read_to_string(Path::new("/etc/urls")).unwrap(),
            "https://a.example/"
        );
        let err = fs.read_to_string(Path::new("/etc/other")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            err.to_string(),
            "Nulled Fs: no file configured at /etc/other"
        );
    }
}
