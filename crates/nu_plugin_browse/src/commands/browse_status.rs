use chaser_oxide::Browser;
use futures::StreamExt;
use nu_plugin::{EngineInterface, EvaluatedCall, SimplePluginCommand};
use nu_protocol::{Category, Example, LabeledError, Record, Signature, Type, Value};
use std::mem::ManuallyDrop;

use crate::session::{has_active_session, load_ws_url, profile_dir, session_file};

#[derive(Clone)]
pub struct BrowseStatus;

impl SimplePluginCommand for BrowseStatus {
    type Plugin = crate::BrowsePlugin;

    fn name(&self) -> &str {
        "browse status"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse status")
            .input_output_type(Type::Nothing, Type::record())
            .category(Category::Network)
    }

    fn description(&self) -> &str {
        "Show the status of the persistent browser opened by 'browse open'."
    }

    fn extra_description(&self) -> &str {
        "Returns a record with the current session status, page URL, and profile path. \
         Does not launch a browser if none is active."
    }

    fn search_terms(&self) -> Vec<&str> {
        vec!["browse", "status", "session", "info", "browser"]
    }

    fn examples(&'_ self) -> Vec<Example<'_>> {
        vec![Example {
            description: "Check persistent browser status",
            example: "browse status",
            result: None,
        }]
    }

    fn run(
        &self,
        _plugin: &crate::BrowsePlugin,
        _engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: &Value,
    ) -> Result<Value, LabeledError> {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            LabeledError::new(format!("{e}")).with_label("browse status failed", call.head)
        })?;

        rt.block_on(async {
            let mut record = Record::new();

            if !has_active_session(&cwd) {
                record.push("status", Value::string("no_session", call.head));
                return Ok(Value::record(record, call.head));
            }

            let session_path = session_file(&cwd).to_string_lossy().into_owned();
            let profile_path = profile_dir(&cwd).to_string_lossy().into_owned();

            record.push("session", Value::string(&session_path, call.head));
            record.push("profile", Value::string(&profile_path, call.head));

            let ws_url = load_ws_url(&cwd).ok_or_else(|| {
                LabeledError::new("no session url").with_label("browse status failed", call.head)
            })?;

            match Browser::connect(&ws_url).await {
                Ok((mut browser, mut handler)) => {
                    let _h =
                        tokio::spawn(async move { while handler.next().await.is_some() {} });

                    let _ = browser.fetch_targets().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    let all_pages = browser.pages().await.unwrap_or_default();
                    let mut found = None;
                    for p in &all_pages {
                        if let Ok(Some(purl)) = p.url().await
                            && !purl.starts_with("chrome://")
                            && !purl.starts_with("devtools://")
                            && !purl.starts_with("about:blank")
                        {
                            found = Some(purl);
                            break;
                        }
                    }

                    match found {
                        Some(url) => {
                            record.push("status", Value::string("active", call.head));
                            record.push("url", Value::string(url, call.head));
                        }
                        None => {
                            record.push("status", Value::string("active", call.head));
                            record.push("url", Value::string("", call.head));
                        }
                    }

                    let _ = ManuallyDrop::new(browser);
                }
                Err(_) => {
                    record.push("status", Value::string("error", call.head));
                    record.push(
                        "message",
                        Value::string("Browser process not responding", call.head),
                    );
                }
            }

            Ok(Value::record(record, call.head))
        })
    }
}
