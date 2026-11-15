//! Explicit final JSON contract. Never repairs output or calls a provider.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredOutput {
    pub name: String,
    pub schema: Value,
}
impl StructuredOutput {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name.len() > 64
            || !self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            || !self.schema.is_object()
            || crate::context::serialized_bytes(&self.schema)? > 64 * 1024
        {
            return Err(Error::Input("structured_output requires an ASCII name of 1..64 letters/digits/_/- and a JSON Schema object <=64 KiB".into()));
        }
        crate::validation::compile(&self.schema)?;
        Ok(())
    }
    pub(crate) fn accepts(&self, text: &str) -> Result<bool> {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Ok(false);
        };
        Ok(crate::validation::compile(&self.schema)?.is_valid(&value))
    }
}
