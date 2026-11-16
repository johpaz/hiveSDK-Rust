//! Neutral UTF-8 bytes and BPE estimates over the explicitly selected context.
use crate::memory::ContextUnit;
use crate::{
    ContextOverflow, ContextReport, Engine, Error, InputTokenFraming, Message, ModelRequest,
    Result, StopReason, Tool,
};
use serde::Serialize;
use serde::ser::SerializeSeq;
use std::io::{self, Write};

#[derive(Default)]
struct ByteCounter(u64);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("context byte count overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) fn serialized_bytes(value: &impl Serialize) -> Result<u64> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

#[derive(Serialize)]
struct Input<'a> {
    model: &'a str,
    messages: SelectedMessages<'a>,
    tools: &'a [Tool],
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_output: Option<&'a crate::StructuredOutput>,
}
struct SelectedMessages<'a> {
    history: &'a [Message],
    units: &'a [ContextUnit],
    kept: &'a [bool],
}
impl SelectedMessages<'_> {
    fn iter(&self) -> impl Iterator<Item = &Message> {
        self.units
            .iter()
            .zip(self.kept)
            .filter(|(_, kept)| **kept)
            .flat_map(|(unit, _)| unit.messages(self.history))
    }
}
impl Serialize for SelectedMessages<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.iter().count()))?;
        for message in self.iter() {
            sequence.serialize_element(message)?;
        }
        sequence.end()
    }
}

/// Same JSON as `Input`, over owned messages whose attachment bytes were removed.
#[derive(Serialize)]
struct StrippedInput<'a> {
    model: &'a str,
    messages: &'a [Message],
    tools: &'a [Tool],
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_output: Option<&'a crate::StructuredOutput>,
}

/// Text to run through the tokenizer plus the number of attachments it omits.
/// Attachment payloads are removed (URLs stay) so megabytes of base64 neither
/// slow the tokenizer nor masquerade as text; callers add a fixed allowance.
fn token_text(input: &Input<'_>, framing: InputTokenFraming) -> Result<(String, u64)> {
    let mut attachments = 0u64;
    let messages: Vec<Message> = input
        .messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            for attachment in &mut message.attachments {
                attachments += 1;
                if attachment.data.is_some() {
                    attachment.data = Some(String::new());
                }
            }
            message
        })
        .collect();
    if framing == InputTokenFraming::NeutralJson {
        let stripped = StrippedInput {
            model: input.model,
            messages: &messages,
            tools: input.tools,
            structured_output: input.structured_output,
        };
        return Ok((serde_json::to_string(&stripped)?, attachments));
    }
    let request = ModelRequest {
        model: input.model.to_owned(),
        messages,
        tools: input.tools.to_vec(),
        max_output_tokens: Some(1),
        deadline_unix_ms: None,
        context: None,
        structured_output: input.structured_output.cloned(),
    };
    let mut body = match framing {
        InputTokenFraming::OpenaiChatJson => crate::provider::request_body(&request)?,
        InputTokenFraming::AnthropicMessagesJson => {
            crate::provider::anthropic::request_body(&request, 1)?
        }
        InputTokenFraming::GeminiGenerateContentJson => {
            crate::provider::gemini::request_body(&request, 1)?
        }
        InputTokenFraming::NeutralJson => unreachable!(),
    };
    let object = body.as_object_mut().expect("provider bodies are objects");
    for key in ["max_completion_tokens", "max_tokens", "stream"] {
        object.remove(key);
    }
    if let Some(config) = object
        .get_mut("generationConfig")
        .and_then(serde_json::Value::as_object_mut)
    {
        config.remove("maxOutputTokens");
        config.remove("candidateCount");
        if config.is_empty() {
            object.remove("generationConfig");
        }
    }
    Ok((serde_json::to_string(&body)?, attachments))
}

