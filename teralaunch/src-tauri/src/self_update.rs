// Launcher self-update module.
//
// Flow:
//   1. check_launcher_update() fetches a small JSON manifest from the server
//      and compares the remote `version` to the current build's CARGO_PKG_VERSION.
//   2. If the remote is newer, the frontend can show a prompt and call
//      apply_launcher_update() to download and install it.
//   3. apply_launcher_update() downloads the new .exe to %TEMP%, writes a
//      helper .bat that waits for this process to exit, replaces the running
//      exe, and relaunches it. The current process then exits.
//
// Why a .bat helper: Windows cannot replace a running executable; the
// standard trick is to spawn a detached helper that waits for the old PID
// to die and then performs the file copy + relaunch.

use std::path::PathBuf;
use std::process::Command;
use std::io::Write as IoWrite;

use serde::{Deserialize, Serialize};

const MANIFEST_PATH: &str = "/tera/launcher/launcher-version.json";

/// Manifest published by the server describing the latest launcher build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherVersionManifest {
    /// SemVer string, e.g. "0.0.7".
    pub version: String,
    /// Absolute URL to the new .exe file.
    pub url: String,
    /// Optional release notes shown to the user.
    #[serde(default)]
    pub notes: String,
    /// Optional SHA-256 (hex) of the downloaded exe for integrity check.
    #[serde(default)]
    pub sha256: String,
    /// Optional flag forcing the user to update before continuing.
    #[serde(default)]
    pub mandatory: bool,
}

/// Result returned to the frontend after checking for an update.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheckResult {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub notes: String,
    pub mandatory: bool,
    pub url: String,
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Very tolerant SemVer-ish comparison: splits on '.', parses each segment as
/// u64, falls back to lexicographic for non-numeric tails.
fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let r = parse(remote);
    let l = parse(local);
    let n = r.len().max(l.len());
    for i in 0..n {
        let rv = r.get(i).copied().unwrap_or(0);
        let lv = l.get(i).copied().unwrap_or(0);
        if rv != lv {
            return rv > lv;
        }
    }
    false
}

/// Build the manifest URL by reusing the existing FILE_SERVER_URL host:port.
/// `FILE_SERVER_URL` is something like `http://host:port/public`; we strip the
/// trailing path and append `MANIFEST_PATH`.
fn manifest_url() -> String {
    let file_server = teralib::config::get_config_value("FILE_SERVER_URL");
    // Strip a trailing `/public` (or any trailing path segment) to get the host root.
    let base = if let Some(idx) = file_server.find("://") {
        let after_scheme = &file_server[idx + 3..];
        if let Some(slash) = after_scheme.find('/') {
            &file_server[..idx + 3 + slash]
        } else {
            &file_server[..]
        }
    } else {
        &file_server[..]
    };
    format!("{}{}", base.trim_end_matches('/'), MANIFEST_PATH)
}

/// Fetch the manifest and compare with the embedded version. Network errors
/// are returned as Err so the frontend can decide whether to surface them
/// (typically: silent fail, the user can still play offline).
pub async fn check_for_update() -> Result<UpdateCheckResult, String> {
    let url = manifest_url();
    log::info!("[self_update] checking manifest at {}", url);

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("http client: {}", e))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("manifest HTTP {}", resp.status()));
    }

    let manifest: LauncherVersionManifest = resp
        .json()
        .await
        .map_err(|e| format!("invalid manifest JSON: {}", e))?;

    let local = current_version().to_string();
    let update_available = is_newer(&manifest.version, &local);

    Ok(UpdateCheckResult {
        update_available,
        current_version: local,
        latest_version: manifest.version,
        notes: manifest.notes,
        mandatory: manifest.mandatory,
        url: manifest.url,
    })
}

/// Download the new exe and spawn the swap-and-relaunch helper.
/// On success, the caller should exit the application; the helper will
/// replace the on-disk exe and relaunch.
pub async fn download_and_apply(url: String, expected_sha256: Option<String>) -> Result<(), String> {
    if url.is_empty() {
        return Err("empty download URL".into());
    }

    // 1. Resolve current exe path.
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?;

    // 2. Download to a temp file in the same dir as the running exe so the
    //    later move/copy is on the same volume (no cross-drive issues).
    let tmp_dir = std::env::temp_dir();
    let new_exe_path: PathBuf = tmp_dir.join(format!(
        "teralaunch_new_{}.exe",
        std::process::id()
    ));

    log::info!("[self_update] downloading {} -> {:?}", url, new_exe_path);

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("http client: {}", e))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download request: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("download HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("download body: {}", e))?;

    // 3. Optional integrity check.
    if let Some(expected) = expected_sha256.filter(|s| !s.is_empty()) {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        let mut got = String::with_capacity(digest.len() * 2);
        for b in digest.iter() {
            use std::fmt::Write as _;
            let _ = write!(got, "{:02x}", b);
        }
        if !got.eq_ignore_ascii_case(&expected) {
            return Err(format!(
                "sha256 mismatch (expected {}, got {})",
                expected, got
            ));
        }
    }

    // 4. Write to temp.
    {
        let mut f = std::fs::File::create(&new_exe_path)
            .map_err(|e| format!("create temp exe: {}", e))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write temp exe: {}", e))?;
        f.flush().ok();
    }

    // 5. Write helper .bat to %TEMP%.
    let bat_path = tmp_dir.join(format!("teralaunch_update_{}.bat", std::process::id()));
    let pid = std::process::id();
    let bat = format!(
        "@echo off\r\n\
         setlocal\r\n\
         set TARGET=\"{target}\"\r\n\
         set SOURCE=\"{source}\"\r\n\
         echo Waiting for launcher (PID {pid}) to exit...\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>NUL | find /I \"{pid}\" >NUL\r\n\
         if not errorlevel 1 (\r\n\
             timeout /t 1 /nobreak >NUL\r\n\
             goto wait\r\n\
         )\r\n\
         echo Replacing launcher binary...\r\n\
         move /Y %SOURCE% %TARGET% >NUL\r\n\
         if errorlevel 1 (\r\n\
             echo Update failed: could not replace %TARGET%.\r\n\
             pause\r\n\
             exit /b 1\r\n\
         )\r\n\
         echo Restarting launcher...\r\n\
         start \"\" %TARGET%\r\n\
         del \"%~f0\" 2>NUL\r\n\
         endlocal\r\n",
        target = current_exe.display(),
        source = new_exe_path.display(),
        pid = pid,
    );
    std::fs::write(&bat_path, bat).map_err(|e| format!("write helper bat: {}", e))?;

    // 6. Spawn helper detached.
    log::info!("[self_update] spawning helper {:?}", bat_path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP (0x0200) + DETACHED_PROCESS (0x0008)
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0200;
        Command::new("cmd")
            .args(["/C", "start", "", "/min", bat_path.to_str().unwrap_or_default()])
            .creation_flags(DETACHED)
            .spawn()
            .map_err(|e| format!("spawn helper: {}", e))?;
    }
    #[cfg(not(windows))]
    {
        Command::new("sh")
            .arg(&bat_path)
            .spawn()
            .map_err(|e| format!("spawn helper: {}", e))?;
    }

    Ok(())
}

// Tauri command wrappers ------------------------------------------------------

#[tauri::command]
pub async fn check_launcher_update() -> Result<UpdateCheckResult, String> {
    check_for_update().await
}

#[tauri::command]
pub async fn apply_launcher_update(
    url: String,
    sha256: Option<String>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    download_and_apply(url, sha256).await?;
    // Give the helper a moment to spin up, then exit so it can replace the exe.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    app_handle.exit(0);
    Ok(())
}
