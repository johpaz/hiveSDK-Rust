//! Explicit context sources, local retrieval and summary projection. Never calls a model.
use crate::{Action, Engine, Error, Message, Result};
use ring::digest::{Context, SHA256};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{self, Write};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextSource {
    pub start: usize,
    pub end: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSummary {
    pub id: String,
    pub source: ContextSource,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPlan {
    pub summary: Option<ContextSummary>,
    pub retrieved: Vec<ContextSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextSearchOptions {
    pub max_turns: usize,
    pub max_bytes: u64,
}
impl Default for ContextSearchOptions {
    fn default() -> Self {
        Self {
            max_turns: 4,
            max_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ContextHit {
    pub source: ContextSource,
    pub score: usize,
    pub messages: Vec<Message>,
}

#[derive(Debug, Serialize)]
pub struct ContextSearchResult {
    pub hits: Vec<ContextHit>,
    /// Exact compact JSON bytes of `hits`, including source metadata.
    pub serialized_bytes: u64,
}

struct HashWriter(Context);
struct SourceFormatter;
impl serde_json::ser::Formatter for SourceFormatter {
    fn write_f64<W: Write + ?Sized>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        // JSON bridges may round-trip 1.0 as 1 and -0.0 as 0. Normalize only
        // exactly integral, portable numbers; retain precision checks elsewhere.
        if value == 0.0 {
            writer.write_all(b"0")
        } else if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
            write!(writer, "{value:.0}")
        } else {
            serde_json::ser::CompactFormatter.write_f64(writer, value)
        }
    }
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn source_hash(messages: &[Message]) -> Result<String> {
    let mut writer = HashWriter(Context::new(&SHA256));
    {
        let mut serializer = serde_json::Serializer::with_formatter(&mut writer, SourceFormatter);
        let mut sequence = serde::Serializer::serialize_seq(&mut serializer, Some(messages.len()))?;
        // Value maps use sorted keys in this locked serde_json configuration.
        // Materialize one message at a time, never the entire source history.
        for message in messages {
            sequence.serialize_element(&serde_json::to_value(message)?)?;
        }
        sequence.end()?;
    }
    Ok(writer
        .0
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn validate_summary_messages(
    source: &ContextSource,
    messages: &[Message],
) -> Result<()> {
    if source.end.checked_sub(source.start) != Some(messages.len())
        || messages.is_empty()
        || messages
            .first()
            .is_none_or(|message| message.role != "user")
        || messages.last().is_none_or(|message| {
            message.role != "assistant"
                || !message.tool_calls.is_empty()
                || message.content.is_none()
        })
        || source_hash(messages)? != source.sha256
    {
        return Err(Error::Input(
            "summary input must match its source hash and contain closed user turns".into(),
        ));
    }
    Ok(())
}

fn terms(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(crate) struct ContextUnit {
    pub start: usize,
    pub end: usize,
    pub protected: bool,
    summary: Option<Message>,
}
impl ContextUnit {
    pub fn messages<'a>(&'a self, history: &'a [Message]) -> &'a [Message] {
        self.summary
            .as_ref()
            .map_or(&history[self.start..self.end], std::slice::from_ref)
    }
}

impl Engine {
    fn completed_context_end(&self) -> usize {
        if matches!(self.action, Action::Complete { .. }) {
            self.messages.len()
        } else {
            self.messages
                .iter()
                .rposition(|message| message.role == "user")
                .unwrap_or(self.messages.len())
        }
    }

    fn context_ranges(&self) -> Vec<(usize, usize)> {
        let end = self.completed_context_end();
        let starts: Vec<_> = self.messages[..end]
            .iter()
            .enumerate()
            .filter_map(|(index, message)| (message.role == "user").then_some(index))
            .collect();
        starts
            .iter()
            .enumerate()
            .map(|(i, &start)| (start, starts.get(i + 1).copied().unwrap_or(end)))
            .collect()
    }

    /// Capture only complete historical turns. Message indices are [start, end).
    pub fn capture_context(&self, start: usize, end: usize) -> Result<ContextSource> {
        let ranges = self.context_ranges();
        if start >= end
            || !ranges.iter().any(|range| range.0 == start)
            || !ranges.iter().any(|range| range.1 == end)
        {
            return Err(Error::Input(
                "context sources must contain complete, closed user turns".into(),
            ));
        }
        Ok(ContextSource {
            start,
            end,
            sha256: source_hash(&self.messages[start..end])?,
        })
    }

    pub fn read_context(&self, source: &ContextSource) -> Result<Vec<Message>> {
        self.validate_context_source(source)?;
        Ok(self.messages[source.start..source.end].to_vec())
    }

    fn validate_context_source(&self, source: &ContextSource) -> Result<()> {
        if self.capture_context(source.start, source.end)? != *source {
            return Err(Error::Input(
                "context source hash no longer matches the history".into(),
            ));
        }
        Ok(())
    }

    /// Local lexical matching; no embeddings, remote I/O, token usage or side effects.
    pub fn search_context(
        &self,
        query: &str,
        options: ContextSearchOptions,
    ) -> Result<ContextSearchResult> {
        if query.len() > 4096
            || !(1..=64).contains(&options.max_turns)
            || !(2..=1024 * 1024).contains(&options.max_bytes)
        {
            return Err(Error::Input("context search requires max_turns 1..64, max_bytes 2..1048576 and query <=4096 bytes".into()));
        }
        let query = terms(query);
        if query.is_empty() || query.len() > 32 {
            return Err(Error::Input(
                "context query requires 1..32 lexical terms".into(),
            ));
        }
        let mut candidates = vec![];
        for (start, end) in self.context_ranges() {
            let messages = &self.messages[start..end];
            if crate::context::serialized_bytes(&messages)? > options.max_bytes {
                continue;
            }
            let mut words = BTreeSet::new();
            for message in messages {
                if let Some(content) = &message.content {
                    words.extend(terms(content));
                }
                for call in &message.tool_calls {
                    words.extend(terms(&call.name));
                    words.extend(terms(&call.arguments.to_string()));
                }
            }
            let score = query.intersection(&words).count();
            if score == 0 {
                continue;
            }
            candidates.push((score, start, end));
            candidates.sort_unstable_by(|a, b| b.cmp(a));
            candidates.truncate(options.max_turns);
        }
        let mut hits = vec![];
        let mut bytes = 2;
        for (score, start, end) in candidates {
            let hit = ContextHit {
                source: self.capture_context(start, end)?,
                score,
                messages: self.messages[start..end].to_vec(),
            };
            let additional = crate::context::serialized_bytes(&hit)? + u64::from(!hits.is_empty());
            if additional <= options.max_bytes - bytes {
                bytes += additional;
                hits.push(hit);
            }
        }
        Ok(ContextSearchResult {
            hits,
            serialized_bytes: bytes,
        })
    }

    pub fn context_plan(&self) -> Option<&ContextPlan> {
        self.context_plan.as_ref()
    }

    fn validate_context_plan(&self, plan: &ContextPlan) -> Result<()> {
        if plan.retrieved.len() > 16 {
            return Err(Error::Input(
                "at most 16 retrieved context turns are supported".into(),
            ));
        }
        if let Some(summary) = &plan.summary {
            let first = self
                .messages
                .iter()
                .position(|message| message.role == "user");
            if summary.id.trim().is_empty()
                || summary.id.len() > 256
                || summary.content.trim().is_empty()
                || summary.content.len() > 64 * 1024
                || first != Some(summary.source.start)
            {
                return Err(Error::Input("summary requires an id <=256 bytes, content <=64 KiB and a source starting at the first user turn".into()));
            }
            self.validate_context_source(&summary.source)?;
        }
        let ranges = self.context_ranges();
        let mut seen = BTreeSet::new();
        for source in &plan.retrieved {
            if !ranges.contains(&(source.start, source.end)) || !seen.insert(source.start) {
                return Err(Error::Input(
                    "retrieved sources must be distinct, single closed turns".into(),
                ));
            }
            self.validate_context_source(source)?;
        }
        Ok(())
    }

    /// An explicit between-turn change; it never rewrites the source messages.
    pub fn set_context_plan(&mut self, plan: Option<ContextPlan>) -> Result<()> {
        if !matches!(
            self.action,
            Action::Idle | Action::Complete { .. } | Action::Stopped { .. }
        ) {
            return Err(Error::State(
                "context plans can only change between turns".into(),
            ));
        }
        if let Some(plan) = &plan {
            self.validate_context_plan(plan)?;
        }
        self.context_plan = plan;
        Ok(())
    }

    pub(crate) fn context_units(&self) -> Result<Vec<ContextUnit>> {
        if let Some(plan) = &self.context_plan {
            self.validate_context_plan(plan)?;
        }
        let first = self
            .messages
            .iter()
            .position(|message| message.role == "user")
            .unwrap_or(self.messages.len());
        let current = self
            .messages
            .iter()
            .rposition(|message| message.role == "user")
            .unwrap_or(self.messages.len());
        let mut units = vec![ContextUnit {
            start: 0,
            end: first,
            protected: true,
            summary: None,
        }];
        let summary = self
            .context_plan
            .as_ref()
            .and_then(|plan| plan.summary.as_ref());
        if let Some(summary) = summary {
            let content = serde_json::json!({"context_summary":{"id":summary.id,"source":summary.source,"content":summary.content}}).to_string();
            units.push(ContextUnit {
                start: 0,
                end: 0,
                protected: true,
                summary: Some(Message::text("user", content)),
            });
        }
        let starts: Vec<_> = self.messages[first..current]
            .iter()
            .enumerate()
            .filter_map(|(index, message)| (message.role == "user").then_some(first + index))
            .collect();
        for (i, &start) in starts.iter().enumerate() {
            let end = starts.get(i + 1).copied().unwrap_or(current);
            let retrieved = self
                .context_plan
                .as_ref()
                .is_some_and(|plan| plan.retrieved.iter().any(|source| source.start == start));
            if summary.is_some_and(|summary| end <= summary.source.end) && !retrieved {
                continue;
            }
            units.push(ContextUnit {
                start,
                end,
                protected: retrieved,
                summary: None,
            });
        }
        if current < self.messages.len() {
            units.push(ContextUnit {
                start: current,
                end: self.messages.len(),
                protected: true,
                summary: None,
            });
        }
        Ok(units)
    }
}
