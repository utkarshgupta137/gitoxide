use git_hash::oid;

use crate::{index, index::checkout::Collision};

pub mod checkout {
    use bstr::BString;
    use quick_error::quick_error;

    #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub struct Collision {
        /// the path that collided with something already present on disk.
        pub path: BString,
        /// The io error we encountered when checking out `path`.
        pub error_kind: std::io::ErrorKind,
    }

    pub struct Outcome {
        pub collisions: Vec<Collision>,
    }

    #[derive(Clone, Copy)]
    pub struct Options {
        /// capabilities of the file system
        pub fs: crate::fs::Capabilities,
        /// If true, we assume no file to exist in the target directory, and want exclusive access to it.
        /// This should be enabled when cloning to avoid checks for freshness of files. This also enables
        /// detection of collisions based on whether or not exclusive file creation succeeds or fails.
        pub destination_is_initially_empty: bool,
        /// If true, default false, try to checkout as much as possible and don't abort on first error which isn't
        /// due to a conflict.
        /// The operation will never fail, but count the encountered errors instead along with their paths.
        pub keep_going: bool,
        /// If true, a files creation time is taken into consideration when checking if a file changed.
        /// Can be set to false in case other tools alter the creation time in ways that interfere with our operation.
        ///
        /// Default true.
        pub trust_ctime: bool,
        /// If true, all stat fields will be used when checking for up-to-date'ness of the entry. Otherwise
        /// nano-second parts of mtime and ctime,uid, gid, inode and device number won't be used, leaving only
        /// the whole-second part of ctime and mtime and the file size to be checked.
        ///
        /// Default true.
        pub check_stat: bool,
    }

    impl Default for Options {
        fn default() -> Self {
            Options {
                fs: Default::default(),
                destination_is_initially_empty: false,
                keep_going: false,
                trust_ctime: true,
                check_stat: true,
            }
        }
    }

    quick_error! {
        #[derive(Debug)]
        pub enum Error {
            IllformedUtf8{ path: BString } {
                display("Could not convert path to UTF8: {}", path)
            }
            Time(err: std::time::SystemTimeError) {
                from()
                source(err)
                display("The clock was off when reading file related metadata after updating a file on disk")
            }
            Io(err: std::io::Error) {
                from()
                source(err)
                display("IO error while writing blob or reading file metadata or changing filetype")
            }
            ObjectNotFound{ oid: git_hash::ObjectId, path: std::path::PathBuf } {
                display("object {} for checkout at {} not found in object database", oid.to_hex(), path.display())
            }
        }
    }
}

pub fn checkout<Find>(
    index: &mut git_index::State,
    path: impl AsRef<std::path::Path>,
    mut find: Find,
    options: checkout::Options,
) -> Result<checkout::Outcome, checkout::Error>
where
    Find: for<'a> FnMut(&oid, &'a mut Vec<u8>) -> Option<git_object::BlobRef<'a>>,
{
    use std::io::ErrorKind::AlreadyExists;
    let root = path.as_ref();
    let mut buf = Vec::new();
    let mut collisions = Vec::new();
    for (entry, entry_path) in index.entries_mut_with_paths() {
        // TODO: write test for that
        if entry.flags.contains(git_index::entry::Flags::SKIP_WORKTREE) {
            continue;
        }

        let res = entry::checkout(entry, entry_path, &mut find, root, options, &mut buf);
        match res {
            Ok(()) => {}
            // TODO: use ::IsDirectory as well when stabilized instead of raw_os_error()
            #[cfg(windows)]
            Err(index::checkout::Error::Io(err))
                if err.kind() == AlreadyExists || err.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                collisions.push(Collision {
                    path: entry_path.into(),
                    error_kind: err.kind(),
                });
            }
            #[cfg(not(windows))]
            Err(index::checkout::Error::Io(err)) if err.kind() == AlreadyExists || err.raw_os_error() == Some(21) => {
                // We are here because a file existed or was blocked by a directory which shouldn't be possible unless
                // we are on a file insensitive file system.
                collisions.push(Collision {
                    path: entry_path.into(),
                    error_kind: err.kind(),
                });
            }
            Err(err) => {
                if options.keep_going {
                    todo!("keep going")
                } else {
                    return Err(err);
                }
            }
        }
    }
    Ok(checkout::Outcome { collisions })
}

