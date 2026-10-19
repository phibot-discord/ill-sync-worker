//! `history/`: a chart a new version changes is kept under `history/<previous version>/`, and history/index.json
//! (the site's version picker) gets an era for the charts the song had until then

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const PREFIX: &str = "history";
pub const INDEX: &str = "history/index.json";

#[derive(Clone, Debug, PartialEq)]
pub struct EraChart {
    pub label: String,
    pub notes: usize,
    pub charter: String,
}

/// Every level a changed song had before, and which of them changed
#[derive(Debug)]
pub struct SongChange {
    pub id: String,
    pub stem: String,
    pub before: BTreeMap<String, EraChart>,
    pub changed: Vec<String>,
}

/// `history/<version>/` plus a main-tree key
pub fn key_in(version: &str, main_key: &str) -> String {
    format!("{PREFIX}/{version}/{main_key}")
}

/// The name this build goes by in the index: its version name, or `<name>-<code>` when an earlier build already took
/// that name (4.0.1.1 shipped as 158 and again as 159)
pub fn version_id(index: &Value, name: &str, code: i64) -> String {
    let taken = versions(index).iter().any(|v| v == name);
    if !taken || index["codes"][name].as_i64() == Some(code) {
        name.to_string()
    } else {
        format!("{name}-{code}")
    }
}

/// A recorded build with a higher code than `code`
pub fn newer_than(index: &Value, code: i64) -> Option<String> {
    index["codes"]
        .as_object()?
        .iter()
        .find(|(_, c)| c.as_i64().is_some_and(|c| c > code))
        .map(|(v, _)| v.clone())
}

/// Where a song's `level` chart of `version` lives
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Current,
    Copy(String),
    Missing,
}

pub fn source_at(index: &Value, stem: &str, level: &str, version: &str) -> Source {
    let Some(song) = song_of(index, stem) else {
        return Source::Current;
    };
    let has = |list: &Value| {
        list.as_array()
            .is_some_and(|l| l.iter().any(|v| v == version))
    };
    if has(&song["current"]) {
        return Source::Current;
    }
    let eras = song["eras"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    match eras.iter().find(|era| has(&era["versions"])) {
        Some(era) => match era["charts"].get(level) {
            Some(chart) => chart["rev"]
                .as_str()
                .map_or(Source::Current, |rev| Source::Copy(rev.to_string())),
            None => Source::Missing,
        },
        None => Source::Current,
    }
}

fn song_of<'a>(index: &'a Value, stem: &str) -> Option<&'a Value> {
    index["songs"]
        .as_array()?
        .iter()
        .find(|s| s["stem"] == stem)
}

/// How a build added out of order relates to the recorded ones, per song
#[derive(Debug)]
pub enum Placement {
    SameAsNext,
    SameAsPrev,
    Era(Map<String, Value>),
}

/// The era chart for `chart` at `source` (`Copy` keeps its rev, `Current` has none)
pub fn era_chart_from(chart: &EraChart, source: &Source) -> Value {
    match source {
        Source::Copy(rev) => era_chart(chart, Some(rev)),
        _ => era_chart(chart, None),
    }
}

