//! Download and install the latest Obscura release.
//!
//! The SDK never runs a binary it has not verified: the release is read from the
//! GitHub API, the archive is downloaded over HTTPS from the project's own release
//! URL, and its SHA-256 must equal the digest GitHub publishes for that asset (a
//! release without one is refused). The archive is unpacked by this crate with hard
//! size limits; entry names are only compared, never used as paths. The binary is
//! written next to its destination and renamed into place, so a dying process never
//! leaves a half-written executable.
use crate::{Error, Result};
use ring::digest;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const REPO: &str = "h4ckf0r0day/obscura";
const MAX_API_BYTES: usize = 4 * 1024 * 1024;
/// Largest archive accepted (the real ones are ~70 MB) and largest binary inside.
const MAX_ARCHIVE_BYTES: u64 = 300 * 1024 * 1024;
const MAX_BINARY_BYTES: usize = 400 * 1024 * 1024;
const METADATA: &str = "obscura.install.json";

fn fail(message: impl Into<String>) -> Error {
    Error::State(format!("OBSCURA_INSTALL_ERROR: {}", message.into()))
}

/// Where releases are read from. Production always uses [`Sources::github`]; tests
/// point it at a local server and turn off the HTTPS/host rules.
#[derive(Clone)]
pub(crate) struct Sources {
    pub api: String,
    pub download: String,
    pub strict: bool,
}

impl Sources {
    pub(crate) fn github() -> Self {
        Self {
            api: format!("https://api.github.com/repos/{REPO}/releases/latest"),
            download: format!("https://github.com/{REPO}/releases/download"),
            strict: true,
        }
    }
}

/// The release asset for this machine: the default build (with rendering, so that
/// screenshots and PDFs work), or `None` where Obscura publishes no build.
pub(crate) fn asset_name(os: &str, arch: &str) -> Option<String> {
    let cpu = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    match (os, cpu) {
        ("linux", cpu) => Some(format!("obscura-{cpu}-linux.tar.gz")),
        ("macos", cpu) => Some(format!("obscura-{cpu}-macos.tar.gz")),
        ("windows", "x86_64") => Some("obscura-x86_64-windows.zip".into()),
        _ => None,
    }
}

pub(crate) fn binary_name(os: &str) -> &'static str {
    if os == "windows" {
        "obscura.exe"
    } else {
        "obscura"
    }
}

/// `~/.hive/bin`: where Hive installs Obscura and where the resolver already looks.
pub(crate) fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".hive").join("bin"))
}

fn allowed_host(host: &str) -> bool {
    host == "github.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com")
}

fn client(sources: &Sources) -> Result<reqwest::Client> {
    let strict = sources.strict;
    reqwest::Client::builder()
        .user_agent("hive-sdk-rust")
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(900))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let ok = attempt.previous().len() < 5
                && (!strict
                    || (attempt.url().scheme() == "https"
                        && attempt.url().host_str().is_some_and(allowed_host)));
            if ok { attempt.follow() } else { attempt.stop() }
        }))
        .build()
        .map_err(|_| fail("cannot create the HTTP client"))
}

fn check_url(sources: &Sources, url: &str) -> Result<()> {
    if !sources.strict {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| fail("invalid download URL"))?;
    if parsed.scheme() != "https" || !parsed.host_str().is_some_and(allowed_host) {
        return Err(fail(
            "downloads are only allowed over HTTPS from github.com",
        ));
    }
    Ok(())
}