pub(crate) mod entry {
    use std::{
        convert::TryInto,
        fs::{create_dir_all, OpenOptions},
        io::Write,
        time::Duration,
    };

    use bstr::BStr;
    use git_hash::oid;
    use git_index::Entry;

    use crate::index;

    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn checkout<Find>(
        entry: &mut Entry,
        entry_path: &BStr,
        find: &mut Find,
        root: &std::path::Path,
        index::checkout::Options {
            fs:
                crate::fs::Capabilities {
                    symlink,
                    executable_bit,
                    ..
                },
            destination_is_initially_empty,
            ..
        }: index::checkout::Options,
        buf: &mut Vec<u8>,
    ) -> Result<(), index::checkout::Error>
    where
        Find: for<'a> FnMut(&oid, &'a mut Vec<u8>) -> Option<git_object::BlobRef<'a>>,
    {
        let dest = root.join(git_features::path::from_byte_slice(entry_path).map_err(|_| {
            index::checkout::Error::IllformedUtf8 {
                path: entry_path.to_owned(),
            }
        })?);
        create_dir_all(dest.parent().expect("entry paths are never empty"))?; // TODO: can this be avoided to create dirs when needed only?

        match entry.mode {
            git_index::entry::Mode::FILE | git_index::entry::Mode::FILE_EXECUTABLE => {
                let obj = find(&entry.id, buf).ok_or_else(|| index::checkout::Error::ObjectNotFound {
                    oid: entry.id,
                    path: root.to_path_buf(),
                })?;
                let mut options = OpenOptions::new();
                options
                    .create_new(destination_is_initially_empty)
                    .create(!destination_is_initially_empty)
                    .write(true);
                #[cfg(unix)]
                if executable_bit && entry.mode == git_index::entry::Mode::FILE_EXECUTABLE {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o777);
                }

                let mut file = options.open(&dest)?;
                file.write_all(obj.data)?;
                // NOTE: we don't call `file.sync_all()` here knowing that some filesystems don't handle this well.
                //       revisit this once there is a bug to fix.
                update_fstat(entry, file.metadata()?)?;
            }
            git_index::entry::Mode::SYMLINK => {
                let obj = find(&entry.id, buf).ok_or_else(|| index::checkout::Error::ObjectNotFound {
                    oid: entry.id,
                    path: root.to_path_buf(),
                })?;
                let symlink_destination = git_features::path::from_byte_slice(obj.data)
                    .map_err(|_| index::checkout::Error::IllformedUtf8 { path: obj.data.into() })?;

                // TODO: how to deal with mode changes? Maybe this info can be passed once we check for whether
                // a checkout is needed at all.
                if symlink {
                    symlink::symlink_auto(symlink_destination, &dest)?;
                } else {
                    std::fs::write(&dest, obj.data)?;
                }

                update_fstat(entry, std::fs::symlink_metadata(&dest)?)?;
            }
            git_index::entry::Mode::DIR => todo!(),
            git_index::entry::Mode::COMMIT => todo!(),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn update_fstat(entry: &mut Entry, meta: std::fs::Metadata) -> Result<(), index::checkout::Error> {
        let ctime = meta
            .created()
            .map_or(Ok(Duration::default()), |x| x.duration_since(std::time::UNIX_EPOCH))?;
        let mtime = meta
            .modified()
            .map_or(Ok(Duration::default()), |x| x.duration_since(std::time::UNIX_EPOCH))?;

        let stat = &mut entry.stat;
        stat.mtime.secs = mtime
            .as_secs()
            .try_into()
            .expect("by 2038 we found a solution for this");
        stat.mtime.nsecs = mtime.subsec_nanos();
        stat.ctime.secs = ctime
            .as_secs()
            .try_into()
            .expect("by 2038 we found a solution for this");
        stat.ctime.nsecs = ctime.subsec_nanos();
        Ok(())
    }
}
