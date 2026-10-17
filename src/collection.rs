use crate::cursor::Cursor;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

pub const LANGS: [&str; 5] = ["zh-CN", "zh-TW", "en", "ja", "ko"];
pub const INDEX: &str = "collection/index.json";

pub fn text_path(lang: &str) -> String {
    format!("collection/text/{lang}.json")
}

pub type Text = [String; 5];

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub song: i32,
    pub kind: i32,
    pub power: i32,
    pub info: [String; 3],
    pub key: String,
    pub sub: i32,
    pub title: Text,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    pub date: String,
    pub author: Text,
    pub style: String,
    pub body: Text,
    pub properties: Text,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub id: String,
    pub name: Text,
    pub label: Option<Text>,
    pub from: i32,
    pub to: i32,
    pub exclude: Option<(i32, i32)>,
    pub unlock: Option<String>,
    pub keys: Vec<String>,
}

impl Chapter {
    pub fn kind(&self) -> &'static str {
        match (self.from, &self.label) {
            (20000.., _) => "side",
            (10000.., None) => "ex",
            (10000.., Some(_)) => "extra",
            _ => "main",
        }
    }

    fn holds(&self, index: i32) -> bool {
        self.from <= index
            && index <= self.to
            && !self.exclude.is_some_and(|(a, b)| a <= index && index <= b)
    }
}

pub struct Collection {
    pub items: Vec<Item>,
    pub bodies: Vec<Body>,
    pub chapters: Vec<Chapter>,
}

