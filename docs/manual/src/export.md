# Export & Publish

## Export

Choose **File ▸ Export** and pick a preset. Vance renders the timeline (video +
the full audio mix, which matches playback exactly) through an encoder sidecar.

**Video codecs**

| Preset | Use |
| --- | --- |
| ProRes 422 / 422 HQ / 4444 | Mastering / intermediate (4444 keeps alpha). |
| DNxHR LB / SQ / HQ | Broadcast intermediate. |
| H.264 | Delivery; hardware-accelerated where available. |
| HEVC (H.265) | Efficient delivery. |
| AV1 | Modern, royalty-free delivery. |

Synthetic clips (titles, motion graphics, Fusion comps) export at the same
animation phase you preview, including on trimmed/split clips.

## Publish to YouTube

Vance can upload straight to **YouTube** from the Publish panel:

1. Register an OAuth app in the YouTube (Google Cloud) developer console and
   add the redirect URI shown in the panel
   (`http://127.0.0.1:8723/callback`).
2. Paste your **Client ID** (and secret, if your app requires one) and click
   **Connect**. Your browser opens a consent page; approve it.
3. Fill in title, description, tags, and privacy, choose your rendered file,
   and **Publish**.

Vance uses OAuth 2.0 with PKCE and a local loopback listener to capture the
callback — your credentials stay on your machine. Large files upload in
resumable chunks and retry across a transient network drop.

> Vimeo and TikTok connect via OAuth; their uploaders are on the roadmap.
> Vance uses **your** platform app credentials — it cannot embed platform keys.