/// Put build `x` (code `code`) into the index before `versions[pos]`: each song joins the version it matches, or gets
/// an era of its own just older than that version's
pub fn insert(
    index: &mut Value,
    pos: usize,
    x: &str,
    code: i64,
    songs: &[(String, String, Placement)],
) {
    let all = versions(index);
    let Some(next) = all.get(pos).cloned() else {
        return;
    };
    let prev = pos.checked_sub(1).and_then(|p| all.get(p)).cloned();
    let later: Vec<Value> = all[pos..].iter().map(|v| json!(v)).collect();
    if let Some(list) = index["versions"].as_array_mut() {
        list.insert(pos, json!(x));
    }
    if !index["codes"].is_object() {
        index["codes"] = json!({});
    }
    index["codes"][x] = json!(code);
    let join = |list: &mut Value, anchor: &str, after: bool| -> bool {
        let Some(list) = list.as_array_mut() else {
            return false;
        };
        let Some(at) = list.iter().position(|v| v == anchor) else {
            return false;
        };
        list.insert(if after { at + 1 } else { at }, json!(x));
        true
    };
    for (id, stem, placement) in songs {
        let list = songs_mut(index);
        let at = list.iter().position(|s| s["stem"] == stem.as_str());
        match placement {
            Placement::SameAsNext | Placement::SameAsPrev => {
                let Some(at) = at else {
                    continue;
                };
                let (anchor, after) = match placement {
                    Placement::SameAsNext => (next.clone(), false),
                    _ => match &prev {
                        Some(prev) => (prev.clone(), true),
                        None => continue,
                    },
                };
                let song = &mut list[at];
                if join(&mut song["current"], &anchor, after) {
                    continue;
                }
                if let Some(eras) = song["eras"].as_array_mut() {
                    for era in eras.iter_mut() {
                        if join(&mut era["versions"], &anchor, after) {
                            break;
                        }
                    }
                }
            }
            Placement::Era(charts) => {
                let era = json!({"versions": [x], "charts": charts});
                match at {
                    Some(at) => {
                        let song = &mut list[at];
                        let in_current = song["current"]
                            .as_array()
                            .is_some_and(|c| c.iter().any(|v| v == next.as_str()));
                        let eras = song["eras"].as_array_mut().expect("eras is an array");
                        let spot = if in_current {
                            0
                        } else {
                            eras.iter()
                                .position(|e| {
                                    e["versions"]
                                        .as_array()
                                        .is_some_and(|v| v.iter().any(|v| v == next.as_str()))
                                })
                                .map_or(0, |i| i + 1)
                        };
                        eras.insert(spot, era);
                    }
                    None => {
                        list.push(json!({"id": id, "stem": stem, "current": later, "eras": [era]}))
                    }
                }
            }
        }
    }
}

