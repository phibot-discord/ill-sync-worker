//! Whether two charts are one revision, by the rules the history import clustered the old APKs with

use serde_json::Value;

const TAIL: f64 = 10.0;
const TOL_T: f64 = 0.002;
const EVENTS: [(&str, f64); 4] = [
    ("speedEvents", 0.005),
    ("judgeLineDisappearEvents", 0.01),
    ("judgeLineMoveEvents", 0.005),
    ("judgeLineRotateEvents", 0.1),
];
const ALPHA: usize = 1;
const ROTATE: usize = 3;

#[derive(Clone, Debug)]
struct Seg {
    start: f64,
    end: f64,
    a: Vec<f64>,
    b: Vec<f64>,
}

#[derive(Debug)]
struct Note {
    side: u8,
    kind: i64,
    time: f64,
    rest: [f64; 3],
}

struct Line {
    notes: Vec<Note>,
    events: Vec<Vec<Seg>>,
}

pub struct Canon {
    lines: Vec<Line>,
    last: f64,
}

fn num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0.0)
}

fn dims(kind: usize, e: &Value, fmt: i64) -> (Vec<f64>, Vec<f64>) {
    match kind {
        0 => {
            let v = num(&e["value"]);
            (vec![v], vec![v])
        }
        2 if fmt == 1 => {
            let packed = |p: f64| vec![(p / 1000.0).floor() / 880.0, p.rem_euclid(1000.0) / 520.0];
            (packed(num(&e["start"])), packed(num(&e["end"])))
        }
        2 => (
            vec![num(&e["start"]), num(&e["start2"])],
            vec![num(&e["end"]), num(&e["end2"])],
        ),
        _ => (vec![num(&e["start"])], vec![num(&e["end"])]),
    }
}

fn lerp(a: &[f64], b: &[f64], f: f64) -> Vec<f64> {
    a.iter().zip(b).map(|(x, y)| x + (y - x) * f).collect()
}

fn segments(kind: usize, events: &Value, fmt: i64, unit: f64, horizon: f64) -> Vec<Seg> {
    let mut list: Vec<&Value> = events
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    list.sort_by(|x, y| {
        (num(&x["startTime"]), num(&x["endTime"]))
            .partial_cmp(&(num(&y["startTime"]), num(&y["endTime"])))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut segs = Vec::new();
    for e in list {
        let (mut start, mut end) = (num(&e["startTime"]) * unit, num(&e["endTime"]) * unit);
        if end <= start || end <= 0.0 || start >= horizon {
            continue;
        }
        let (mut a, mut b) = dims(kind, e, fmt);
        if start < 0.0 {
            a = lerp(&a, &b, -start / (end - start));
            start = 0.0;
        }
        if end > horizon {
            b = lerp(&a, &b, (horizon - start) / (end - start));
            end = horizon;
        }
        segs.push(Seg { start, end, a, b });
    }
    let tv = EVENTS[kind].1 / 4.0;
    let mut out: Vec<Seg> = Vec::new();
    for seg in segs {
        if let Some(prev) = out.last_mut() {
            let touches = (prev.end - seg.start).abs() < 1e-3
                && prev.b.iter().zip(&seg.a).all(|(x, y)| (x - y).abs() < tv);
            if touches {
                let slope = |s: &Seg| -> Vec<f64> {
                    s.a.iter()
                        .zip(&s.b)
                        .map(|(x, y)| (y - x) / (s.end - s.start))
                        .collect()
                };
                let same_slope = slope(prev)
                    .iter()
                    .zip(slope(&seg))
                    .all(|(x, y)| (x - y).abs() < 1e-3 * 1f64.max(x.abs()).max(y.abs()));
                if same_slope {
                    prev.end = seg.end;
                    prev.b = seg.b;
                    continue;
                }
            }
        }
        out.push(seg);
    }
    out
}

/// A chart's canonical form; None when it is not chart JSON
pub fn canon(bytes: &[u8]) -> Option<Canon> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let chart: Value = serde_json::from_slice(bytes).ok()?;
    let fmt = chart["formatVersion"].as_i64().unwrap_or(0);
    let mut last: f64 = 0.0;
    let mut raw = Vec::new();
    for line in chart["judgeLineList"].as_array()? {
        let bpm = num(&line["bpm"]);
        let unit = 1.875 / if bpm == 0.0 { 1.0 } else { bpm };
        let mut notes = Vec::new();
        for (side, field) in [(b'A', "notesAbove"), (b'B', "notesBelow")] {
            for n in line[field]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let time = num(&n["time"]) * unit;
                let hold = num(&n["holdTime"]) * unit;
                last = last.max(time + hold);
                notes.push(Note {
                    side,
                    kind: n["type"].as_i64().unwrap_or(0),
                    time,
                    rest: [num(&n["positionX"]), hold, num(&n["speed"])],
                });
            }
        }
        raw.push((notes, line, unit));
    }
    let horizon = last + TAIL;
    let mut lines = Vec::new();
    for (notes, line, unit) in raw {
        let events: Vec<Vec<Seg>> = EVENTS
            .iter()
            .enumerate()
            .map(|(k, (field, _))| segments(k, &line[*field], fmt, unit, horizon))
            .collect();
        let alpha = &events[ALPHA];
        let invisible = !alpha.is_empty()
            && alpha
                .iter()
                .all(|s| s.a.iter().chain(&s.b).all(|v| v.abs() <= 0.01));
        if notes.is_empty() && invisible {
            continue;
        }
        lines.push(Line { notes, events });
    }
    Some(Canon { lines, last })
}

