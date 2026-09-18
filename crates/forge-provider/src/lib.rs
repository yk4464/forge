mod anthropic;
mod openai;
mod responses;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;
pub use responses::ResponsesProvider;

use forge_core::error::{Error, Result};
use forge_core::traits::ModelProvider;
use std::sync::Arc;

/// Build the provider selected by config protocol name.
pub fn from_config(protocol: &str, base_url: &str) -> Result<Arc<dyn ModelProvider>> {
    match protocol {
        "openai" => Ok(Arc::new(
            OpenAiProvider::new(base_url).with_label("openai-compatible"),
        )),
        "responses" => Ok(Arc::new(ResponsesProvider::new(base_url))),
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(base_url))),
        other => Err(Error::Config(format!(
            "unknown provider protocol: {other} (supported: openai, responses, anthropic)"
        ))),
    }
}
