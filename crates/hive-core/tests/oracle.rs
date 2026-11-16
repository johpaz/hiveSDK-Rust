use hive_agent_core::objective::{Objective, ObjectivePolicy};
use hive_agent_core::oracle::{Answer, Oracle, OracleReply, Question};
use hive_agent_core::provider::CancellationToken;
use hive_agent_core::{Error, Usage};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

struct Server {
    url: String,
    requests: mpsc::UnboundedReceiver<String>,
    queue: Arc<Mutex<VecDeque<String>>>,
}
impl Server {
    fn push(&self, status: u16, body: Value) {
        self.push_raw(status, body.to_string());
    }
    fn push_raw(&self, status: u16, body: String) {
        self.queue.lock().unwrap().push_back(format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ));
    }
    fn sent(&mut self) -> bool {
        self.requests.try_recv().is_ok()
    }
}
async fn server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/decisions", listener.local_addr().unwrap());
    let queue: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let (sender, requests) = mpsc::unbounded_channel();
    let answers = queue.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (sender, answers) = (sender.clone(), answers.clone());
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buffer = vec![0u8; 16384];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    raw.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= length || read == 0 {
                            break;
                        }
                    } else if read == 0 {
                        break;
                    }
                }
                let _ = sender.send(String::from_utf8_lossy(&raw).into_owned());
                let next = answers.lock().unwrap().pop_front();
                if let Some(next) = next {
                    let _ = socket.write_all(next.as_bytes()).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    Server {
        url,
        requests,
        queue,
    }
}

fn oracle(server: &Server, extra: Value) -> Oracle {
    let mut config =
        json!({"endpoint":server.url,"model":"jev-test","api_key":"secret-key","timeout_ms":1000});
    config
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    Oracle::from_json(config).unwrap()
}
fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([
        (
            "next".into(),
            Question::Choice {
                instructions: "what next".into(),
                criteria: BTreeMap::from([
                    ("finish".into(), "done".into()),
                    ("continue".into(), "more".into()),
                ]),
            },
        ),
        (
            "sure".into(),
            Question::Noul {
                instructions: "is it done".into(),
                criteria: None,
            },
        ),
    ])
}
async fn ask(oracle: &Oracle) -> OracleReply {
    oracle
        .ask(&json!({"turn":1}), &questions(), &CancellationToken::new())
        .await
        .unwrap()
}
fn good() -> Value {
    json!({"answers":{"next":{"type":"choice","choice":"finish","confidence":0.9,"probabilities":{"finish":0.9,"continue":0.1}},"sure":{"type":"noul","noul":0.75}},
           "usage":{"input_tokens":120,"output_tokens":3,"cost":0.0004}})
}
fn fallback(reply: &OracleReply) -> (&str, bool) {
    match reply {
        OracleReply::Fallback { reason, sent } => (reason, *sent),
        other => panic!("expected fallback, got {other:?}"),
    }
}

#[test]
fn configuration_is_validated() {
    let ok = json!({"endpoint":"https://decisions.example/v1","model":"m"});
    assert!(Oracle::from_json(ok.clone()).is_ok());
    for patch in [
        json!({"endpoint":"ftp://x"}),
        json!({"endpoint":"https://u:p@x"}),
        json!({"endpoint":"nope"}),
        json!({"model":""}),
        json!({"timeout_ms":0}),
        json!({"timeout_ms":600000}),
        json!({"max_output_tokens":0}),
        json!({"distrust_strikes":0}),
        json!({"bogus":1}),
    ] {
        let mut config = ok.clone();
        config
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(Oracle::from_json(config).is_err(), "{patch}");
    }
}

