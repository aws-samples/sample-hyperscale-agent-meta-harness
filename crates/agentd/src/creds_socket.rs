//! The `credential_process` channel: a 0600 Unix socket served by agentd, read by `agentd creds`.
//! Each connection gets one line: `ProcessCredentials` JSON, or `{"error": ...}` (fail closed).

use crate::creds::CredStore;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

const MAX_REPLY: usize = 64 * 1024;

/// Who may take the credentials from the socket. A file mode and a Landlock domain do not stop a `connect()` to a pathname Unix socket, so in the
/// `landlock` mode the socket decides for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callers {
    /// The `uid` mode: the 0600 mode and the engine's ownership of the file are the check, as they have always been.
    Anyone,
    /// The `landlock` mode: only a caller of this user id, read from the connection itself (`SO_PEERCRED`).
    Uid(u32),
}

impl Callers {
    /// The callers of a run in the given mode: the `landlock` mode admits agentd's own user alone.
    pub fn for_mode(mode: crate::sandbox::Mode) -> Self {
        match mode {
            crate::sandbox::Mode::Uid => Callers::Anyone,
            crate::sandbox::Mode::Landlock => Callers::Uid(rustix::process::geteuid().as_raw()),
        }
    }

    /// Whether a caller whose user id is `peer_uid` (`None` when the connection did not say) may take the credentials.
    pub fn allows(&self, peer_uid: Option<u32>) -> bool {
        match self {
            Callers::Anyone => true,
            Callers::Uid(uid) => peer_uid == Some(*uid),
        }
    }
}

/// Binds the socket (0600) and serves until the handle is aborted.
pub async fn serve_credentials(
    path: &Path,
    store: Arc<CredStore>,
) -> std::io::Result<JoinHandle<()>> {
    serve_credentials_owned(path, store, None).await
}

/// As [`serve_credentials`], with the socket owned by the engine's uid and gid when they differ.
pub async fn serve_credentials_owned(
    path: &Path,
    store: Arc<CredStore>,
    owner: Option<(u32, u32)>,
) -> std::io::Result<JoinHandle<()>> {
    serve_credentials_for(path, store, owner, Callers::Anyone).await
}

/// As [`serve_credentials_owned`], with the callers the socket admits.
pub async fn serve_credentials_for(
    path: &Path,
    store: Arc<CredStore>,
    owner: Option<(u32, u32)>,
    callers: Callers,
) -> std::io::Result<JoinHandle<()>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    Ok(tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(answer(stream, store.clone(), callers));
        }
    }))
}

async fn answer(mut stream: UnixStream, store: Arc<CredStore>, callers: Callers) {
    // The caller's user id comes from the connection itself. A caller that is not admitted is answered before the store is asked, so a
    // refusal costs nothing and shows nothing.
    let peer = stream.peer_cred().ok().map(|c| c.uid());
    if !callers.allows(peer) {
        // The request is read and dropped so that the refusal is not turned into a reset by the unread bytes.
        let mut request = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request)).await;
        let reply =
            serde_json::json!({ "error": "credential socket: the caller is not authorised" });
        let _ = stream.write_all(reply.to_string().as_bytes()).await;
        let _ = stream.write_all(b"\n").await;
        let _ = stream.shutdown().await;
        return;
    }
    let mut buf = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
    let reply = match store.current().await {
        Ok(c) => serde_json::to_string(&c).unwrap_or_default(),
        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
    };
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.shutdown().await;
}

/// Client side of `agentd creds`: the credentials JSON, or the store's error.
pub async fn fetch_credentials(path: &Path) -> Result<String, String> {
    let mut s = UnixStream::connect(path)
        .await
        .map_err(|e| format!("credential socket {}: {e}", path.display()))?;
    s.write_all(b"credentials\n")
        .await
        .map_err(|e| format!("credential socket write: {e}"))?;
    let mut buf = Vec::new();
    (&mut s)
        .take(MAX_REPLY as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| format!("credential socket read: {e}"))?;
    let text =
        String::from_utf8(buf).map_err(|_| "credential socket reply is not UTF-8".to_string())?;
    let text = text.trim().to_string();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "credential socket reply is not JSON".to_string())?;
    if let Some(e) = v.get("error").and_then(serde_json::Value::as_str) {
        return Err(e.to_string());
    }
    Ok(text)
}