/// Held flat before, after and between segments
fn eval(segs: &[Seg], t: f64) -> Vec<f64> {
    let i = segs.partition_point(|s| s.start <= t);
    if i == 0 {
        return segs[0].a.clone();
    }
    let s = &segs[i - 1];
    if t >= s.end {
        return s.b.clone();
    }
    lerp(&s.a, &s.b, (t - s.start) / (s.end - s.start))
}

fn same_events(a: &[Seg], b: &[Seg], tol: f64, horizon: f64, modulo: Option<f64>) -> bool {
    if a.is_empty() || b.is_empty() {
        return a.is_empty() && b.is_empty();
    }
    let mut points: Vec<f64> = a
        .iter()
        .chain(b)
        .flat_map(|s| [s.start, s.end])
        .chain([0.0, horizon])
        .collect();
    points.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    points.dedup();
    for w in points.windows(2) {
        let (p, q) = (w[0], w[1]);
        if q - p <= 2.0 * TOL_T {
            continue;
        }
        for t in [p + TOL_T, (p + q) / 2.0, q - TOL_T] {
            for (u, v) in eval(a, t).iter().zip(eval(b, t)) {
                let mut d = (u - v).abs();
                if let Some(m) = modulo {
                    d %= m;
                    d = d.min(m - d);
                }
                if d > tol {
                    return false;
                }
            }
        }
    }
    true
}