#[tokio::test]
async fn decisions_use_the_wire_protocol_and_both_answer_spellings() {
    let mut server = server().await;
    let oracle = oracle(&server, json!({}));
    server.push(200, good());
    let OracleReply::Decision(decision) = ask(&oracle).await else {
        panic!("decision expected")
    };
    assert_eq!(decision.answers["sure"], Answer::Noul { noul: 0.75 });
    let Answer::Choice {
        choice,
        confidence,
        probabilities,
    } = &decision.answers["next"]
    else {
        panic!()
    };
    assert_eq!(
        (choice.as_str(), *confidence, probabilities.len()),
        ("finish", 0.9, 2)
    );
    assert_eq!(
        (decision.charge.clone(), decision.cost_usd),
        (
            Some(Usage {
                input_tokens: 120,
                output_tokens: 3
            }),
            Some(0.0004)
        )
    );

    let raw = server.requests.recv().await.unwrap();
    assert!(raw.starts_with("POST /decisions "));
    assert!(
        raw.to_ascii_lowercase()
            .contains("authorization: bearer secret-key")
    );
    let body: Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        (&body["model"], &body["state"]),
        (&json!("jev-test"), &json!({"turn":1}))
    );
    assert_eq!(body["questions"]["next"]["type"], "choice");
    assert_eq!(body["questions"]["next"]["criteria"]["finish"], "done");

    // llama.cpp style: the choice's confidence is its own probability; noul arrives as `probability`.
    server.push(200, json!({"answers":{"next":{"choice":"continue","probabilities":{"finish":0.2,"continue":0.8}},"sure":{"probability":0.1}}}));
    let OracleReply::Decision(decision) = ask(&oracle).await else {
        panic!("decision expected")
    };
    let Answer::Choice { confidence, .. } = &decision.answers["next"] else {
        panic!()
    };
    assert_eq!(
        (*confidence, &decision.answers["sure"]),
        (0.8, &Answer::Noul { noul: 0.1 })
    );
    assert_eq!(
        (decision.charge, decision.cost_usd),
        (None, None),
        "no usage and no assumption means unknown"
    );
    let status = oracle.status().unwrap();
    assert_eq!(
        (status.state, status.decisions, status.last_error),
        ("ready", 2, None)
    );
}

#[tokio::test]
async fn self_hosted_calls_are_free_and_assumed_usage_fills_a_missing_report() {
    let server = server().await;
    let oracle = oracle(
        &server,
        json!({"self_hosted":true,"assumed_input_tokens":500,"max_output_tokens":8}),
    );
    server.push(200, json!({"answers":good()["answers"]}));
    let OracleReply::Decision(decision) = ask(&oracle).await else {
        panic!()
    };
    assert_eq!(decision.cost_usd, Some(0.0));
    assert_eq!(
        decision.charge,
        Some(Usage {
            input_tokens: 500,
            output_tokens: 8
        })
    );
}

#[tokio::test]
async fn answers_that_break_the_contract_fall_back_and_never_pass_silently() {
    let mut server = server().await;
    let oracle = oracle(&server, json!({"cooldown_ms":150}));
    let broken = [
        json!({"answers":{"next":{"choice":"banana","confidence":0.9},"sure":{"noul":0.5}}}),
        json!({"answers":{"next":{"choice":"finish","confidence":1.2},"sure":{"noul":0.5}}}),
        json!({"answers":{"next":{"choice":"finish","confidence":0.5},"sure":{"noul":-0.1}}}),
        json!({"answers":{"next":{"choice":"finish","confidence":0.5}}}),
        json!({"answers":{"next":"finish","sure":{"noul":0.5}}}),
        json!({"answers":{"next":{"choice":"finish"},"sure":{"noul":0.5}}}),
    ];
    for (index, body) in broken.iter().cloned().enumerate() {
        if index == 3 {
            break;
        }
        server.push(200, body);
        let reply = ask(&oracle).await;
        let (reason, sent) = fallback(&reply);
        assert!(sent && reason.starts_with("invalid "), "{index}: {reason}");
    }
    // Three failures in a row start a cooldown: the fourth call is not even sent.
    while server.sent() {}
    let reply = ask(&oracle).await;
    assert_eq!(fallback(&reply), ("oracle in cooldown", false));
    assert!(!server.sent());
    assert_eq!(oracle.status().unwrap().state, "fallback");

    tokio::time::sleep(Duration::from_millis(200)).await;
    server.push(200, good());
    assert!(
        matches!(ask(&oracle).await, OracleReply::Decision(_)),
        "recovers after the cooldown"
    );
    let status = oracle.status().unwrap();
    assert_eq!((status.state, status.last_error), ("ready", None));

    for body in [broken[3].clone(), broken[4].clone(), broken[5].clone()] {
        server.push(200, body.clone());
        let reply = ask(&oracle).await;
        assert!(fallback(&reply).0.starts_with("invalid "), "{body}");
    }
    server.push_raw(200, "not json".into());
    let reply = ask(&oracle).await;
    assert_eq!(
        fallback(&reply).0,
        "oracle in cooldown",
        "failures 1-3 above already tripped it"
    );
}

