//! Gateway adapter for the native sealed-group queue; providers stay in Rust.
use super::*;
use hive_agent_core::{
    ToolCall,
    harness::{GroupSpec, Job, JobOutcome, JobStatus, QueuePolicy, SpecialistTask, WorkerPool},
};

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work).await?
}
fn store(app: &App, user: &str) -> Result<hive_agent_core::sessions::SessionStore> {
    app.store_handle(user)
}
fn tasks(call: &ToolCall) -> Result<Vec<SpecialistTask>> {
    if call.name == "task_delegate" {
        return Ok(vec![SpecialistTask {
            id: call.id.clone(),
            specialist_id: config::string(&call.arguments, "worker_id")?.into(),
            prompt: config::string(&call.arguments, "task_description")?.into(),
        }]);
    }
    call.arguments["tasks"]
        .as_array()
        .ok_or("Grupo sin tareas")?
        .iter()
        .map(|task| {
            Ok(SpecialistTask {
                id: config::string(task, "task_id")?.into(),
                specialist_id: config::string(task, "worker_id")?.into(),
                prompt: config::string(task, "task_description")?.into(),
            })
        })
        .collect()
}
fn bounded_error(message: String) -> String {
    let mut end = message.len().min(4096);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].into()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<P: TurnProvider>(
    app: Arc<App>,
    provider: Arc<P>,
    context: RunContext,
    progress: Progress,
    call: ToolCall,
    execution_id: String,
    output: mpsc::Sender<Value>,
    cancel: CancellationToken,
) -> Result<Value> {
    let spec = GroupSpec {
        objective_id: context.objective_id.clone(),
        coordinator_agent_id: progress.agent.clone(),
        tasks: tasks(&call)?,
        depth: context.depth + 1,
        require_verification: false,
    };
    // The parent operation fence identifies one immutable delegation batch.
    let group_id = format!("{}:{execution_id}", progress.run);
    let host = app.clone();
    let id = group_id.clone();
    let tool = call.name.clone();
    let user = context.user.clone();
    let (queue, group) = blocking(move || {
        if !host.delegation_enabled()?
            || (tool == "task_delegate_group"
                && host
                    .db
                    .col_get("tools", &tool)?
                    .is_some_and(|row| row.doc["enabled"] == false || row.doc["active"] == false))
        {
            return Err("La delegación está desactivada en HiveDB".into());
        }
        let queue =
            store(&host, &user)?.queue(&format!("gateway.specialists.{}", spec.objective_id))?;
        queue.open(QueuePolicy {
            max_payload_bytes: 1_048_576,
            ..Default::default()
        })?;
        let group = queue.create_group(&id, spec)?;
        Ok((queue, group))
    })
    .await?;
    let mut pool = WorkerPool::new(queue.clone(), format!("gateway:{execution_id}"))?;
    let kind = config::string(&group, "specialist_kind")?;
    let executor = Arc::new(ChildExecutor {
        app,
        provider,
        context,
        progress,
        output,
        call_id: call.id.clone(),
        grouped: call.name == "task_delegate_group",
    });
    let grouped = executor.grouped;
    pool.register(kind, move |job, token| {
        execute_child(executor.clone(), job, token)
    })?;
    loop {
        let batch = pool.run_once();
        tokio::pin!(batch);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                pool.stop();
                let q=queue.clone();let id=group_id.clone();
                let cancelled=blocking(move||Ok(q.cancel_group(&id)?)).await;
                // Always await dispatched children, including if the cancellation write failed.
                let _ = batch.await;
                cancelled?;
                return Err("Turno cancelado; revisa las operaciones pendientes antes de reanudar".into());
            },
            result=&mut batch=>result,
        };
        if let Err(error) = result
            && !matches!(error, hive_agent_core::Error::ObjectiveLimit(_))
        {
            return Err(error.into());
        }
        let q = queue.clone();
        let id = group_id.clone();
        let report = blocking(move || Ok(q.group(&id)?)).await?;
        if report["status"] == "blocked_uncertain" {
            let details = report["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["job"]["error"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!("Delegación incierta; requiere reconciliación: {details}").into());
        }
        if report["ready"] == true {
            return Ok(tool_result(&report, grouped));
        }
        if report["cancelled"] == true {
            return Err("Grupo de delegación cancelado".into());
        }
        // Other batches of this turn may occupy capacity. Never claim unknown kinds.
        tokio::select! { _=cancel.cancelled()=>{}, _=tokio::time::sleep(Duration::from_millis(25))=>{} }
    }
}