fn printable(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

fn text(c: &mut Cursor, max: usize) -> Result<String, String> {
    c.align(4);
    let len = c.i32()?;
    if len < 0 || len as usize > max {
        return Err(format!("string length {len}"));
    }
    let raw = c.bytes(len as usize)?;
    let s = std::str::from_utf8(raw).map_err(|e| e.to_string())?;
    if !printable(s) {
        return Err("control character in text".into());
    }
    c.align(4);
    Ok(s.to_string())
}

fn five(c: &mut Cursor, max: usize) -> Result<Text, String> {
    let mut out: Text = Default::default();
    for slot in out.iter_mut() {
        *slot = text(c, max)?;
    }
    Ok(out)
}

fn item_at(data: &[u8], pos: usize) -> Result<(Item, usize), String> {
    let mut c = Cursor::new(data);
    c.seek(pos)?;
    let song = c.i32()?;
    let kind = c.i32()?;
    let power = c.i32()?;
    let info = [text(&mut c, 200)?, text(&mut c, 200)?, text(&mut c, 200)?];
    let key = text(&mut c, 64)?;
    let sub = c.i32()?;
    let _code = text(&mut c, 64)?;
    let title = five(&mut c, 400)?;
    if key.is_empty()
        || !key.bytes().all(|b| b.is_ascii_graphic())
        || !(0..=100).contains(&sub)
        || !(-1..=20).contains(&kind)
        || title[0].is_empty()
    {
        return Err("not a collection entry".into());
    }
    Ok((
        Item {
            song,
            kind,
            power,
            info,
            key,
            sub,
            title,
        },
        c.pos(),
    ))
}

pub fn parse_items(level0: &[u8]) -> Result<Vec<Item>, String> {
    for start in (4..level0.len()).step_by(4) {
        let count = i32::from_le_bytes(level0[start - 4..start].try_into().unwrap());
        if !(50..=5000).contains(&count) {
            continue;
        }
        let mut items = Vec::with_capacity(count as usize);
        let mut pos = start;
        while items.len() < count as usize {
            let Ok((item, next)) = item_at(level0, pos) else {
                break;
            };
            items.push(item);
            pos = next;
        }
        if items.len() == count as usize {
            return Ok(items);
        }
    }
    Err("no collection list in level0".into())
}

// unused zeroed fields sit between records
const RECORD_GAP: usize = 256;

fn body_at(data: &[u8], pos: usize, item: &Item) -> Result<(Body, usize), String> {
    let mut c = Cursor::new(data);
    c.seek(pos)?;
    if text(&mut c, 64)? != item.key {
        return Err("another key".into());
    }
    if c.i32()? != item.sub || c.i32()? != item.song {
        return Err("another entry".into());
    }
    let _unused = c.i32()?;
    let _titles = five(&mut c, 400)?;
    let date = text(&mut c, 64)?;
    let _unused = c.i32()?;
    let author = five(&mut c, 200)?;
    let style = text(&mut c, 64)?;
    let _unused = c.i32()?;
    let body = five(&mut c, 200_000)?;
    let end = c.pos();
    let properties = text(&mut c, 64).and_then(|_code| five(&mut c, 2_000)).ok();
    let end = if properties.is_some() { c.pos() } else { end };
    Ok((
        Body {
            date,
            author,
            style,
            body,
            properties: properties.unwrap_or_default(),
        },
        end,
    ))
}

pub fn parse_bodies(data: &[u8], items: &[Item]) -> Result<Vec<Body>, String> {
    let first = items.first().ok_or("no collection entries")?;
    for start in (0..data.len()).step_by(4) {
        let Ok((body, mut pos)) = body_at(data, start, first) else {
            continue;
        };
        let mut out = vec![body];
        for item in &items[1..] {
            let found = (pos..(pos + RECORD_GAP).min(data.len()))
                .step_by(4)
                .find_map(|at| body_at(data, at, item).ok());
            let Some((body, next)) = found else {
                break;
            };
            out.push(body);
            pos = next;
        }
        if out.len() == items.len() {
            return Ok(out);
        }
    }
    Err("no collection text in the scene data".into())
}

pub fn clean(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace("\\n\n", "\n")
        .replace("\\n", "\n")
}

#[derive(Debug)]
enum Tok {
    S(String),
    I(i32),
}

fn tokens(data: &[u8], from: usize) -> Vec<(usize, Tok)> {
    let mut out = Vec::new();
    let mut pos = from & !3;
    while pos + 4 <= data.len() {
        let n = i32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        if (1..=2000).contains(&n) {
            let end = pos + 4 + n as usize;
            let padded = (end + 3) & !3;
            if let Some(raw) = data.get(pos + 4..end) {
                let zero_pad = data
                    .get(end..padded)
                    .is_some_and(|p| p.iter().all(|b| *b == 0));
                if let (Ok(s), true) = (std::str::from_utf8(raw), zero_pad) {
                    if printable(s) {
                        out.push((pos, Tok::S(s.to_string())));
                        pos = padded;
                        continue;
                    }
                }
            }
        }
        out.push((pos, Tok::I(n)));
        pos += 4;
    }
    out
}

fn five_slots(toks: &[(usize, Tok)], at: usize) -> Option<Text> {
    let mut out: Text = Default::default();
    let mut filled = 0;
    for (k, slot) in out.iter_mut().enumerate() {
        match &toks.get(at + k)?.1 {
            Tok::S(s) if !s.starts_with("Assets/") => {
                *slot = s.clone();
                filled += 1;
            }
            Tok::I(0) => {}
            _ => return None,
        }
    }
    (matches!(toks[at].1, Tok::S(_)) && filled >= 4).then_some(out)
}

fn chapter_at(toks: &[(usize, Tok)], at: usize) -> Option<(Chapter, usize)> {
    let name = five_slots(toks, at)?;
    let mut next = at + 5;
    let label_at = if matches!(toks.get(next), Some((_, Tok::I(0)))) {
        next + 1
    } else {
        next
    };
    let label = five_slots(toks, label_at);
    if label.is_some() {
        next = label_at + 5;
    }
    let mut ints = Vec::new();
    while let Some((_, Tok::I(v))) = toks.get(next) {
        ints.push(*v);
        next += 1;
        if ints.len() > 16 {
            return None;
        }
    }
    let Some((_, Tok::S(cover))) = toks.get(next) else {
        return None;
    };
    if !cover.starts_with("Assets/Tracks/") || ints.len() < 3 {
        return None;
    }
    let ex = ints.len() >= 5
        && ints[ints.len() - 5] >= 10000
        && ints[ints.len() - 4] >= ints[ints.len() - 5];
    let tail = &ints[ints.len() - if ex { 5 } else { 3 }..];
    let (from, to) = (tail[0], tail[1]);
    if from < 0 || to < from {
        return None;
    }
    Some((
        Chapter {
            id: String::new(),
            name,
            label,
            from,
            to,
            exclude: ex.then(|| (tail[3], tail[4])),
            unlock: None,
            keys: Vec::new(),
        },
        next + 1,
    ))
}

const TAIL_MAX: usize = 600;

fn chapters_of(scene: &[u8], keys: &HashSet<&str>) -> Result<Vec<Chapter>, String> {
    let marker = b"//COLLECTION";
    let at = scene
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or("the scene has no //COLLECTION")?;
    let toks = tokens(scene, at.saturating_sub(4));
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Some((mut chapter, mut j)) = chapter_at(&toks, i) else {
            i += 1;
            continue;
        };
        let stop = (j + TAIL_MAX).min(toks.len());
        while j < stop && chapter_at(&toks, j).is_none() {
            if let Tok::S(s) = &toks[j].1 {
                if s.contains("UnityEngine") {
                    break;
                }
                if let Some(event) = s.strip_prefix("unlockFlagOf") {
                    chapter.unlock = Some(event.to_string());
                } else if keys.contains(s.as_str()) && !chapter.keys.contains(s) {
                    chapter.keys.push(s.clone());
                }
            }
            j += 1;
        }
        if chapter.keys.len() < 3 {
            chapter.keys.clear();
        }
        chapters.push(chapter);
        i = j;
    }
    if chapters.is_empty() {
        return Err("no chapters in the SaturnOS scene".into());
    }
    name_chapters(&mut chapters)?;
    Ok(chapters)
}

