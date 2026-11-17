//! Try the desktop backends on a real machine (Linux X11 or Wayland, Windows, macOS).
//!
//! ```text
//! cargo run -p hive-agent-core --features computer-use --example computer_demo -- screenshot shot.png
//! cargo run -p hive-agent-core --features computer-use --example computer_demo -- --allow-input move 400 300
//! cargo run -p hive-agent-core --features computer-use --example computer_demo -- --allow-input click 400 300
//! cargo run -p hive-agent-core --features computer-use --example computer_demo -- --allow-input type "hola"
//! cargo run -p hive-agent-core --features computer-use --example computer_demo -- --allow-input key ctrl+a
//! ```
//!
//! Without `--allow-input` only screenshots are possible, so a first run cannot touch anything.
//! Coordinates are pixels of the screenshot the tool returns (the screen scaled to fit 1280). Add
//! `--backend x11|portal` on Linux. The Windows and macOS backends are theoretical (never run before
//! this): please report exactly what happens, including the error text.
use hive_agent_core::{provider::CancellationToken, tools::NativeTools};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let allow_input = take_flag(&mut args, "--allow-input");
    let backend = take_option(&mut args, "--backend").unwrap_or_else(|| "auto".into());
    let dir = std::env::temp_dir().join("hive-computer-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let tools = NativeTools::from_json(json!({"root":dir,"computer":{"backend":backend,"allow_input":allow_input,"max_dimension":1280}})).unwrap();
    let cancel = CancellationToken::new();
    let run = |name: &'static str, arguments: Value| {
        let (tools, cancel) = (tools.clone(), cancel.clone());
        async move {
            match tools.execute_tool(name, arguments, &cancel).await {
                Ok(result) => result,
                Err(error) => {
                    println!("{name}: ERROR {error}");
                    std::process::exit(1);
                }
            }
        }
    };
    let number = |text: Option<&String>| {
        text.and_then(|t| t.parse::<u64>().ok())
            .unwrap_or_else(|| usage())
    };
    match args.first().map(String::as_str) {
        Some("screenshot") => {
            let name = args.get(1).cloned().unwrap_or_else(|| "shot.png".into());
            let result = run(
                "computer_screenshot",
                json!({"path":name,"create_dirs":true}),
            )
            .await;
            println!(
                "backend={} size={}x{} (the pointer space is {}x{}) saved to {}",
                result["backend"],
                result["width"],
                result["height"],
                result["screen"]["width"],
                result["screen"]["height"],
                dir.join(&name).display()
            );
        }
        Some("move") => {
            run(
                "computer_mouse_move",
                json!({"x":number(args.get(1)),"y":number(args.get(2))}),
            )
            .await;
            println!("moved");
        }
        Some("click") => {
            run(
                "computer_click",
                json!({"x":number(args.get(1)),"y":number(args.get(2))}),
            )
            .await;
            println!("clicked");
        }
        Some("type") => {
            run(
                "computer_type",
                json!({"text":args.get(1).cloned().unwrap_or_else(|| usage())}),
            )
            .await;
            println!("typed");
        }
        Some("key") => {
            run(
                "computer_key",
                json!({"keys":args.get(1).cloned().unwrap_or_else(|| usage())}),
            )
            .await;
            println!("pressed");
        }
        _ => usage(),
    }
    tools.close().await;
}

fn usage() -> ! {
    eprintln!(
        "usage: computer_demo [--allow-input] [--backend x11|portal] screenshot [file] | move X Y | click X Y | type TEXT | key KEYS"
    );
    std::process::exit(2);
}
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}
fn take_option(args: &mut Vec<String>, name: &str) -> Option<String> {
    let at = args.iter().position(|a| a == name)?;
    args.remove(at);
    (at < args.len()).then(|| args.remove(at))
}
