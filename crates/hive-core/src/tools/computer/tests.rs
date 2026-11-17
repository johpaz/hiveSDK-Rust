use super::{fake::Fake, *};
use crate::tools::{NativeTools, ToolsConfig};

fn tools(policy: Value, screen: (u32, u32)) -> (NativeTools, Arc<Fake>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ToolsConfig::new(dir.path());
    config.computer = Some(serde_json::from_value(policy).unwrap());
    let tools = NativeTools::new(config).unwrap();
    let fake = Arc::new(Fake::new(screen));
    *tools.inner.computer_state.preset.lock().unwrap() =
        Some(Arc::new(Backend::Fake(fake.clone())));
    (tools, fake, dir)
}
async fn call(tools: &NativeTools, name: &str, arguments: Value) -> Result<Value> {
    tools
        .execute_tool(name, arguments, &CancellationToken::new())
        .await
}
fn names(tools: &NativeTools) -> Vec<String> {
    tools
        .tool_definitions()
        .into_iter()
        .map(|t| t.name)
        .filter(|n| n.starts_with("computer_"))
        .collect()
}

#[test]
fn it_is_off_unless_configured_and_input_is_a_separate_grant() {
    let dir = tempfile::tempdir().unwrap();
    let plain = NativeTools::new(ToolsConfig::new(dir.path())).unwrap();
    assert!(
        names(&plain).is_empty(),
        "no computer tools without the policy"
    );
    let (shots, _, _) = tools(json!({}), (1920, 1080));
    assert_eq!(
        names(&shots),
        ["computer_screenshot", "computer_wait"],
        "input is not granted by default"
    );
    let (all, _, _) = tools(json!({"allow_input":true}), (1920, 1080));
    assert_eq!(names(&all).len(), 8);
    let (input_only, _, _) = tools(
        json!({"allow_screenshot":false,"allow_input":true}),
        (1920, 1080),
    );
    assert!(!names(&input_only).contains(&"computer_screenshot".to_owned()));
    for bad in [
        json!({"allow_screenshot":false}),
        json!({"max_dimension":10}),
        json!({"max_actions":0}),
        json!({"restore_token_path":"relative"}),
        json!({"surprise":1}),
    ] {
        let mut config = ToolsConfig::new(dir.path());
        let parsed: std::result::Result<ComputerPolicy, _> = serde_json::from_value(bad.clone());
        config.computer = parsed.ok();
        assert!(
            config.computer.is_none() || NativeTools::new(config).is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn a_tool_that_was_not_granted_cannot_run_even_when_called_by_name() {
    let (tools, fake, _dir) = tools(json!({}), (1920, 1080));
    assert!(
        call(&tools, "computer_click", json!({"x":1,"y":1}))
            .await
            .is_err()
    );
    assert!(fake.events().is_empty());
}

#[test]
fn the_view_is_the_screen_scaled_to_fit_and_clicks_map_back_without_leaving_the_screen() {
    assert_eq!(view_size((1920, 1080), 1280), (1280, 720));
    assert_eq!(view_size((800, 600), 1280), (800, 600), "never enlarged");
    assert_eq!(view_size((1080, 1920), 1280), (720, 1280));
    assert_eq!(
        to_screen((0, 0), (1280, 720), (1920, 1080)).unwrap(),
        (0, 0)
    );
    assert_eq!(
        to_screen((640, 360), (1280, 720), (1920, 1080)).unwrap(),
        (960, 540)
    );
    assert_eq!(
        to_screen((1279, 719), (1280, 720), (1920, 1080)).unwrap(),
        (1919, 1079),
        "the last pixel is the last pixel"
    );
    assert!(to_screen((1280, 0), (1280, 720), (1920, 1080)).is_err());
    assert_eq!(
        to_screen((5, 5), (800, 600), (800, 600)).unwrap(),
        (5, 5),
        "same size is exact"
    );
}

#[tokio::test]
async fn a_screenshot_is_in_the_view_and_a_region_zooms_into_it() {
    let (tools, _, _dir) = tools(json!({"max_dimension":640}), (1920, 1080));
    let shot = call(&tools, "computer_screenshot", json!({}))
        .await
        .unwrap();
    assert_eq!(
        (shot["width"].as_u64(), shot["height"].as_u64()),
        (Some(640), Some(360))
    );
    assert_eq!(shot["screen"], json!({"width":640,"height":360}));
    assert!(shot["data"].as_str().is_some_and(|d| !d.is_empty()) && shot["backend"] == "fake");
    let zoom = call(
        &tools,
        "computer_screenshot",
        json!({"region":{"x":0,"y":0,"width":320,"height":180}}),
    )
    .await
    .unwrap();
    assert_eq!(
        (zoom["width"].as_u64(), zoom["height"].as_u64()),
        (Some(640), Some(360)),
        "the region fills the view: more detail"
    );
    assert!(
        call(
            &tools,
            "computer_screenshot",
            json!({"region":{"x":600,"y":0,"width":100,"height":10}})
        )
        .await
        .is_err(),
        "outside the view"
    );
}

#[tokio::test]
async fn a_capture_at_another_resolution_is_brought_to_the_pointer_space() {
    let (tools, fake, _dir) = tools(json!({"max_dimension":500}), (1000, 500));
    // A HiDPI capture is twice the pointer space.
    let doubled = Arc::new(Fake {
        capture: (2000, 1000),
        ..Fake::new((1000, 500))
    });
    let _ = fake;
    *tools.inner.computer_state.preset.lock().unwrap() = Some(Arc::new(Backend::Fake(doubled)));
    let shot = call(&tools, "computer_screenshot", json!({}))
        .await
        .unwrap();
    assert_eq!(
        (shot["width"].as_u64(), shot["height"].as_u64()),
        (Some(500), Some(250))
    );
}

#[tokio::test]
async fn a_large_screenshot_falls_back_to_smaller_encodings_and_can_be_saved_instead() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ToolsConfig::new(dir.path());
    config.max_output_bytes = 96 * 1024;
    config.computer = Some(serde_json::from_value(json!({"max_dimension":1280})).unwrap());
    let tools = NativeTools::new(config).unwrap();
    *tools.inner.computer_state.preset.lock().unwrap() =
        Some(Arc::new(Backend::Fake(Arc::new(Fake::new((1920, 1080))))));
    let shot = call(&tools, "computer_screenshot", json!({}))
        .await
        .unwrap();
    assert_eq!(
        shot["mime_type"], "image/jpeg",
        "PNG did not fit, a JPEG did"
    );
    assert!(shot["data"].as_str().unwrap().len() <= 96 * 1024);
    let saved = call(
        &tools,
        "computer_screenshot",
        json!({"path":"shots/a.png","create_dirs":true}),
    )
    .await
    .unwrap();
    assert!(
        saved["data"].is_null()
            && dir.path().join("shots/a.png").exists()
            && saved["mime_type"] == "image/png"
    );
    assert!(
        call(
            &tools,
            "computer_screenshot",
            json!({"path":"../escape.png"})
        )
        .await
        .is_err(),
        "the sandbox root still applies"
    );
}

#[tokio::test]
async fn clicks_moves_drags_and_scrolls_reach_the_screen_at_the_mapped_points() {
    let (tools, fake, _dir) = tools(
        json!({"allow_input":true,"max_dimension":960}),
        (1920, 1080),
    );
    call(
        &tools,
        "computer_click",
        json!({"x":480,"y":270,"button":"right","count":2}),
    )
    .await
    .unwrap();
    assert_eq!(
        fake.events(),
        [
            "move 961 541",
            "button Right down",
            "button Right up",
            "button Right down",
            "button Right up"
        ]
    );
    fake.events.lock().unwrap().clear();
    call(
        &tools,
        "computer_drag",
        json!({"from_x":0,"from_y":0,"to_x":959,"to_y":539}),
    )
    .await
    .unwrap();
    let events = fake.events();
    assert_eq!(events[0], "move 1 1");
    assert_eq!(events[1], "button Left down");
    assert_eq!(events[events.len() - 2], "move 1919 1079");
    assert_eq!(events.last().unwrap(), "button Left up");
    fake.events.lock().unwrap().clear();
    call(&tools, "computer_scroll", json!({"dy":3,"dx":-1}))
        .await
        .unwrap();
    assert_eq!(fake.events(), ["scroll -1 3"]);
    for bad in [
        ("computer_click", json!({"x":5})),
        ("computer_click", json!({"x":960,"y":0})),
        ("computer_scroll", json!({})),
        ("computer_mouse_move", json!({"x":1})),
    ] {
        assert!(call(&tools, bad.0, bad.1.clone()).await.is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn text_and_key_combinations_are_pressed_in_order_and_modifiers_are_released() {
    let (tools, fake, _dir) = tools(json!({"allow_input":true}), (800, 600));
    call(&tools, "computer_key", json!({"keys":"ctrl+shift+T"}))
        .await
        .unwrap();
    assert_eq!(
        fake.events(),
        [
            "key 0xffe3 down",
            "key 0xffe1 down",
            "key 0x54 down",
            "key 0x54 up",
            "key 0xffe1 up",
            "key 0xffe3 up"
        ]
    );
    fake.events.lock().unwrap().clear();
    call(&tools, "computer_type", json!({"text":"hi\né€"}))
        .await
        .unwrap();
    let typed: Vec<String> = fake
        .events()
        .into_iter()
        .filter(|e| e.ends_with("down"))
        .collect();
    assert_eq!(
        typed,
        [
            "key 0x68 down",
            "key 0x69 down",
            "key 0xff0d down",
            "key 0xe9 down",
            "key 0x10020ac down"
        ]
    );
    for bad in [
        "",
        "ctrl+",
        "a+b",
        "nonsense",
        "ctrl+shift+alt+super+x+y",
        "F99",
    ] {
        assert!(
            call(&tools, "computer_key", json!({"keys":bad}))
                .await
                .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        call(&tools, "computer_type", json!({"text":"x".repeat(2001)}))
            .await
            .is_err(),
        "over max_text_chars"
    );
}

#[tokio::test]
async fn the_action_budget_runs_out_and_only_the_host_refills_it() {
    let (tools, fake, _dir) = tools(json!({"allow_input":true,"max_actions":3}), (800, 600));
    for _ in 0..3 {
        call(&tools, "computer_mouse_move", json!({"x":1,"y":1}))
            .await
            .unwrap();
    }
    let error = call(&tools, "computer_mouse_move", json!({"x":1,"y":1}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("budget"), "{error}");
    assert_eq!(fake.events().len(), 3, "the fourth did nothing");
    assert_eq!(
        tools.computer_status(),
        json!({"stopped":false,"actions":3})
    );
    tools
        .dispatch(json!({"op":"computer_reset"}), &CancellationToken::new())
        .await
        .unwrap();
    call(&tools, "computer_mouse_move", json!({"x":1,"y":1}))
        .await
        .unwrap();
}

#[tokio::test]
async fn stop_refuses_everything_until_the_host_resumes() {
    let (tools, fake, _dir) = tools(json!({"allow_input":true}), (800, 600));
    call(&tools, "computer_mouse_move", json!({"x":1,"y":1}))
        .await
        .unwrap();
    let status = tools
        .dispatch(json!({"op":"computer_stop"}), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(status["stopped"], true);
    for (name, args) in [
        ("computer_mouse_move", json!({"x":1,"y":1})),
        ("computer_screenshot", json!({})),
        ("computer_wait", json!({"ms":1})),
    ] {
        let error = call(&tools, name, args).await.unwrap_err().to_string();
        assert!(error.contains("stopped"), "{name}: {error}");
    }
    assert_eq!(fake.events().len(), 1);
    tools.computer_resume();
    call(&tools, "computer_mouse_move", json!({"x":2,"y":2}))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_interrupted_action_releases_what_it_held_and_is_reported_uncertain() {
    let (tools, fake, _dir) = tools(
        json!({"allow_input":true,"action_timeout_ms":300}),
        (800, 600),
    );
    *fake.stall.lock().unwrap() = Some(Duration::from_secs(5));
    // The stall happens inside a key event, but the fake's key is synchronous; cancel instead while a drag is running.
    *fake.stall.lock().unwrap() = None;
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let runner = {
        let tools = tools.clone();
        tokio::spawn(async move {
            tools
                .execute_tool(
                    "computer_drag",
                    json!({"from_x":0,"from_y":0,"to_x":700,"to_y":500}),
                    &token,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;
    cancel.cancel();
    let error = runner.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("UNCERTAIN"), "{error}");
    let events = fake.events();
    assert_eq!(
        events.last().unwrap(),
        "button Left up",
        "the button was let go: {events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| e.ends_with("down")).count(),
        events.iter().filter(|e| e.ends_with("up")).count()
    );
}

#[tokio::test]
async fn wait_is_bounded_and_cancellable() {
    let (tools, _, _dir) = tools(json!({}), (800, 600));
    assert!(
        call(&tools, "computer_wait", json!({"ms":10001}))
            .await
            .is_err()
    );
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let waiting = tokio::spawn({
        let tools = tools.clone();
        async move {
            tools
                .execute_tool("computer_wait", json!({"ms":9000}), &token)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancel.cancel();
    assert!(matches!(waiting.await.unwrap(), Err(Error::Cancelled)));
}

#[tokio::test]
async fn stop_interrupts_an_action_that_is_already_running() {
    let (tools, fake, _dir) = tools(json!({"allow_input":true}), (800, 600));
    let runner = tokio::spawn({
        let tools = tools.clone();
        async move {
            call(
                &tools,
                "computer_drag",
                json!({"from_x":0,"from_y":0,"to_x":700,"to_y":500}),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    tools.computer_stop();
    let error = runner.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("UNCERTAIN"), "{error}");
    let events = fake.events();
    assert_eq!(
        events.last().unwrap(),
        "button Left up",
        "what was held is let go: {events:?}"
    );
    assert!(events.len() < 11, "it stopped midway: {events:?}");
}
