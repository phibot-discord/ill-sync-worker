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
