# rustvideoplatform-processor

Media processing service that handles video transcoding, audio extraction, picture generation, subtitle extraction, and DASH manifest creation. Supports hardware-accelerated encoding via NVIDIA NVENC, Intel Quick Sync (QSV), and Linux VAAPI.

## Requirements

- FFmpeg with codec support (AV1, H.264, HEVC, Opus, SVT-AV1, libaom-av1)
- PostgreSQL database
- Hardware acceleration drivers (depending on encoder):
  - **NVENC**: NVIDIA GPU + CUDA drivers
  - **QSV**: Intel Arc / 11th Gen+ with `intel-media-driver` and `onevpl-intel-gpu`
  - **VAAPI**: `libva`, `mesa-va-gallium`, or `intel-media-driver`
- Optional: [Whisper.cpp](https://github.com/ggerganov/whisper.cpp) server for automatic transcription
- Optional: [llama.cpp](https://github.com/ggerganov/llama.cpp) server with [TranslateGemma](https://huggingface.co/google/translate-gemma) for subtitle translation

## Deployment

### Docker

Build and run using the provided Dockerfile (Alpine Linux):

```bash
docker build -t rustvideoplatform-processor .
docker run -v ./config.json:/config.json \
           -v ./upload:/upload \
           --device /dev/dri:/dev/dri \
           rustvideoplatform-processor
```

For NVIDIA GPU support, use the NVIDIA Container Toolkit:

```bash
docker run --gpus all \
           -v ./config.json:/config.json \
           -v ./upload:/upload \
           rustvideoplatform-processor
```

The container expects:
- `config.json` in the working directory
- `upload/` directory for input files and processed output
- Access to GPU devices (`/dev/dri` for VAAPI/QSV, CUDA for NVENC)

### Building from source

```bash
cargo build --release
```

The binary reads `config.json` from the current working directory and requires FFmpeg libraries at runtime.

### Database

The processor connects to PostgreSQL and polls the `media_concepts` table for unprocessed entries:

```sql
SELECT id, type FROM media_concepts WHERE processed = false;
```

After processing, it sets `processed = true`. Upload files are read from `upload/{id}` and output goes to `upload/{id}_processing/`.

## Processing pipeline

The processor detects the media type of each file and routes it accordingly:

| Type | Detection | Output |
|------|-----------|--------|
| **Video** | Multiple frames + audio | WebM transcodes at multiple quality levels, DASH manifest, thumbnails, preview sprites, subtitles |
| **Audio** | Audio stream without real video | Opus transcode, embedded cover art extraction, subtitles/lyrics |
| **Picture** | Single frame, no audio | AVIF + JPEG thumbnails at configured resolutions |

Subtitle extraction tries embedded streams first, then falls back to Whisper transcription if no subtitles are found. Long audio is split at silence boundaries (targeting 10-minute chunks, up to 15 minutes) so that Whisper never receives a chunk that cuts through speech.

### Subtitle translation

When `translation.languages` is configured, the processor ensures that subtitles in all specified languages are always available for every video and audio file. The translation pipeline works as follows:

1. **Embedded subtitles**: Language tags from the container metadata (ISO 639-2/B codes like `eng`, `cze`, or full names like `English`) are normalized to ISO 639-1 codes and used as filenames (`en.vtt`, `cs.vtt`).

2. **Whisper transcription**: When no embedded subtitles exist, the audio language is auto-detected via Whisper's `verbose_json` response. The transcription is saved as `AI_<detected_lang>.vtt` (e.g., `AI_en.vtt`) instead of the generic `AI_transcription.vtt`.

3. **Translation**: Any configured language that is not already covered by an existing subtitle track is translated using [TranslateGemma](https://huggingface.co/google/translate-gemma) running on a llama.cpp server. Translated subtitles are named `AI_<target_lang>.vtt`. English is preferred as the source language when multiple tracks are available.

For example, with `"languages": ["en", "cs"]`:
- A video with English audio and no subtitles produces `AI_en.vtt` (Whisper) and `AI_cs.vtt` (translated).
- A video with embedded Czech subtitles produces `cs.vtt` (extracted) and `AI_en.vtt` (translated).
- A video with both English and Czech embedded subtitles produces `en.vtt` and `cs.vtt` — no translation needed.

If `translation.languages` is empty or omitted, the old behavior is preserved (`AI_transcription` naming, no translation, no llama.cpp dependency).

## Configuration

All settings are defined in `config.json`. See `config.json.example` for a complete reference. Every section except `dbconnection` and `video` has sensible defaults and can be omitted.

### Top-level structure

```json
{
    "dbconnection": "postgresql://user:password@host:5432/db",
    "whisper": { },
    "translation": { },
    "audio": { },
    "picture": { },
    "video": { }
}
```

### `dbconnection` (required)

PostgreSQL connection string.

### `whisper`

Whisper.cpp API integration for automatic transcription when no embedded subtitles are found. Audio is extracted at 16 kHz mono PCM for optimal Whisper input. The API timeout is calculated as 2x the chunk duration.

Long audio files are automatically split into chunks at silence boundaries to avoid cutting through speech. The splitter uses FFmpeg's `silencedetect` filter to find natural pauses, then picks split points closest to the target chunk duration. If no silence is found near the target, the chunk extends up to the maximum duration before forcing a split.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `url` | `http://whisper:8080/inference` | Whisper.cpp server endpoint |
| `model` | `whisper-1` | Model name sent to the API |
| `response_format` | `vtt` | Subtitle format (`vtt`) |
| `output_label` | `AI_transcription` | Filename label for generated subtitles |
| `target_chunk_secs` | `600` (10 min) | Target chunk duration in seconds. Splits aim for a silence boundary near this point |
| `max_chunk_secs` | `900` (15 min) | Maximum chunk duration in seconds. If no silence is found by the target, extend up to this limit |
| `silence_noise_db` | `-30` | Noise floor threshold in dB for silence detection (lower = stricter) |
| `silence_min_duration` | `0.5` | Minimum silence gap in seconds to be considered a valid split candidate |

### `translation`

Subtitle translation via TranslateGemma on llama.cpp. When `languages` is set, the processor ensures subtitles exist in every listed language for all media. Existing subtitle language tags are normalized to ISO 639-1 codes, Whisper output is labeled with the detected language, and any missing languages are translated automatically.

Requires a [llama.cpp](https://github.com/ggerganov/llama.cpp) server running a TranslateGemma model. Start the server with:

```bash
llama-server -m translate-gemma-2b.gguf --port 8081
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `languages` | `[]` (disabled) | Target languages as ISO 639-1 codes (e.g., `["en", "cs"]`). When empty, translation is disabled and legacy naming is used |
| `llama_url` | `http://llama:8081` | llama.cpp server base URL (the `/completion` endpoint is appended automatically) |
| `source_language` | `en` | Preferred source language for translation. When multiple subtitle tracks exist, this language is chosen as the translation source |
| `timeout_secs` | `120` | HTTP timeout in seconds for each individual cue translation request |

Supported language code normalization:
- **ISO 639-1** (2-letter): `en`, `cs`, `de`, `fr`, etc. — used as-is
- **ISO 639-2/B** (3-letter): `eng` → `en`, `cze` → `cs`, `ger` → `de`, `fre` → `fr`, etc.
- **ISO 639-2/T** (3-letter): `ces` → `cs`, `deu` → `de`, `fra` → `fr`, etc.
- **Full names**: `English` → `en`, `Czech` → `cs`, `German` → `de`, etc.

### `audio`

Standalone audio transcoding settings (used when processing audio files).

| Parameter | Default | Description |
|-----------|---------|-------------|
| `codec` | `libopus` | Output audio codec |
| `lossless_bitrate` | `300k` | Bitrate for lossless sources (FLAC, WAV) |
| `lossy_bitrate` | `256k` | Bitrate for lossy sources |
| `vbr` | `on` | Variable bitrate mode (`on`, `constrained`) |
| `application` | `audio` | Opus application type (`audio`, `voip`, `lowdelay`) |
| `output_format` | `ogg` | Output container format |
| `lossless_codecs` | `["flac", "wav", "pcm_s16le"]` | Source codecs treated as lossless |

### `picture`

Still image encoding settings for pictures, thumbnails, and audio cover art.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `crf` | `26` | CRF for full-size AVIF output (lower = better quality) |
| `thumbnail_crf` | `28` | CRF for thumbnail AVIF |
| `jpg_quality` | `25` | JPEG quality level for fallback thumbnails |
| `thumbnail_width` | `1280` | Maximum thumbnail width |
| `thumbnail_height` | `720` | Maximum thumbnail height |
| `cover_crf` | `26` | CRF for audio cover art AVIF |
| `cover_thumbnail_crf` | `30` | CRF for audio cover art thumbnail |

### `video` (required)

Video transcoding configuration. The `encoder`, `quality_steps`, and related fields are required.

| Parameter | Description |
|-----------|-------------|
| `encoder` | Hardware encoder: `nvenc`, `qsv`, or `vaapi` |
| `max_resolution_steps` | Maximum number of quality ladder steps to generate |
| `min_dimension` | Minimum width or height in pixels |
| `fps_cap` | Maximum output framerate |
| `audio_bitrate_base` | Base audio bitrate in kbps |
| `threshold_2k_pixels` | Pixel count threshold for 2K bonus (width * height) |
| `audio_bitrate_2k_bonus` | Extra kbps added for content above 2K threshold |
| `quality_steps` | Array of resolution ladder steps (see below) |
| `filters` | FFmpeg video filter chain (e.g. `unsharp=3:3:1.0:3:3:0.0,format=p010le`) |

#### `video.quality_steps`

Each step defines a resolution level in the output:

```json
{
    "label": "half_resolution",
    "scale_divisor": 2,
    "audio_bitrate_divisor": 2
}
```

- `label`: Identifier for this quality level
- `scale_divisor`: Divide source dimensions by this value
- `audio_bitrate_divisor`: Divide base audio bitrate by this value

#### `video.nvenc` (NVIDIA)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `codec` | — | Video codec (`av1_nvenc`, `h264_nvenc`, `hevc_nvenc`) |
| `preset` | — | Quality preset (`p1` fastest to `p7` best quality) |
| `tier` | — | Encoder tier (`high`, `main`, `low`) |
| `rc` | — | Rate control (`cq`, `vbr`, `cbr`, `cbr_ld_hq`) |
| `cq` | — | Quality level, 0-51 (lower = better). Also sets qmin=cq+10, qmax=cq-10 |
| `lookahead` | *optional* | Lookahead frames (0-32) |
| `temporal_aq` | *optional* | Temporal adaptive quantization (`true`/`false`) |

Requires CUDA. Hardware acceleration flags: `-hwaccel cuda -hwaccel_device cuda0`.

#### `video.qsv` (Intel Quick Sync)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `codec` | — | Video codec (`av1_qsv`, `h264_qsv`, `hevc_qsv`) |
| `preset` | — | Quality preset (`veryfast` to `veryslow`) |
| `global_quality` | — | Quality level, 0-51 (lower = better) |
| `look_ahead_depth` | `0` | Lookahead analysis depth (0 = disabled) |

Hardware acceleration flags: `-hwaccel qsv -hwaccel_output_format qsv`. Uses `vpp_qsv` for hardware scaling and tonemapping. Enables `extbrc` (extended bitrate control).

#### `video.vaapi` (Linux)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `codec` | — | Video codec (`av1_vaapi`, `h264_vaapi`, `hevc_vaapi`) |
| `quality` | — | Quality level, 0-51 (higher = more compression) |
| `compression_ratio` | — | Reserved for future use |

Uses `/dev/dri/renderD128` for hardware access. Output pixel format: `p010le`.

#### `video.dash`

DASH manifest generation settings.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `audio_codec` | `libopus` | Audio codec in DASH output |
| `audio_vbr` | `constrained` | VBR mode for DASH audio |
| `audio_channels` | `2` | Audio channel count |
| `segment_duration` | `10500` | DASH segment duration in milliseconds |

#### `video.thumbnail`

Video thumbnail dimensions (JPEG).

| Parameter | Default | Description |
|-----------|---------|-------------|
| `width` | `1920` | Maximum thumbnail width |
| `height` | `1080` | Maximum thumbnail height |

#### `video.showcase`

Animated AVIF preview generation.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `width` | `480` | Output width (height scales proportionally) |
| `fps` | `2` | Frame rate |
| `max_frames` | `60` | Maximum number of frames |
| `quality` | `40` | AV1 quality level (`q:v`) |
| `cpu_used` | `2` | libaom-av1 `cpu-used` setting (higher = faster) |

#### `video.preview_sprites`

Thumbnail sprite sheets for seek preview.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `interval_seconds` | `5.0` | Seconds between sprite captures |
| `thumb_width` | `640` | Individual thumbnail width |
| `thumb_height` | `360` | Individual thumbnail height |
| `max_sprites_per_file` | `100` | Maximum thumbnails per sprite image |
| `sprites_across` | `10` | Thumbnails per row in sprite grid |
| `quality` | `36` | AV1 quality level for sprites |
| `parallel_limit` | `4` | Maximum number of sprite files generated in parallel |

## HDR handling

HDR content (SMPTE 2084 / PQ, ARIB STD-B67 / HLG, BT.2020 color primaries) is detected automatically. When HDR is detected:

- **QSV**: Hardware tonemapping via `vpp_qsv` with `tonemap=1`
- **NVENC / VAAPI**: Software tonemapping using `zscale` + `tonemap=mobius` filter chain

Output is always SDR (BT.709) in `yuv420p10le` pixel format.

## Hardware detection

Check what your system supports:

```bash
# NVIDIA (NVENC)
ffmpeg -encoders 2>/dev/null | grep nvenc
nvidia-smi

# Intel (QSV)
ffmpeg -encoders 2>/dev/null | grep qsv
vainfo | grep -E "AV1|H264|HEVC"

# Linux (VAAPI)
vainfo
ls /dev/dri/
```

## Troubleshooting

**Encoder not found** — Verify hardware support with the commands above. Install appropriate drivers (`nvidia-driver`, `intel-media-driver`, `mesa-va-gallium`).

**Poor quality** — Lower the `cq` / `global_quality` / `quality` value. Use a slower preset. Enable lookahead (NVENC/QSV).

**Encoding too slow** — Use a faster preset. Reduce lookahead depth. Lower `max_resolution_steps`.

**Files too large** — Increase the quality value (higher = more compression for VAAPI). Use stricter rate control modes.

**Whisper transcription fails** — Verify the Whisper.cpp server is reachable at the configured URL. Check that the audio file has a valid audio stream.

**Translation not working** — Verify the llama.cpp server is running with a TranslateGemma model and is reachable at the configured `llama_url`. Check that `translation.languages` is set to a non-empty array. The `/completion` endpoint must be available.

**Wrong subtitle language detected** — Whisper language detection uses the first 30 seconds of audio. Short intros in a different language (e.g., foreign-language music before dialogue) may cause incorrect detection. The transcription itself still uses auto-detection per chunk.

**Translation quality is poor** — Use a larger TranslateGemma model variant. Ensure `temperature` is low (the processor uses 0.1 by default). For long subtitle files, check that `timeout_secs` is sufficient — each cue is translated individually and a timeout aborts the request.
