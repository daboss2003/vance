# Audio test fixtures

## `sine_440_48k_2s.m4a`

2-second 440 Hz sine, 48 kHz stereo, AAC in MP4 container.

```sh
ffmpeg \
  -f lavfi -i 'sine=frequency=440:duration=2:sample_rate=48000' \
  -ac 2 -c:a aac -b:a 128k \
  -y sine_440_48k_2s.m4a
```

~33 KB; safe to commit.