impl Engine {
    pub(crate) fn prepare_request(
        &self,
        cap: Option<u64>,
    ) -> Result<std::result::Result<ModelRequest, StopReason>> {
        if let Some(output) = &self.policy.structured_output {
            output.validate()?;
        }
        let units = self.context_units()?;
        let mut kept = vec![true; units.len()];
        let bounded =
            self.policy.max_context_bytes.is_some() || self.policy.input_token_budget.is_some();
        let mut bytes = if bounded || self.context_plan.is_some() {
            serialized_bytes(&Input {
                model: &self.model,
                messages: SelectedMessages {
                    history: &self.messages,
                    units: &units,
                    kept: &kept,
                },
                tools: &self.tools,
                structured_output: self.policy.structured_output.as_ref(),
            })?
        } else {
            0
        };
        let mut token_estimate = None;
        loop {
            let byte_fits = self
                .policy
                .max_context_bytes
                .is_none_or(|limit| bytes <= limit);
            let mut tokens_fit = true;
            let mut total_fits = true;
            let mut window_fits = true;
            if byte_fits && let Some(budget) = &self.policy.input_token_budget {
                let input = Input {
                    model: &self.model,
                    messages: SelectedMessages {
                        history: &self.messages,
                        units: &units,
                        kept: &kept,
                    },
                    tools: &self.tools,
                    structured_output: self.policy.structured_output.as_ref(),
                };
                let (text, attachments) = token_text(&input, budget.framing)?;
                let mut estimate = crate::tokens::estimate(&text, budget)?;
                if attachments > 0 {
                    let allowance = budget
                        .attachment_tokens
                        .and_then(|each| each.checked_mul(attachments))
                        .ok_or_else(|| {
                            Error::Input(
                                "attachments need a valid attachment_tokens allowance".into(),
                            )
                        })?;
                    estimate.overhead_tokens = estimate.overhead_tokens.saturating_add(allowance);
                    estimate.total_tokens = estimate.total_tokens.saturating_add(allowance);
                }
                tokens_fit = estimate.total_tokens <= budget.max_tokens;
                total_fits = budget.max_total_tokens.is_none_or(|limit| {
                    estimate.total_tokens <= limit.saturating_sub(self.stats.charged_input_tokens)
                });
                window_fits = budget.context_window_tokens.is_none_or(|limit| {
                    cap.is_some_and(|output| {
                        estimate
                            .total_tokens
                            .checked_add(output)
                            .is_some_and(|combined| combined <= limit)
                    })
                });
                token_estimate = Some(estimate);
            }
            if byte_fits && tokens_fit && total_fits && window_fits {
                break;
            }
            let candidate = (self.policy.context_overflow == ContextOverflow::DropOldestTurns)
                .then(|| {
                    units
                        .iter()
                        .enumerate()
                        .find(|(index, unit)| kept[*index] && !unit.protected)
                })
                .flatten();
            let Some((index, unit)) = candidate else {
                return Ok(Err(if !total_fits {
                    StopReason::InputLimit
                } else {
                    StopReason::ContextLimit
                }));
            };
            for message in unit.messages(&self.messages) {
                bytes -= serialized_bytes(message)? + 1;
            }
            kept[index] = false;
        }
        let selected = SelectedMessages {
            history: &self.messages,
            units: &units,
            kept: &kept,
        };
        let retained: usize = units
            .iter()
            .zip(&kept)
            .filter(|(_, kept)| **kept)
            .map(|(unit, _)| unit.end - unit.start)
            .sum();
        let context = (bounded || self.context_plan.is_some()).then(|| ContextReport {
            serialized_bytes: bytes,
            dropped_messages: (self.messages.len() - retained) as u64,
            token_estimate,
            summary_id: self
                .context_plan
                .as_ref()
                .and_then(|plan| plan.summary.as_ref())
                .map(|summary| summary.id.clone()),
            retrieved_sources: self
                .context_plan
                .as_ref()
                .map_or_else(Vec::new, |plan| plan.retrieved.clone()),
        });
        Ok(Ok(ModelRequest {
            model: self.model.clone(),
            messages: selected.iter().cloned().collect(),
            tools: self.tools.clone(),
            max_output_tokens: cap,
            deadline_unix_ms: self.deadline_unix_ms,
            context,
            structured_output: self.policy.structured_output.clone(),
        }))
    }
}
