// =====================================================================
// TERA Language Switcher
// Swaps S1Game/S1Data/DataCenter_Final_EUR.dat between the original
// English file and a bundled Spanish translation.
//
// Layout:
//   <game>/S1Game/S1Data/DataCenter_Final_EUR.dat           <- the file the game reads
//   <game>/S1Game/S1Data/DataCenter_Final_EUR.dat.english_backup
//                                                             <- created on first switch
//   <launcher>/resources/lang/DataCenter_Final_EUR.dat       <- bundled Spanish file
//
// State detection: a sentinel file `.tera_lang_spanish` is dropped next to
// the dat so we don't need hashing.
// =====================================================================

use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

const DAT_NAME: &str = "DataCenter_Final_EUR.dat";
const BACKUP_SUFFIX: &str = ".english_backup";
const SENTINEL: &str = ".tera_lang_spanish";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageStatus {
    /// Currently installed language: "english" | "spanish" | "unknown"
    pub current: String,
    /// Spanish .dat is bundled and ready to install
    pub spanish_available: bool,
    /// English backup exists (only after first switch to Spanish)
    pub english_backup_exists: bool,
    /// Absolute path to the live DataCenter file
    pub dat_path: String,
    /// Size of the live .dat in bytes (0 if missing)
    pub dat_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn dat_path(game_path: &Path) -> PathBuf {
    game_path.join("S1Game").join("S1Data").join(DAT_NAME)
}

fn backup_path(game_path: &Path) -> PathBuf {
    game_path
        .join("S1Game")
        .join("S1Data")
        .join(format!("{}{}", DAT_NAME, BACKUP_SUFFIX))
}

fn sentinel_path(game_path: &Path) -> PathBuf {
    game_path.join("S1Game").join("S1Data").join(SENTINEL)
}

pub fn get_status(game_path: &Path, spanish_src: Option<&Path>) -> LanguageStatus {
    let dat = dat_path(game_path);
    let backup = backup_path(game_path);
    let sentinel = sentinel_path(game_path);

    let size = fs::metadata(&dat).map(|m| m.len()).unwrap_or(0);
    let backup_exists = backup.exists();
    let spanish_available = spanish_src.map(|p| p.exists()).unwrap_or(false);

    let current = if !dat.exists() {
        "unknown".to_string()
    } else if sentinel.exists() {
        "spanish".to_string()
    } else if backup_exists {
        // Backup exists but no sentinel -> live file is English (restored)
        "english".to_string()
    } else {
        // No backup, no sentinel -> assume original English
        "english".to_string()
    };

    LanguageStatus {
        current,
        spanish_available,
        english_backup_exists: backup_exists,
        dat_path: dat.to_string_lossy().to_string(),
        dat_size: size,
    }
}

/// Switch the game to Spanish.
/// - On first call: backs up the current English .dat -> .english_backup
/// - Copies bundled spanish_src over the live .dat
/// - Drops sentinel file
pub fn switch_to_spanish(game_path: &Path, spanish_src: &Path) -> Result<StepResult, String> {
    if !spanish_src.exists() {
        return Ok(StepResult {
            name: "Switch to Spanish".into(),
            ok: false,
            detail: format!(
                "Bundled Spanish file not found at: {}",
                spanish_src.display()
            ),
        });
    }
    let dat = dat_path(game_path);
    let backup = backup_path(game_path);
    let sentinel = sentinel_path(game_path);

    if !dat.parent().map(|p| p.exists()).unwrap_or(false) {
        return Ok(StepResult {
            name: "Switch to Spanish".into(),
            ok: false,
            detail: format!("S1Data folder not found near: {}", dat.display()),
        });
    }

    // Backup English original once (only if we don't already have one AND current is NOT spanish)
    if !backup.exists() && !sentinel.exists() && dat.exists() {
        fs::copy(&dat, &backup).map_err(|e| format!("Backup failed: {}", e))?;
    }

    // Overwrite with Spanish
    fs::copy(spanish_src, &dat).map_err(|e| format!("Copy Spanish failed: {}", e))?;

    // Drop sentinel
    fs::write(&sentinel, b"spanish").map_err(|e| format!("Sentinel write failed: {}", e))?;

    Ok(StepResult {
        name: "Switch to Spanish".into(),
        ok: true,
        detail: format!(
            "Installed Spanish DataCenter ({} bytes)",
            fs::metadata(&dat).map(|m| m.len()).unwrap_or(0)
        ),
    })
}

/// Switch the game back to English by restoring the backup.
pub fn switch_to_english(game_path: &Path) -> Result<StepResult, String> {
    let dat = dat_path(game_path);
    let backup = backup_path(game_path);
    let sentinel = sentinel_path(game_path);

    if !backup.exists() {
        // Nothing to do: probably never switched. If sentinel exists, it's stale.
        if sentinel.exists() {
            let _ = fs::remove_file(&sentinel);
        }
        return Ok(StepResult {
            name: "Switch to English".into(),
            ok: true,
            detail: "Already English (no backup found)".into(),
        });
    }

    fs::copy(&backup, &dat).map_err(|e| format!("Restore failed: {}", e))?;
    if sentinel.exists() {
        let _ = fs::remove_file(&sentinel);
    }

    Ok(StepResult {
        name: "Switch to English".into(),
        ok: true,
        detail: format!(
            "Restored English DataCenter ({} bytes)",
            fs::metadata(&dat).map(|m| m.len()).unwrap_or(0)
        ),
    })
}
