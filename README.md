# phi-unpackd

Unpacks the Phigros APK and publishes what the web UI and bot need

## Build & deploy

```sh
cargo build --release --locked
install -m 755 target/release/phi-unpackd ~/mnt/nvme/phi/ill-sync~/phi-unpackd
systemctl --user restart phi-unpackd   # or: sudo systemctl restart phi-unpackd
```

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `UNPACK_SECRET` | (required) | Shared key for `x-phi-proxy-key`. |
| `UNPACK_LISTEN` | `127.0.0.1:8788` | HTTP listen address. |
| `UNPACK_ROOT` | `/mnt/nvme/phi/ill-sync` | `apk/`, `output/`, `state.json`, `published.json`, `.lock`. |
| `UNPACK_THREADS` | cores, at most 8 | Worker threads for decoding, jackets and packs. A full run peaked at about 700 MB RSS with 8 threads and 600 MB with 2 (the old serial build: about 600 MB). Output does not depend on the count. |
| `UNPACK_MIRROR` | unset | `rsync -a --delete` target for `output/`. |
| `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_KV_NAMESPACE_ID` | (required) | Checked before the APK is downloaded, the token with Cloudflare's verify endpoint. The token needs R2 and KV read and write. |
| `CLOUDFLARE_R2_BUCKET` | `phi-web-assets` | Target bucket. |
| `UNPACK_PUBLISH_RPS` | `3` | Upload requests per second across all workers (Cloudflare allows 1,200 API calls per 5 minutes per user, shared with the web UI and bot). `0` disables pacing. |
| `UNPACK_PUBLISH_ALL` | unset | `1` uploads every object and rewrites KV without checking what R2 and KV hold. |
| `UNPACK_HISTORY` | unset | `off` skips the history step, `report` lists what it would keep and stops the publish, `force` lifts its cap of 30 changed charts per build. |
| `UNPACK_APK_ARCHIVE` | `apk/archive` under the root | Where a replaced build's APK is kept, as `Phigros_<version>.apk` (or `-<code>` when that name is taken). |

## Older charts (`history/`)

