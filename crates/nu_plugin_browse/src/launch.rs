use chaser_oxide::{Browser, BrowserConfig, Page, handler::viewport::Viewport};
use futures::StreamExt;
use std::error::Error;
use std::time::Duration;

use crate::session::{DEFAULT_DEBUG_PORT, ensure_profile_dir, profile_dir, save_session};

const FIRST_PAGE_WAIT: Duration = Duration::from_millis(500);

pub fn viewport_config(with_head: bool) -> Option<Viewport> {
    if with_head {
        None
    } else {
        Some(Viewport {
            width: 1920,
            height: 1080,
            device_scale_factor: Some(1.0),
            emulating_mobile: false,
            has_touch: false,
            is_landscape: false,
        })
    }
}

pub async fn launch_persistent(
    with_head: bool,
    cwd: &str,
) -> Result<(Browser, Page), Box<dyn Error>> {
    let dir = profile_dir(cwd);
    std::fs::create_dir_all(&dir)?;

    let mut config = BrowserConfig::builder()
        .port(DEFAULT_DEBUG_PORT)
        .user_data_dir(&dir)
        .window_size(1920, 1080)
        .viewport(viewport_config(with_head))
        .arg("--test-type");

    if with_head {
        config = config.with_head();
    } else {
        config = config.new_headless_mode();
    }

    let (mut browser, mut handler) = Browser::launch(config.build()?).await?;

    let ws_url = format!("http://localhost:{}/json/version", DEFAULT_DEBUG_PORT);
    save_session(cwd, &ws_url)?;

    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = first_page(&mut browser).await?;
    Ok((browser, page))
}

pub async fn launch_ephemeral(
    with_head: bool,
    cwd: &str,
) -> Result<(Browser, Page), Box<dyn Error>> {
    let dir = ensure_profile_dir(cwd);

    let mut config = BrowserConfig::builder()
        .user_data_dir(&dir)
        .window_size(1920, 1080)
        .viewport(viewport_config(with_head))
        .arg("--test-type");

    if with_head {
        config = config.with_head();
    } else {
        config = config.new_headless_mode();
    }

    let (mut browser, mut handler) = Browser::launch(config.build()?).await?;

    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = first_page(&mut browser).await?;
    Ok((browser, page))
}

pub async fn first_page(browser: &mut Browser) -> Result<Page, Box<dyn Error>> {
    tokio::time::sleep(FIRST_PAGE_WAIT).await;
    match browser.pages().await {
        Ok(pages) if !pages.is_empty() => {
            let mut pages = pages.into_iter();
            let page = pages.next().unwrap();
            for other in pages {
                let _ = other.close().await;
            }
            Ok(page)
        }
        _ => browser.new_page("about:blank").await.map_err(Into::into),
    }
}
