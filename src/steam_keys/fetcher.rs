use regex::Regex;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::Read;

use super::cache::build_http_client;

// ---------------------------------------------------------------------------
// manifest_fetcher — GitHub manifest download (ManifestHub3 branch style,
// tags fallback for pjy612-style repos), Pro deflate, depotcache + backup.
// ---------------------------------------------------------------------------

const DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const MAX_DECOMPRESS_BYTES: u64 = 512 * 1024 * 1024; // 512 MB safety limit
const BACKUP_DIR_NAME: &str = "manifest-backup";

#[derive(Debug, Clone)]
pub struct FetchedManifest {
    pub depot_id: u64,
    pub manifest_gid: String,
    pub is_latest: bool,
    pub placed_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubContentEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubRefEntry {
    #[serde(rename = "ref")]
    ref_path: Option<String>,
}

fn compare_gid(a: &str, b: &str) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    a.cmp(b)
}

fn steam_root() -> Result<std::path::PathBuf, String> {
    crate::depot_downloader::steam_root()
        .ok_or_else(|| "Steam installation not found".to_string())
}

fn depotcache_dir() -> Result<std::path::PathBuf, String> {
    let dir = steam_root()?.join("depotcache");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create depotcache: {e}"))?;
    Ok(dir)
}

fn backup_root() -> std::path::PathBuf {
    crate::platform::local_data_dir().join(BACKUP_DIR_NAME)
}

fn branch_raw_url(owner: &str, repo: &str, app_id: u64, file_name: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        owner, repo, app_id, file_name
    )
}

fn try_inflate_at(raw: &[u8], offset: usize) -> Option<Vec<u8>> {
    if offset >= raw.len() {
        return None;
    }
    let mut decoder = flate2::read::DeflateDecoder::new(&raw[offset..]);
    let mut output = Vec::new();
    let mut buf = [0u8; 65536];
    let mut total: u64 = 0;
    loop {
        let n = decoder.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_DECOMPRESS_BYTES {
            return None;
        }
        output.extend_from_slice(&buf[..n]);
    }
    if output.is_empty() {
        return None;
    }
    Some(output)
}

/// Validate manifest bytes: raw parse first, then Pro-format deflate at
/// offsets 10/2/0 (validated with manifest_parser magic scan).
pub fn try_prepare_manifest(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 16 {
        return None;
    }
    if crate::manifest_parser::try_read_manifest_bytes(raw).is_some() {
        return Some(raw.to_vec());
    }
    for offset in [10, 2, 0] {
        if let Some(inflated) = try_inflate_at(raw, offset) {
            if crate::manifest_parser::try_read_manifest_bytes(&inflated).is_some() {
                return Some(inflated);
            }
        }
    }
    None
}

fn try_download_bytes(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().ok()?;
    if bytes.len() < 16 {
        return None;
    }
    Some(bytes.to_vec())
}