#[tokio::test]
async fn authorization_failures_cool_down_at_once_and_errors_do_not_leak_secrets() {
    let server = server().await;
    let oracle = oracle(&server, json!({"cooldown_ms":10000}));
    server.push(401, json!({"error":"bad key secret-key"}));
    let reply = ask(&oracle).await;
    let (reason, sent) = fallback(&reply);
    assert_eq!((reason, sent), ("oracle HTTP 401", true));
    assert!(
        !oracle
            .status()
            .unwrap()
            .last_error
            .unwrap()
            .contains("secret-key")
    );
    assert_eq!(fallback(&ask(&oracle).await), ("oracle in cooldown", false));
    oracle.reset().unwrap();
    server.push(200, good());
    assert!(
        matches!(ask(&oracle).await, OracleReply::Decision(_)),
        "reset clears the cooldown"
    );

    let server = self::server().await;
    let flaky = self::oracle(&server, json!({}));
    server.push(500, json!({}));
    let reply = ask(&flaky).await;
    assert_eq!(fallback(&reply), ("oracle HTTP 500", true));
    assert_eq!(flaky.status().unwrap().state, "fallback");
    server.push(200, good());
    assert!(
        matches!(ask(&flaky).await, OracleReply::Decision(_)),
        "a single error does not cool down"
    );
    assert_eq!(flaky.status().unwrap().state, "ready");
}

#[tokio::test]
async fn timeouts_oversized_responses_and_cancellation() {
    let server = server().await;
    let slow = oracle(&server, json!({"timeout_ms":100}));
    let reply = ask(&slow).await;
    assert_eq!(
        fallback(&reply),
        ("oracle timed out", true),
        "an unanswered request times out"
    );

    let big = self::server().await;
    let oracle = self::oracle(&big, json!({}));
    big.push_raw(
        200,
        format!("{{\"pad\":\"{}\"}}", "x".repeat(2 * 1024 * 1024)),
    );
    assert_eq!(
        fallback(&ask(&oracle).await).0,
        "oracle response exceeds the size limit"
    );

    let hang = self::server().await;
    let patient = self::oracle(&hang, json!({"timeout_ms":5000}));
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.cancel();
    });
    let error = patient
        .ask(&json!({}), &questions(), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    assert_eq!(
        patient.status().unwrap().state,
        "ready",
        "cancelling is not an oracle failure"
    );
}

#[tokio::test]
async fn invalid_questions_are_rejected_before_sending() {
    let mut server = server().await;
    let oracle = oracle(&server, json!({}));
    let cancel = CancellationToken::new();
    let none = oracle
        .ask(&json!({}), &BTreeMap::new(), &cancel)
        .await
        .unwrap();
    assert_eq!(fallback(&none), ("no questions", false));
    let one = BTreeMap::from([(
        "q".into(),
        Question::Choice {
            instructions: "i".into(),
            criteria: BTreeMap::from([("a".into(), "x".into())]),
        },
    )]);
    assert!(
        oracle.ask(&json!({}), &one, &cancel).await.is_err(),
        "a choice needs two options"
    );
    let many: BTreeMap<String, Question> = (0..33)
        .map(|i| {
            (
                format!("q{i}"),
                Question::Noul {
                    instructions: "i".into(),
                    criteria: None,
                },
            )
        })
        .collect();
    assert!(oracle.ask(&json!({}), &many, &cancel).await.is_err());
    assert!(!server.sent());
}

#[test]
fn contradictions_in_a_row_set_the_oracle_aside() {
    let oracle = Oracle::from_json(json!({"endpoint":"http://127.0.0.1:9/x","model":"m","distrust_strikes":3,"distrust_ms":60000})).unwrap();
    assert!(!oracle.record_overruled("a").unwrap());
    assert!(!oracle.record_overruled("b").unwrap());
    oracle.record_agreement().unwrap();
    assert!(
        !oracle.record_overruled("c").unwrap(),
        "agreement restarted the streak"
    );
    assert!(!oracle.record_overruled("d").unwrap());
    assert!(oracle.record_overruled("e").unwrap());
    let status = oracle.status().unwrap();
    assert_eq!(status.state, "fallback");
    assert!(status.last_error.unwrap().contains("last: e"));
    oracle.reset().unwrap();
    assert_eq!(oracle.status().unwrap().state, "ready");
}