pub fn versions(index: &Value) -> Vec<String> {
    index["versions"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn songs_mut(index: &mut Value) -> &mut Vec<Value> {
    if !index["songs"].is_array() {
        index["songs"] = json!([]);
    }
    index["songs"].as_array_mut().expect("songs is an array")
}

/// (era position, rev) of each older copy of `level` the index lists for `stem`
pub fn revs_of(index: &Value, stem: &str, level: &str) -> Vec<(usize, String)> {
    let Some(song) = index["songs"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["stem"] == stem))
    else {
        return Vec::new();
    };
    song["eras"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(i, era)| Some((i, era["charts"][level]["rev"].as_str()?.to_string())))
        .collect()
}

fn era_chart(chart: &EraChart, rev: Option<&str>) -> Value {
    let mut out = json!({"label": chart.label, "notes": chart.notes, "charter": chart.charter});
    if let Some(rev) = rev {
        out["rev"] = json!(rev);
    }
    out
}

/// Move the index from `prev` to `next`: each changed song gets an era for its old charts. `back_to(stem, level, era)`
/// marks an older era's copy that `next` restored; an era left with no copy of its own is dropped
pub fn advance(
    index: &mut Value,
    prev: &str,
    next: &str,
    code: i64,
    present: &[String],
    new_songs: &[(String, String)],
    changes: &[SongChange],
    back_to: &dyn Fn(&str, &str, usize) -> bool,
) -> bool {
    if versions(index).iter().any(|v| v == next) {
        return false;
    }
    if !index["versions"].is_array() {
        index["versions"] = json!([]);
    }
    index["versions"]
        .as_array_mut()
        .expect("versions is an array")
        .push(json!(next));
    if !index["codes"].is_object() {
        index["codes"] = json!({});
    }
    index["codes"][next] = json!(code);
    for song in songs_mut(index).iter_mut() {
        let stem = song["stem"].as_str().unwrap_or_default().to_string();
        let changed = changes.iter().any(|c| c.stem == stem);
        if !changed && present.contains(&stem) {
            if let Some(current) = song["current"].as_array_mut() {
                current.push(json!(next));
            }
        }
    }
    for (id, stem) in new_songs {
        let list = songs_mut(index);
        if !list.iter().any(|s| s["stem"] == stem.as_str()) {
            list.push(json!({"id": id, "stem": stem, "current": [next], "eras": []}));
        }
    }
    for change in changes {
        let list = songs_mut(index);
        let at = match list.iter().position(|s| s["stem"] == change.stem.as_str()) {
            Some(at) => at,
            None => {
                list.push(
                    json!({"id": change.id, "stem": change.stem, "current": [prev], "eras": []}),
                );
                list.len() - 1
            }
        };
        let song = &mut list[at];
        let mut eras: Vec<Value> = song["eras"].as_array().cloned().unwrap_or_default();
        let mut emptied = vec![false; eras.len()];
        for (i, era) in eras.iter_mut().enumerate() {
            let Some(charts) = era["charts"].as_object_mut() else {
                continue;
            };
            for level in &change.changed {
                let Some(chart) = charts
                    .get_mut(level.as_str())
                    .and_then(Value::as_object_mut)
                else {
                    continue;
                };
                if !chart.contains_key("rev") {
                    chart.insert("rev".into(), json!(prev));
                } else if back_to(&change.stem, level, i) {
                    chart.remove("rev");
                    emptied[i] = true;
                }
            }
        }
        let has_copy = |era: &Value| {
            era["charts"]
                .as_object()
                .is_some_and(|c| c.values().any(|chart| chart.get("rev").is_some()))
        };
        let mut kept: Vec<Value> = eras
            .into_iter()
            .zip(emptied)
            .filter(|(era, emptied)| !*emptied || has_copy(era))
            .map(|(era, _)| era)
            .collect();
        let charts: Map<String, Value> = change
            .before
            .iter()
            .map(|(level, chart)| {
                let rev = change.changed.contains(level).then_some(prev);
                (level.clone(), era_chart(chart, rev))
            })
            .collect();
        let versions = song["current"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![json!(prev)]);
        kept.insert(0, json!({"versions": versions, "charts": charts}));
        song["eras"] = Value::Array(kept);
        song["current"] = json!([next]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(label: &str, notes: usize) -> EraChart {
        EraChart {
            label: label.into(),
            notes,
            charter: "c".into(),
        }
    }

    fn index() -> Value {
        json!({
            "v": 1,
            "versions": ["3.19.0.1", "4.0.1"],
            "songs": [
                {"id": "A.x.0", "stem": "A.x", "current": ["3.19.0.1", "4.0.1"], "eras": [
                    {"versions": ["3.12.0"], "charts": {
                        "EZ": {"label": "3.0", "notes": 100, "charter": "c"},
                        "IN": {"label": "14.0", "notes": 900, "charter": "c", "rev": "3.12.0"}
                    }}
                ]},
                {"id": "B.y.0", "stem": "B.y", "current": ["4.0.1"], "eras": [
                    {"versions": ["3.19.0.1"], "charts": {"AT": {"label": "16.0", "notes": 1500, "charter": "c", "rev": "3.19.0.1"}}}
                ]}
            ]
        })
    }

    fn change(id: &str, stem: &str, changed: &[&str]) -> SongChange {
        SongChange {
            id: id.into(),
            stem: stem.into(),
            before: [
                ("EZ", chart("3.0", 100)),
                ("IN", chart("14.5", 950)),
                ("AT", chart("16.2", 1600)),
            ]
            .into_iter()
            .map(|(l, c)| (l.to_string(), c))
            .collect(),
            changed: changed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_changed_chart_becomes_an_era_and_older_eras_keep_their_view() {
        let mut idx = index();
        let present = vec!["A.x".to_string(), "B.y".to_string()];
        let changes = [change("A.x.0", "A.x", &["EZ"])];
        assert!(advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            159,
            &present,
            &[],
            &changes,
            &|_, _, _| false
        ));
        let a = &idx["songs"][0];
        assert_eq!(a["current"], json!(["4.0.1.1"]));
        assert_eq!(a["eras"][0]["versions"], json!(["3.19.0.1", "4.0.1"]));
        assert_eq!(a["eras"][0]["charts"]["EZ"]["rev"], "4.0.1");
        assert_eq!(a["eras"][0]["charts"]["IN"].get("rev"), None);
        assert_eq!(a["eras"][1]["charts"]["EZ"]["rev"], "4.0.1");
        assert_eq!(a["eras"][1]["charts"]["IN"]["rev"], "3.12.0");
        assert_eq!(idx["songs"][1]["current"], json!(["4.0.1", "4.0.1.1"]));
        assert_eq!(idx["versions"], json!(["3.19.0.1", "4.0.1", "4.0.1.1"]));
        assert_eq!(idx["codes"]["4.0.1.1"], 159);
        let again = idx.clone();
        assert!(!advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            159,
            &present,
            &[],
            &changes,
            &|_, _, _| false
        ));
        assert_eq!(idx, again);
    }

    #[test]
    fn a_second_build_under_the_same_name_gets_its_code() {
        let mut idx = index();
        assert_eq!(version_id(&idx, "4.0.1.1", 158), "4.0.1.1");
        advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            158,
            &[],
            &[],
            &[],
            &|_, _, _| false,
        );
        assert_eq!(version_id(&idx, "4.0.1.1", 158), "4.0.1.1");
        assert_eq!(version_id(&idx, "4.0.1.1", 159), "4.0.1.1-159");
        assert_eq!(version_id(&idx, "4.0.1", 157), "4.0.1-157");
        let changes = [change("B.y.0", "B.y", &["AT"])];
        advance(
            &mut idx,
            "4.0.1.1",
            "4.0.1.1-159",
            159,
            &[],
            &[],
            &changes,
            &|_, _, _| false,
        );
        assert_eq!(version_id(&idx, "4.0.1.1", 159), "4.0.1.1-159");
        assert_eq!(idx["songs"][1]["current"], json!(["4.0.1.1-159"]));
        assert_eq!(idx["songs"][1]["eras"][0]["charts"]["AT"]["rev"], "4.0.1.1");
    }

    #[test]
    fn a_build_published_out_of_order_slots_in_before_the_next_one() {
        let mut idx = json!({
            "v": 1,
            "versions": ["4.0.1", "4.0.1.1"],
            "codes": {"4.0.1.1": 159},
            "songs": [
                {"id": "H.0", "stem": "H", "current": ["4.0.1.1"], "eras": [
                    {"versions": ["4.0.1"], "charts": {
                        "EZ": {"label": "9.0", "notes": 1, "charter": "c"},
                        "AT": {"label": "17.9", "notes": 2, "charter": "c", "rev": "4.0.1"}
                    }}
                ]},
                {"id": "A.0", "stem": "A", "current": ["4.0.1", "4.0.1.1"], "eras": []},
                {"id": "P.0", "stem": "P", "current": ["4.0.1.1"], "eras": [
                    {"versions": ["4.0.1"], "charts": {"IN": {"label": "15", "notes": 3, "charter": "c", "rev": "4.0.1"}}}
                ]}
            ]
        });
        assert_eq!(
            source_at(&idx, "H", "AT", "4.0.1"),
            Source::Copy("4.0.1".into())
        );
        assert_eq!(source_at(&idx, "H", "EZ", "4.0.1"), Source::Current);
        assert_eq!(source_at(&idx, "H", "IN", "4.0.1"), Source::Missing);
        assert_eq!(source_at(&idx, "H", "AT", "4.0.1.1"), Source::Current);
        assert_eq!(source_at(&idx, "Z", "AT", "4.0.1"), Source::Current);
        assert_eq!(newer_than(&idx, 158).as_deref(), Some("4.0.1.1"));
        assert_eq!(newer_than(&idx, 160), None);
        let x = version_id(&idx, "4.0.1.1", 158);
        assert_eq!(x, "4.0.1.1-158");
        let era = |rev: Option<&str>| {
            let mut charts = Map::new();
            charts.insert(
                "EZ".into(),
                era_chart_from(&chart("9.0", 1), &Source::Current),
            );
            let at = rev.map_or(Source::Current, |r| Source::Copy(r.into()));
            charts.insert("AT".into(), era_chart_from(&chart("17.9", 2), &at));
            charts
        };
        insert(
            &mut idx,
            1,
            &x,
            158,
            &[
                (
                    "H.0".into(),
                    "H".into(),
                    Placement::Era(era(Some("4.0.1.1-158"))),
                ),
                ("A.0".into(), "A".into(), Placement::SameAsNext),
                ("P.0".into(), "P".into(), Placement::SameAsPrev),
                (
                    "N.0".into(),
                    "N".into(),
                    Placement::Era(era(Some("4.0.1.1-158"))),
                ),
            ],
        );
        assert_eq!(idx["versions"], json!(["4.0.1", "4.0.1.1-158", "4.0.1.1"]));
        assert_eq!(idx["codes"]["4.0.1.1-158"], 158);
        let h = &idx["songs"][0];
        assert_eq!(h["current"], json!(["4.0.1.1"]));
        assert_eq!(h["eras"][0]["versions"], json!(["4.0.1.1-158"]));
        assert_eq!(h["eras"][0]["charts"]["AT"]["rev"], "4.0.1.1-158");
        assert_eq!(h["eras"][0]["charts"]["EZ"].get("rev"), None);
        assert_eq!(h["eras"][1]["versions"], json!(["4.0.1"]));
        assert_eq!(
            idx["songs"][1]["current"],
            json!(["4.0.1", "4.0.1.1-158", "4.0.1.1"])
        );
        assert_eq!(
            idx["songs"][2]["eras"][0]["versions"],
            json!(["4.0.1", "4.0.1.1-158"])
        );
        assert_eq!(idx["songs"][3]["current"], json!(["4.0.1.1"]));
        assert_eq!(version_id(&idx, "4.0.1.1", 158), "4.0.1.1-158");
    }

    #[test]
    fn new_songs_are_recorded_from_their_first_version() {
        let mut idx = index();
        let new = [("N.z.0".to_string(), "N.z".to_string())];
        advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            159,
            &[],
            &new,
            &[],
            &|_, _, _| false,
        );
        assert_eq!(
            idx["songs"][2],
            json!({"id": "N.z.0", "stem": "N.z", "current": ["4.0.1.1"], "eras": []})
        );
    }

    #[test]
    fn a_song_new_to_the_index_starts_from_the_previous_version() {
        let mut idx = index();
        let changes = [change("ハテ.rNFrums.0", "ハテ.rNFrums", &["AT"])];
        advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            159,
            &[],
            &[],
            &changes,
            &|_, _, _| false,
        );
        let song = &idx["songs"][2];
        assert_eq!(song["id"], "ハテ.rNFrums.0");
        assert_eq!(song["current"], json!(["4.0.1.1"]));
        assert_eq!(song["eras"][0]["versions"], json!(["4.0.1"]));
        assert_eq!(song["eras"][0]["charts"]["AT"]["rev"], "4.0.1");
        assert_eq!(song["eras"][0]["charts"]["AT"]["notes"], 1600);
        assert_eq!(
            key_in("4.0.1", "phira/AT/ハテ.rNFrums-AT.pez"),
            "history/4.0.1/phira/AT/ハテ.rNFrums-AT.pez"
        );
    }

    #[test]
    fn a_chart_brought_back_is_current_again_and_its_empty_era_goes() {
        let mut idx = index();
        let changes = [change("B.y.0", "B.y", &["AT"])];
        assert_eq!(
            revs_of(&idx, "B.y", "AT"),
            vec![(0, "3.19.0.1".to_string())]
        );
        advance(
            &mut idx,
            "4.0.1",
            "4.0.1.1",
            159,
            &["B.y".into()],
            &[],
            &changes,
            &|stem, level, era| stem == "B.y" && level == "AT" && era == 0,
        );
        let b = &idx["songs"][1];
        let eras = b["eras"].as_array().unwrap();
        assert_eq!(eras.len(), 1);
        assert_eq!(eras[0]["versions"], json!(["4.0.1"]));
        assert_eq!(eras[0]["charts"]["AT"]["rev"], "4.0.1");
    }
}
