//! The X11 backend against a real X server. Set `HIVE_TEST_X11_DISPLAY` to a *disposable* display
//! (for example a nested `Xwayland :9`): the test moves the pointer and types into it. Without the
//! variable it does nothing, so it never touches the display of whoever runs the suite.
#![cfg(all(feature = "computer-use", target_os = "linux"))]
use hive_agent_core::{provider::CancellationToken, tools::NativeTools};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

async fn run(tools: &NativeTools, name: &str, arguments: Value) -> Value {
    tools
        .execute_tool(name, arguments, &CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn next_event(conn: &RustConnection, wait: Duration) -> Option<Event> {
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if let Some(event) = conn.poll_for_event().unwrap() {
            return Some(event);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[tokio::test]
async fn it_captures_moves_clicks_scrolls_and_types_on_a_real_x_server() {
    let Ok(display) = std::env::var("HIVE_TEST_X11_DISPLAY") else {
        eprintln!("HIVE_TEST_X11_DISPLAY is not set: skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tools = NativeTools::from_json(json!({"root":dir.path(),
        "computer":{"backend":"x11","display":display,"allow_input":true,"max_dimension":400}}))
    .unwrap();

    // A window that receives what the SDK injects.
    let (conn, screen) = RustConnection::connect(Some(&display)).unwrap();
    let root = conn.setup().roots[screen].clone();
    let window = conn.generate_id().unwrap();
    conn.create_window(
        root.root_depth,
        window,
        root.root,
        0,
        0,
        root.width_in_pixels,
        root.height_in_pixels,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS | EventMask::EXPOSURE)
            .background_pixel(root.white_pixel),
    )
    .unwrap();
    conn.map_window(window).unwrap();
    conn.set_input_focus(InputFocus::POINTER_ROOT, window, 0u32)
        .unwrap();
    conn.sync().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    while conn.poll_for_event().unwrap().is_some() {}
    let (width, height) = (
        u32::from(root.width_in_pixels),
        u32::from(root.height_in_pixels),
    );

    // Screenshot: in the model's view, a PNG of the right size.
    let shot = run(&tools, "computer_screenshot", json!({})).await;
    let view = (400.0 * f64::from(width.min(height)) / f64::from(width.max(height))).round() as u64;
    assert_eq!(shot["mime_type"], "image/png");
    let (w, h) = (
        shot["width"].as_u64().unwrap(),
        shot["height"].as_u64().unwrap(),
    );
    assert_eq!(w.max(h), 400);
    assert_eq!(w.min(h), view);
    assert_eq!(shot["backend"], "x11");

    // The pointer lands where the view says.
    run(&tools, "computer_mouse_move", json!({"x":w / 2,"y":h / 2})).await;
    let pointer = conn.query_pointer(root.root).unwrap().reply().unwrap();
    let expect =
        |value: u64, view: u64, screen: u32| (value * 2 + 1) * u64::from(screen) / (view * 2);
    assert_eq!(i64::from(pointer.root_x), expect(w / 2, w, width) as i64);
    assert_eq!(i64::from(pointer.root_y), expect(h / 2, h, height) as i64);

    // A click and a wheel notch arrive as the matching button events.
    run(&tools, "computer_click", json!({})).await;
    run(&tools, "computer_scroll", json!({"dy":1})).await;
    let mut buttons = vec![];
    while let Some(event) = next_event(&conn, Duration::from_millis(500)) {
        if let Event::ButtonPress(press) = event {
            buttons.push(press.detail);
        }
    }
    assert_eq!(buttons, [1, 5], "left click, then wheel down");

    // Typing: a letter, a shifted letter, a character the layout has no key for, and a combination.
    let map = conn
        .get_keyboard_mapping(
            conn.setup().min_keycode,
            conn.setup().max_keycode - conn.setup().min_keycode + 1,
        )
        .unwrap()
        .reply()
        .unwrap();
    run(&tools, "computer_type", json!({"text":"aB€"})).await;
    run(&tools, "computer_key", json!({"keys":"ctrl+a"})).await;
    // The map has to be read again: typing '€' may have borrowed a spare keycode.
    let map_after = conn
        .get_keyboard_mapping(
            conn.setup().min_keycode,
            conn.setup().max_keycode - conn.setup().min_keycode + 1,
        )
        .unwrap()
        .reply()
        .unwrap();
    let _ = map;
    let per = usize::from(map_after.keysyms_per_keycode);
    let mut typed = vec![];
    while let Some(event) = next_event(&conn, Duration::from_millis(600)) {
        if let Event::KeyPress(press) = event {
            let row = usize::from(press.detail - conn.setup().min_keycode) * per;
            let level = usize::from(u16::from(press.state) & 1);
            let keysym = map_after.keysyms[row + level];
            // Modifier keys are keysyms in the 0xffe_ range.
            if (0xffe1..=0xffee).contains(&keysym) {
                continue;
            }
            typed.push((keysym, u16::from(press.state) & 0x4 != 0));
        }
    }
    // The euro sign has two keysyms (legacy 0x20ac and Unicode 0x1000000 | 0x20ac); both mean it.
    for (keysym, _) in &mut typed {
        if *keysym == 0x0100_20ac {
            *keysym = 0x20ac;
        }
    }
    assert_eq!(
        typed,
        [(0x61, false), (0x42, false), (0x20ac, false), (0x61, true)],
        "a, B, €, then ctrl+a"
    );

    // Nothing is left held down.
    let pointer = conn.query_pointer(root.root).unwrap().reply().unwrap();
    assert_eq!(
        u16::from(pointer.mask) & 0x1f00,
        0,
        "no mouse button is held"
    );
    tools.close().await;
}