fn name_chapters(chapters: &mut [Chapter]) -> Result<(), String> {
    let (mut main, mut side) = (0, 0);
    for chapter in chapters.iter_mut() {
        chapter.id = match chapter.kind() {
            "main" => {
                main += 1;
                format!("c{main}")
            }
            "side" => {
                side += 1;
                format!("s{side}")
            }
            kind => kind.to_string(),
        };
    }
    let mut seen = HashSet::new();
    for chapter in chapters.iter() {
        if !seen.insert(chapter.id.clone()) {
            return Err(format!("two chapters are called {}", chapter.id));
        }
    }
    Ok(())
}

// a chapter's own key list first, then event-flag unlocks (getType >= 5) go to the chapter that lists keys, then the range holding abs(getSong)
fn chapter_for(item: &Item, chapters: &[Chapter]) -> Option<usize> {
    if let Some(at) = chapters.iter().position(|c| c.keys.contains(&item.key)) {
        return Some(at);
    }
    if item.kind >= 5 {
        if let Some(at) = chapters.iter().position(|c| !c.keys.is_empty()) {
            return Some(at);
        }
    }
    let index = item.song.abs();
    chapters
        .iter()
        .enumerate()
        .filter(|(_, c)| c.holds(index))
        .min_by_key(|(_, c)| c.to - c.from)
        .map(|(at, _)| at)
}

pub fn parse(level0: &[u8], scene: &[u8], content: &[u8]) -> Result<Collection, String> {
    let items = parse_items(level0)?;
    let bodies = parse_bodies(content, &items)?;
    let keys: HashSet<&str> = items.iter().map(|i| i.key.as_str()).collect();
    let chapters = chapters_of(scene, &keys)?;
    Ok(Collection {
        items,
        bodies,
        chapters,
    })
}

