use crate::{
    store::{file, file::log},
    FullName,
};
use std::{convert::TryInto, io::Read, path::PathBuf};

impl file::Store {
    /// Implements the logic required to transform a fully qualified refname into its log name
    fn reflog_path(&self, name: FullName<'_>) -> PathBuf {
        self.base.join("logs").join(name.to_path())
    }
}

impl file::Store {
    /// Return a reflog reverse iterator for the given fully qualified `name`, reading chunks from the back into the fixed buffer `buf`.
    ///
    /// The iterator will traverse log entries from most recent to oldest, reading the underlying file in chunks from the back.
    /// Return `Ok(None)` if no reflog exists.
    pub fn reflog_iter_rev<'a, 'b, Name, E>(
        &self,
        name: Name,
        buf: &'b mut [u8],
    ) -> Result<Option<log::iter::Reverse<'b, std::fs::File>>, Error>
    where
        Name: TryInto<FullName<'a>, Error = E>,
        crate::name::Error: From<E>,
    {
        let name: FullName<'_> = name.try_into().map_err(|err| Error::RefnameValidation(err.into()))?;
        let file = match std::fs::File::open(self.reflog_path(name)) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(log::iter::reverse(file, buf)?))
    }

    /// Return a reflog forward iterator for the given fully qualified `name` and write its file contents into `buf`.
    ///
    /// The iterator will traverse log entries from oldest to newest.
    /// Return `Ok(None)` if no reflog exists.
    pub fn reflog_iter<'a, 'b, Name, E>(
        &self,
        name: Name,
        buf: &'b mut Vec<u8>,
    ) -> Result<Option<impl Iterator<Item = Result<log::Line<'b>, log::iter::decode::Error>>>, Error>
    where
        Name: TryInto<FullName<'a>, Error = E>,
        crate::name::Error: From<E>,
    {
        let name: FullName<'_> = name.try_into().map_err(|err| Error::RefnameValidation(err.into()))?;
        match std::fs::File::open(self.reflog_path(name)) {
            Ok(mut file) => {
                buf.clear();
                file.read_to_end(buf)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(log::iter::forward(buf)))
    }
}

mod error {
    use quick_error::quick_error;
    use std::io;

    quick_error! {
        /// The error returned by [file::Store::reflog_iter()].
        #[derive(Debug)]
        #[allow(missing_docs)]
        pub enum Error {
            RefnameValidation(err: crate::name::Error) {
                display("The reflog name or path is not a valid ref name")
                from()
                source(err)
            }
            Io(err: io::Error) {
                display("The reflog file could not read")
                from()
                source(err)
            }
        }
    }
}
pub use error::Error;
