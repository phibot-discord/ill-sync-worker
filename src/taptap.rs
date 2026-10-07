use md5::{Digest, Md5};
use serde_json::Value;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const APP_ID: u64 = 165287;

fn ua(uid: &str) -> String {
    format!(
        "V=1&PN=TapTap&VN=2.40.1-rel.100000&VN_CODE=240011000&LOC=CN&LANG=zh_CN&CH=default&UID={uid}&NT=1&SR=1080x2030&DEB=Xiaomi&DEM=Redmi+Note+5&OSV=9"
    )
}

pub struct Apk {
    pub download_url: String,
    pub version_name: String,
    pub version_code: String,
}

pub fn latest_apk() -> Result<Apk, String> {
    let uid = format!("{:x}", rand_u64());
    let x_ua = ua(&uid);
    let detail = curl_json(&[
        "-G",
        &format!(
            "https://api.taptapdada.com/app/v2/detail-by-id/{APP_ID}?X-UA={}",
            urlencoding_min(&x_ua)
        ),
    ])?;
    let apk_id = detail["data"]["download"]["apk_id"]
        .as_u64()
        .or_else(|| {
            detail["data"]["download"]["apk_id"]
                .as_str()
                .and_then(|s| s.parse().ok())
        })
        .ok_or("taptap detail missing apk_id")?;
    let nonce = format!("{:x}", rand_u64())
        .chars()
        .take(5)
        .collect::<String>();
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let param = format!(
        "abi=arm64-v8a,armeabi-v7a,armeabi&id={apk_id}&node={uid}&nonce={nonce}&sandbox=1&screen_densities=xhdpi&time={t}"
    );
    let sign = format!(
        "{:x}",
        Md5::digest(format!("X-UA={x_ua}&{param}PeCkE6Fu0B10Vm9BKfPfANwCUAn5POcs").as_bytes())
    );
    let info = curl_json(&[
        "-X",
        "POST",
        "-H",
        "content-type: application/x-www-form-urlencoded",
        "--data",
        &format!("{param}&sign={sign}"),
        &format!(
            "https://api.taptapdada.com/apk/v1/detail?X-UA={}",
            urlencoding_min(&x_ua)
        ),
    ])?;
    let apk = &info["data"]["apk"];
    let download_url = apk["download"].as_str().unwrap_or("").to_string();
    if download_url.is_empty() {
        return Err("taptap apk missing download url".into());
    }
    Ok(Apk {
        download_url,
        version_name: apk["version_name"].as_str().unwrap_or("").to_string(),
        version_code: apk["version_code"]
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| apk["version_code"].as_str().unwrap_or("").to_string()),
    })
}

fn curl_json(args: &[&str]) -> Result<Value, String> {
    let mut cmd = Command::new("curl");
    // The daemon answers the Worker only after this resolves, so a stalled TapTap must not hang it
    cmd.args([
        "-fsS",
        "--connect-timeout",
        "10",
        "--max-time",
        "30",
        "-A",
        "okhttp/3.12.1",
    ]);
    cmd.args(args);
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

fn urlencoding_min(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
