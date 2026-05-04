// Jackson Coxson
//
// Windows-only build script:
//
//  1. Reads `vendor-manifest.toml` to find the right vendor zip for the
//     active build target (libusbK + libwdi binaries, per-arch).
//  2. If `vendor/.vendor-stamp-{version}-{target}` is missing, downloads
//     the zip from the manifest URL, verifies its SHA-256, and extracts
//     it into `vendor/`. Subsequent builds find the stamp and no-op.
//  3. Emits the link directives for libusbK + libwdi + their transitive
//     Win32 SDK deps.
//
// Bypass: setting `LIBUSBK_DIR` and `LIBWDI_DIR` env vars skips the
// fetch entirely and uses the directories directly. Useful when
// developing libwdi locally.
//
// Cross-compiling to Windows from a non-Windows host isn't supported.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    println!("cargo:rerun-if-changed=vendor-manifest.toml");
    println!("cargo:rerun-if-env-changed=LIBUSBK_DIR");
    println!("cargo:rerun-if-env-changed=LIBWDI_DIR");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = std::env::var("TARGET").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let arch_dir = match arch.as_str() {
        "x86_64" => "amd64",
        "x86" => "x86",
        "aarch64" => "arm64",
        other => panic!("Unsupported Windows arch: {other}"),
    };

    let libusbk_env = std::env::var("LIBUSBK_DIR").ok();
    let libwdi_env = std::env::var("LIBWDI_DIR").ok();

    // If both override env vars are set we trust the caller's
    // directories and skip the auto-fetch entirely.
    if (libusbk_env.is_none() || libwdi_env.is_none())
        && let Err(e) = ensure_vendor(&manifest_dir, &target)
    {
        panic!("Failed to set up vendor binaries for {target}: {e}");
    }

    let libusbk_root =
        libusbk_env.unwrap_or_else(|| format!("{}/vendor/libusbK", manifest_dir.display()));
    println!("cargo:rustc-link-search=native={libusbk_root}/lib/{arch_dir}");
    println!("cargo:rustc-link-lib=dylib=libusbK");

    let libwdi_root =
        libwdi_env.unwrap_or_else(|| format!("{}/vendor/libwdi", manifest_dir.display()));
    println!("cargo:rustc-link-search=native={libwdi_root}/lib/{arch_dir}");
    println!("cargo:rustc-link-lib=static=libwdi");

    // libwdi's transitive Win32 SDK deps. The static archive doesn't
    // pull these in via #pragma comment(lib,...) when consumed from
    // Rust, so declare them explicitly.
    for lib in &[
        "setupapi", "cfgmgr32", "ole32", "user32", "advapi32", "crypt32", "wintrust", "newdev",
        "version", "shell32", "shlwapi",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}

