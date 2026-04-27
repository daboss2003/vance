# Test fixtures

Tiny generated media for `vance-decoder` integration tests. Regenerate any
of these with the FFmpeg commands below if they go missing.

## `testsrc_320x240_5s.mp4`

5-second 320×240 H.264 (yuv420p) test pattern at 30 fps.

```sh
ffmpeg \
  -f lavfi -i testsrc=duration=5:size=320x240:rate=30 \
  -c:v libx264 -preset ultrafast -crf 30 -pix_fmt yuv420p -g 15 \
  -y testsrc_320x240_5s.mp4
```

~93 KB; safe to commit. Bigger / less synthetic fixtures live in a separate
LFS repo (PLAN.md §9.3).