struct ChildExecutor<P> {
    app: Arc<App>,
    provider: Arc<P>,
    context: RunContext,
    progress: Progress,
    output: mpsc::Sender<Value>,
    call_id: String,
    grouped: bool,
}
fn prepare_child<P>(
    executor: &ChildExecutor<P>,
    job: &Job,
) -> Result<(config::Selection, Arc<DurableEngineHandle>)> {
    let app = &executor.app;
    let context = &executor.context;
    if !app.delegation_enabled()?
        || (executor.grouped
            && app
                .db
                .col_get("tools", "task_delegate_group")?
                .is_some_and(|row| row.doc["enabled"] == false || row.doc["active"] == false))
    {
        return Err("La delegación está desactivada en HiveDB".into());
    }
    if !crate::conversations::owned(app, &context.thread, &context.user)? {
        return Err("Conversación no accesible".into());
    }
    let binding = &job.input.payload["delegation"];
    let worker = config::string(binding, "agent_id")?;
    let selected = app.selection_for_user_agent(&context.user, worker)?;
    if selected.user != context.user
        || job.input.objective_id.as_deref() != Some(context.objective_id.as_str())
        || job.input.depth != context.depth + 1
    {
        return Err("Especialista o presupuesto de otro ámbito".into());
    }
    let child = Arc::new(DurableEngineHandle::open(
        app.store_for(&context.user)?,
        worker.into(),
        config::string(binding, "session_id")?.into(),
        &selected.engine_config,
        false,
    )?);
    Ok((selected, child))
}
async fn execute_child<P: TurnProvider>(
    executor: Arc<ChildExecutor<P>>,
    job: Job,
    token: CancellationToken,
) -> hive_agent_core::Result<JobOutcome> {
    // No external operation has started; preparation failures are confirmed outcomes.
    let host = executor.clone();
    let queued = job.clone();
    let (selected, child) = match blocking(move || prepare_child(&host, &queued)).await {
        Ok(value) => value,
        Err(error) => {
            return Ok(JobOutcome::Failed {
                error: bounded_error(error.to_string()),
                retryable: false,
            });
        }
    };
    let worker = config::string(&selected.agent, "id")
        .map_err(|error| hive_agent_core::Error::Input(error.to_string()))?
        .to_owned();
    let name = selected.agent["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&worker)
        .to_owned();
    let step = if executor.grouped {
        format!("{}:{}", executor.call_id, job.id)
    } else {
        executor.call_id.clone()
    };
    let prompt = config::string(&job.input.payload, "prompt")
        .map_err(|error| hive_agent_core::Error::Input(error.to_string()))?
        .to_owned();
    let progress = &executor.progress;
    let output = &executor.output;
    let work = async {
        let event = progress
            .emit(
                &format!("{step}:call"),
                "delegated",
                "tool_call",
                "running",
                &format!("Consultando a {name}"),
                Some(prompt.chars().take(240).collect()),
            )
            .await?;
        output.send(event).await?;
        let context = &executor.context;
        let child_context = RunContext {
            provider_id: selected.provider_id,
            model_id: selected.model_id,
            user: context.user.clone(),
            actor: selected.agent,
            thread: context.thread.clone(),
            root: Some(progress.root.clone()),
            parent: Some(progress.agent.clone()),
            depth: job.input.depth,
            objective_id: context.objective_id.clone(),
            attachments: vec![],
        };
        let answer = run_engine(
            executor.app.clone(),
            child,
            executor.provider.child(selected.provider),
            child_context,
            prompt,
            output.clone(),
            token,
        )
        .await?;
        let event = progress
            .emit(
                &format!("{step}:result"),
                "tool_result",
                "thinking",
                "done",
                &format!("Recibí la respuesta de {name}"),
                None,
            )
            .await?;
        output.send(event).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(answer)
    }
    .await;
    Ok(match work {
        Ok(content) => JobOutcome::Completed {
            result: json!({"worker_id":worker,"content":content,"checks":{"status":"unchecked"}}),
        },
        Err(error) => JobOutcome::Uncertain {
            error: bounded_error(format!(
                "Falló la consulta al especialista {name} ({worker}): {error}"
            )),
        },
    })
}

fn tool_result(report: &Value, grouped: bool) -> Value {
    let results = report["tasks"].as_array().into_iter().flatten().map(|task| {
        let job=&task["job"]; let completed=job["status"]==serde_json::to_value(JobStatus::Completed).expect("status");
        json!({"task_id":task["task"]["id"],"worker_id":task["task"]["specialist_id"],"ok":completed && task["verification"] != "failed",
            "status":job["status"],"content":job["result"]["content"],"error":job["error"],
            "checks":{"status":task["verification"]},"proof":task["proof"],"job_id":job["id"]})
    }).collect::<Vec<_>>();
    if !grouped {
        let mut result = results.into_iter().next().expect("nonempty group");
        result["group_id"] = report["id"].clone();
        result
    } else {
        json!({"group_id":report["id"],"status":"completed","ok":results.iter().all(|r|r["ok"]==true),"results":results})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_agent_core::{objective::ObjectivePolicy, sessions::SessionStore};
    #[test]
    fn explicit_durable_proofs_reach_the_parent_and_negative_checks_prevent_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        let queue = store.queue("fixture").unwrap();
        queue.open(QueuePolicy::default()).unwrap();
        let group = queue
            .create_group(
                "turn",
                GroupSpec {
                    objective_id: "root".into(),
                    coordinator_agent_id: "lead".into(),
                    depth: 1,
                    require_verification: false,
                    tasks: vec![SpecialistTask {
                        id: "task".into(),
                        specialist_id: "worker".into(),
                        prompt: "Check receipt".into(),
                    }],
                },
            )
            .unwrap();
        let job = queue
            .claim(
                "worker",
                &[group["specialist_kind"].as_str().unwrap().into()],
            )
            .unwrap()
            .unwrap();
        queue
            .finish(
                &job.id,
                job.execution_id.as_deref().unwrap(),
                JobOutcome::Completed {
                    result: json!({"content":"claimed result"}),
                },
            )
            .unwrap();
        let unchecked = tool_result(&queue.group("turn").unwrap(), false);
        assert_eq!(unchecked["ok"], true);
        assert_eq!(unchecked["checks"]["status"], "unchecked");
        assert!(unchecked["proof"].is_null());
        let input = queue.group_verification_input("turn", "task").unwrap();
        let claim = store
            .objective_command(
                "root",
                &json!({"op":"verify_begin","key":"check","input":input}).to_string(),
            )
            .unwrap();
        store.objective_command("root", &json!({"op":"verify_finish","key":"check","execution_id":claim["execution_id"],
            "verdict":{"passed":false,"reason":"receipt did not match","evidence":["fixture assertion"]}}).to_string()).unwrap();
        queue.record_group_proof("turn", "task", "check").unwrap();
        let checked = tool_result(&queue.group("turn").unwrap(), false);
        assert_eq!(checked["ok"], false);
        assert_eq!(checked["checks"]["status"], "failed");
        assert_eq!(
            checked["proof"]["verdict"]["reason"],
            "receipt did not match"
        );
    }
}