fn ensure_vendor(manifest_dir: &Path, target: &str) -> Result<(), String> {
    let toml_path = manifest_dir.join("vendor-manifest.toml");
    let toml_text = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("read {}: {e}", toml_path.display()))?;
    let manifest = parse_manifest(&toml_text)?;
    let entry = manifest
        .targets
        .iter()
        .find(|(t, _)| t == target)
        .map(|(_, e)| e)
        .ok_or_else(|| format!("no vendor entry for target {target} in vendor-manifest.toml"))?;

    let vendor_dir = manifest_dir.join("vendor");
    std::fs::create_dir_all(&vendor_dir)
        .map_err(|e| format!("create_dir_all {}: {e}", vendor_dir.display()))?;
    let stamp = vendor_dir.join(format!(".vendor-stamp-{}-{}", manifest.version, target));
    if stamp.exists() {
        return Ok(());
    }

    let url = manifest
        .url_template
        .replace("{version}", &manifest.version)
        .replace("{filename}", &entry.filename);
    println!("cargo:warning=Downloading vendor bundle for {target}: {url}");

    // Download to OUT_DIR so cargo cleans it up automatically.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let zip_path = out_dir.join(&entry.filename);
    download(&url, &zip_path)?;

    let actual = sha256_hex(&zip_path)?;
    let expected = entry.sha256.to_ascii_lowercase();
    if actual != expected {
        return Err(format!(
            "SHA-256 mismatch for {}: expected {expected}, got {actual}",
            entry.filename
        ));
    }

    // Zips have `vendor/...` as their top-level layout, so extract to
    // the manifest dir (repo root). `tar -xf` merges into existing
    // dirs, so our checked-in headers under vendor/<lib>/include/ stay
    // put.
    extract_zip(&zip_path, manifest_dir)?;

    std::fs::write(&stamp, "").map_err(|e| format!("write stamp {}: {e}", stamp.display()))?;
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "-fL",
            "-sS",
            "--retry",
            "3",
            "-o",
            &dest.to_string_lossy(),
            url,
        ])
        .output()
        .map_err(|e| format!("invoke curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl exit {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let output = Command::new("certutil")
        .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
        .output()
        .map_err(|e| format!("invoke certutil: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "certutil exit {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(trimmed.to_ascii_lowercase());
        }
    }
    Err(format!(
        "could not find SHA-256 hex line in certutil output:\n{stdout}"
    ))
}

fn extract_zip(zip: &Path, dest: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .args(["-xf", &zip.to_string_lossy(), "-C", &dest.to_string_lossy()])
        .output()
        .map_err(|e| format!("invoke tar: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar exit {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    Ok(())
}

// --- minimal vendor-manifest.toml parser -------------------------------
//
// We don't take a `toml` crate build-dep just for this file. Format we
// accept (and only this format):
//
//   version = "..."
//   url_template = "..."
//   [targets."<triple>"]
//   filename = "..."
//   sha256   = "..."

struct Manifest {
    version: String,
    url_template: String,
    targets: Vec<(String, TargetEntry)>,
}

struct TargetEntry {
    filename: String,
    sha256: String,
}

fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut version: Option<String> = None;
    let mut url_template: Option<String> = None;
    let mut targets: Vec<(String, TargetEntry)> = Vec::new();
    let mut current_target: Option<String> = None;
    let mut cur_filename: Option<String> = None;
    let mut cur_sha256: Option<String> = None;

    let flush = |target: &mut Option<String>,
                 filename: &mut Option<String>,
                 sha256: &mut Option<String>,
                 out: &mut Vec<(String, TargetEntry)>|
     -> Result<(), String> {
        if let Some(t) = target.take() {
            let f = filename
                .take()
                .ok_or_else(|| format!("section [{t}] missing filename"))?;
            let s = sha256
                .take()
                .ok_or_else(|| format!("section [{t}] missing sha256"))?;
            out.push((
                t,
                TargetEntry {
                    filename: f,
                    sha256: s,
                },
            ));
        }
        Ok(())
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush(
                &mut current_target,
                &mut cur_filename,
                &mut cur_sha256,
                &mut targets,
            )?;
            let inner = &line[1..line.len() - 1];
            // Only [targets."..."] sections are recognized; ignore others.
            if let Some(rest) = inner.strip_prefix("targets.") {
                current_target = Some(rest.trim_matches('"').to_string());
            }
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim().trim_matches('"');
        match (current_target.is_some(), key) {
            (false, "version") => version = Some(val.to_string()),
            (false, "url_template") => url_template = Some(val.to_string()),
            (true, "filename") => cur_filename = Some(val.to_string()),
            (true, "sha256") => cur_sha256 = Some(val.to_string()),
            _ => {}
        }
    }
    flush(
        &mut current_target,
        &mut cur_filename,
        &mut cur_sha256,
        &mut targets,
    )?;

    Ok(Manifest {
        version: version.ok_or_else(|| "manifest missing top-level `version`".to_string())?,
        url_template: url_template
            .ok_or_else(|| "manifest missing top-level `url_template`".to_string())?,
        targets,
    })
}
