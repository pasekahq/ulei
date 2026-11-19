// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The ingress's [`FileSource`] over the Bot API: `getFile`, then a download from the file base
//! bounded by the caller's cap and the server's limit.

use std::io::{self, Write};

use super::client::Client;
use super::error::{Error, ErrorKind};
use crate::ingress::{FileSource, RemoteFile};

impl FileSource for Client {
    fn locate(&self, file_id: &str) -> io::Result<RemoteFile> {
        let info = self.get_file(file_id).map_err(into_io)?;
        let file_path = info.file_path.ok_or_else(|| {
            into_io(Error::new(
                ErrorKind::InvalidResponse,
                "file has no file_path",
                &self.config().token,
            ))
        })?;
        Ok(RemoteFile { file_path })
    }

    fn download(&self, file: &RemoteFile, max_bytes: u64, out: &mut dyn Write) -> io::Result<u64> {
        let limit = max_bytes.min(self.config().max_file_bytes);
        self.download_path(&file.file_path, limit, out)
            .map_err(into_io)
    }
}

/// The ingress names a failed download by its I/O kind in the note the model sees.
fn into_io(err: Error) -> io::Error {
    let kind = match err.kind {
        ErrorKind::Timeout | ErrorKind::ConnectTimeout => io::ErrorKind::TimedOut,
        ErrorKind::TooLarge => io::ErrorKind::FileTooLarge,
        ErrorKind::Network => io::ErrorKind::ConnectionAborted,
        ErrorKind::Unauthorized | ErrorKind::Forbidden => io::ErrorKind::PermissionDenied,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
}
