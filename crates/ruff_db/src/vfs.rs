use std::sync::Arc;

use countme::Count;
use dashmap::mapref::entry::Entry;

pub use path::{VendoredPath, VendoredPathBuf, VfsPath};

use crate::file_system::{FileRevision, FileSystemPath};
use crate::{Db, FxDashMap};

mod path;

/// Virtual file system that supports files from different sources.
///
/// The [`Vfs`] supports accessing files from:
///
/// * The file system
/// * Vendored files that are part of the distributed Ruff binary
///
/// ## Why do both the [`Vfs`] and [`FileSystem`](crate::FileSystem) trait exist?
///
/// It would have been an option to define [`FileSystem`](crate::FileSystem) in a way that all its operation accept
/// a [`VfsPath`]. This would have allowed to unify most of [`Vfs`] and [`FileSystem`](crate::FileSystem). The reason why they are
/// separate is that not all operations are supported for all [`VfsPath`]s:
///
/// * The only relevant operations for [`VendoredPath`]s are testing for existence and reading the content.
/// * The vendored file system is immutable and doesn't support writing nor does it require watching for changes.
/// * There's no requirement to walk the vendored typesystem.
///
/// The other reason is that most operations know if they are working with vendored or file system paths.
/// Requiring them to convert the path to an `VfsPath` to test if the file exist is cumbersome.
///
/// The main downside of the approach is that vendored files needs their own stubbing mechanism.
#[derive(Default)]
pub struct Vfs {
    inner: Arc<VfsInner>,
}

#[derive(Default)]
struct VfsInner {
    /// Lookup table that maps the path to a salsa interned [`VfsFile`] instance.
    ///
    /// The map also stores entries for files that don't exist on the file system. This is necessary
    /// so that queries that depend on the existence of a file are re-executed when the file is created.
    ///
    files_by_path: FxDashMap<VfsPath, VfsFile>,
    vendored: VendoredVfs,
}

impl Vfs {
    /// Creates a new [`Vfs`] instance where the vendored files are stubbed out.
    pub fn with_stubbed_vendored() -> Self {
        Self {
            inner: Arc::new(VfsInner {
                vendored: VendoredVfs::Stubbed(FxDashMap::default()),
                ..VfsInner::default()
            }),
        }
    }

    /// Looks up a file by its path.
    ///
    /// For a non-existing file, creates a new salsa [`VfsFile`] ingredient and stores it for future lookups.
    ///
    /// The operation always succeeds even if the path doesn't exist on disk, isn't accessible or if the path points to a directory.
    /// In these cases, a file with status [`FileStatus::Deleted`] is returned.
    pub fn file(&self, db: &dyn Db, path: &FileSystemPath) -> VfsFile {
        *self
            .inner
            .files_by_path
            .entry(VfsPath::FileSystem(path.to_path_buf()))
            .or_insert_with(|| {
                let metadata = db.file_system().metadata(path);

                match metadata {
                    Ok(metadata) if metadata.file_type().is_file() => VfsFile::new(
                        db,
                        VfsPath::FileSystem(path.to_path_buf()),
                        metadata.permissions(),
                        metadata.revision(),
                        FileStatus::Exists,
                        Count::default(),
                    ),
                    _ => VfsFile::new(
                        db,
                        VfsPath::FileSystem(path.to_path_buf()),
                        None,
                        FileRevision::zero(),
                        FileStatus::Deleted,
                        Count::default(),
                    ),
                }
            })
    }

    /// Lookups a vendored file by its path. Returns `Some` if a vendored file for the given path
    /// exists and `None` otherwise.
    pub fn vendored(&self, db: &dyn Db, path: &VendoredPath) -> Option<VfsFile> {
        let file = match self
            .inner
            .files_by_path
            .entry(VfsPath::Vendored(path.to_path_buf()))
        {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let revision = self.inner.vendored.revision(path)?;

                let file = VfsFile::new(
                    db,
                    VfsPath::Vendored(path.to_path_buf()),
                    Some(0o444),
                    revision,
                    FileStatus::Exists,
                    Count::default(),
                );

                entry.insert(file);

                file
            }
        };

        Some(file)
    }

    /// Stubs out the vendored files with the given content.
    ///
    /// ## Panics
    /// If there are pending snapshots referencing this `Vfs` instance.
    pub fn stub_vendored<P, S>(&mut self, vendored: impl IntoIterator<Item = (P, S)>)
    where
        P: AsRef<VendoredPath>,
        S: ToString,
    {
        let inner = Arc::get_mut(&mut self.inner).unwrap();

        let stubbed = FxDashMap::default();

        for (path, content) in vendored {
            stubbed.insert(path.as_ref().to_path_buf(), content.to_string());
        }

        inner.vendored = VendoredVfs::Stubbed(stubbed);
    }

    /// Creates a salsa like snapshot of the files. The instances share
    /// the same path to file mapping.
    pub fn snapshot(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    fn read(&self, db: &dyn Db, path: &VfsPath) -> String {
        match path {
            VfsPath::FileSystem(path) => db.file_system().read(path).unwrap_or_default(),

            VfsPath::Vendored(vendored) => db
                .vfs()
                .inner
                .vendored
                .read(vendored)
                .expect("Vendored file to exist"),
        }
    }
}

