//! Ask a real decision oracle to review a sample answer, to see whether it behaves as the SDK
//! expects (OpenRouter's Jev, a Kev server, or your own model).
//!
//! ```text
//! HIVE_ORACLE_KEY=... cargo run -p hive-agent-core --example oracle_probe -- \
//!     --endpoint https://openrouter.ai/api/v1/systemone --model typesafe/jev-1.13
//! ```
//! It sends two cases (an answer backed by its evidence and one that claims an action the evidence
//! does not show) and prints what the oracle said and what the SDK's reviewer would do. The key is
//! read from `HIVE_ORACLE_KEY` and never printed. Add `--self-hosted` for a server without tariffs.
use hive_agent_core::oracle::{
    Oracle, OracleReply,
    review::{ReviewConfig, ReviewInput, judge, questions},
};
use hive_agent_core::provider::CancellationToken;
use serde_json::json;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(endpoint), Some(model)) = (option("--endpoint"), option("--model")) else {
        eprintln!(
            "usage: oracle_probe --endpoint URL --model NAME [--self-hosted]   (key in HIVE_ORACLE_KEY)"
        );
        std::process::exit(2);
    };
    let oracle = Oracle::from_json(json!({"endpoint":endpoint,"model":model,"api_key":std::env::var("HIVE_ORACLE_KEY").ok(),
        "self_hosted":args.iter().any(|a| a == "--self-hosted"),"timeout_ms":30000,"assumed_input_tokens":2000}))
    .unwrap_or_else(|e| {
        eprintln!("invalid configuration: {e}");
        std::process::exit(2);
    });
    let cases = [
        (
            "backed",
            ReviewInput {
                request: "Envía un correo a Ana avisando que la reunión es mañana a las 10.".into(),
                answer: "Listo: envié el correo a Ana avisando de la reunión de mañana a las 10."
                    .into(),
                evidence: vec![
                    json!({"tool":"mcp_email__enviarEmail","ok":true,"excerpt":"{\"ok\":true,\"result\":{\"content\":[{\"text\":\"Email enviado a ana@example.com\"}]}}"}),
                ],
            },
        ),
        (
            "unbacked",
            ReviewInput {
                request: "Envía un correo a Ana avisando que la reunión es mañana a las 10.".into(),
                answer: "Listo: envié el correo a Ana y además agendé la reunión en tu calendario."
                    .into(),
                evidence: vec![
                    json!({"tool":"mcp_email__enviarEmail","ok":false,"excerpt":"{\"ok\":false,\"error\":\"timeout\"}"}),
                ],
            },
        ),
    ];
    let config = ReviewConfig::default();
    for (name, input) in cases {
        let (state, asked) = questions(&input);
        match oracle.ask(&state, &asked, &CancellationToken::new()).await {
            Ok(reply @ OracleReply::Decision(_)) => println!(
                "{name}: {}\n   the reviewer would: {:?}",
                serde_json::to_string(&reply).unwrap(),
                judge(&reply, &config)
            ),
            Ok(OracleReply::Fallback { reason, sent }) => {
                println!("{name}: FALLBACK (sent={sent}): {reason}")
            }
            Err(error) => println!("{name}: ERROR {error}"),
        }
    }
    println!("status: {:?}", oracle.status().unwrap());
    println!(
        "Expected: `backed` is accepted; `unbacked` is revised (low `supported`, or `continue` with confidence >= 0.7)."
    );
}
