use std::{fs::File, io, path::Path};

#[cfg(unix)]
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    io::{AsRawFd, FromRawFd, RawFd},
};

pub(crate) fn open_default_secret_file(
    root_directory_path: &Path,
    descendant_directory_names: &[&str],
    file_name: &str,
) -> io::Result<Option<File>> {
    #[cfg(unix)]
    {
        let mut directory_handle = match open_private_directory(root_directory_path) {
            Ok(directory_handle) => directory_handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        for directory_name in descendant_directory_names {
            directory_handle = match open_private_directory_at(&directory_handle, directory_name) {
                Ok(directory_handle) => directory_handle,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        }
        match open_regular_file_at(&directory_handle, file_name) {
            Ok(file_handle) => Ok(Some(file_handle)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(unix))]
    {
        let file_path = descendant_directory_names
            .iter()
            .fold(root_directory_path.to_path_buf(), |path, component| {
                path.join(component)
            })
            .join(file_name);
        match File::open(file_path) {
            Ok(file_handle) => Ok(Some(file_handle)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn open_private_directory(directory_path: &Path) -> io::Result<File> {
    let directory_handle = open_path_no_follow(directory_path, libc::O_RDONLY | libc::O_DIRECTORY)?;
    validate_and_restrict_directory(&directory_handle)?;
    Ok(directory_handle)
}

#[cfg(unix)]
fn open_private_directory_at(
    parent_directory_handle: &File,
    directory_name: &str,
) -> io::Result<File> {
    let directory_handle = open_at_no_follow(
        parent_directory_handle,
        directory_name,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    validate_and_restrict_directory(&directory_handle)?;
    Ok(directory_handle)
}

#[cfg(unix)]
fn open_regular_file_at(parent_directory_handle: &File, file_name: &str) -> io::Result<File> {
    let file_handle = open_at_no_follow(parent_directory_handle, file_name, libc::O_RDONLY)?;
    let metadata = file_handle.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe default secret file",
        ));
    }
    file_handle.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file_handle)
}

#[cfg(unix)]
fn validate_and_restrict_directory(directory_handle: &File) -> io::Result<()> {
    let metadata = directory_handle.metadata()?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe default secret directory",
        ));
    }
    directory_handle.set_permissions(std::fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn open_path_no_follow(path: &Path, flags: libc::c_int) -> io::Result<File> {
    use std::os::unix::ffi::OsStrExt;

    let path_bytes = path.as_os_str().as_bytes();
    let path = std::ffi::CString::new(path_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let raw_file_descriptor =
        unsafe { libc::open(path.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    file_from_raw_descriptor(raw_file_descriptor)
}

#[cfg(unix)]
fn open_at_no_follow(
    parent_directory_handle: &File,
    name: &str,
    flags: libc::c_int,
) -> io::Result<File> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let raw_file_descriptor = unsafe {
        libc::openat(
            parent_directory_handle.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    file_from_raw_descriptor(raw_file_descriptor)
}

#[cfg(unix)]
fn file_from_raw_descriptor(raw_file_descriptor: RawFd) -> io::Result<File> {
    if raw_file_descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(raw_file_descriptor) })
    }
}
