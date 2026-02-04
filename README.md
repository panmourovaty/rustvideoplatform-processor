# Video Transcoding Configuration

The processor supports three hardware-accelerated video encoders:

- **NVENC** (NVIDIA) - Best performance on NVIDIA RTX 30/40 series GPUs
- **QSV** (Intel Quick Sync) - Available on Intel Arc/11th Gen+ integrated graphics
- **VAAPI** - Linux API for Intel/AMD GPUs

## Configuration File

Video encoding settings are defined in `config.json` under the `video` object:

```json
{
    "encoder": "qsv",
    "max_resolution_steps": 4,
    "min_dimension": 240,
    "fps_cap": 120.0
}
```

## Encoder Selection

Set `encoder` to one of: `nvenc`, `qsv`, or `vaapi`

Each encoder has its own specific settings for optimal performance.

---

## NVENC (NVIDIA) Settings

```json
"nvenc": {
    "codec": "av1_nvenc",
    "preset": "p7",
    "tier": "high",
    "rc": "cq",
    "cq": 26,
    "lookahead": 32,
    "temporal_aq": true
}
```

### Parameters

| Parameter | Description | Values |
|-----------|-------------|--------|
| `codec` | Video codec | `av1_nvenc`, `h264_nvenc`, `hevc_nvenc` |
| `preset` | Quality preset | `p1` (fastest) to `p7` (best quality) |
| `tier` | Encoder tier | `high`, `main`, `low` |
| `rc` | Rate control mode | `cq`, `vbr`, `cbr`, `cbr_ld_hq` |
| `cq` | Quality level | 0-51 (lower = better quality) |
| `lookahead` | Lookahead frames (optional) | 0-32 (NVENC specific) |
| `temporal_aq` | Temporal AQ (optional) | `true`/`false` |

---

## QSV (Intel Quick Sync) Settings

```json
"qsv": {
    "codec": "av1_qsv",
    "preset": "veryslow",
    "global_quality": 28,
    "lookahead": 40,
    "look_ahead_depth": 100
}
```

### Parameters

| Parameter | Description | Values |
|-----------|-------------|--------|
| `codec` | Video codec | `av1_qsv`, `h264_qsv`, `hevc_qsv` |
| `preset` | Quality preset | `veryfast`, `faster`, `fast`, `medium`, `slow`, `slower`, `veryslow` |
| `global_quality` | Global quality level | 0-51 (lower = better quality) |
| `lookahead` | Enable lookahead | **0** = off, **1+** = on |
| `look_ahead_depth` | Analysis depth (optional) | Depends on hardware (10-200 typical) |

**Note:** QSV's `lookahead` and `look_ahead_depth` features provide significantly better quality by analyzing future frames before encoding.

---

## VAAPI (Linux) Settings

```json
"vaapi": {
    "codec": "av1_vaapi",
    "quality": 51,
    "compression_ratio": 5
}
```

### Parameters

| Parameter | Description | Values |
|-----------|-------------|--------|
| `codec` | Video codec | `av1_vaapi`, `h264_vaapi`, `hevc_vaapi` |
| `quality` | Quality level | 0-51 (higher = more compression) |
| `compression_ratio` | Reserved for future use | Currently unused |

---

## Common Settings

### Quality Steps

Define the ladder of resolutions to generate:

```json
"quality_steps": [
    {
        "label": "original",
        "scale_divisor": 1,
        "audio_bitrate_divisor": 1
    },
    {
        "label": "half_resolution",
        "scale_divisor": 2,
        "audio_bitrate_divisor": 2
    }
]
```

Each step halves the resolution by dividing dimensions by `scale_divisor`.

### Audio Settings

```json
"audio_bitrate_base": 256,
"threshold_2k_pixels": 3686400,
"audio_bitrate_2k_bonus": 100
```

- Base audio bitrate is 256 kbps
- For 2K+ content (3686400+ pixels), add 100 kbps bonus

### Performance Settings

```json
"fps_cap": 120.0,
"min_dimension": 240
```

- `fps_cap`: Limits output framerate (default: 120)
- `min_dimension`: Minimum width/height (default: 240)

---

## Hardware Detection

**Check your hardware support:**

### NVIDIA (NVENC)
```bash
ffmpeg -encoders 2>/dev/null | grep nvenc
nvidia-smi
```

### Intel (QSV)
```bash
ffmpeg -encoders 2>/dev/null | grep qsv
vainfo | grep -E "AV1|H264|HEVC"
```

### Linux (VAAPI)
```bash
vainfo
ls /dev/dri/
```

---

## Recommended Configurations

### NVENC (NVIDIA RTX 40 series)
```json
{
    "encoder": "nvenc",
    "nvenc": {
        "codec": "av1_nvenc",
        "preset": "p7",
        "rc": "cq",
        "cq": 24,
        "lookahead": 32
    }
}
```

### QSV (Intel Arc)
```json
{
    "encoder": "qsv",
    "qsv": {
        "codec": "av1_qsv",
        "preset": "slower",
        "global_quality": 26,
        "lookahead": 40,
        "look_ahead_depth": 100
    }
}
```

### VAAPI (Intel UHD)
```json
{
    "encoder": "vaapi",
    "vaapi": {
        "codec": "av1_vaapi",
        "quality": 40
    }
}
```

---

## Troubleshooting

### FFmpeg fails with encoder not found
- Verify hardware support using commands above
- Install appropriate drivers (nvidia-driver, intel-media-driver, mesa-va)

### Quality is poor
- Lower the `cq`/`global_quality`/`quality` value
- Use a slower preset
- Enable lookahead (NVENC/QSV)

### Encoding is too slow
- Use faster preset
- Reduce lookahead depth
- Disable quality steps (reduce `max_resolution_steps`)

### Files are too large
- Increase quality value (for VAAPI)
- Decrease `cq` value closer to 51 (for NVENC)
- Enable stricter rate control modes