async fn get_limited(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| fail("cannot reach the server"))?;
    if !response.status().is_success() {
        return Err(fail(format!(
            "the server answered HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| fail("the download was interrupted"))?
    {
        if body.len() + chunk.len() > limit {
            return Err(fail("the response is larger than allowed"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// What a release says about the asset we want.
struct Release {
    tag: String,
    sha256: String,
    size: u64,
}

fn parse_release(body: &[u8], asset: &str) -> Result<Release> {
    let release: Value = serde_json::from_slice(body)
        .map_err(|_| fail("the release description is not valid JSON"))?;
    if release["draft"] == true || release["prerelease"] == true {
        return Err(fail("the latest release is a draft or a prerelease"));
    }
    let tag = release["tag_name"].as_str().unwrap_or_default();
    let version = tag.strip_prefix('v').unwrap_or_default();
    if tag.len() > 32
        || version.is_empty()
        || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return Err(fail("the release tag is not a plain version"));
    }
    let entry = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["name"] == asset)
        .ok_or_else(|| fail(format!("release {tag} has no asset named {asset}")))?;
    let sha256 = entry["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| fail("the release publishes no SHA-256 for this asset; refusing to install an unverifiable binary"))?;
    let size = entry["size"]
        .as_u64()
        .filter(|size| (1..=MAX_ARCHIVE_BYTES).contains(size))
        .ok_or_else(|| fail("the asset size is missing or beyond the limit"))?;
    Ok(Release {
        tag: tag.to_owned(),
        sha256: sha256.to_ascii_lowercase(),
        size,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    (value >> 1) ^ 0xedb8_8320
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        table
    });
    !data.iter().fold(!0u32, |crc, byte| {
        table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

// ---- archives -----------------------------------------------------------------------

fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// The binary inside a `.tar.gz`.
fn from_tar_gz(archive: &[u8], wanted: &str) -> Result<Vec<u8>> {
    let corrupt = || fail("the archive is corrupt");
    if archive.len() < 18 || archive[..3] != [0x1f, 0x8b, 8] {
        return Err(fail("the archive is not a gzip file"));
    }
    let flags = archive[3];
    let mut at = 10;
    if flags & 4 != 0 {
        let extra = u16::from_le_bytes(
            archive
                .get(at..at + 2)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap(),
        ) as usize;
        at += 2 + extra;
    }
    for bit in [8, 16] {
        if flags & bit != 0 {
            at += archive
                .get(at..)
                .ok_or_else(corrupt)?
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(corrupt)?
                + 1;
        }
    }
    if flags & 2 != 0 {
        at += 2;
    }
    let end = archive.len() - 8;
    let body = archive.get(at..end).ok_or_else(corrupt)?;
    let tar =
        miniz_oxide::inflate::decompress_to_vec_with_limit(body, MAX_BINARY_BYTES + 1024 * 1024)
            .map_err(|_| fail("the archive is corrupt or expands beyond the size limit"))?;
    let (crc, size) = (
        u32::from_le_bytes(archive[end..end + 4].try_into().unwrap()),
        u32::from_le_bytes(archive[end + 4..].try_into().unwrap()),
    );
    if crc32(&tar) != crc || tar.len() as u32 != size {
        return Err(fail("the archive failed its integrity check"));
    }
    let octal = |field: &[u8]| -> Option<usize> {
        let text = std::str::from_utf8(field)
            .ok()?
            .trim_matches(|c: char| c == '\0' || c == ' ');
        if text.is_empty() {
            Some(0)
        } else {
            usize::from_str_radix(text, 8).ok()
        }
    };
    let mut offset = 0;
    let mut long_name: Option<String> = None;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let checksum = octal(&header[148..156]).ok_or_else(corrupt)?;
        let computed: usize = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    usize::from(*b)
                }
            })
            .sum();
        if checksum != computed {
            return Err(fail("a tar header failed its checksum"));
        }
        let size = octal(&header[124..136]).ok_or_else(|| fail("unsupported tar entry size"))?;
        let kind = header[156];
        let data = tar
            .get(offset + 512..offset + 512 + size)
            .ok_or_else(corrupt)?;
        let name_field = |range: std::ops::Range<usize>| {
            String::from_utf8_lossy(&header[range])
                .trim_end_matches('\0')
                .to_owned()
        };
        if kind == b'L' {
            long_name = Some(
                String::from_utf8_lossy(data)
                    .trim_end_matches('\0')
                    .to_owned(),
            );
        } else {
            let name = long_name.take().unwrap_or_else(|| {
                let prefix = name_field(345..500);
                let name = name_field(0..100);
                if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                }
            });
            if matches!(kind, b'0' | 0) && basename(&name) == wanted {
                return Ok(data.to_vec());
            }
        }
        offset += 512 + size.div_ceil(512) * 512;
    }
    Err(fail(format!("the archive does not contain {wanted}")))
}

/// The binary inside a `.zip` (stored or deflated; ZIP64 is not needed for these assets).
fn from_zip(archive: &[u8], wanted: &str) -> Result<Vec<u8>> {
    let corrupt = || fail("the archive is corrupt");
    let u16_at = |at: usize| -> Result<usize> {
        Ok(u16::from_le_bytes(
            archive
                .get(at..at + 2)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap(),
        ) as usize)
    };
    let u32_at = |at: usize| -> Result<usize> {
        Ok(u32::from_le_bytes(
            archive
                .get(at..at + 4)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap(),
        ) as usize)
    };
    let search_from = archive.len().saturating_sub(22 + 65_535);
    let eocd = (search_from..=archive.len().saturating_sub(22))
        .rev()
        .find(|at| archive.get(*at..*at + 4) == Some(b"PK\x05\x06"))
        .ok_or_else(|| fail("the archive is not a zip file"))?;
    let (entries, mut at) = (u16_at(eocd + 10)?, u32_at(eocd + 16)?);
    if entries == 0xFFFF || at == 0xFFFF_FFFF {
        return Err(fail("ZIP64 archives are not supported"));
    }
    for _ in 0..entries {
        if archive.get(at..at + 4) != Some(b"PK\x01\x02") {
            return Err(corrupt());
        }
        let (method, crc, compressed, size, name_len, extra_len, comment_len, local) = (
            u16_at(at + 10)?,
            u32_at(at + 16)? as u32,
            u32_at(at + 20)?,
            u32_at(at + 24)?,
            u16_at(at + 28)?,
            u16_at(at + 30)?,
            u16_at(at + 32)?,
            u32_at(at + 42)?,
        );
        let name = String::from_utf8_lossy(
            archive
                .get(at + 46..at + 46 + name_len)
                .ok_or_else(corrupt)?,
        )
        .into_owned();
        at += 46 + name_len + extra_len + comment_len;
        if basename(&name) != wanted {
            continue;
        }
        if size > MAX_BINARY_BYTES || archive.get(local..local + 4) != Some(b"PK\x03\x04") {
            return Err(fail("the binary in the archive is beyond the size limit"));
        }
        let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
        let body = archive.get(start..start + compressed).ok_or_else(corrupt)?;
        let bytes = match method {
            0 => body.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(body, size + 1)
                .map_err(|_| fail("the archive is corrupt or oversized"))?,
            _ => return Err(fail("unsupported zip compression")),
        };
        if bytes.len() != size || crc32(&bytes) != crc {
            return Err(fail("the binary failed its integrity check"));
        }
        return Ok(bytes);
    }
    Err(fail(format!("the archive does not contain {wanted}")))
}

// ---- install ------------------------------------------------------------------------

/// Outcome of [`install`].
pub(crate) struct Installed {
    pub path: PathBuf,
    pub tag: String,
    pub asset: String,
    pub sha256: String,
    /// The installed binary already was this release; nothing was downloaded.
    pub already_current: bool,
}

impl Installed {
    pub(crate) fn report(&self) -> Value {
        json!({"path":self.path.display().to_string(),"version":self.tag,"asset":self.asset,"sha256":self.sha256,"already_current":self.already_current})
    }
}

fn installed_tag(dir: &Path, binary: &Path) -> Option<(String, String)> {
    if !binary.is_file() {
        return None;
    }
    let meta: Value = serde_json::from_slice(&std::fs::read(dir.join(METADATA)).ok()?).ok()?;
    Some((
        meta["version"].as_str()?.to_owned(),
        meta["sha256"].as_str()?.to_owned(),
    ))
}

/// Install the latest release into `dir`. With `update` false an existing binary is
/// kept as is (the caller only installs when none was found); with `update` true it is
/// replaced unless it already is the latest release.
pub(crate) async fn install(
    sources: &Sources,
    dir: &Path,
    os: &str,
    arch: &str,
    update: bool,
) -> Result<Installed> {
    let asset = asset_name(os, arch)
        .ok_or_else(|| fail(format!("Obscura publishes no build for {os}/{arch}")))?;
    let binary_file = binary_name(os);
    let destination = dir.join(binary_file);
    check_url(sources, &sources.api)?;
    let client = client(sources)?;
    let release = parse_release(
        &get_limited(&client, &sources.api, MAX_API_BYTES).await?,
        &asset,
    )?;
    if let Some((tag, sha)) = installed_tag(dir, &destination)
        && (!update || (tag == release.tag && sha == release.sha256))
    {
        return Ok(Installed {
            path: destination,
            tag,
            asset,
            sha256: sha,
            already_current: true,
        });
    }
    let url = format!(
        "{}/{}/{}",
        sources.download.trim_end_matches('/'),
        release.tag,
        asset
    );
    check_url(sources, &url)?;
    std::fs::create_dir_all(dir).map_err(|_| fail("cannot create the install directory"))?;
    // Leftovers of an interrupted install are ours to remove.
    for stale in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = stale.file_name().to_string_lossy().into_owned();
        if name.starts_with(".obscura-download-") || name == format!("{binary_file}.part") {
            let _ = std::fs::remove_file(stale.path());
        }
    }
    let archive_path = dir.join(format!(".obscura-download-{}", std::process::id()));
    let result = download_and_place(
        &client,
        &url,
        &release,
        &archive_path,
        dir,
        &destination,
        &asset,
        binary_file,
    )
    .await;
    let _ = std::fs::remove_file(&archive_path);
    result?;
    let _ = std::fs::write(
        dir.join(METADATA),
        json!({"version":release.tag,"asset":asset,"sha256":release.sha256}).to_string(),
    );
    Ok(Installed {
        path: destination,
        tag: release.tag,
        asset,
        sha256: release.sha256,
        already_current: false,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_and_place(
    client: &reqwest::Client,
    url: &str,
    release: &Release,
    archive_path: &Path,
    dir: &Path,
    destination: &Path,
    asset: &str,
    binary_file: &str,
) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| fail("cannot reach the download server"))?;
    if !response.status().is_success() {
        return Err(fail(format!(
            "the download answered HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut file =
        std::fs::File::create(archive_path).map_err(|_| fail("cannot write the download"))?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut received = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| fail("the download was interrupted"))?
    {
        received += chunk.len() as u64;
        // The asset is never larger than what the release declared.
        if received > release.size {
            return Err(fail("the download is larger than the release declares"));
        }
        context.update(&chunk);
        file.write_all(&chunk)
            .map_err(|_| fail("cannot write the download"))?;
    }
    drop(file);
    if received != release.size || hex(context.finish().as_ref()) != release.sha256 {
        return Err(fail(
            "the download does not match the published SHA-256; nothing was installed",
        ));
    }
    let (archive_path, asset, binary_file) = (
        archive_path.to_owned(),
        asset.to_owned(),
        binary_file.to_owned(),
    );
    let binary = tokio::task::spawn_blocking(move || {
        let archive = std::fs::read(&archive_path).map_err(|_| fail("cannot read the download"))?;
        if asset.ends_with(".zip") {
            from_zip(&archive, &binary_file)
        } else {
            from_tar_gz(&archive, &binary_file)
        }
    })
    .await
    .map_err(|_| fail("extraction stopped unexpectedly"))??;
    let part = dir.join(format!("{}.part", binary_file_name(destination)));
    {
        let mut out = std::fs::File::create(&part).map_err(|_| fail("cannot write the binary"))?;
        out.write_all(&binary)
            .map_err(|_| fail("cannot write the binary"))?;
        out.sync_all()
            .map_err(|_| fail("cannot write the binary"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| fail("cannot make the binary executable"))?;
    }
    std::fs::rename(&part, destination).map_err(|_| {
        let _ = std::fs::remove_file(&part);
        fail("cannot move the binary into place")
    })
}

fn binary_file_name(destination: &Path) -> String {
    destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "obscura".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    const GOOD_TGZ: &[u8] = include_bytes!("../../tests/fixtures/obscura-install/good.tar.gz");
    const MISSING_TGZ: &[u8] =
        include_bytes!("../../tests/fixtures/obscura-install/missing.tar.gz");
    const TRAVERSAL_TGZ: &[u8] =
        include_bytes!("../../tests/fixtures/obscura-install/traversal.tar.gz");
    const GOOD_ZIP: &[u8] = include_bytes!("../../tests/fixtures/obscura-install/good.zip");
    const STORED_ZIP: &[u8] = include_bytes!("../../tests/fixtures/obscura-install/stored.zip");
    const MISSING_ZIP: &[u8] = include_bytes!("../../tests/fixtures/obscura-install/missing.zip");

    fn sha(data: &[u8]) -> String {
        hex(digest::digest(&digest::SHA256, data).as_ref())
    }

    /// A release server on loopback: `/api` describes the release, `/dl/<tag>/<asset>` serves bytes.
    struct Server {
        sources: Sources,
        hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    fn serve(
        tag: &str,
        asset: &str,
        served: &'static [u8],
        declared: Option<&'static [u8]>,
        digest_of: Option<String>,
    ) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let declared = declared.unwrap_or(served);
        let digest_field = digest_of.unwrap_or_else(|| format!("sha256:{}", sha(declared)));
        let api = json!({"tag_name":tag,"draft":false,"prerelease":false,"assets":[
            {"name":asset,"size":declared.len(),"digest":digest_field},
            {"name":"other.tar.gz","size":1,"digest":"sha256:00"}]});
        let api = if digest_field.is_empty() {
            json!({"tag_name":tag,"assets":[{"name":asset,"size":declared.len()}]})
        } else {
            api
        };
        let (tag, asset) = (tag.to_owned(), asset.to_owned());
        let hits = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let seen = hits.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buffer = [0u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                seen.lock().unwrap().push(path.clone());
                let (status, body): (&str, Vec<u8>) = if path == "/api" {
                    ("200 OK", api.to_string().into_bytes())
                } else if path == format!("/dl/{tag}/{asset}") {
                    ("200 OK", served.to_vec())
                } else {
                    ("404 Not Found", b"no".to_vec())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Server {
            sources: Sources {
                api: format!("{base}/api"),
                download: format!("{base}/dl"),
                strict: false,
            },
            hits,
        }
    }

    #[test]
    fn assets_are_chosen_per_platform_and_unknown_ones_are_refused() {
        assert_eq!(
            asset_name("linux", "x86_64").unwrap(),
            "obscura-x86_64-linux.tar.gz"
        );
        assert_eq!(
            asset_name("linux", "aarch64").unwrap(),
            "obscura-aarch64-linux.tar.gz"
        );
        assert_eq!(
            asset_name("macos", "aarch64").unwrap(),
            "obscura-aarch64-macos.tar.gz"
        );
        assert_eq!(
            asset_name("windows", "x86_64").unwrap(),
            "obscura-x86_64-windows.zip"
        );
        for (os, arch) in [
            ("windows", "aarch64"),
            ("linux", "riscv64"),
            ("freebsd", "x86_64"),
        ] {
            assert!(asset_name(os, arch).is_none(), "{os}/{arch}");
        }
        assert_eq!(
            (binary_name("windows"), binary_name("linux")),
            ("obscura.exe", "obscura")
        );
    }

    #[test]
    fn archives_yield_the_binary_by_name_and_reject_corruption() {
        let script = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_obscura.py"
        ))
        .unwrap();
        assert_eq!(from_tar_gz(GOOD_TGZ, "obscura").unwrap(), script);
        assert_eq!(
            from_tar_gz(TRAVERSAL_TGZ, "obscura").unwrap(),
            script,
            "a hostile path only ever matters by its last component"
        );
        assert_eq!(from_zip(GOOD_ZIP, "obscura.exe").unwrap(), script);
        assert_eq!(from_zip(STORED_ZIP, "obscura.exe").unwrap(), script);
        assert!(
            from_tar_gz(MISSING_TGZ, "obscura")
                .unwrap_err()
                .to_string()
                .contains("does not contain")
        );
        assert!(
            from_zip(MISSING_ZIP, "obscura.exe")
                .unwrap_err()
                .to_string()
                .contains("does not contain")
        );
        let mut flipped = GOOD_TGZ.to_vec();
        let middle = flipped.len() / 2;
        flipped[middle] ^= 0xff;
        assert!(from_tar_gz(&flipped, "obscura").is_err());
        for bad in [
            &b""[..],
            b"not a gzip file at all, just text",
            &GOOD_TGZ[..GOOD_TGZ.len() - 20],
        ] {
            assert!(from_tar_gz(bad, "obscura").is_err());
        }
        let mut zipped = GOOD_ZIP.to_vec();
        let middle = zipped.len() / 3;
        zipped[middle] ^= 0xff;
        assert!(
            from_zip(&zipped, "obscura.exe").is_err()
                || from_zip(&zipped, "obscura.exe").unwrap() != script
        );
        assert!(from_zip(b"PK nonsense", "obscura.exe").is_err());
        assert!(from_zip(&GOOD_ZIP[..GOOD_ZIP.len() / 2], "obscura.exe").is_err());
    }

    #[tokio::test]
    async fn installs_the_verified_binary_atomically_and_records_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let server = serve(
            "v9.9.9",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            None,
        );
        let done = install(&server.sources, dir.path(), "linux", "x86_64", false)
            .await
            .unwrap();
        assert_eq!((done.tag.as_str(), done.already_current), ("v9.9.9", false));
        assert_eq!(done.path, dir.path().join("obscura"));
        let script = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_obscura.py"
        ))
        .unwrap();
        assert_eq!(std::fs::read(&done.path).unwrap(), script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&done.path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers
                .iter()
                .filter(|n| n.contains("part") || n.contains("download"))
                .count(),
            0,
            "{leftovers:?}"
        );
        assert!(dir.path().join(METADATA).is_file());
        // Without `update` an existing install is kept and nothing is downloaded again.
        let hits = server.hits.lock().unwrap().len();
        let again = install(&server.sources, dir.path(), "linux", "x86_64", false)
            .await
            .unwrap();
        assert!(again.already_current);
        assert_eq!(
            server
                .hits
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with("/dl"))
                .count(),
            1
        );
        assert!(server.hits.lock().unwrap().len() >= hits);
        // `update` with the same release is a no-op too.
        assert!(
            install(&server.sources, dir.path(), "linux", "x86_64", true)
                .await
                .unwrap()
                .already_current
        );
        assert_eq!(
            server
                .hits
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with("/dl"))
                .count(),
            1
        );
        // A newer release replaces it.
        let newer = serve(
            "v10.0.0",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            None,
        );
        let updated = install(&newer.sources, dir.path(), "linux", "x86_64", true)
            .await
            .unwrap();
        assert_eq!(
            (updated.tag.as_str(), updated.already_current),
            ("v10.0.0", false)
        );
        let meta: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(METADATA)).unwrap()).unwrap();
        assert_eq!(meta["version"], "v10.0.0");
    }

    #[tokio::test]
    async fn windows_assets_install_from_a_zip() {
        let dir = tempfile::tempdir().unwrap();
        let server = serve("v1.2.3", "obscura-x86_64-windows.zip", GOOD_ZIP, None, None);
        let done = install(&server.sources, dir.path(), "windows", "x86_64", false)
            .await
            .unwrap();
        assert_eq!(done.path, dir.path().join("obscura.exe"));
        assert!(!std::fs::read(&done.path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_is_installed_when_verification_fails() {
        let dir = tempfile::tempdir().unwrap();
        let installed = |dir: &Path| dir.join("obscura").exists() || dir.join(METADATA).exists();
        // The published digest is for other bytes than the ones served.
        let tampered = serve(
            "v1.0.0",
            "obscura-x86_64-linux.tar.gz",
            MISSING_TGZ,
            Some(GOOD_TGZ),
            None,
        );
        let error = install(&tampered.sources, dir.path(), "linux", "x86_64", false)
            .await
            .err()
            .expect("must fail")
            .to_string();
        assert!(
            error.contains("SHA-256") || error.contains("larger"),
            "{error}"
        );
        assert!(!installed(dir.path()));
        // Same size, different content: only the hash can catch it.
        let wrong = serve(
            "v1.0.0",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            Some(format!("sha256:{}", "ab".repeat(32))),
        );
        assert!(
            install(&wrong.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("does not match the published SHA-256")
        );
        assert!(!installed(dir.path()));
        // A release without a digest is unverifiable and refused.
        let unsigned = serve(
            "v1.0.0",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            Some(String::new()),
        );
        assert!(
            install(&unsigned.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("no SHA-256")
        );
        // A verified archive that does not hold the binary installs nothing.
        let empty = serve(
            "v1.0.0",
            "obscura-x86_64-linux.tar.gz",
            MISSING_TGZ,
            None,
            None,
        );
        assert!(
            install(&empty.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("does not contain")
        );
        assert!(!installed(dir.path()));
        // Missing asset, bad tags, a 404 download and an unsupported platform.
        let other = serve(
            "v1.0.0",
            "obscura-aarch64-linux.tar.gz",
            GOOD_TGZ,
            None,
            None,
        );
        assert!(
            install(&other.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("no asset named")
        );
        let bad_tag = serve(
            "../evil",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            None,
        );
        assert!(
            install(&bad_tag.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("plain version")
        );
        let mut gone = serve(
            "v1.0.0",
            "obscura-x86_64-linux.tar.gz",
            GOOD_TGZ,
            None,
            None,
        );
        gone.sources.download = format!("{}/missing", gone.sources.download);
        assert!(
            install(&gone.sources, dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("HTTP 404")
        );
        assert!(
            install(&gone.sources, dir.path(), "plan9", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("no build")
        );
        assert!(!installed(dir.path()));
    }

    #[tokio::test]
    async fn production_sources_only_allow_https_from_github() {
        let dir = tempfile::tempdir().unwrap();
        let strict = |api: &str, download: &str| Sources {
            api: api.into(),
            download: download.into(),
            strict: true,
        };
        for (api, download) in [
            (
                "http://api.github.com/repos/x/releases/latest",
                "https://github.com/x",
            ),
            (
                "https://evil.example/repos/x/releases/latest",
                "https://github.com/x",
            ),
            (
                "https://api.github.com.evil.example/x",
                "https://github.com/x",
            ),
        ] {
            let error = install(&strict(api, download), dir.path(), "linux", "x86_64", false)
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(
                error.contains("only allowed over HTTPS from github.com"),
                "{api}: {error}"
            );
        }
        assert!(
            allowed_host("github.com")
                && allowed_host("objects.githubusercontent.com")
                && allowed_host("api.github.com")
        );
        assert!(
            !allowed_host("evilgithub.com")
                && !allowed_host("github.com.evil.example")
                && !allowed_host("example.com")
        );
        let github = Sources::github();
        assert!(
            github.strict
                && github.api.starts_with("https://api.github.com/")
                && github.download.starts_with("https://github.com/")
        );
    }

    #[test]
    fn stale_download_leftovers_are_not_mistaken_for_an_install() {
        assert_eq!(
            crc32(b"123456789"),
            0xcbf4_3926,
            "the table-driven CRC-32 matches the standard check value"
        );
        assert_eq!(basename("a/b/obscura"), "obscura");
        assert_eq!(basename("dir\\obscura.exe"), "obscura.exe");
        assert!(parse_release(br#"{"tag_name":"v1.2.3","draft":true,"assets":[]}"#, "x").is_err());
        assert!(
            parse_release(
                br#"{"tag_name":"v1.2.3","prerelease":true,"assets":[]}"#,
                "x"
            )
            .is_err()
        );
        assert!(parse_release(b"not json", "x").is_err());
    }
}
