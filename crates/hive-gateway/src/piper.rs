//! Native Piper service hosted by the authenticated gateway; no Bun/Python daemon.
use crate::{Result, config::App, integrations};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
const VOICES: [&str; 3] = [
    "es_MX-claude-14947-epoch-high",
    "es_MX-cortana-19669-epoch-high",
    "es_MX-gevy-10196-epoch-high",
];
const NAMES: [&str; 3] = ["Claude (México)", "Cortana (México)", "Gevy (México)"];
#[derive(Default)]
pub(crate) struct Runtime {
    ready: AtomicBool,
    busy: AtomicBool,
    logs: Mutex<Vec<String>>,
}
fn root(app: &App) -> PathBuf {
    std::env::var_os("HIVE_TTS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| app.home.join("tts"))
}
fn binary(root: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "piper.exe" } else { "piper" };
    [
        root.join("bin").join(name),
        root.join("bin/piper").join(name),
        root.join("bin/native").join(name),
    ]
    .into_iter()
    .find(|p| p.is_file())
}
fn installed(root: &Path, voice: &str) -> bool {
    root.join("voices").join(format!("{voice}.onnx")).is_file()
        && root
            .join("voices")
            .join(format!("{voice}.onnx.json"))
            .is_file()
}
fn settings(app: &App) -> Result<Value> {
    Ok(app.db.col_get("config","piper_local")?.map(|r|r.doc).unwrap_or_else(||json!({"id":"piper_local","voice":VOICES[0],"length_scale":1.0,"noise_scale":0.667,"noise_w":0.8,"sentence_silence":0.2})))
}
fn log(app: &App, message: &str) {
    if let Ok(mut logs) = app.piper.logs.lock() {
        logs.push(message.chars().take(300).collect());
        if logs.len() > 100 {
            logs.remove(0);
        }
    }
}
fn status(app: &App) -> Result<Value> {
    let root = root(app);
    let cfg = settings(app)?;
    let voices=VOICES.iter().zip(NAMES).map(|(id,name)|json!({"id":id,"name":name,"installed":installed(&root,id),"selected":cfg["voice"]==*id})).collect::<Vec<_>>();
    Ok(
        json!({"installed":binary(&root).is_some(),"running":app.piper.ready.load(Ordering::SeqCst),"busy":app.piper.busy.load(Ordering::SeqCst),"models":voices,"settings":cfg,"logs":app.piper.logs.lock().map(|l|l.clone()).unwrap_or_default(),"service":"gateway-native"}),
    )
}
fn valid_voice(body: &Value) -> Result<&str> {
    let voice = body["voice"].as_str().ok_or("Selecciona una voz")?;
    if !VOICES.contains(&voice) {
        return Err("Voz local desconocida".into());
    }
    Ok(voice)
}
fn config_patch(mut cfg: Value, body: &Value, root: &Path) -> Result<Value> {
    if body.get("voice").is_some() {
        let voice = valid_voice(body)?;
        if !installed(root, voice) {
            return Err("Descarga la voz antes de seleccionarla".into());
        }
        cfg["voice"] = json!(voice);
    }
    for (name, min, max) in [
        ("length_scale", 0.5, 2.0),
        ("noise_scale", 0.0, 1.0),
        ("noise_w", 0.0, 1.0),
        ("sentence_silence", 0.0, 2.0),
    ] {
        if let Some(value) = body.get(name) {
            let n = value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= min && *n <= max)
                .ok_or("Parámetro de síntesis fuera de rango")?;
            cfg[name] = json!(n);
        }
    }
    Ok(cfg)
}
async fn download(url: &str, path: &Path, max: usize) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "No se pudo descargar el archivo de Piper")?;
    if !response.status().is_success() {
        return Err(format!(
            "La descarga de Piper respondió HTTP {}",
            response.status().as_u16()
        )
        .into());
    }
    let bytes = integrations::limited(response, max).await?;
    if bytes.is_empty() {
        return Err("Archivo descargado vacío".into());
    }
    std::fs::write(path, bytes)?;
    Ok(())
}
fn archive() -> Result<&'static str> {
    match (std::env::consts::OS,std::env::consts::ARCH){
 ("linux","x86_64")=>Ok("piper_linux_x86_64.tar.gz"),("linux","aarch64")=>Ok("piper_linux_aarch64.tar.gz"),
 ("macos","x86_64")=>Ok("piper_macos_x86_64.tar.gz"),("macos","aarch64")=>Ok("piper_macos_aarch64.tar.gz"),
 _=>Err("La instalación automática del binario original está disponible en Linux/macOS x64 y ARM64".into())}
}
async fn install(root: PathBuf) -> Result<()> {
    let file = archive()?;
    std::fs::create_dir_all(root.join("bin"))?;
    let stage = root.join(format!(".install-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&stage)?;
    let result = async {
        let path = stage.join("piper.tar.gz");
        download(
            &format!("https://github.com/rhasspy/piper/releases/download/2023.11.14-2/{file}"),
            &path,
            150 * 1024 * 1024,
        )
        .await?;
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new("tar")
                .arg("-xzf")
                .arg(&path)
                .arg("-C")
                .arg(&stage)
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        if !output.status.success() {
            return Err("No se pudo extraer el binario de Piper".into());
        }
        let executable = stage.join("piper/piper");
        if !executable.is_file() {
            return Err("La distribución de Piper no contiene el binario esperado".into());
        }
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&executable)
                .arg("--help")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        if !output.status.success() {
            return Err("El binario de Piper no se puede ejecutar en este sistema".into());
        }
        let target = root.join("bin/native");
        if target.exists() {
            return Err("Ya existe bin/native; revisa la instalación anterior".into());
        }
        std::fs::rename(stage.join("piper"), target)?;
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(stage);
    result
}
async fn download_voice(root: PathBuf, voice: String) -> Result<()> {
    std::fs::create_dir_all(root.join("voices"))?;
    let stage = root.join(format!(".voice-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&stage)?;
    let result=async {
  for (ext,max) in [("onnx",150*1024*1024),("onnx.json",1024*1024)] {
   download(&format!("https://huggingface.co/spaces/HirCoir/Piper-TTS-Spanish/resolve/main/{voice}.{ext}"),&stage.join(format!("{voice}.{ext}")),max).await?;
  }
  let cfg:Value=serde_json::from_slice(&std::fs::read(stage.join(format!("{voice}.onnx.json")))?)?;
  if cfg["audio"]["sample_rate"].as_u64().is_none(){return Err("Configuración de voz sin frecuencia de audio".into());}
  for ext in ["onnx.json","onnx"] {std::fs::rename(stage.join(format!("{voice}.{ext}")),root.join("voices").join(format!("{voice}.{ext}")))?;}
  Ok(())
 }.await;
    let _ = std::fs::remove_dir_all(stage);
    result
}
async fn synthesize(root: PathBuf, cfg: Value, text: String) -> Result<Value> {
    let binary = binary(&root).ok_or("Instala Piper primero")?;
    let voice = cfg["voice"].as_str().ok_or("Selecciona una voz")?;
    if !VOICES.contains(&voice) || !installed(&root, voice) {
        return Err("Descarga la voz seleccionada primero".into());
    }
    let output = root.join(format!(".speak-{}.wav", uuid::Uuid::new_v4()));
    let result = async {
        let mut command = Command::new(binary);
        command
            .arg("--model")
            .arg(root.join("voices").join(format!("{voice}.onnx")))
            .arg("--output_file")
            .arg(&output);
        for flag in ["length_scale", "noise_scale", "noise_w", "sentence_silence"] {
            if let Some(n) = cfg[flag].as_f64() {
                command.arg(format!("--{flag}")).arg(n.to_string());
            }
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(format!("{text}\n").as_bytes()).await?;
        }
        let status = tokio::time::timeout(Duration::from_secs(60), child.wait()).await??;
        if !status.success() {
            return Err("Piper no pudo sintetizar la voz; comprueba el modelo descargado".into());
        }
        let metadata = std::fs::metadata(&output)?;
        if metadata.len() > 8 * 1024 * 1024 {
            return Err("El audio sintetizado supera el límite".into());
        }
        let bytes = std::fs::read(&output)?;
        if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err("Piper no produjo audio WAV válido".into());
        }
        Ok(json!({"ok":true,"audio":STANDARD.encode(bytes),"mime":"audio/wav"}))
    }
    .await;
    let _ = std::fs::remove_file(output);
    result
}
struct Busy<'a>(&'a AtomicBool);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    if !path.starts_with("/api/tts-local/") {
        return Ok(None);
    }
    let operation = path.trim_start_matches("/api/tts-local/");
    let result = (|| -> Result<Value> {
        if method == "GET" && matches!(operation, "status" | "models" | "voices" | "logs") {
            return status(app);
        }
        if method != "POST" {
            return Err("Operación de Piper desconocida".into());
        }
        if app.piper.busy.swap(true, Ordering::SeqCst) {
            return Err("Piper está ocupado; espera a que termine la operación".into());
        }
        let _busy = Busy(&app.piper.busy);
        let body: Value = serde_json::from_slice(body)?;
        let root = root(app);
        match operation {
            "install" => {
                if binary(&root).is_none() {
                    log(app, "Descargando el binario nativo de Piper…");
                    integrations::blocking(install(root))?;
                }
                log(app, "Binario de Piper instalado");
            }
            "models/download" => {
                let voice = valid_voice(&body)?.to_owned();
                if !installed(&root, &voice) {
                    log(app, "Descargando voz local…");
                    integrations::blocking(download_voice(root, voice))?;
                }
                log(app, "Voz local descargada");
            }
            "start" => {
                let cfg = settings(app)?;
                if binary(&root).is_none() {
                    return Err("Instala Piper primero".into());
                }
                let voice = cfg["voice"].as_str().unwrap_or(VOICES[0]);
                if !installed(&root, voice) {
                    return Err("Descarga y selecciona una voz antes de iniciar".into());
                }
                app.piper.ready.store(true, Ordering::SeqCst);
                log(app, "Servicio Piper iniciado en el gateway nativo");
            }
            "stop" => {
                app.piper.ready.store(false, Ordering::SeqCst);
                log(app, "Servicio Piper detenido");
            }
            "config" => {
                let cfg = config_patch(settings(app)?, &body, &root)?;
                let _guard = app
                    .activity_lock
                    .write()
                    .map_err(|_| "Configuración bloqueada")?;
                let old = app.db.col_get("config", "piper_local")?;
                app.db.col_put(
                    "config",
                    "piper_local",
                    &cfg,
                    hivedb_core::PutOptions {
                        expected_version: Some(old.map_or(0, |r| r.version)),
                    },
                )?;
            }
            "speak" => {
                if !app.piper.ready.load(Ordering::SeqCst) {
                    return Err("Inicia Piper primero".into());
                }
                let text = body["text"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty() && s.len() <= 2000)
                    .ok_or("Escribe de 1 a 2000 caracteres para probar la voz")?
                    .to_owned();
                return integrations::blocking(synthesize(root, settings(app)?, text));
            }
            _ => return Err("Operación de Piper desconocida".into()),
        }
        // Report the resulting state, not the request's busy flag.
        drop(_busy);
        status(app)
    })();
    Ok(Some(match result {
        Ok(v) => (200, v),
        Err(e) => {
            log(app, &e.to_string());
            (400, json!({"ok":false,"error":e.to_string()}))
        }
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn native_synthesis_checks_the_process_and_returns_wav_without_network() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::create_dir(root.join("voices")).unwrap();
        for ext in ["onnx", "onnx.json"] {
            std::fs::write(
                root.join("voices").join(format!("{}.{ext}", VOICES[0])),
                "fixture",
            )
            .unwrap();
        }
        let mut wav = vec![0u8; 46];
        wav[..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        std::fs::write(root.join("fixture.wav"), wav).unwrap();
        let script = r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
 if [ "$1" = "--output_file" ]; then shift; destination="$1"; fi
 shift
done
read text
cp "$(dirname "$0")/../fixture.wav" "$destination"
"#;
        let path = root.join("bin/piper");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = json!({"voice":VOICES[0],"length_scale":0.8});
        let result =
            integrations::blocking(synthesize(root.to_owned(), cfg, "Prueba".into())).unwrap();
        assert_eq!(
            STANDARD
                .decode(result["audio"].as_str().unwrap())
                .unwrap()
                .len(),
            46
        );
        assert!(!std::fs::read_dir(root).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".speak-")
        }));
    }
    #[test]
    fn voice_files_and_inference_settings_are_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("voices")).unwrap();
        let cfg = json!({"voice":VOICES[0],"length_scale":1.0});
        assert!(config_patch(cfg.clone(), &json!({"voice":"../outside"}), root).is_err());
        assert!(config_patch(cfg.clone(), &json!({"voice":VOICES[0]}), root).is_err());
        std::fs::write(root.join("voices").join(format!("{}.onnx", VOICES[0])), [1]).unwrap();
        assert!(!installed(root, VOICES[0]));
        std::fs::write(
            root.join("voices").join(format!("{}.onnx.json", VOICES[0])),
            "{}",
        )
        .unwrap();
        assert!(installed(root, VOICES[0]));
        assert!(config_patch(cfg.clone(), &json!({"length_scale":-1}), root).is_err());
        assert_eq!(
            config_patch(cfg, &json!({"voice":VOICES[0],"length_scale":0.8}), root).unwrap()["length_scale"],
            0.8
        );
    }
}