Before a publish overwrites anything, each EZ–AT chart whose bytes differ from R2 is compared with the one R2 holds
(`chart_eq`: notes within 0.002 s, judge-line motion as functions of time, invisible padding lines ignored). A real
change, or a chart the build no longer has, is copied with its pack to `history/<previous version>/`, and
`history/index.json` gets an era for the song's charts until then. The index is written before the main upload, so a
retry finds the build done. New songs are recorded from their first build. A build whose name is already recorded
under another code is `<name>-<code>` (`codes` in the index holds each build's code). Nothing under `history/` is ever
deleted.

## Builds TapTap replaces

When a new build appears while the previous one never finished, the previous APK is kept and queued (`pending` in
`state.json`); queued builds are published first, oldest first, and one that fails three runs is left in the archive.
A build that never got through at all can be added later, between the recorded ones around its code, without touching
the main tree: `phi-unpackd --insert-build <apk> <code> <name> [--dry-run]`.

Other one-offs: `--export <apk> <dir> <code> <name>` (extract only), `--history-report <dir>` (what publishing it would
keep, against R2), `--same-chart <old> <new>…`.

## Jackets

Every song gets `original_ill/{ill,illLow,illBlur}/<id>.png` from its `Illustration.jpg`.

A few songs ship one jacket per difficulty instead (`Illustration_EZ.jpg` … `Illustration_AT.jpg`, with a
`.c9Locked` placeholder where the plain file would be). Those are decoded too, and each level gets the
same three sizes, derived like the plain ones:

```
original_ill/ill/<LV>/<id>.png        (illLow/, illBlur/ alike; LV is EZ, HD, IN or AT)
original_ill/levels.json              { "v": 1, "songs": { "<id>": ["EZ", "HD", "IN", "AT"] } }
```

The plain `<id>.png` of such a song is its IN jacket (else HD, EZ, AT), so anything that does not know
about levels still finds one. The web UI reads `levels.json` to decide when to ask for a level's jacket;
it is uploaded after the jackets it lists. `IllustrationLowRes_*` and `IllustrationBlur_*` in the APK are
not used.

## Charts and packs

```
chart/<folder>/<LV>.json         every chart, as the game stores it; folder is the track folder, `.0` included
                                 (`Stasis.Maozon.0`); LV is whatever level the track's `Chart_<LV>.json` names: EZ, HD,
                                 IN, AT, Legacy, or a level only an unlock plays (4.0.1: SP, EZ_Error, HD_Error, IN_Error)
phira/<LV>/<id>-<LV>.pez         a Phira pack of one chart with its song's music and low-res jacket
```

Every level found is extracted and packed; nothing lists them. A pack's `Level:` line takes the song table's
difficulty and charter for that level (Legacy included), or is `<LV> Lv.?` with no charter for a level the table does
not list (`_` shown as a space: `EZ Error Lv.?`). EZ to AT with no difficulty in the table are not packed, as the game
does not offer them.

Both trees go up to R2 under the same keys (the API path percent-encodes each segment, so a folder such as
`彼方へ名もなき海辺より.打打だいず&月乃.0` encodes alike in both). The packs go up with the jackets and music,
before KV, and a failure there fails the publish. The chart JSON (`application/json`, 1,064 files and about
2.7 GB) is a group of its own after KV, the last one: a failure is a warning, and since objects R2 already
holds are skipped, the next run sends only what is missing. There is no list of the charts; the web UI builds
`chart/<id>.0/<LV>.json` itself and should cope with one that is not there yet. R2 is listed once under
`chart/`, not once per folder. Keys a build no longer produces are pruned from R2 after a clean publish (see `prune`); `history/` keeps a copy of each chart first.

## Videos and Collection

The player's own data (`data.unity3d` in the APK) holds what the song bundles do not. It is read by
its byte layout, since the game ships no type tree for it, and every part is cross-checked (a video
object must be consumed to its last byte, the Collection's count must match the entries parsed, the scene's
text must carry the same keys in the same order as `level0`).

```
pv/video/<id>.mp4                the game's videos, byte for byte (H.264 + AAC), no re-encoding
pv/index.json                    { v: 1, clips: [{ id, name, group, scene, file, size, width, height, fps, frames, duration, audio }] }
collection/index.json            { v: 1, langs, chapters: [...], items: [{ key, chapter, entries: [{ sub, song, type, power, info, style, date }] }] }
collection/text/<lang>.json      { v: 1, lang, entries: { "<key>#<sub>": { title, author, body } } }   lang: zh-CN zh-TW en ja ko
collection-art/<id>.png          a Chapter 9 map illustration, lossless RGB at its own size (2048 wide)
collection-art/<id>.jpg          the same as a baseline JPEG (quality 85), what the web UI shows
collection-art/index.json        { v: 1, entries: [{ id, key, sub, layout, scene, art: { file, original, source, width, height } | null }] }
                                 layout: side banner text
```

* **Videos** are Unity `VideoClip`s: 17 in the player's scenes (`sharedassetsN.assets`, their MP4 loose in the APK
  as `sharedassetsN.resource`, several clips to one file) and the chapters' `c<N>.video.*` addressables (two in
  Chapter 9). `group` is the event or chapter folder of the scene the clip plays in (`SpasmodicEvent`,
  `Chapter9`), `scene` its path in the build.
* **Collection** is the SaturnOS scene's `//COLLECTION`: chapters (name, label, the range of song indexes
  whose rewards it holds, which event's clip unlocks it), entries (`level0`'s `GetCollectionControl`) and their
  text (the scene's streamed data). An item is filed by its first entry; its later clues unlock further on, so
  their `song` can be in a later chapter. `type`, `power` and `info` are the entry's unlock condition as the
  game stores it, not interpreted here. Many entries have no body text outside Chinese.
* **Collection art** is what the Chapter 9 maps (OS mode) show. A map button calls
  `OpenCollection{Horizontal,HorizontalWithImage,Vertical}` with the entry's collection id, `key` + (`sub` - 1)
  (`heimu1#2` is `heimu11`), and the method picks the panel: `text`, `banner` (image above the text) or `side`
  (image beside it). The image is the addressable `Assets/Chapter9/Arts/<id>.jpg`. Every entry a map opens is
  listed, with `scene` the map it is on and `art` its image (`file` the JPEG, `original` the PNG, `source` the
  catalog key).
