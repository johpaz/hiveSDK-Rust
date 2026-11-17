mod activity;
mod channels;
mod computer;
mod config;
mod conversations;
mod dashboard;
mod discord;
mod integrations;
mod management;
mod mcp_servers;
mod meetings;
mod model_catalog;
mod model_defaults;
mod ocr;
mod oracle;
mod panel;
mod piper;
mod ratelimit;
mod realtime;
mod scheduler;
mod server;
mod slack;
mod telegram;
mod whatsapp;
mod whatsapp_qr;

use std::{path::PathBuf, sync::Arc};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Hive gateway Rust: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--prepare-test-home") {
        let neutral = args.last().map(String::as_str) == Some("--neutral");
        if args.len() != 3 + usize::from(neutral) {
            return Err(
                "Uso: --prepare-test-home ORIGEN DESTINO [--neutral] (Hive anterior detenido)"
                    .into(),
            );
        }
        config::copy_home(&PathBuf::from(&args[1]), &PathBuf::from(&args[2]))?;
        if neutral {
            for line in config::neutralize_home(&PathBuf::from(&args[2]))? {
                println!("{line}");
            }
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--help") {
        println!(
            "hive-gateway-rust [start [--skip-check]]\nHIVE_HOME: perfil HiveDB; HIVE_PORT: puerto local (18790)\n--check-config: valida HiveDB y credenciales del proveedor sin llamar al modelo\n--prepare-test-home ORIGEN DESTINO [--neutral]: copia aislada con Hive detenido (--neutral apaga MCP y pausa el cron en la copia)"
        );
        return Ok(());
    }
    if !args.is_empty()
        && args != ["start"]
        && args != ["start", "--skip-check"]
        && args != ["--check-config"]
    {
        return Err("Argumentos desconocidos; usa --help".into());
    }
    let home = config::home()?;
    let state = Arc::new(config::App::open(home)?);
    if args == ["--check-config"] {
        state.selection()?;
        println!("HiveDB y configuración de chat Rust válidas (sin llamadas al modelo)");
        return Ok(());
    }
    let port: u16 = std::env::var("HIVE_PORT")
        .unwrap_or_else(|_| "18790".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("hive-gateway-rust listo en {}", listener.local_addr()?);
    tokio::spawn(scheduler::run(state.clone()));
    tokio::spawn(telegram::run(state.clone()));
    tokio::spawn(discord::run(state.clone()));
    tokio::spawn(slack::run(state.clone()));
    tokio::spawn(whatsapp_qr::run(state.clone()));
    tokio::spawn(mcp_servers::run(state.clone()));
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => { state.shutdown.cancel(); break; }
            connection = listener.accept() => {
                let (stream, _) = connection?;
                // Replies are written whole: without this, Nagle holds the tail of a large reply until the
                // client's delayed ACK arrives (tens of milliseconds per request).
                let _ = stream.set_nodelay(true);
                let app = state.clone();
                tokio::spawn(async move {
                    let _ = server::connection(app, stream).await;
                });
            }
        }
    }
    Ok(())
}
