//! Offline BPE over the neutral JSON envelope, not the provider's chat template.
use crate::{Error, InputTokenBudget, Result, TokenEncoding, TokenEstimate};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

fn encoding(kind: TokenEncoding) -> Result<&'static CoreBPE> {
    type Cached = OnceLock<std::result::Result<CoreBPE, String>>;
    static CL100K: Cached = OnceLock::new();
    static O200K: Cached = OnceLock::new();
    let value = match kind {
        TokenEncoding::Cl100kBase => CL100K.get_or_init(|| {
            tiktoken_rs::cl100k_base().map_err(|_| "cannot initialize cl100k_base".into())
        }),
        TokenEncoding::O200kBase => O200K.get_or_init(|| {
            tiktoken_rs::o200k_base().map_err(|_| "cannot initialize o200k_base".into())
        }),
    };
    value
        .as_ref()
        .map_err(|message| Error::State(message.clone()))
}

pub(crate) fn estimate(text: &str, budget: &InputTokenBudget) -> Result<TokenEstimate> {
    // Literal special-token-looking strings are ordinary user data.
    let serialized_tokens = encoding(budget.encoding)?.count_ordinary(text) as u64;
    let total_tokens = serialized_tokens
        .checked_add(budget.overhead_tokens)
        .ok_or_else(|| Error::Input("input token estimate overflow".into()))?;
    Ok(TokenEstimate {
        encoding: budget.encoding,
        framing: budget.framing,
        serialized_tokens,
        overhead_tokens: budget.overhead_tokens,
        total_tokens,
    })
}
