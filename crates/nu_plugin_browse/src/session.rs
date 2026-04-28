use chaser_oxide::Browser;
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_DEBUG_PORT: u32 = 9223;
pub const PROFILE_DIR: &str = ".nu_browse_profile";
pub const SESSION_FILE: &str = ".session";

pub fn profile_dir(cwd: &str) -> PathBuf {
    PathBuf::from(cwd).join(PROFILE_DIR)
}

pub fn ensure_profile_dir(cwd: &str) -> PathBuf {
    let dir = profile_dir(cwd);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn session_file(cwd: &str) -> PathBuf {
    profile_dir(cwd).join(SESSION_FILE)
}

pub fn has_active_session(cwd: &str) -> bool {
    session_file(cwd).exists()
}

pub fn load_ws_url(cwd: &str) -> Option<String> {
    std::fs::read_to_string(session_file(cwd))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_session(cwd: &str, ws_url: &str) -> std::io::Result<()> {
    let dir = profile_dir(cwd);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(session_file(cwd), ws_url)
}

pub fn clear_session(cwd: &str) -> std::io::Result<()> {
    if session_file(cwd).exists() {
        std::fs::remove_file(session_file(cwd))
    } else {
        Ok(())
    }
}

pub async fn try_close_existing(cwd: &str) {
    if let Some(ws_url) = load_ws_url(cwd)
        && let Ok((mut browser, mut handler)) = Browser::connect(&ws_url).await
    {
        let _handle = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let _ = browser.close().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = clear_session(cwd);
}