fn objective(calls: u32, output: u64) -> Objective {
    Objective::new(ObjectivePolicy {
        max_model_calls: calls,
        max_total_output_tokens: output,
        max_output_tokens: output,
        ..Default::default()
    })
    .unwrap()
}
async fn charged(oracle: &Oracle, objective: &Objective) -> Result<OracleReply, Error> {
    oracle
        .ask_charged(
            objective,
            0,
            &json!({}),
            &questions(),
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn every_call_is_charged_to_the_root_objective() {
    let mut server = server().await;
    let oracle = oracle(&server, json!({"max_output_tokens":16}));
    let root = objective(2, 100);
    server.push(200, good());
    assert!(matches!(
        charged(&oracle, &root).await.unwrap(),
        OracleReply::Decision(_)
    ));
    let snapshot = root.snapshot().unwrap();
    assert_eq!(
        (
            &snapshot["model_calls"],
            &snapshot["input_tokens"],
            &snapshot["output_tokens"],
            &snapshot["reservations"]
        ),
        (&json!(1), &json!(120), &json!(3), &json!({}))
    );

    // The call limit is shared with every other model call and is an error, not a fallback.
    server.push(200, good());
    charged(&oracle, &root).await.unwrap();
    while server.sent() {}
    let error = charged(&oracle, &root).await.unwrap_err();
    assert!(
        matches!(&error, Error::ObjectiveLimit(reason) if reason == "model_call_limit"),
        "{error:?}"
    );
    assert!(!server.sent(), "a spent budget sends nothing");
}

#[tokio::test]
async fn unconfirmed_and_unmeasured_calls_never_create_free_capacity() {
    let server = server().await;
    let oracle = oracle(&server, json!({"max_output_tokens":16,"cooldown_ms":10000}));
    let root = objective(8, 100);
    server.push(500, json!({}));
    assert!(matches!(
        charged(&oracle, &root).await.unwrap(),
        OracleReply::Fallback { sent: true, .. }
    ));
    let snapshot = root.snapshot().unwrap();
    assert_eq!(
        (&snapshot["model_calls"], &snapshot["reservations"]),
        (&json!(1), &json!({"1":16})),
        "a sent request keeps its reservation"
    );

    oracle.reset().unwrap();
    server.push(200, json!({"answers":good()["answers"]}));
    assert!(
        matches!(charged(&oracle, &root).await.unwrap(), OracleReply::Decision(d) if d.charge.is_none())
    );
    // The caller settles `None`... here ask_charged did, so the objective is halted.
    assert_eq!(root.snapshot().unwrap()["stopped"], "usage_unavailable");
    assert!(matches!(
        charged(&oracle, &root).await.unwrap_err(),
        Error::ObjectiveLimit(_)
    ));

    let assumed = self::oracle(
        &server,
        json!({"max_output_tokens":16,"assumed_input_tokens":300}),
    );
    let root = objective(8, 100);
    server.push(200, json!({"answers":good()["answers"]}));
    charged(&assumed, &root).await.unwrap();
    let snapshot = root.snapshot().unwrap();
    assert_eq!(
        (
            &snapshot["input_tokens"],
            &snapshot["output_tokens"],
            &snapshot["stopped"]
        ),
        (&json!(300), &json!(16), &Value::Null)
    );
}

#[tokio::test]
async fn a_cooldown_skips_the_call_without_reserving_anything() {
    let server = server().await;
    let oracle = oracle(&server, json!({"cooldown_ms":10000}));
    let root = objective(8, 100);
    server.push(401, json!({}));
    charged(&oracle, &root).await.unwrap();
    assert_eq!(root.snapshot().unwrap()["model_calls"], 1);
    let reply = charged(&oracle, &root).await.unwrap();
    assert_eq!(fallback(&reply), ("oracle in cooldown", false));
    assert_eq!(
        root.snapshot().unwrap()["model_calls"],
        1,
        "no reservation while cooling down"
    );
}
