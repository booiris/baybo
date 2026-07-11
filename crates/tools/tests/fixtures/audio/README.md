# Audio duration-probe fixtures

Synthetic 2-second 440 Hz sine tones — machine-generated test signals, no
recorded material, nothing copyrightable. Regenerate with ffmpeg:

```bash
ffmpeg -f lavfi -i "sine=frequency=440:duration=2" \
    -c:a libmp3lame -q:a 5 -write_xing 0 vbr-noxing-2s.mp3
ffmpeg -f lavfi -i "sine=frequency=440:duration=2" \
    -c:a libopus -b:a 64k opus-2s.ogg
ffmpeg -f lavfi -i "sine=frequency=440:duration=2" \
    -ac 2 -c:a vorbis -strict -2 vorbis-2s.ogg
```

Each pins a duration-probe failure class that shipped wrong numbers in the
field (see `attach_file.rs::duration_probe_survives_vbr_mp3_and_opus_in_ogg`):

- `vbr-noxing-2s.mp3` — VBR without a Xing header: first-frame bitrate math
  estimated ~6× off; only a frame walk counts honestly.
- `opus-2s.ogg` — an Opus stream inside `.ogg`: lofty's extension guess
  assumes Vorbis and errors; the content sniff reads it exactly.
- `vorbis-2s.ogg` — the healthy control.
