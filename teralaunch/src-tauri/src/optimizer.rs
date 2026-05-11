// =====================================================================
// TERA FPS Optimizer - integrated module for teralauncher
// Provides apply/revert/status for INI patches, LAA, DXVK and LFH.
// =====================================================================

use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// Public types exposed to the Tauri frontend
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationStatus {
    pub ini_patched: bool,
    pub ini_aggressive: bool,
    pub laa_patched: bool,
    pub dxvk_installed: bool,
    pub lfh_enabled: bool,
    pub game_bar_disabled: bool,
    pub nvidia_tweaks_applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationReport {
    pub success: bool,
    pub steps: Vec<StepResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Profile {
    /// Solo INI conservador, sin DXVK ni LAA
    Safe,
    /// INI base + LAA + LFH (sin DXVK)
    Balanced,
    /// INI base + LAA + DXVK + LFH
    Maximum,
    /// Todo de Maximum + INI agresivo + Game Bar off + NVIDIA tweaks
    Ultra,
}

// ---------------------------------------------------------------------
// Utility: read/write UTF-16 LE BOM files (S1*.ini)
// ---------------------------------------------------------------------

fn read_text_auto(path: &Path) -> Result<(String, bool), String> {
    let bytes = fs::read(path).map_err(|e| format!("read {:?}: {}", path, e))?;
    // Detect UTF-16 LE BOM (FF FE)
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let u16_buf: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16(&u16_buf).map_err(|e| format!("utf16 decode: {}", e))?;
        Ok((s, true))
    } else {
        let s = String::from_utf8_lossy(&bytes).into_owned();
        Ok((s, false))
    }
}

fn write_text_auto(path: &Path, content: &str, is_utf16: bool) -> Result<(), String> {
    let mut out: Vec<u8> = Vec::with_capacity(content.len() * if is_utf16 { 2 } else { 1 } + 2);
    if is_utf16 {
        out.push(0xFF);
        out.push(0xFE);
        for u in content.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
    } else {
        out.extend_from_slice(content.as_bytes());
    }
    fs::write(path, out).map_err(|e| format!("write {:?}: {}", path, e))?;
    Ok(())
}

fn backup_once(path: &Path) -> Result<(), String> {
    let backup = path.with_extension(format!(
        "{}.optimizer_backup",
        path.extension().and_then(|s| s.to_str()).unwrap_or("bak")
    ));
    if !backup.exists() && path.exists() {
        fs::copy(path, &backup).map_err(|e| format!("backup: {}", e))?;
    }
    Ok(())
}

fn restore_backup(path: &Path) -> Result<bool, String> {
    let backup = path.with_extension(format!(
        "{}.optimizer_backup",
        path.extension().and_then(|s| s.to_str()).unwrap_or("bak")
    ));
    if backup.exists() {
        fs::copy(&backup, path).map_err(|e| format!("restore: {}", e))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------
// INI patches
// ---------------------------------------------------------------------

const S1_ENGINE_PATCHES: &[(&str, &str)] = &[
    // FPS uncap
    ("bSmoothFrameRate=TRUE",                              "bSmoothFrameRate=FALSE"),
    // Multi-threaded rendering
    ("OneFrameThreadLag=False",                            "OneFrameThreadLag=True"),
    // Disable expensive post-process
    ("MotionBlur=True",                                    "MotionBlur=False"),
    ("DepthOfField=True",                                  "DepthOfField=False"),
    ("AmbientOcclusion=True",                              "AmbientOcclusion=False"),
    ("UseHighQualityBloom=True",                           "UseHighQualityBloom=False"),
    ("Distortion=True",                                    "Distortion=False"),
    ("FogVolumes=True",                                    "FogVolumes=False"),
    ("LensFlares=True",                                    "LensFlares=False"),
    ("FXAA=True",                                          "FXAA=False"),
    // Shadow + foliage
    ("MaxShadowResolution=2048",                           "MaxShadowResolution=1024"),
    ("FoliageDrawRadiusMultiplier=1.050000",               "FoliageDrawRadiusMultiplier=0.700000"),
    // Garbage collector tuning
    ("TimeBetweenPurgingPendingKillObjects=120",           "TimeBetweenPurgingPendingKillObjects=300"),
    ("MaxObjectsNotConsideredByGC=0",                      "MaxObjectsNotConsideredByGC=500000"),
    ("SizeOfPermanentObjectPool=0",                        "SizeOfPermanentObjectPool=16777216"),
    // Texture streaming pool (safe thanks to LAA)
    ("PoolSize=2048",                                      "PoolSize=3072"),
    ("HysteresisLimit=30",                                 "HysteresisLimit=50"),
    ("FudgeFactorDecreaseRateOfChange=-0.4",               "FudgeFactorDecreaseRateOfChange=-0.8"),
];

/// Extra aggressive set — visual quality reduced further for max FPS.
/// Applied only by Ultra profile (or via the dedicated toggle).
const S1_ENGINE_AGGRESSIVE: &[(&str, &str)] = &[
    // Texture pool bigger (only safe with LAA)
    ("PoolSize=3072",                                      "PoolSize=4096"),
    // Shadows minimal
    ("MaxShadowResolution=1024",                           "MaxShadowResolution=512"),
    ("DynamicShadows=True",                                "DynamicShadows=False"),
    ("bAllowWholeSceneDominantShadows=True",               "bAllowWholeSceneDominantShadows=False"),
    ("bEnableForegroundShadowsOnWorld=True",               "bEnableForegroundShadowsOnWorld=False"),
    ("bEnableForegroundSelfShadowing=True",                "bEnableForegroundSelfShadowing=False"),
    // LODs more aggressive (further objects load lower-poly meshes)
    ("SkeletalMeshLODBias=0",                              "SkeletalMeshLODBias=2"),
    ("ParticleLODBias=0",                                  "ParticleLODBias=2"),
    ("SpeedTreeLODBias=0",                                 "SpeedTreeLODBias=2"),
    ("DetailMode=2",                                       "DetailMode=1"),
    // Anisotropy / multisample
    ("MaxAnisotropy=16",                                   "MaxAnisotropy=4"),
    ("MaxAnisotropy=8",                                    "MaxAnisotropy=4"),
    ("MaxMultiSamples=4",                                  "MaxMultiSamples=1"),
    ("MaxMultiSamples=2",                                  "MaxMultiSamples=1"),
    // Foliage even further
    ("FoliageDrawRadiusMultiplier=0.700000",               "FoliageDrawRadiusMultiplier=0.500000"),
    // Misc — disable on-demand shader compilation hitches
    ("bInitializeShadersOnDemand=False",                   "bInitializeShadersOnDemand=True"),
    // CPU-side skinning is slower than GPU
    ("bForceCPUAccessToGPUSkinVerts=True",                 "bForceCPUAccessToGPUSkinVerts=False"),
    // Translucency lowest tier
    ("TranslucencyVolumeBlur=True",                        "TranslucencyVolumeBlur=False"),
    ("AllowSubsurfaceScattering=True",                     "AllowSubsurfaceScattering=False"),
    // Bloom completely off
    ("Bloom=True",                                         "Bloom=False"),
    // No light shafts
    ("LightShafts=True",                                   "LightShafts=False"),
    // Lower precision shadow filter
    ("ShadowFilterQualityBias=0",                          "ShadowFilterQualityBias=-1"),
];

const DEFAULT_ENGINE_PATCHES: &[(&str, &str)] = &[
    ("bSmoothFrameRate=TRUE",     "bSmoothFrameRate=FALSE"),
    ("MaxSmoothedFrameRate=80",   "MaxSmoothedFrameRate=300"),
    ("AmbientOcclusion=True",     "AmbientOcclusion=False"),
];

fn apply_patches_to(path: &Path, patches: &[(&str, &str)]) -> Result<usize, String> {
    if !path.exists() {
        return Err(format!("file not found: {:?}", path));
    }
    backup_once(path)?;
    let (mut content, is_utf16) = read_text_auto(path)?;
    let mut applied = 0;
    for (from, to) in patches {
        if content.contains(from) {
            content = content.replace(from, to);
            applied += 1;
        }
    }
    write_text_auto(path, &content, is_utf16)?;
    Ok(applied)
}

pub fn patch_ini_files(game_path: &Path) -> Result<StepResult, String> {
    let s1 = game_path.join("S1Game").join("Config").join("S1Engine.ini");
    let def = game_path.join("S1Game").join("Config").join("DefaultEngine.ini");

    let a = apply_patches_to(&s1, S1_ENGINE_PATCHES)?;
    let b = apply_patches_to(&def, DEFAULT_ENGINE_PATCHES)?;
    Ok(StepResult {
        name: "INI patches".into(),
        ok: true,
        detail: format!("S1Engine: {} replacements, DefaultEngine: {} replacements", a, b),
    })
}

pub fn revert_ini_files(game_path: &Path) -> Result<StepResult, String> {
    let mut restored = 0;
    for rel in &["S1Game/Config/S1Engine.ini", "S1Game/Config/DefaultEngine.ini"] {
        let p = game_path.join(rel);
        if restore_backup(&p)? {
            restored += 1;
        }
    }
    Ok(StepResult {
        name: "INI revert".into(),
        ok: true,
        detail: format!("{} file(s) restored from backup", restored),
    })
}

/// Apply the aggressive set on top of base patches. Visual quality dropped further.
pub fn patch_ini_aggressive(game_path: &Path) -> Result<StepResult, String> {
    let s1 = game_path.join("S1Game").join("Config").join("S1Engine.ini");
    let a = apply_patches_to(&s1, S1_ENGINE_AGGRESSIVE)?;
    Ok(StepResult {
        name: "INI aggressive".into(),
        ok: true,
        detail: format!("{} aggressive replacements", a),
    })
}

// ---------------------------------------------------------------------
// LAA patch (Large Address Aware) — direct PE header modification
// ---------------------------------------------------------------------

pub fn patch_exe_laa(exe_path: &Path) -> Result<StepResult, String> {
    if !exe_path.exists() {
        return Err(format!("exe not found: {:?}", exe_path));
    }
    backup_once(exe_path)?;

    let mut bytes = fs::read(exe_path).map_err(|e| format!("read exe: {}", e))?;
    if bytes.len() < 0x40 {
        return Err("file too small to be a PE".into());
    }

    let pe_offset = u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
    if pe_offset + 24 > bytes.len() {
        return Err("invalid PE offset".into());
    }
    if &bytes[pe_offset..pe_offset + 4] != b"PE\0\0" {
        return Err("PE signature not found".into());
    }

    let char_offset = pe_offset + 18;
    let chars = u16::from_le_bytes([bytes[char_offset], bytes[char_offset + 1]]);
    let new_chars = chars | 0x0020; // IMAGE_FILE_LARGE_ADDRESS_AWARE
    if new_chars == chars {
        return Ok(StepResult {
            name: "LAA patch".into(),
            ok: true,
            detail: "Already LAA-aware (no change)".into(),
        });
    }
    let nb = new_chars.to_le_bytes();
    bytes[char_offset] = nb[0];
    bytes[char_offset + 1] = nb[1];

    fs::write(exe_path, &bytes).map_err(|e| format!("write exe: {}", e))?;

    Ok(StepResult {
        name: "LAA patch".into(),
        ok: true,
        detail: format!("Characteristics 0x{:04X} -> 0x{:04X}", chars, new_chars),
    })
}

pub fn check_exe_laa(exe_path: &Path) -> Result<bool, String> {
    if !exe_path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(exe_path).map_err(|e| e.to_string())?;
    if bytes.len() < 0x40 { return Ok(false); }
    let pe_offset = u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
    if pe_offset + 20 > bytes.len() { return Ok(false); }
    if &bytes[pe_offset..pe_offset + 4] != b"PE\0\0" { return Ok(false); }
    let char_offset = pe_offset + 18;
    let chars = u16::from_le_bytes([bytes[char_offset], bytes[char_offset + 1]]);
    Ok((chars & 0x0020) != 0)
}

pub fn revert_exe_laa(exe_path: &Path) -> Result<StepResult, String> {
    if restore_backup(exe_path)? {
        Ok(StepResult { name: "LAA revert".into(), ok: true, detail: "exe restored".into() })
    } else {
        Ok(StepResult { name: "LAA revert".into(), ok: false, detail: "no backup found".into() })
    }
}

// ---------------------------------------------------------------------
// DXVK install (resources are bundled by Tauri)
// ---------------------------------------------------------------------

pub fn install_dxvk(
    game_path: &Path,
    dxvk_dll_src: &Path,
    dxvk_conf_src: &Path,
) -> Result<StepResult, String> {
    let binaries = game_path.join("Binaries");
    if !binaries.exists() {
        return Err(format!("Binaries dir not found at {:?}", binaries));
    }
    let d3d9 = binaries.join("d3d9.dll");
    // Backup only if existing d3d9.dll is NOT already DXVK (size <2MB is system stub)
    if d3d9.exists() {
        let meta = fs::metadata(&d3d9).map_err(|e| e.to_string())?;
        // Heuristic: DXVK x32 d3d9.dll ~4.5MB. Anything else gets backed up.
        if meta.len() < 4_000_000 {
            backup_once(&d3d9)?;
        }
    }
    fs::copy(dxvk_dll_src, &d3d9).map_err(|e| format!("copy d3d9.dll: {}", e))?;
    let conf_dst = binaries.join("dxvk.conf");
    if !conf_dst.exists() {
        fs::copy(dxvk_conf_src, &conf_dst).map_err(|e| format!("copy dxvk.conf: {}", e))?;
    }
    Ok(StepResult {
        name: "DXVK install".into(),
        ok: true,
        detail: format!("d3d9.dll -> {:?}", d3d9),
    })
}

pub fn uninstall_dxvk(game_path: &Path) -> Result<StepResult, String> {
    let binaries = game_path.join("Binaries");
    let d3d9 = binaries.join("d3d9.dll");
    if !d3d9.exists() {
        return Ok(StepResult { name: "DXVK uninstall".into(), ok: true, detail: "nothing to remove".into() });
    }
    if !restore_backup(&d3d9)? {
        // No backup: just delete (most TERA installs ship without d3d9.dll)
        fs::remove_file(&d3d9).map_err(|e| format!("remove d3d9: {}", e))?;
    }
    Ok(StepResult { name: "DXVK uninstall".into(), ok: true, detail: "d3d9.dll restored/removed".into() })
}

pub fn check_dxvk_installed(game_path: &Path) -> bool {
    let d3d9 = game_path.join("Binaries").join("d3d9.dll");
    if let Ok(meta) = fs::metadata(&d3d9) {
        // DXVK d3d9.dll x32 ~4.5MB; system stubs (if shipped) are much smaller
        meta.len() > 3_000_000
    } else {
        false
    }
}

// ---------------------------------------------------------------------
// Low Fragmentation Heap (registry, requires admin)
// ---------------------------------------------------------------------

#[cfg(windows)]
pub fn enable_lfh() -> Result<StepResult, String> {
    use std::process::Command;
    // Write via reg.exe which can elevate via UAC if needed when run from elevated process
    let output = Command::new("reg")
        .args(&[
            "add",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\TERA.exe",
            "/v", "FrontEndHeapDebugOptions",
            "/t", "REG_DWORD",
            "/d", "8",
            "/f",
        ])
        .output()
        .map_err(|e| format!("reg.exe: {}", e))?;
    if !output.status.success() {
        return Ok(StepResult {
            name: "LFH enable".into(),
            ok: false,
            detail: format!(
                "reg.exe failed (admin required?): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(StepResult { name: "LFH enable".into(), ok: true, detail: "registry key set".into() })
}

#[cfg(not(windows))]
pub fn enable_lfh() -> Result<StepResult, String> {
    Ok(StepResult { name: "LFH enable".into(), ok: false, detail: "Windows only".into() })
}

#[cfg(windows)]
pub fn check_lfh_enabled() -> bool {
    use std::process::Command;
    let output = Command::new("reg")
        .args(&[
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\TERA.exe",
            "/v", "FrontEndHeapDebugOptions",
        ])
        .output();
    if let Ok(o) = output {
        let txt = String::from_utf8_lossy(&o.stdout);
        return txt.contains("FrontEndHeapDebugOptions") && txt.contains("0x8");
    }
    false
}

#[cfg(not(windows))]
pub fn check_lfh_enabled() -> bool { false }

// ---------------------------------------------------------------------
// INI status check
// ---------------------------------------------------------------------

pub fn check_ini_patched(game_path: &Path) -> bool {
    let s1 = game_path.join("S1Game").join("Config").join("S1Engine.ini");
    if !s1.exists() { return false; }
    if let Ok((content, _)) = read_text_auto(&s1) {
        return content.contains("OneFrameThreadLag=True")
            && content.contains("MotionBlur=False");
    }
    false
}

pub fn check_ini_aggressive(game_path: &Path) -> bool {
    let s1 = game_path.join("S1Game").join("Config").join("S1Engine.ini");
    if !s1.exists() { return false; }
    if let Ok((content, _)) = read_text_auto(&s1) {
        // Sentinels distinctive of aggressive set
        return content.contains("DynamicShadows=False")
            && content.contains("SkeletalMeshLODBias=2");
    }
    false
}

// ---------------------------------------------------------------------
// Windows Game Bar / Xbox DVR — disable for older games
// ---------------------------------------------------------------------

#[cfg(windows)]
pub fn disable_game_bar() -> Result<StepResult, String> {
    use std::process::Command;
    let cmds: &[(&str, &str, &str, &str)] = &[
        // (hive\path, value, type, data)
        (r"HKCU\SOFTWARE\Microsoft\GameBar",                              "AllowAutoGameMode",       "REG_DWORD", "0"),
        (r"HKCU\SOFTWARE\Microsoft\GameBar",                              "AutoGameModeEnabled",     "REG_DWORD", "0"),
        (r"HKCU\System\GameConfigStore",                                  "GameDVR_Enabled",         "REG_DWORD", "0"),
        (r"HKLM\SOFTWARE\Policies\Microsoft\Windows\GameDVR",             "AllowGameDVR",            "REG_DWORD", "0"),
        (r"HKLM\SOFTWARE\Microsoft\PolicyManager\default\ApplicationManagement\AllowGameDVR", "value", "REG_DWORD", "0"),
    ];
    let mut errs = Vec::new();
    for (k, v, t, d) in cmds {
        let out = Command::new("reg")
            .args(&["add", k, "/v", v, "/t", t, "/d", d, "/f"])
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => errs.push(format!("{}\\{}: {}", k, v, String::from_utf8_lossy(&o.stderr).trim().to_string())),
            Err(e) => errs.push(format!("{}\\{}: {}", k, v, e)),
        }
    }
    if errs.is_empty() {
        Ok(StepResult { name: "Game Bar disable".into(), ok: true, detail: "all keys set".into() })
    } else {
        Ok(StepResult {
            name: "Game Bar disable".into(),
            ok: false,
            detail: format!("{} key(s) failed (admin?): {}", errs.len(), errs.join(" | ")),
        })
    }
}

#[cfg(windows)]
pub fn enable_game_bar() -> Result<StepResult, String> {
    use std::process::Command;
    let cmds: &[(&str, &str, &str, &str)] = &[
        (r"HKCU\SOFTWARE\Microsoft\GameBar",  "AllowAutoGameMode",   "REG_DWORD", "1"),
        (r"HKCU\SOFTWARE\Microsoft\GameBar",  "AutoGameModeEnabled", "REG_DWORD", "1"),
        (r"HKCU\System\GameConfigStore",      "GameDVR_Enabled",     "REG_DWORD", "1"),
    ];
    for (k, v, t, d) in cmds {
        let _ = Command::new("reg").args(&["add", k, "/v", v, "/t", t, "/d", d, "/f"]).output();
    }
    Ok(StepResult { name: "Game Bar revert".into(), ok: true, detail: "defaults restored".into() })
}

#[cfg(windows)]
pub fn check_game_bar_disabled() -> bool {
    use std::process::Command;
    let out = Command::new("reg")
        .args(&["query", r"HKCU\System\GameConfigStore", "/v", "GameDVR_Enabled"])
        .output();
    if let Ok(o) = out {
        let txt = String::from_utf8_lossy(&o.stdout);
        return txt.contains("0x0");
    }
    false
}

#[cfg(not(windows))]
pub fn disable_game_bar() -> Result<StepResult, String> {
    Ok(StepResult { name: "Game Bar disable".into(), ok: false, detail: "Windows only".into() })
}
#[cfg(not(windows))]
pub fn enable_game_bar() -> Result<StepResult, String> {
    Ok(StepResult { name: "Game Bar revert".into(), ok: false, detail: "Windows only".into() })
}
#[cfg(not(windows))]
pub fn check_game_bar_disabled() -> bool { false }

// ---------------------------------------------------------------------
// NVIDIA per-app profile tweaks
// We write known NVIDIA registry keys under HKCU\Software\NVIDIA Corporation\Global\.
// This is heuristic — the official way is NvAPI; the user can also use NVIDIA Inspector
// with the bundled profile we ship.
// ---------------------------------------------------------------------

#[cfg(windows)]
pub fn apply_nvidia_tweaks() -> Result<StepResult, String> {
    use std::process::Command;
    // These keys influence the user's PRESERVED settings across the driver.
    let cmds: &[(&str, &str, &str, &str)] = &[
        // Power management mode: 1 = adaptive, 2 = max performance
        (r"HKCU\Software\NVIDIA Corporation\Global\NVTweak",          "PowerMizerLevelAC", "REG_DWORD", "1"),
        // Threaded optimization
        (r"HKCU\Software\NVIDIA Corporation\Global\FTS",              "EnableRID66610",    "REG_DWORD", "1"),
        // Shader cache: unlimited
        (r"HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\Global\Startup", "DisableShaderDiskCache", "REG_DWORD", "0"),
    ];
    let mut errs = Vec::new();
    for (k, v, t, d) in cmds {
        let out = Command::new("reg").args(&["add", k, "/v", v, "/t", t, "/d", d, "/f"]).output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => errs.push(format!("{}: {}", v, String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => errs.push(format!("{}: {}", v, e)),
        }
    }
    if errs.is_empty() {
        Ok(StepResult {
            name: "NVIDIA tweaks".into(),
            ok: true,
            detail: "registry hints applied — for full effect use NVIDIA Control Panel per-app or Inspector".into(),
        })
    } else {
        Ok(StepResult {
            name: "NVIDIA tweaks".into(),
            ok: false,
            detail: format!("{} key(s) failed (admin?): {}", errs.len(), errs.join(" | ")),
        })
    }
}

#[cfg(windows)]
pub fn check_nvidia_tweaks() -> bool {
    use std::process::Command;
    let out = Command::new("reg")
        .args(&["query", r"HKCU\Software\NVIDIA Corporation\Global\NVTweak", "/v", "PowerMizerLevelAC"])
        .output();
    if let Ok(o) = out {
        let txt = String::from_utf8_lossy(&o.stdout);
        return txt.contains("PowerMizerLevelAC");
    }
    false
}

#[cfg(not(windows))]
pub fn apply_nvidia_tweaks() -> Result<StepResult, String> {
    Ok(StepResult { name: "NVIDIA tweaks".into(), ok: false, detail: "Windows only".into() })
}
#[cfg(not(windows))]
pub fn check_nvidia_tweaks() -> bool { false }

pub fn get_status(game_path: &Path) -> OptimizationStatus {
    let tera_exe = game_path.join("Binaries").join("TERA.exe");
    OptimizationStatus {
        ini_patched: check_ini_patched(game_path),
        ini_aggressive: check_ini_aggressive(game_path),
        laa_patched: check_exe_laa(&tera_exe).unwrap_or(false),
        dxvk_installed: check_dxvk_installed(game_path),
        lfh_enabled: check_lfh_enabled(),
        game_bar_disabled: check_game_bar_disabled(),
        nvidia_tweaks_applied: check_nvidia_tweaks(),
    }
}

// ---------------------------------------------------------------------
// Apply profile (orchestrator)
// ---------------------------------------------------------------------

pub fn apply_profile(
    profile: Profile,
    game_path: &Path,
    dxvk_dll_src: Option<&Path>,
    dxvk_conf_src: Option<&Path>,
) -> OptimizationReport {
    let mut steps = Vec::new();

    // INI base (always)
    match patch_ini_files(game_path) {
        Ok(s) => steps.push(s),
        Err(e) => steps.push(StepResult { name: "INI patches".into(), ok: false, detail: e }),
    }

    // LAA + LFH (Balanced, Maximum, Ultra)
    if matches!(profile, Profile::Balanced | Profile::Maximum | Profile::Ultra) {
        let exe = game_path.join("Binaries").join("TERA.exe");
        match patch_exe_laa(&exe) {
            Ok(s) => steps.push(s),
            Err(e) => steps.push(StepResult { name: "LAA patch".into(), ok: false, detail: e }),
        }
        match enable_lfh() {
            Ok(s) => steps.push(s),
            Err(e) => steps.push(StepResult { name: "LFH enable".into(), ok: false, detail: e }),
        }
    }

    // DXVK (Maximum + Ultra)
    if matches!(profile, Profile::Maximum | Profile::Ultra) {
        if let (Some(dll), Some(conf)) = (dxvk_dll_src, dxvk_conf_src) {
            match install_dxvk(game_path, dll, conf) {
                Ok(s) => steps.push(s),
                Err(e) => steps.push(StepResult { name: "DXVK install".into(), ok: false, detail: e }),
            }
        } else {
            steps.push(StepResult {
                name: "DXVK install".into(),
                ok: false,
                detail: "bundled DXVK resources not provided".into(),
            });
        }
    }

    // Ultra extras: aggressive INI + Game Bar off + NVIDIA tweaks
    if matches!(profile, Profile::Ultra) {
        match patch_ini_aggressive(game_path) {
            Ok(s) => steps.push(s),
            Err(e) => steps.push(StepResult { name: "INI aggressive".into(), ok: false, detail: e }),
        }
        match disable_game_bar() {
            Ok(s) => steps.push(s),
            Err(e) => steps.push(StepResult { name: "Game Bar disable".into(), ok: false, detail: e }),
        }
        match apply_nvidia_tweaks() {
            Ok(s) => steps.push(s),
            Err(e) => steps.push(StepResult { name: "NVIDIA tweaks".into(), ok: false, detail: e }),
        }
    }

    let success = steps.iter().all(|s| s.ok);
    OptimizationReport { success, steps }
}

pub fn revert_all(game_path: &Path) -> OptimizationReport {
    let mut steps = Vec::new();
    match revert_ini_files(game_path) {
        Ok(s) => steps.push(s),
        Err(e) => steps.push(StepResult { name: "INI revert".into(), ok: false, detail: e }),
    }
    let exe = game_path.join("Binaries").join("TERA.exe");
    match revert_exe_laa(&exe) {
        Ok(s) => steps.push(s),
        Err(e) => steps.push(StepResult { name: "LAA revert".into(), ok: false, detail: e }),
    }
    match uninstall_dxvk(game_path) {
        Ok(s) => steps.push(s),
        Err(e) => steps.push(StepResult { name: "DXVK uninstall".into(), ok: false, detail: e }),
    }
    match enable_game_bar() {
        Ok(s) => steps.push(s),
        Err(e) => steps.push(StepResult { name: "Game Bar revert".into(), ok: false, detail: e }),
    }
    let success = steps.iter().all(|s| s.ok);
    OptimizationReport { success, steps }
}

