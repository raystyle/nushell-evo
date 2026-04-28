use nu_plugin::{EngineInterface, EvaluatedCall, SimplePluginCommand};
use nu_protocol::{Category, Example, LabeledError, Record, Signature, Type, Value};

use crate::session::{has_active_session, try_close_existing};

#[derive(Clone)]
pub struct BrowseClose;

impl SimplePluginCommand for BrowseClose {
    type Plugin = crate::BrowsePlugin;

    fn name(&self) -> &str {
        "browse close"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse close")
            .input_output_type(Type::Nothing, Type::record())
            .category(Category::Network)
    }

    fn description(&self) -> &str {
        "Close the persistent browser opened by 'browse open'."
    }

    fn extra_description(&self) -> &str {
        "Closes the browser and removes the session file. \
         The profile directory (.nu_browse_profile) is preserved for next time."
    }

    fn search_terms(&self) -> Vec<&str> {
        vec!["browse", "close", "browser", "shutdown", "stop"]
    }

    fn examples(&'_ self) -> Vec<Example<'_>> {
        vec![Example {
            description: "Close the persistent browser",
            example: "browse close",
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
            LabeledError::new(format!("{e}")).with_label("browse close failed", call.head)
        })?;

        rt.block_on(async {
            if !has_active_session(&cwd) {
                let mut record = Record::new();
                record.push("status", Value::string("no_session", call.head));
                return Ok(Value::record(record, call.head));
            }

            try_close_existing(&cwd).await;

            let mut record = Record::new();
            record.push("status", Value::string("closed", call.head));
            Ok(Value::record(record, call.head))
        })
    }
}