pub fn by_lang(text: &Text) -> Value {
    let mut map = Map::new();
    for (lang, s) in LANGS.iter().zip(text) {
        map.insert((*lang).to_string(), Value::String(s.clone()));
    }
    Value::Object(map)
}

impl Collection {
    pub fn entries(&self) -> usize {
        self.items.len()
    }

    pub fn index(&self) -> Result<Vec<u8>, String> {
        let chapters: Vec<Value> = self
            .chapters
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "kind": c.kind(),
                    "name": by_lang(&c.name),
                    "label": c.label.as_ref().map(by_lang),
                    "from": c.from,
                    "to": c.to,
                    "unlock": c.unlock,
                })
            })
            .collect();
        let mut order: Vec<&str> = Vec::new();
        let mut groups: HashMap<&str, (Option<usize>, Vec<Value>)> = HashMap::new();
        for (at, item) in self.items.iter().enumerate() {
            let body = &self.bodies[at];
            // an item is filed by its first entry; its later clues unlock further on, in later chapters
            let group = groups.entry(&item.key).or_insert_with(|| {
                order.push(&item.key);
                (chapter_for(item, &self.chapters), Vec::new())
            });
            group.1.push(json!({
                "sub": item.sub,
                "song": item.song,
                "type": item.kind,
                "power": item.power,
                "info": item.info,
                "style": body.style,
                "date": body.date,
            }));
        }
        let items: Vec<Value> = order
            .into_iter()
            .map(|key| {
                let (chapter, entries) = &groups[key];
                json!({
                    "key": key,
                    "chapter": chapter.map(|c| self.chapters[c].id.clone()),
                    "entries": entries,
                })
            })
            .collect();
        serde_json::to_vec(&json!({
            "v": 1,
            "langs": LANGS,
            "chapters": chapters,
            "items": items,
        }))
        .map_err(|e| e.to_string())
    }

    pub fn texts(&self, lang: usize) -> Result<Vec<u8>, String> {
        let mut entries = Map::new();
        for (item, body) in self.items.iter().zip(&self.bodies) {
            let mut entry = json!({
                "title": item.title[lang],
                "author": body.author[lang],
                "body": clean(&body.body[lang]),
            });
            let properties = body.properties[lang].trim();
            if !properties.is_empty() {
                entry["properties"] = json!(properties);
            }
            entries.insert(format!("{}#{}", item.key, item.sub), entry);
        }
        serde_json::to_vec(&json!({ "v": 1, "lang": LANGS[lang], "entries": entries }))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(out: &mut Vec<u8>, text: &str) {
        out.extend((text.len() as i32).to_le_bytes());
        out.extend(text.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    fn five_text(out: &mut Vec<u8>, base: &str) {
        for lang in LANGS {
            s(out, &format!("{base} {lang}"));
        }
    }

    fn item_bytes(item: &Item) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [item.song, item.kind, item.power] {
            b.extend(v.to_le_bytes());
        }
        for i in &item.info {
            s(&mut b, i);
        }
        s(&mut b, &item.key);
        b.extend(item.sub.to_le_bytes());
        s(&mut b, "");
        for t in &item.title {
            s(&mut b, t);
        }
        b
    }

    fn body_bytes(item: &Item, date: &str) -> Vec<u8> {
        let mut b = Vec::new();
        s(&mut b, &item.key);
        for v in [item.sub, item.song, 0] {
            b.extend(v.to_le_bytes());
        }
        for t in &item.title {
            s(&mut b, t);
        }
        s(&mut b, date);
        b.extend(0i32.to_le_bytes());
        five_text(&mut b, "author");
        s(&mut b, "main");
        b.extend(0i32.to_le_bytes());
        for lang in LANGS {
            s(&mut b, &format!("line one\\n\nline two {lang}"));
        }
        b.extend([0u8; 24]);
        b
    }

    #[test]
    fn reads_the_properties_after_the_body() {
        let entry = item("Yrecord", 1, -1, 0);
        let mut b = body_bytes(&entry, "770/02/29");
        b.truncate(b.len() - 20);
        for lang in LANGS {
            s(&mut b, &format!("长度={lang};x=1"));
        }
        b.extend(s_key("next"));
        let (body, end) = body_at(&b, 0, &entry).unwrap();
        assert_eq!(body.properties[2], "长度=en;x=1");
        assert_eq!(&b[end..end + 4], &4i32.to_le_bytes());
        let (plain, _) = body_at(&body_bytes(&entry, "770/02/29"), 0, &entry).unwrap();
        assert!(plain.properties.iter().all(String::is_empty));
    }

    fn s_key(key: &str) -> Vec<u8> {
        let mut out = Vec::new();
        s(&mut out, key);
        out
    }

    fn item(key: &str, sub: i32, song: i32, kind: i32) -> Item {
        Item {
            song,
            kind,
            power: 30,
            info: Default::default(),
            key: key.into(),
            sub,
            title: LANGS.map(|l| format!("{key} {sub} {l}")),
        }
    }

    fn sample_items() -> Vec<Item> {
        let mut items = vec![item("alpa", 1, 0, -1), item("alpa", 2, 7, -1)];
        for n in 0..60 {
            items.push(item(&format!("filler{n}"), 1, 3, -1));
        }
        items.push(item("later", 1, -10135, 0));
        items.push(item("flagged", 1, 0, 7));
        items.push(item("nizhi", 1, 0, 5));
        items.push(item("themirage", 1, 0, 7));
        items.push(item("m9beginning", 1, 0, 5));
        items
    }

    fn level0(items: &[Item]) -> Vec<u8> {
        let mut data = vec![0xAB; 40];
        data.extend((items.len() as i32).to_le_bytes());
        for i in items {
            data.extend(item_bytes(i));
        }
        data.extend([1u8; 64]);
        data
    }

    #[test]
    fn finds_the_item_list_by_its_count() {
        let items = sample_items();
        assert_eq!(parse_items(&level0(&items)).unwrap(), items);
        assert!(parse_items(&vec![0u8; 400]).is_err());
        let mut short = level0(&items);
        short.truncate(short.len() - 3000);
        assert!(parse_items(&short).is_err());
    }

    #[test]
    fn reads_the_text_of_every_entry_across_the_unused_fields() {
        let items = sample_items();
        let mut data = vec![0x55; 28];
        for (n, i) in items.iter().enumerate() {
            data.extend(body_bytes(i, &format!("770/0{}/01", n % 9)));
        }
        let bodies = parse_bodies(&data, &items).unwrap();
        assert_eq!(bodies.len(), items.len());
        assert_eq!(bodies[0].date, "770/00/01");
        assert_eq!(bodies[1].author[2], "author en");
        assert_eq!(bodies[1].style, "main");
        let mut other = items.clone();
        other[3].key = "missing".into();
        assert!(parse_bodies(&data, &other).is_err());
        assert_eq!(clean("a\\n\nb\\nc\r\nd"), "a\nb\nc\nd");
    }

    fn scene() -> Vec<u8> {
        let mut b = vec![9u8; 12];
        s(&mut b, "//COLLECTION");
        for v in [1355i32, 0, 1036] {
            b.extend(v.to_le_bytes());
        }
        s(&mut b, "曲绘");
        b.extend(0i32.to_le_bytes());
        s(&mut b, "Illustration");
        for _ in 0..3 {
            b.extend(0i32.to_le_bytes());
        }
        s(&mut b, "Assets/Tracks/#ChapterCover/IllustrationsCover.jpg");
        b.extend(0i32.to_le_bytes());
        five_text(&mut b, "ex name");
        for _ in 0..6 {
            b.extend(0i32.to_le_bytes());
        }
        for v in [10000i32, 19999, 1, 10133, 10137] {
            b.extend(v.to_le_bytes());
        }
        s(&mut b, "Assets/Tracks/#ChapterCover/Single.jpg");
        for (n, (from, to)) in [(0i32, 5i32), (6, 10), (46, 59)].into_iter().enumerate() {
            b.extend(0i32.to_le_bytes());
            five_text(&mut b, &format!("name{n}"));
            b.extend(0i32.to_le_bytes());
            five_text(&mut b, &format!("Chapter {n}"));
            for v in [from, to, 0] {
                b.extend(v.to_le_bytes());
            }
            s(
                &mut b,
                &format!("Assets/Tracks/#ChapterCover/MainStory{n}.jpg"),
            );
            if n == 1 {
                s(&mut b, "unlockFlagOfSpasmodic");
                s(&mut b, "Assets/Tracks/#ChapterCover/MainStory1Locked.png");
            }
            if n == 2 {
                for key in ["nizhi", "themirage", "m9beginning"] {
                    b.extend(2i32.to_le_bytes());
                    s(&mut b, key);
                }
            }
        }
        b.extend(0i32.to_le_bytes());
        five_text(&mut b, "side name");
        b.extend(0i32.to_le_bytes());
        five_text(&mut b, "Side story 1");
        for v in [20000i32, 20005, 0] {
            b.extend(v.to_le_bytes());
        }
        s(&mut b, "Assets/Tracks/#ChapterCover/SideStory1.jpg");
        b.extend(0i32.to_le_bytes());
        five_text(&mut b, "extra name");
        b.extend(0i32.to_le_bytes());
        for lang in &LANGS[..4] {
            s(&mut b, &format!("Extra Story {lang}"));
        }
        b.extend(0i32.to_le_bytes());
        for v in [10133i32, 10137, 0] {
            b.extend(v.to_le_bytes());
        }
        s(&mut b, "Assets/Tracks/SATELLITE.x.0/Illustration.jpg");
        s(&mut b, "UnityEngine.Object, UnityEngine");
        b
    }

    #[test]
    fn reads_the_chapters_and_files_every_item() {
        let items = sample_items();
        let keys: HashSet<&str> = items.iter().map(|i| i.key.as_str()).collect();
        let chapters = chapters_of(&scene(), &keys).unwrap();
        let ids: Vec<&str> = chapters.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["ex", "c1", "c2", "c3", "s1", "extra"]);
        assert_eq!(chapters[0].exclude, Some((10133, 10137)));
        assert_eq!(chapters[0].kind(), "ex");
        assert_eq!(chapters[1].kind(), "main");
        assert_eq!((chapters[1].from, chapters[1].to), (0, 5));
        assert_eq!(chapters[2].unlock.as_deref(), Some("Spasmodic"));
        assert_eq!(chapters[3].keys, ["nizhi", "themirage", "m9beginning"]);
        assert_eq!(chapters[4].kind(), "side");
        let extra = &chapters[5];
        assert_eq!(extra.label.as_ref().map(|l| l[4].as_str()), Some(""));
        assert_eq!(extra.name[2], "extra name en");
        let at = |key: &str, sub: i32| {
            let item = items.iter().find(|i| i.key == key && i.sub == sub).unwrap();
            chapter_for(item, &chapters).map(|c| chapters[c].id.as_str())
        };
        assert_eq!(at("alpa", 1), Some("c1"));
        assert_eq!(at("alpa", 2), Some("c2"));
        assert_eq!(at("filler3", 1), Some("c1"));
        assert_eq!(at("later", 1), Some("extra"));
        assert_eq!(at("flagged", 1), Some("c3"));
        assert_eq!(at("nizhi", 1), Some("c3"));
    }

    #[test]
    fn writes_an_index_and_a_text_file_per_language() {
        let items = sample_items();
        let mut content = Vec::new();
        for i in &items {
            content.extend(body_bytes(i, "770/02/29"));
        }
        let collection = parse(&level0(&items), &scene(), &content).unwrap();
        assert_eq!(collection.entries(), items.len());
        let index: Value = serde_json::from_slice(&collection.index().unwrap()).unwrap();
        assert_eq!(index["v"], 1);
        let listed = index["items"].as_array().unwrap();
        assert_eq!(listed[0]["key"], "alpa");
        assert_eq!(listed[0]["chapter"], "c1");
        assert_eq!(listed[0]["entries"].as_array().unwrap().len(), 2);
        assert_eq!(listed[0]["entries"][1]["song"], 7);
        assert_eq!(listed.len(), items.len() - 1);
        assert_eq!(index["chapters"][1]["name"]["en"], "name0 en");
        assert_eq!(index["chapters"][2]["unlock"], "Spasmodic");
        let en: Value = serde_json::from_slice(&collection.texts(2).unwrap()).unwrap();
        assert_eq!(en["lang"], "en");
        assert_eq!(en["entries"]["alpa#2"]["title"], "alpa 2 en");
        assert_eq!(en["entries"]["alpa#2"]["author"], "author en");
        assert_eq!(en["entries"]["alpa#1"]["body"], "line one\nline two en");
    }

    // The 4.0.1 APK's own data (see /tmp/phi-fixtures, written by the fixture script)
    #[test]
    fn reads_the_collection_of_the_401_apk() {
        let read = |name: &str| std::fs::read(format!("/tmp/phi-fixtures/{name}")).ok();
        let (Some(level0), Some(scene), Some(content)) = (
            read("level0"),
            read("level22"),
            read("sharedassets22.assets"),
        ) else {
            return;
        };
        let collection = parse(&level0, &scene, &content).unwrap();
        assert_eq!(collection.entries(), 503);
        let ids: Vec<&str> = collection.chapters.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "ex", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "s1", "s2", "s3", "s4",
                "extra"
            ]
        );
        let named = |id: &str| {
            collection
                .chapters
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name[2].clone())
        };
        assert_eq!(named("c1").as_deref(), Some("Glaciaxion"));
        assert_eq!(named("c9").as_deref(), Some("Solivault"));
        assert_eq!(named("extra").as_deref(), Some("BassAreUs"));
        let filed = |item: &Item| {
            chapter_for(item, &collection.chapters).map(|c| collection.chapters[c].id.as_str())
        };
        let alpa = collection.items.iter().find(|i| i.key == "alpa").unwrap();
        assert_eq!(filed(alpa), Some("c1"));
        let mut keys = HashSet::new();
        assert!(collection
            .items
            .iter()
            .filter(|i| keys.insert(&i.key))
            .all(|i| filed(i).is_some()));
        let alpa = collection
            .items
            .iter()
            .position(|i| i.key == "alpa")
            .unwrap();
        assert_eq!(collection.items[alpa].title[0], "录音笔Alpa");
        assert!(collection.bodies[alpa].body[2].starts_with("A pure white recording pen"));
        assert_eq!(collection.bodies[alpa].date, "770/02/29");
        let with_properties = collection
            .bodies
            .iter()
            .filter(|b| !b.properties[0].is_empty())
            .count();
        assert_eq!(with_properties, 61);
        let shanfeng = collection
            .items
            .iter()
            .position(|i| i.key == "Yshanfeng" && i.sub == 1)
            .unwrap();
        assert_eq!(
            collection.bodies[shanfeng].properties,
            [
                "长度=10min32s",
                "長度=10min32s",
                "LENGTH=10'32\"",
                "時間=10min32s",
                "길이 = 10분 32초"
            ]
        );
        let ko: Value = serde_json::from_slice(&collection.texts(4).unwrap()).unwrap();
        assert_eq!(
            ko["entries"]["Yshanfeng#1"]["properties"],
            "길이 = 10분 32초"
        );
        assert!(ko["entries"]["alpa#1"].get("properties").is_none());
    }
}