impl std::fmt::Debug for Vfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();

        for entry in self.inner.files_by_path.iter() {
            map.entry(entry.key(), entry.value());
        }
        map.finish()
    }
}

#[salsa::input]
pub struct VfsFile {
    /// The path of the file.
    #[id]
    #[return_ref]
    pub path: VfsPath,

    /// The unix permissions of the file. Only supported on unix systems. Always `None` on Windows
    /// or when the file has been deleted.
    pub permissions: Option<u32>,

    /// The file revision. A file has changed if the revisions don't compare equal.
    pub revision: FileRevision,

    /// The status of the file.
    ///
    /// Salsa doesn't support deleting inputs. The only way to signal to the depending queries that
    /// the file has been deleted is to change the status to `Deleted`.
    pub status: FileStatus,

    /// Counter that counts the number of created file instances and active file instances.
    /// Only enabled in debug builds.
    #[allow(unused)]
    count: Count<VfsFile>,
}

impl VfsFile {
    /// Reads the content of the file into a [`String`].
    ///
    /// Reading the same file multiple times isn't guaranteed to return the same content. It's possible
    /// that the file has been modified in between the reads. It's even possible that a file that
    /// is considered to exist has been deleted in the meantime. If this happens, then the method returns
    /// an empty string, which is the closest to the content that the file contains now. Returning
    /// an empty string shouldn't be a problem because the query will be re-executed as soon as the
    /// changes are applied to the database.
    #[allow(unused)]
    pub(crate) fn read(&self, db: &dyn Db) -> String {
        let path = self.path(db);

        if path.is_file_system_path() {
            // Add a dependency on the revision to ensure the operation gets re-executed when the file changes.
            let _ = self.revision(db);
        }

        db.vfs().read(db, path)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FileStatus {
    /// The file exists.
    Exists,

    /// The file was deleted, didn't exist to begin with or the path isn't a file.
    Deleted,
}

#[derive(Default, Debug)]
enum VendoredVfs {
    #[default]
    Real,
    Stubbed(FxDashMap<VendoredPathBuf, String>),
}

impl VendoredVfs {
    fn revision(&self, path: &VendoredPath) -> Option<FileRevision> {
        match self {
            VendoredVfs::Real => todo!(),
            VendoredVfs::Stubbed(stubbed) => stubbed
                .contains_key(&path.to_path_buf())
                .then_some(FileRevision::new(1)),
        }
    }

    fn read(&self, path: &VendoredPath) -> Option<String> {
        match self {
            VendoredVfs::Real => todo!(),
            VendoredVfs::Stubbed(stubbed) => stubbed.get(&path.to_path_buf()).as_deref().cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::file_system::{FileRevision, FileSystemPath};
    use crate::tests::TestDb;
    use crate::vfs::{FileStatus, VendoredPath};
    use crate::Db;

    #[test]
    fn file_system_existing_file() {
        let mut db = TestDb::new();

        db.file_system_mut()
            .write_files([("test.py", "print('Hello world')")]);

        let test = db.file(FileSystemPath::new("test.py"));

        assert_eq!(test.status(&db), FileStatus::Exists);
        assert_eq!(test.permissions(&db), Some(0o755));
        assert_ne!(test.revision(&db), FileRevision::zero());
        assert_eq!(&test.read(&db), "print('Hello world')");
    }

    #[test]
    fn file_system_non_existing_file() {
        let db = TestDb::new();

        let test = db.file(FileSystemPath::new("test.py"));

        assert_eq!(test.status(&db), FileStatus::Deleted);
        assert_eq!(test.permissions(&db), None);
        assert_eq!(test.revision(&db), FileRevision::zero());
        assert_eq!(&test.read(&db), "");
    }

    #[test]
    fn stubbed_vendored_file() {
        let mut db = TestDb::new();

        db.vfs_mut()
            .stub_vendored([("test.py", "def foo() -> str")]);

        let test = db
            .vendored_file(VendoredPath::new("test.py"))
            .expect("Vendored file to exist.");

        assert_eq!(test.status(&db), FileStatus::Exists);
        assert_eq!(test.permissions(&db), Some(0o444));
        assert_ne!(test.revision(&db), FileRevision::zero());
        assert_eq!(&test.read(&db), "def foo() -> str");
    }

    #[test]
    fn stubbed_vendored_file_non_existing() {
        let db = TestDb::new();

        assert_eq!(db.vendored_file(VendoredPath::new("test.py")), None);
    }
}