fn get_branch_files(
    client: &reqwest::blocking::Client,
    owner: &str,
    repo: &str,
    app_id: u64,
) -> Result<Option<HashMap<u64, String>>, String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/contents/?ref={}",
        owner, repo, app_id
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .map_err(|e| format!("Failed to fetch branch files: {e}"))?;
    if resp.status().is_success() {
        let entries: Vec<GitHubContentEntry> = resp
            .json()
            .map_err(|e| format!("Failed to parse GitHub contents: {e}"))?;
        let re = Regex::new(r"^(\d+)_(\d+)\.manifest$").unwrap();
        let mut map: HashMap<u64, String> = HashMap::new();
        for entry in &entries {
            if entry.entry_type.as_deref() != Some("file") {
                continue;
            }
            if let Some(name) = &entry.name {
                if let Some(caps) = re.captures(name) {
                    if let (Some(depot_str), Some(gid_str)) = (caps.get(1), caps.get(2)) {
                        if let Ok(depot_id) = depot_str.as_str().parse::<u64>() {
                            let gid = gid_str.as_str().to_string();
                            match map.get(&depot_id) {
                                Some(existing)
                                    if compare_gid(&gid, existing) != Ordering::Greater => {}
                                _ => {
                                    map.insert(depot_id, gid);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(Some(map))
    } else if resp.status().as_u16() == 404 {
        Ok(None)
    } else {
        Err(format!("GitHub API returned status {}", resp.status()))
    }
}

fn get_max_tag_gid(
    client: &reqwest::blocking::Client,
    owner: &str,
    repo: &str,
    depot_id: u64,
) -> Option<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/git/matching-refs/tags/{}_",
        owner, repo, depot_id
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let refs: Vec<GitHubRefEntry> = resp.json().ok()?;
    let re = Regex::new(r"^(\d+)_(\d+)$").unwrap();
    let depot_str = depot_id.to_string();
    let mut best: Option<String> = None;
    for entry in &refs {
        if let Some(ref_path) = &entry.ref_path {
            let tag_name = ref_path.split('/').last().unwrap_or("");
            if let Some(caps) = re.captures(tag_name) {
                if caps.get(1).map(|m| m.as_str()) == Some(depot_str.as_str()) {
                    if let Some(gid_match) = caps.get(2) {
                        let gid = gid_match.as_str().to_string();
                        match &best {
                            Some(cur) if compare_gid(&gid, cur) != Ordering::Greater => {}
                            _ => best = Some(gid),
                        }
                    }
                }
            }
        }
    }
    best
}

fn fetch_one_depot(
    client: &reqwest::blocking::Client,
    owner: &str,
    repo: &str,
    app_id: u64,
    depot_id: u64,
    pics_gid: &str,
    branch_files: &HashMap<u64, String>,
) -> Option<FetchedManifest> {
    // Strategy 1: direct PICS gid raw download
    if let Ok(gid_num) = pics_gid.parse::<u64>() {
        if gid_num != 0 {
            let name = format!("{}_{}.manifest", depot_id, pics_gid);
            let url = branch_raw_url(owner, repo, app_id, &name);
            if let Some(bytes) = try_download_bytes(client, &url) {
                if try_prepare_manifest(&bytes).is_some() {
                    return Some(FetchedManifest {
                        depot_id,
                        manifest_gid: pics_gid.to_string(),
                        is_latest: true,
                        placed_path: None,
                    });
                }
            }
        }
    }

    // Strategy 2: latest file from branch
    if let Some(branch_gid) = branch_files.get(&depot_id) {
        let name = format!("{}_{}.manifest", depot_id, branch_gid);
        let url = branch_raw_url(owner, repo, app_id, &name);
        if let Some(bytes) = try_download_bytes(client, &url) {
            if try_prepare_manifest(&bytes).is_some() {
                let is_latest = pics_gid.is_empty() || branch_gid == pics_gid;
                return Some(FetchedManifest {
                    depot_id,
                    manifest_gid: branch_gid.clone(),
                    is_latest,
                    placed_path: None,
                });
            }
        }
    }

    // Strategy 3: tags fallback (pjy612-style repos)
    if let Some(tag_gid) = get_max_tag_gid(client, owner, repo, depot_id) {
        let tag = format!("{}_{}", depot_id, tag_gid);
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/refs/tags/{}/{}.manifest",
            owner, repo, tag, tag
        );
        if let Some(bytes) = try_download_bytes(client, &url) {
            if try_prepare_manifest(&bytes).is_some() {
                return Some(FetchedManifest {
                    depot_id,
                    manifest_gid: tag_gid,
                    is_latest: false,
                    placed_path: None,
                });
            }
        }
    }

    None
}

fn save_manifest_to_depotcache(
    depot_id: u64,
    manifest_gid: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let depotcache = depotcache_dir()?;
    let filename = format!("{}_{}.manifest", depot_id, manifest_gid);
    let dest = depotcache.join(&filename);
    fs::write(&dest, bytes).map_err(|e| format!("Failed to write manifest: {e}"))?;
    Ok(dest.to_string_lossy().to_string())
}

fn backup_manifest(app_id: u64, depot_id: u64, manifest_gid: &str) {
    let backup_dir = backup_root().join(app_id.to_string());
    let _ = fs::create_dir_all(&backup_dir);
    let filename = format!("{}_{}.manifest", depot_id, manifest_gid);
    if let Ok(depotcache) = depotcache_dir() {
        let source = depotcache.join(&filename);
        if source.exists() {
            let _ = fs::copy(&source, backup_dir.join(&filename));
        }
    }
}

fn try_restore_from_backup(app_id: u64, depot_ids: &[u64]) -> Option<Vec<FetchedManifest>> {
    let backup_dir = backup_root().join(app_id.to_string());
    if !backup_dir.exists() {
        return None;
    }
    let depotcache = depotcache_dir().ok()?;
    let mut results = Vec::new();
    for &depot_id in depot_ids {
        let Ok(entries) = fs::read_dir(&backup_dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".manifest") {
                continue;
            }
            let stem = name.strip_suffix(".manifest").unwrap_or(&name);
            let parts: Vec<&str> = stem.splitn(2, '_').collect();
            if parts.len() != 2 {
                continue;
            }
            if let Ok(file_depot_id) = parts[0].parse::<u64>() {
                if file_depot_id == depot_id {
                    let dest = depotcache.join(&name);
                    if !dest.exists() {
                        let _ = fs::copy(entry.path(), &dest);
                    }
                    results.push(FetchedManifest {
                        depot_id,
                        manifest_gid: parts[1].to_string(),
                        is_latest: true,
                        placed_path: Some(dest.to_string_lossy().to_string()),
                    });
                }
            }
        }
    }
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

pub fn fetch_manifests_for_game(
    app_id: u64,
    depot_ids: &[u64],
    owner: &str,
    repo: &str,
) -> Result<Vec<FetchedManifest>, String> {
    // 1. Try local backup first (silent, no network)
    if let Some(local) = try_restore_from_backup(app_id, depot_ids) {
        if !local.is_empty() {
            return Ok(local);
        }
    }

    // 2. Fetch from the manifest repository
    let client = build_http_client(DOWNLOAD_TIMEOUT_SECS)?;
    let branch_files = get_branch_files(&client, owner, repo, app_id)?;
    let branch_files = branch_files.ok_or(format!(
        "Game {} has no branch in the manifest repository",
        app_id
    ))?;

    let pics_gids: HashMap<u64, String> = HashMap::new();
    let mut results = Vec::new();
    for &depot_id in depot_ids {
        let pics_gid = pics_gids.get(&depot_id).cloned().unwrap_or_default();
        if let Some(manifest) =
            fetch_one_depot(&client, owner, repo, app_id, depot_id, &pics_gid, &branch_files)
        {
            results.push(manifest);
        }
    }

    // Save each fetched manifest to depotcache + backup
    for manifest in &mut results {
        let name = format!("{}_{}.manifest", manifest.depot_id, manifest.manifest_gid);
        let url = branch_raw_url(owner, repo, app_id, &name);
        if let Some(bytes) = try_download_bytes(&client, &url) {
            if let Some(prepared) = try_prepare_manifest(&bytes) {
                if let Ok(path) =
                    save_manifest_to_depotcache(manifest.depot_id, &manifest.manifest_gid, &prepared)
                {
                    manifest.placed_path = Some(path);
                    backup_manifest(app_id, manifest.depot_id, &manifest.manifest_gid);
                }
            }
        }
    }

    Ok(results)
}
