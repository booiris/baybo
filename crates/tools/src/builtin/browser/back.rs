use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolOutput};

pub struct BrowserBackTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserBackTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserBackTool {
    fn name(&self) -> &str {
        "browser_back"
    }

    fn description(&self) -> &str {
        "Navigate back in the page's session history (equivalent to the back button). \
         Returns the resulting URL."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(json!({}), &[])
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let result = call_sidecar(&self.client, "back", json!({}), ctx).await?;
        Ok(ToolOutput::Json(result))
    }
}