fn same_notes(a: &[Note], b: &[Note]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut pool: std::collections::HashMap<(u8, i64), Vec<&Note>> = Default::default();
    for n in b {
        pool.entry((n.side, n.kind)).or_default().push(n);
    }
    for list in pool.values_mut() {
        list.sort_by(|x, y| {
            x.time
                .partial_cmp(&y.time)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut used: std::collections::HashSet<(u8, i64, usize)> = Default::default();
    for x in a {
        let Some(list) = pool.get(&(x.side, x.kind)) else {
            return false;
        };
        let mut i = list.partition_point(|y| y.time < x.time - TOL_T);
        let mut found = false;
        while i < list.len() && list[i].time <= x.time + TOL_T {
            let close = x
                .rest
                .iter()
                .zip(&list[i].rest)
                .all(|(p, q)| (p - q).abs() <= TOL_T);
            if close && used.insert((x.side, x.kind, i)) {
                found = true;
                break;
            }
            i += 1;
        }
        if !found {
            return false;
        }
    }
    true
}

/// None when `a` and `b` are one revision, else what first differs
pub fn difference(a: &Canon, b: &Canon) -> Option<String> {
    if a.lines.len() != b.lines.len() {
        return Some(format!(
            "{} judge lines, was {}",
            b.lines.len(),
            a.lines.len()
        ));
    }
    let horizon = a.last.max(b.last) + TAIL;
    for (i, (la, lb)) in a.lines.iter().zip(&b.lines).enumerate() {
        if la.notes.len() != lb.notes.len() {
            return Some(format!(
                "line {i}: {} notes, was {}",
                lb.notes.len(),
                la.notes.len()
            ));
        }
        if !same_notes(&la.notes, &lb.notes) {
            return Some(format!("line {i}: notes"));
        }
    }
    for (i, (la, lb)) in a.lines.iter().zip(&b.lines).enumerate() {
        for (k, (field, tol)) in EVENTS.iter().enumerate() {
            let modulo = (k == ROTATE).then_some(360.0);
            if !same_events(&la.events[k], &lb.events[k], *tol, horizon, modulo) {
                return Some(format!("line {i}: {field}"));
            }
        }
    }
    None
}

/// None when the two chart files are one revision, else why not
pub fn compare(old: &[u8], new: &[u8]) -> Option<String> {
    match (canon(old), canon(new)) {
        (Some(a), Some(b)) => difference(&a, &b),
        (None, Some(_)) => Some("the old file is not chart JSON".into()),
        (_, None) => Some("the new file is not chart JSON".into()),
    }
}

/// Notes in a chart file, as the site counts them
pub fn note_count(bytes: &[u8]) -> Option<usize> {
    canon(bytes).map(|c| c.lines.iter().map(|l| l.notes.len()).sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chart(lines: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"formatVersion": 3, "offset": 0, "judgeLineList": lines}))
            .unwrap()
    }

    fn line(notes: Value, alpha_end: f64) -> Value {
        json!({
            "bpm": 120,
            "notesAbove": notes,
            "notesBelow": [],
            "speedEvents": [{"startTime": -999999, "endTime": 1000000000, "value": 1.0}],
            "judgeLineDisappearEvents": [
                {"startTime": 0, "endTime": 64, "start": 0, "end": alpha_end},
                {"startTime": 64, "endTime": 1000000000, "start": alpha_end, "end": alpha_end}
            ],
            "judgeLineMoveEvents": [{"startTime": -999999, "endTime": 1000000000, "start": 0.5, "end": 0.5, "start2": 0.3, "end2": 0.3}],
            "judgeLineRotateEvents": [{"startTime": -999999, "endTime": 1000000000, "start": 0, "end": 0}]
        })
    }

    fn tap(time: f64, x: f64) -> Value {
        json!({"type": 1, "time": time, "positionX": x, "holdTime": 0, "speed": 1, "floorPosition": 0})
    }

    #[test]
    fn float_noise_split_events_and_invisible_padding_are_the_same_revision() {
        let a = chart(json!([line(json!([tap(32.0, 1.0), tap(64.0, -2.0)]), 1.0)]));
        let mut split = line(json!([tap(64.0000001, -2.0), tap(32.0, 1.0000004)]), 1.0);
        split["judgeLineDisappearEvents"] = json!([
            {"startTime": 0, "endTime": 32, "start": 0, "end": 0.5},
            {"startTime": 32, "endTime": 64, "start": 0.5, "end": 1.0},
            {"startTime": 64, "endTime": 5000, "start": 1.0, "end": 1.0}
        ]);
        let padded = chart(json!([split, line(json!([]), 0.0)]));
        assert_eq!(compare(&a, &padded), None);
        assert_eq!(note_count(&padded), Some(2));
    }

    #[test]
    fn moved_notes_and_new_motion_are_a_new_revision() {
        let a = chart(json!([line(json!([tap(32.0, 1.0)]), 1.0)]));
        let moved = chart(json!([line(json!([tap(32.0, 1.5)]), 1.0)]));
        assert_eq!(compare(&a, &moved).as_deref(), Some("line 0: notes"));
        let fades = chart(json!([line(json!([tap(32.0, 1.0)]), 0.5)]));
        assert_eq!(
            compare(&a, &fades).as_deref(),
            Some("line 0: judgeLineDisappearEvents")
        );
        let more = chart(json!([
            line(json!([tap(32.0, 1.0)]), 1.0),
            line(json!([tap(8.0, 0.0)]), 1.0)
        ]));
        assert!(compare(&a, &more).is_some());
        assert!(compare(b"not json", &a).is_some());
    }

    #[test]
    fn a_full_turn_of_rotation_is_no_change() {
        let a = chart(json!([line(json!([tap(32.0, 1.0)]), 1.0)]));
        let mut turned = line(json!([tap(32.0, 1.0)]), 1.0);
        turned["judgeLineRotateEvents"] =
            json!([{"startTime": -999999, "endTime": 1000000000, "start": 360, "end": 360}]);
        assert_eq!(compare(&a, &chart(json!([turned]))), None);
    }
}
