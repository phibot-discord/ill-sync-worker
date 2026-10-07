use serde_json::Value;

#[derive(Clone, Debug)]
pub enum Want {
    Chart { folder: String, diff: String },
    Jacket { folder: String },
    Music { folder: String },
    Avatar { name: String },
}

#[derive(Clone, Debug)]
pub struct BundleJob {
    pub zip_path: String,
    pub want: Want,
}

pub fn jobs(catalog: &Value) -> Result<Vec<BundleJob>, String> {
    let loc = find_locator(catalog).ok_or("catalog has no addressable locator")?;
    let keys = decode_b64(loc["m_KeyDataString"].as_str().ok_or("m_KeyDataString")?)?;
    let buckets = decode_b64(
        loc["m_BucketDataString"]
            .as_str()
            .ok_or("m_BucketDataString")?,
    )?;
    let entries = decode_b64(
        loc["m_EntryDataString"]
            .as_str()
            .ok_or("m_EntryDataString")?,
    )?;
    let internals = loc["m_InternalIds"].as_array().ok_or("m_InternalIds")?;
    let key_at = parse_keys(&keys)?;
    let key_entry = parse_buckets(&buckets, &key_at)?;
    let mut jobs = Vec::new();
    for (key, entry_index) in key_entry {
        let Some(want) = classify(&key) else { continue };
        let entry = read_entry(&entries, entry_index)?;
        let bundle_entry = if entry[1] == 0 || entry[1] == 1 {
            entry
        } else if entry[2] >= 0 {
            read_entry(&entries, entry[2])?
        } else {
            continue;
        };
        let internal = internals
            .get(bundle_entry[0] as usize)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if internal.is_empty() {
            continue;
        }
        jobs.push(BundleJob {
            zip_path: bundle_zip_path(internal),
            want,
        });
    }
    if jobs.is_empty() {
        return Err("catalog listed no track bundles".into());
    }
    Ok(jobs)
}

fn classify(key: &str) -> Option<Want> {
    if let Some(rest) = key.strip_prefix("Assets/Tracks/") {
        let (folder, file) = rest.rsplit_once('/')?;
        if let Some(diff) = file
            .strip_prefix("Chart_")
            .and_then(|s| s.strip_suffix(".json"))
        {
            if matches!(diff, "EZ" | "HD" | "IN" | "AT" | "Legacy") {
                return Some(Want::Chart {
                    folder: folder.to_string(),
                    diff: diff.to_string(),
                });
            }
        }
        if file == "Illustration.jpg" || file == "Illustration.png" {
            return Some(Want::Jacket {
                folder: folder.to_string(),
            });
        }
        if file.starts_with("music.") {
            return Some(Want::Music {
                folder: folder.to_string(),
            });
        }
        return None;
    }
    if let Some(name) = key.strip_prefix("avatar.") {
        if !name.is_empty() && !name.contains('/') {
            return Some(Want::Avatar {
                name: name.to_string(),
            });
        }
    }
    None
}

fn bundle_zip_path(internal: &str) -> String {
    internal.replace(
        "{UnityEngine.AddressableAssets.Addressables.RuntimePath}",
        "assets/aa",
    )
}

fn find_locator(value: &Value) -> Option<&Value> {
    if value.get("m_KeyDataString").is_some() {
        return Some(value);
    }
    match value {
        Value::Object(map) => map.values().find_map(find_locator),
        Value::Array(list) => list.iter().find_map(find_locator),
        _ => None,
    }
}

fn parse_keys(keys: &[u8]) -> Result<Vec<(i32, String)>, String> {
    if keys.len() < 4 {
        return Err("empty key blob".into());
    }
    let count = i32::from_le_bytes(keys[0..4].try_into().unwrap());
    if count < 0 {
        return Err("bad key count".into());
    }
    let mut i = 4;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let off = i as i32;
        let kind = *keys.get(i).ok_or("truncated key")?;
        i += 1;
        match kind {
            0 | 1 => {
                let len = u32::from_le_bytes(
                    keys.get(i..i + 4)
                        .ok_or("truncated key")?
                        .try_into()
                        .unwrap(),
                ) as usize;
                i += 4;
                let raw = keys.get(i..i + len).ok_or("truncated key string")?;
                i += len;
                let text = if kind == 0 {
                    String::from_utf8_lossy(raw).into_owned()
                } else {
                    let units: Vec<u16> = raw
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&units)
                };
                out.push((off, text));
            }
            4 => {
                i += 4;
            }
            other => return Err(format!("unknown addressable key type {other}")),
        }
    }
    Ok(out)
}

fn parse_buckets(buckets: &[u8], keys: &[(i32, String)]) -> Result<Vec<(String, i32)>, String> {
    let count = i32::from_le_bytes(buckets.get(0..4).ok_or("buckets")?.try_into().unwrap());
    let mut i = 4;
    let mut map = std::collections::HashMap::new();
    for (off, text) in keys {
        map.insert(*off, text.clone());
    }
    let mut out = Vec::new();
    for _ in 0..count {
        let off = i32::from_le_bytes(buckets.get(i..i + 4).ok_or("bucket")?.try_into().unwrap());
        i += 4;
        let n = i32::from_le_bytes(buckets.get(i..i + 4).ok_or("bucket")?.try_into().unwrap());
        i += 4;
        if n < 0 {
            return Err("bad bucket".into());
        }
        let first = if n == 0 {
            i += 0;
            continue;
        } else {
            let idx = i32::from_le_bytes(
                buckets
                    .get(i..i + 4)
                    .ok_or("bucket idx")?
                    .try_into()
                    .unwrap(),
            );
            i += 4 * n as usize;
            idx
        };
        if let Some(text) = map.get(&off) {
            out.push((text.clone(), first));
        }
    }
    Ok(out)
}

fn read_entry(entries: &[u8], index: i32) -> Result<[i32; 7], String> {
    let at = 4 + index as usize * 28;
    let raw = entries.get(at..at + 28).ok_or("entry index")?;
    let mut nums = [0i32; 7];
    for (i, chunk) in raw.chunks_exact(4).enumerate() {
        nums[i] = i32::from_le_bytes(chunk.try_into().unwrap());
    }
    Ok(nums)
}

fn decode_b64(text: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("bad base64".into()),
        })
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            break;
        }
        let b0 = val(bytes[i])?;
        let b1 = val(*bytes.get(i + 1).ok_or("bad base64")?)?;
        let b2 = if i + 2 < bytes.len() && bytes[i + 2] != b'=' {
            Some(val(bytes[i + 2])?)
        } else {
            None
        };
        let b3 = if i + 3 < bytes.len() && bytes[i + 3] != b'=' {
            Some(val(bytes[i + 3])?)
        } else {
            None
        };
        out.push((b0 << 2) | (b1 >> 4));
        if let Some(b2) = b2 {
            out.push((b1 << 4) | (b2 >> 2));
            if let Some(b3) = b3 {
                out.push((b2 << 6) | b3);
            }
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_chart_bundle_from_the_401_catalog() {
        let path = "/tmp/catalog.json";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = jobs(&value).unwrap();
        assert!(jobs.len() > 1000, "{}", jobs.len());
        assert!(jobs.iter().any(|j| matches!(j.want, Want::Chart { .. })
            && j.zip_path.contains("assets/aa/Android/")
            && j.zip_path.ends_with(".bundle")));
        assert!(jobs.iter().any(|j| matches!(&j.want, Want::Jacket { folder } if folder.contains("Exoplanetary") || folder.contains("Glaciaxion"))));
    }
}
