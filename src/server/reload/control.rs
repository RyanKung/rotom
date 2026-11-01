//! Private, bounded local command/acknowledgement protocol, independent of API keys.

use super::{ReloadError, Runtime};
use crate::{Error, Result};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::fd::AsRawFd,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
    owner: u32,
    inode: u64,
    // Never unlink this file: its stable inode serializes stale-socket cleanup.
    _lock: fs::File,
}

impl ControlSocket {
    pub(super) fn bind(config_path: &Path) -> Result<Self> {
        let path = config_path.with_extension("reload.sock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(config_path.with_extension("reload.lock"))?;
        // SAFETY: the descriptor is owned by `lock` and stays open for this call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(ReloadError::ControlInUse.into());
            }
            return Err(error.into());
        }
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                // Never unlink a live socket, a symlink, or an unrelated regular file.
                let stale = fs::symlink_metadata(&path)?.file_type().is_socket()
                    && UnixStream::connect(&path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused);
                if !stale {
                    return Err(ReloadError::ControlInUse.into());
                }
                fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = fs::symlink_metadata(&path)?;
        let owner = metadata.uid();
        let socket = Self {
            listener,
            path,
            owner,
            inode: metadata.ino(),
            _lock: lock,
        };
        fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))?;
        Ok(socket)
    }

    pub(super) async fn serve(&self, runtime: &Arc<Runtime>) -> Result<()> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            if !stream
                .peer_cred()
                .is_ok_and(|peer| peer.uid() == self.owner)
            {
                continue;
            }
            // A bounded read prevents an idle local client from blocking reload forever.
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                let mut command = [0; 7];
                stream.read_exact(&mut command).await?;
                if &command != b"reload\n" {
                    return Err(Error::from(ReloadError::InvalidCommand));
                }
                Ok(())
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                continue;
            }
            // Publish is never cancelled by client disconnect or a response timeout.
            let reply = match runtime.reload().await {
                Ok(()) => serde_json::json!({"ok": true}),
                Err(error) => serde_json::json!({"ok": false, "error": error.to_string()}),
            };
            let bytes = format!("{reply}\n");
            let _ =
                tokio::time::timeout(Duration::from_secs(2), stream.write_all(bytes.as_bytes()))
                    .await;
        }
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn request(config_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(config_path.with_extension("reload.sock"))?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    stream.write_all(b"reload\n")?;
    let mut reply = String::new();
    BufReader::new(stream.take(64 * 1024)).read_line(&mut reply)?;
    let reply: serde_json::Value = serde_json::from_str(&reply)?;
    if reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(ReloadError::Rejected(
            reply
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("invalid reload acknowledgement")
                .to_owned(),
        )
        .into())
    }
}
