use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use super::{ActivationError, internal};

pub(super) async fn read_string(path: PathBuf) -> Result<String, ActivationError> {
    fs::read_to_string(path)
        .await
        .map_err(|source| internal("failed to read activation recovery state", source))
}

pub(super) async fn read_json<T: DeserializeOwned>(path: PathBuf) -> Result<T, ActivationError> {
    let payload = fs::read(path)
        .await
        .map_err(|source| internal("failed to read persisted activation state", source))?;
    serde_json::from_slice(&payload)
        .map_err(|source| internal("failed to parse persisted activation state", source))
}

pub(super) async fn create_private_dir(path: &Path) -> Result<(), ActivationError> {
    fs::create_dir_all(path)
        .await
        .map_err(|source| internal("failed to create a private activation directory", source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|source| {
                internal("failed to secure a private activation directory", source)
            })?;
    }
    Ok(())
}

pub(super) async fn write_new_file(
    path: PathBuf,
    contents: &[u8],
    mode: u32,
) -> Result<(), ActivationError> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(mode);
    #[cfg(not(unix))]
    let _ = mode;

    let mut file = options
        .open(&path)
        .await
        .map_err(|source| internal("failed to create a private activation file", source))?;
    file.write_all(contents)
        .await
        .map_err(|source| internal("failed to write a private activation file", source))?;
    file.sync_all()
        .await
        .map_err(|source| internal("failed to sync a private activation file", source))
}

pub(super) async fn atomic_write(
    parent: &Path,
    target: PathBuf,
    prefix: &str,
    contents: &[u8],
    mode: u32,
) -> Result<(), ActivationError> {
    let stage_path = parent.join(format!("{prefix}-{}.tmp", Uuid::new_v4()));
    // 先同步临时文件再同目录改名，避免断电后暴露部分写入的激活状态。
    //
    // Sync the staged file before a same-directory rename so a crash cannot expose partial state.
    let result = async {
        write_new_file(stage_path.clone(), contents, mode).await?;
        fs::rename(&stage_path, target)
            .await
            .map_err(|source| internal("failed to commit an activation file", source))?;
        sync_directory(parent.to_path_buf()).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(stage_path).await;
    }
    result
}

#[cfg(unix)]
pub(super) async fn sync_directory(path: PathBuf) -> Result<(), ActivationError> {
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(|source| internal("failed to join a directory sync task", source))?
        .map_err(|source| internal("failed to sync an activation directory", source))
}

#[cfg(not(unix))]
async fn sync_directory(_path: PathBuf) -> Result<(), ActivationError> {
    Ok(())
}
