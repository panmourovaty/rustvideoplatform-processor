#![forbid(unsafe_code)]
#![allow(non_snake_case)]
use mimalloc::MiMalloc;
use std::fs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use ffmpeg_next::{codec, format, media};
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::process::Command;
use std::time::Duration;
use reqwest::blocking::{Client, multipart};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task;
use pdfium_render::prelude::*;

#[derive(Deserialize, Debug)]
struct FfprobeOutput {
    streams: Option<Vec<FfprobeStream>>,
}

#[derive(Deserialize, Debug)]
struct FfprobeChaptersOutput {
    chapters: Option<Vec<FfprobeChapter>>,
}

#[derive(Deserialize, Debug)]
struct FfprobeChapter {
    start_time: Option<String>,
    end_time: Option<String>,
    tags: Option<FfprobeChapterTags>,
}

#[derive(Deserialize, Debug)]
struct FfprobeChapterTags {
    title: Option<String>,
}

#[derive(Deserialize, Debug)]
struct FfprobeStream {
    index: Option<u32>,
    codec_name: Option<String>,
    tags: Option<FfprobeTags>,
}

#[derive(Deserialize, Debug)]
struct FfprobeTags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Config {
    dbconnection: String,
    video: VideoConfig,
    #[serde(default = "default_whisper_config")]
    whisper: WhisperConfig,
    #[serde(default = "default_audio_transcode_config")]
    audio: AudioTranscodeConfig,
    #[serde(default = "default_picture_config")]
    picture: PictureConfig,
    #[serde(default = "default_pdf_config")]
    pdf: PdfConfig,
    #[serde(default = "default_translation_config")]
    translation: TranslationConfig,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
enum VideoEncoder {
    Nvenc,
    Qsv,
    Vaapi,
}

#[derive(Deserialize, Clone, Debug)]
struct QualityStep {
    label: String,
    scale_divisor: u32,
    audio_bitrate_divisor: u32,
}

#[derive(Deserialize, Clone, Debug)]
struct NvencSettings {
    codec: String,
    preset: String,
    tier: String,
    rc: String,
    cq: u32,
    #[serde(default)]
    lookahead: Option<u32>,
    #[serde(default)]
    temporal_aq: Option<bool>,
}

#[derive(Deserialize, Clone, Debug)]
struct QsvSettings {
    codec: String,
    preset: String,
    global_quality: u32,
    #[serde(default)]
    look_ahead_depth: u32,
}

#[derive(Deserialize, Clone, Debug)]
struct VaapiSettings {
    codec: String,
    quality: u32,
    compression_ratio: u32,
}

#[derive(Deserialize, Clone, Debug)]
struct WhisperConfig {
    #[serde(default = "default_whisper_url")]
    url: String,
    #[serde(default = "default_whisper_model")]
    model: String,
    #[serde(default = "default_whisper_response_format")]
    response_format: String,
    #[serde(default = "default_whisper_output_label")]
    output_label: String,
    /// Target chunk duration in seconds (default: 600 = 10 minutes).
    /// Audio will be split at silence boundaries near this target.
    #[serde(default = "default_whisper_target_chunk_secs")]
    target_chunk_secs: f64,
    /// Maximum chunk duration in seconds (default: 900 = 15 minutes).
    /// If no silence is found by the target, extend up to this limit.
    #[serde(default = "default_whisper_max_chunk_secs")]
    max_chunk_secs: f64,
    /// Silence detection noise threshold in dB (default: -30).
    #[serde(default = "default_whisper_silence_noise_db")]
    silence_noise_db: f64,
    /// Minimum silence duration in seconds to consider as a split point (default: 0.5).
    #[serde(default = "default_whisper_silence_min_duration")]
    silence_min_duration: f64,
    /// Maximum number of parallel ffmpeg processes for silence detection (default: 4).
    #[serde(default = "default_whisper_silence_detect_parallel")]
    silence_detect_parallel: u32,
    /// Sampling temperature for Whisper (default: 0.0).
    /// 0.0 uses greedy decoding (most deterministic and accurate for transcription).
    /// Higher values (up to 1.0) increase randomness; useful if transcription quality
    /// is poor, but may reduce consistency.
    #[serde(default = "default_whisper_temperature")]
    temperature: f32,
}

fn default_whisper_url() -> String { "http://whisper:8080/inference".to_string() }
fn default_whisper_model() -> String { "whisper-1".to_string() }
fn default_whisper_response_format() -> String { "vtt".to_string() }
fn default_whisper_output_label() -> String { "AI_transcription".to_string() }
fn default_whisper_target_chunk_secs() -> f64 { 600.0 }
fn default_whisper_max_chunk_secs() -> f64 { 900.0 }
fn default_whisper_silence_noise_db() -> f64 { -30.0 }
fn default_whisper_silence_min_duration() -> f64 { 0.5 }
fn default_whisper_silence_detect_parallel() -> u32 { 4 }
fn default_whisper_temperature() -> f32 { 0.0 }

fn default_whisper_config() -> WhisperConfig {
    WhisperConfig {
        url: default_whisper_url(),
        model: default_whisper_model(),
        response_format: default_whisper_response_format(),
        output_label: default_whisper_output_label(),
        target_chunk_secs: default_whisper_target_chunk_secs(),
        max_chunk_secs: default_whisper_max_chunk_secs(),
        silence_noise_db: default_whisper_silence_noise_db(),
        silence_min_duration: default_whisper_silence_min_duration(),
        silence_detect_parallel: default_whisper_silence_detect_parallel(),
        temperature: default_whisper_temperature(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct TranslationConfig {
    /// Target languages as ISO 639-1 codes (e.g., ["en", "cs"]).
    /// If empty, translation is disabled and Whisper uses the output_label name.
    #[serde(default)]
    languages: Vec<String>,
    /// llama.cpp server URL for TranslateGemma (e.g., "http://llama:8081")
    #[serde(default = "default_llama_url")]
    llama_url: String,
    /// Preferred source language for translation (default: "en").
    /// When multiple subtitle tracks exist, this language is preferred as the translation source.
    #[serde(default = "default_translation_source_language")]
    source_language: String,
    /// Timeout in seconds for each translation request (default: 120).
    #[serde(default = "default_translation_timeout_secs")]
    timeout_secs: u64,
}

fn default_llama_url() -> String { "http://llama:8081".to_string() }
fn default_translation_source_language() -> String { "en".to_string() }
fn default_translation_timeout_secs() -> u64 { 120 }

fn default_translation_config() -> TranslationConfig {
    TranslationConfig {
        languages: Vec::new(),
        llama_url: default_llama_url(),
        source_language: default_translation_source_language(),
        timeout_secs: default_translation_timeout_secs(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct AudioTranscodeConfig {
    #[serde(default = "default_audio_codec")]
    codec: String,
    #[serde(default = "default_audio_lossless_bitrate")]
    lossless_bitrate: String,
    #[serde(default = "default_audio_lossy_bitrate")]
    lossy_bitrate: String,
    #[serde(default = "default_audio_vbr")]
    vbr: String,
    #[serde(default = "default_audio_application")]
    application: String,
    #[serde(default = "default_audio_output_format")]
    output_format: String,
    #[serde(default = "default_audio_lossless_codecs")]
    lossless_codecs: Vec<String>,
}

fn default_audio_codec() -> String { "libopus".to_string() }
fn default_audio_lossless_bitrate() -> String { "300k".to_string() }
fn default_audio_lossy_bitrate() -> String { "256k".to_string() }
fn default_audio_vbr() -> String { "on".to_string() }
fn default_audio_application() -> String { "audio".to_string() }
fn default_audio_output_format() -> String { "ogg".to_string() }
fn default_audio_lossless_codecs() -> Vec<String> {
    vec!["flac".to_string(), "wav".to_string(), "pcm_s16le".to_string()]
}

fn default_audio_transcode_config() -> AudioTranscodeConfig {
    AudioTranscodeConfig {
        codec: default_audio_codec(),
        lossless_bitrate: default_audio_lossless_bitrate(),
        lossy_bitrate: default_audio_lossy_bitrate(),
        vbr: default_audio_vbr(),
        application: default_audio_application(),
        output_format: default_audio_output_format(),
        lossless_codecs: default_audio_lossless_codecs(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct PictureConfig {
    #[serde(default = "default_picture_crf")]
    crf: u32,
    #[serde(default = "default_picture_thumbnail_crf")]
    thumbnail_crf: u32,
    #[serde(default = "default_picture_jpg_quality")]
    jpg_quality: u32,
    #[serde(default = "default_picture_thumbnail_width")]
    thumbnail_width: u32,
    #[serde(default = "default_picture_thumbnail_height")]
    thumbnail_height: u32,
    #[serde(default = "default_picture_cover_crf")]
    cover_crf: u32,
    #[serde(default = "default_picture_cover_thumbnail_crf")]
    cover_thumbnail_crf: u32,
}

fn default_picture_crf() -> u32 { 26 }
fn default_picture_thumbnail_crf() -> u32 { 28 }
fn default_picture_jpg_quality() -> u32 { 25 }
fn default_picture_thumbnail_width() -> u32 { 1280 }
fn default_picture_thumbnail_height() -> u32 { 720 }
fn default_picture_cover_crf() -> u32 { 26 }
fn default_picture_cover_thumbnail_crf() -> u32 { 30 }

fn default_picture_config() -> PictureConfig {
    PictureConfig {
        crf: default_picture_crf(),
        thumbnail_crf: default_picture_thumbnail_crf(),
        jpg_quality: default_picture_jpg_quality(),
        thumbnail_width: default_picture_thumbnail_width(),
        thumbnail_height: default_picture_thumbnail_height(),
        cover_crf: default_picture_cover_crf(),
        cover_thumbnail_crf: default_picture_cover_thumbnail_crf(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct PdfConfig {
    #[serde(default = "default_pdf_thumbnail_width")]
    thumbnail_width: u32,
    #[serde(default = "default_pdf_thumbnail_height")]
    thumbnail_height: u32,
    #[serde(default = "default_pdf_thumbnail_crf")]
    thumbnail_crf: u32,
    #[serde(default = "default_pdf_jpg_quality")]
    jpg_quality: u32,
    #[serde(default = "default_pdf_render_width")]
    render_width: u32,
}

fn default_pdf_thumbnail_width() -> u32 { 1280 }
fn default_pdf_thumbnail_height() -> u32 { 720 }
fn default_pdf_thumbnail_crf() -> u32 { 28 }
fn default_pdf_jpg_quality() -> u32 { 25 }
fn default_pdf_render_width() -> u32 { 2000 }

fn default_pdf_config() -> PdfConfig {
    PdfConfig {
        thumbnail_width: default_pdf_thumbnail_width(),
        thumbnail_height: default_pdf_thumbnail_height(),
        thumbnail_crf: default_pdf_thumbnail_crf(),
        jpg_quality: default_pdf_jpg_quality(),
        render_width: default_pdf_render_width(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct ThumbnailConfig {
    #[serde(default = "default_thumbnail_width")]
    width: u32,
    #[serde(default = "default_thumbnail_height")]
    height: u32,
}

fn default_thumbnail_width() -> u32 { 1920 }
fn default_thumbnail_height() -> u32 { 1080 }

fn default_thumbnail_config() -> ThumbnailConfig {
    ThumbnailConfig {
        width: default_thumbnail_width(),
        height: default_thumbnail_height(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct ShowcaseConfig {
    #[serde(default = "default_showcase_width")]
    width: u32,
    #[serde(default = "default_showcase_fps")]
    fps: u32,
    #[serde(default = "default_showcase_max_frames")]
    max_frames: u32,
    #[serde(default = "default_showcase_quality")]
    quality: u32,
    #[serde(default = "default_showcase_cpu_used")]
    cpu_used: u32,
}

fn default_showcase_width() -> u32 { 480 }
fn default_showcase_fps() -> u32 { 2 }
fn default_showcase_max_frames() -> u32 { 60 }
fn default_showcase_quality() -> u32 { 40 }
fn default_showcase_cpu_used() -> u32 { 2 }

fn default_showcase_config() -> ShowcaseConfig {
    ShowcaseConfig {
        width: default_showcase_width(),
        fps: default_showcase_fps(),
        max_frames: default_showcase_max_frames(),
        quality: default_showcase_quality(),
        cpu_used: default_showcase_cpu_used(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct PreviewSpriteConfig {
    #[serde(default = "default_preview_interval_seconds")]
    interval_seconds: f64,
    #[serde(default = "default_preview_thumb_width")]
    thumb_width: u32,
    #[serde(default = "default_preview_thumb_height")]
    thumb_height: u32,
    #[serde(default = "default_preview_max_sprites_per_file")]
    max_sprites_per_file: u32,
    #[serde(default = "default_preview_sprites_across")]
    sprites_across: u32,
    #[serde(default = "default_preview_quality")]
    quality: u32,
    #[serde(default = "default_preview_parallel_limit")]
    parallel_limit: u32,
}

fn default_preview_interval_seconds() -> f64 { 5.0 }
fn default_preview_thumb_width() -> u32 { 640 }
fn default_preview_thumb_height() -> u32 { 360 }
fn default_preview_max_sprites_per_file() -> u32 { 100 }
fn default_preview_sprites_across() -> u32 { 10 }
fn default_preview_quality() -> u32 { 36 }
fn default_preview_parallel_limit() -> u32 { 4 }

fn default_preview_sprite_config() -> PreviewSpriteConfig {
    PreviewSpriteConfig {
        interval_seconds: default_preview_interval_seconds(),
        thumb_width: default_preview_thumb_width(),
        thumb_height: default_preview_thumb_height(),
        max_sprites_per_file: default_preview_max_sprites_per_file(),
        sprites_across: default_preview_sprites_across(),
        quality: default_preview_quality(),
        parallel_limit: default_preview_parallel_limit(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct DashConfig {
    #[serde(default = "default_dash_audio_codec")]
    audio_codec: String,
    #[serde(default = "default_dash_audio_vbr")]
    audio_vbr: String,
    #[serde(default = "default_dash_audio_channels")]
    audio_channels: u32,
    #[serde(default = "default_dash_segment_duration")]
    segment_duration: u32,
}

fn default_dash_audio_codec() -> String { "libopus".to_string() }
fn default_dash_audio_vbr() -> String { "constrained".to_string() }
fn default_dash_audio_channels() -> u32 { 2 }
fn default_dash_segment_duration() -> u32 { 8 }

fn default_dash_config() -> DashConfig {
    DashConfig {
        audio_codec: default_dash_audio_codec(),
        audio_vbr: default_dash_audio_vbr(),
        audio_channels: default_dash_audio_channels(),
        segment_duration: default_dash_segment_duration(),
    }
}

#[derive(Deserialize, Clone, Debug)]
struct VideoConfig {
    encoder: VideoEncoder,
    max_resolution_steps: u32,
    min_dimension: u32,
    fps_cap: f32,
    audio_bitrate_base: u32,
    threshold_2k_pixels: u32,
    audio_bitrate_2k_bonus: u32,
    quality_steps: Vec<QualityStep>,
    filters: String,
    #[serde(default)]
    nvenc: Option<NvencSettings>,
    #[serde(default)]
    qsv: Option<QsvSettings>,
    #[serde(default)]
    vaapi: Option<VaapiSettings>,
    #[serde(default = "default_dash_config")]
    dash: DashConfig,
    #[serde(default = "default_thumbnail_config")]
    thumbnail: ThumbnailConfig,
    #[serde(default = "default_showcase_config")]
    showcase: ShowcaseConfig,
    #[serde(default = "default_preview_sprite_config")]
    preview_sprites: PreviewSpriteConfig,
}

#[tokio::main]
async fn main() {
    let config: Config = serde_json::from_str(&fs::read_to_string("config.json").unwrap()).unwrap();

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.dbconnection)
        .await
        .unwrap();

    process(pool, config).await;
}

fn detect_file_type(input_file: &str) -> Option<String> {
    // Check for PDF magic bytes (%PDF)
    if let Ok(mut file) = std::fs::File::open(input_file) {
        let mut magic = [0u8; 4];
        if std::io::Read::read_exact(&mut file, &mut magic).is_ok() && &magic == b"%PDF" {
            return Some("document_pdf".to_string());
        }
    }

    // Check for video stream
    let video_probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    // Check for audio stream
    let audio_probe_cmd = format!(
        "ffprobe -v error -select_streams a:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    let video_output = Command::new("sh").arg("-c").arg(&video_probe_cmd).output();
    let audio_output = Command::new("sh").arg("-c").arg(&audio_probe_cmd).output();

    let has_video = match video_output {
        Ok(result) if result.status.success() => {
            let output_str = String::from_utf8_lossy(&result.stdout);
            output_str.contains("video")
        }
        _ => false,
    };

    let has_audio = match audio_output {
        Ok(result) if result.status.success() => {
            let output_str = String::from_utf8_lossy(&result.stdout);
            output_str.contains("audio")
        }
        _ => false,
    };

    if !has_video && !has_audio {
        return None;
    }

    // If we have audio and no video, it's an audio file
    if has_audio && !has_video {
        return Some("audio".to_string());
    }

    // If we have both, determine if video is just cover art or actual video
    if has_video && has_audio {
        // Check duration - if video duration is very short compared to audio,
        // it's likely just cover art for an audio file
        let duration_cmd = format!(
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let dur_result = Command::new("sh").arg("-c").arg(&duration_cmd).output();

        match dur_result {
            Ok(dur_res) if dur_res.status.success() => {
                let dur_str = String::from_utf8_lossy(&dur_res.stdout);
                let duration: f64 = dur_str.trim().parse().unwrap_or(0.0);
                if duration <= 1.0 {
                    return Some("audio".to_string());
                }
            }
            _ => {}
        }

        // Check if it's a real video
        let nb_frames_cmd = format!(
            "ffprobe -v error -select_streams v:0 -show_entries stream=nb_frames -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let nb_frames_output = Command::new("sh").arg("-c").arg(&nb_frames_cmd).output();

        match nb_frames_output {
            Ok(result) if result.status.success() => {
                let frames_str = String::from_utf8_lossy(&result.stdout);
                let frames_str = frames_str.trim();
                if !frames_str.is_empty() && frames_str != "N/A" {
                    if let Ok(frame_count) = frames_str.parse::<i64>() {
                        if frame_count > 1 {
                            return Some("video".to_string());
                        } else {
                            return Some("audio".to_string());
                        }
                    }
                }
                // If parsing fails or empty, continue to next method
            }
            _ => {}
        }

        // Calculate frame count from duration * fps
        let stream_info_cmd = format!(
            "ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate,avg_frame_rate -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let info_output = Command::new("sh").arg("-c").arg(&stream_info_cmd).output();

        match info_output {
            Ok(result) if result.status.success() => {
                let info_str = String::from_utf8_lossy(&result.stdout);
                let lines: Vec<&str> = info_str.lines().collect();

                if lines.len() >= 3 {
                    // Parse duration from format section
                    let duration: f64 = lines[0].trim().parse().unwrap_or(0.0);

                    // Parse frame rate (try r_frame_rate first, then avg_frame_rate)
                    let fps_str = lines[1].trim();
                    let fps = if fps_str.contains('/') {
                        let parts: Vec<&str> = fps_str.split('/').collect();
                        if parts.len() == 2 {
                            let num: f64 = parts[0].parse().unwrap_or(0.0);
                            let den: f64 = parts[1].parse().unwrap_or(1.0);
                            if den > 0.0 {
                                num / den
                            } else {
                                0.0
                            }
                        } else {
                            fps_str.parse().unwrap_or(0.0)
                        }
                    } else {
                        fps_str.parse().unwrap_or(0.0)
                    };

                    if duration > 0.0 && fps > 0.0 {
                        let estimated_frames = (duration * fps) as i64;
                        // If video is longer than 1 second with valid fps, it's likely a real video
                        if duration > 1.0 && estimated_frames > 5 {
                            return Some("video".to_string());
                        } else {
                            return Some("audio".to_string());
                        }
                    }
                }
            }
            _ => {}
        }

        // Use duration as fallback heuristic
        let duration_cmd = format!(
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let dur_result = Command::new("sh").arg("-c").arg(&duration_cmd).output();

        match dur_result {
            Ok(dur_res) if dur_res.status.success() => {
                let dur_str = String::from_utf8_lossy(&dur_res.stdout);
                let duration: f64 = dur_str.trim().parse().unwrap_or(0.0);
                if duration > 5.0 {
                    return Some("video".to_string());
                } else {
                    return Some("audio".to_string());
                }
            }
            _ => return Some("audio".to_string()),
        }
    }

    // Only video stream (no audio) - determine if it's a static image or silent video
    if has_video {
        // Check frame count to distinguish between picture (1 frame) and video (>1 frame)
        let nb_frames_cmd = format!(
            "ffprobe -v error -select_streams v:0 -show_entries stream=nb_frames -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let nb_frames_output = Command::new("sh").arg("-c").arg(&nb_frames_cmd).output();

        match nb_frames_output {
            Ok(result) if result.status.success() => {
                let frames_str = String::from_utf8_lossy(&result.stdout);
                let frames_str = frames_str.trim();
                if !frames_str.is_empty() && frames_str != "N/A" {
                    if let Ok(frame_count) = frames_str.parse::<i64>() {
                        if frame_count > 1 {
                            return Some("video".to_string());
                        } else {
                            return Some("picture".to_string());
                        }
                    }
                }
            }
            _ => {}
        }

        // Calculate frame count from duration * fps
        let stream_info_cmd = format!(
            "ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate,avg_frame_rate -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let info_output = Command::new("sh").arg("-c").arg(&stream_info_cmd).output();

        match info_output {
            Ok(result) if result.status.success() => {
                let info_str = String::from_utf8_lossy(&result.stdout);
                let lines: Vec<&str> = info_str.lines().collect();

                if lines.len() >= 3 {
                    let duration: f64 = lines[0].trim().parse().unwrap_or(0.0);
                    let fps_str = lines[1].trim();
                    let fps = if fps_str.contains('/') {
                        let parts: Vec<&str> = fps_str.split('/').collect();
                        if parts.len() == 2 {
                            let num: f64 = parts[0].parse().unwrap_or(0.0);
                            let den: f64 = parts[1].parse().unwrap_or(1.0);
                            if den > 0.0 {
                                num / den
                            } else {
                                0.0
                            }
                        } else {
                            fps_str.parse().unwrap_or(0.0)
                        }
                    } else {
                        fps_str.parse().unwrap_or(0.0)
                    };

                    if duration > 0.0 && fps > 0.0 {
                        let estimated_frames = (duration * fps) as i64;
                        if estimated_frames > 1 {
                            return Some("video".to_string());
                        } else {
                            return Some("picture".to_string());
                        }
                    }
                }
            }
            _ => {}
        }

        // Use duration as fallback - short duration likely a picture
        let duration_cmd = format!(
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
            input_file
        );

        let dur_result = Command::new("sh").arg("-c").arg(&duration_cmd).output();

        match dur_result {
            Ok(dur_res) if dur_res.status.success() => {
                let dur_str = String::from_utf8_lossy(&dur_res.stdout);
                let duration: f64 = dur_str.trim().parse().unwrap_or(0.0);
                // If duration is effectively 0 or very short, it's a picture
                if duration <= 0.1 {
                    return Some("picture".to_string());
                } else {
                    return Some("video".to_string());
                }
            }
            _ => return Some("picture".to_string()),
        }
    }

    return None;
}

async fn process(pool: PgPool, config: Config) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(60000));

    loop {
        interval.tick().await;
        let unprocessed_concepts =
            match sqlx::query!("SELECT id,type FROM media_concepts WHERE processed = false;")
                .fetch_all(&pool)
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("Database query error (will retry): {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

        for concept in unprocessed_concepts {
            let input_file = format!("upload/{}", concept.id);

            // Check that the upload file actually exists before attempting processing
            if !std::path::Path::new(&input_file).exists() {
                eprintln!(
                    "Upload file not found for concept {}, marking as processed to avoid infinite retry",
                    concept.id
                );
                if let Err(e) = sqlx::query!(
                    "UPDATE media_concepts SET processed = true WHERE id = $1;",
                    concept.id
                )
                .execute(&pool)
                .await
                {
                    eprintln!("Failed to mark missing concept {} as processed: {}", concept.id, e);
                }
                continue;
            }

            let detected_type = detect_file_type(&input_file);

            // Override database type if detection yields different result
            let actual_type = if let Some(dt) = detected_type {
                dt
            } else {
                concept.r#type.clone()
            };

            let process_result: Result<(), String> = if actual_type == "video" {
                println!("processing concept: {} as video", concept.id);
                process_video(concept.id.clone(), pool.clone(), &config)
                    .await
                    .map_err(|e| format!("video processing failed: {}", e))
            } else if actual_type == "picture" {
                println!("processing concept: {} as picture", concept.id);
                process_picture(concept.id.clone(), pool.clone(), &config.picture)
                    .await
                    .map_err(|e| format!("picture processing failed: {}", e))
            } else if actual_type == "audio" {
                println!("processing concept: {} as audio", concept.id);
                process_audio(concept.id.clone(), pool.clone(), &config.audio, &config.whisper, &config.picture, &config.translation)
                    .await
                    .map_err(|e| format!("audio processing failed: {}", e))
            } else if actual_type == "document_pdf" {
                println!("processing concept: {} as document_pdf", concept.id);
                process_document_pdf(concept.id.clone(), pool.clone(), &config.pdf)
                    .await
                    .map_err(|e| format!("document_pdf processing failed: {}", e))
            } else {
                eprintln!(
                    "Unknown media type '{}' for concept {}, marking as processed",
                    actual_type, concept.id
                );
                if let Err(e) = sqlx::query!(
                    "UPDATE media_concepts SET processed = true WHERE id = $1;",
                    concept.id
                )
                .execute(&pool)
                .await
                {
                    eprintln!("Failed to mark unknown-type concept {} as processed: {}", concept.id, e);
                }
                continue;
            };

            if let Err(e) = process_result {
                eprintln!("Error processing concept {}: {}", concept.id, e);
            }
        }
    }
}

async fn process_video(concept_id: String, pool: PgPool, config: &Config) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;

    // Extract subtitles, chapters, and transcode video all in parallel
    let input_file = format!("upload/{}", concept_id);
    let input_dir = format!("upload/{}", concept_id);
    let output_dir = format!("upload/{}_processing", concept_id);
    let input_file_sub = input_file.clone();
    let output_dir_sub = output_dir.clone();
    let whisper_config = config.whisper.clone();
    let translation_config = config.translation.clone();
    let input_file_chap = input_file.clone();
    let output_dir_chap = output_dir.clone();
    let (_, _, transcode_result) = tokio::join!(
        task::spawn_blocking(move || {
            extract_subtitles_to_vtt(&input_file_sub, &output_dir_sub, &whisper_config, &translation_config);
        }),
        task::spawn_blocking(move || {
            extract_chapters_to_vtt(&input_file_chap, &output_dir_chap);
        }),
        transcode_video(
            &input_dir,
            &output_dir,
            &config.video,
        )
    );
    let transcode_result: Result<(), String> = transcode_result.map_err(|e| format!("{}", e));
    match transcode_result {
        Ok(()) => {
            sqlx::query!(
                "UPDATE media_concepts SET processed = true WHERE id = $1;",
                concept_id
            )
            .execute(&pool)
            .await
            .map_err(|e| format!("Database update error: {}", e))?;
            let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
            Ok(())
        }
        Err(e) => Err(format!("Video transcode failed: {}", e)),
    }
}

async fn process_picture(concept_id: String, pool: PgPool, picture_config: &PictureConfig) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;
    let transcode_result = transcode_picture(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
        picture_config,
    ).await;
    match transcode_result {
        Ok(()) => {
            sqlx::query!(
                "UPDATE media_concepts SET processed = true WHERE id = $1;",
                concept_id
            )
            .execute(&pool)
            .await
            .map_err(|e| format!("Database update error: {}", e))?;
            let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
            Ok(())
        }
        Err(e) => Err(format!("Picture transcode failed: {}", e)),
    }
}

async fn process_audio(concept_id: String, pool: PgPool, audio_config: &AudioTranscodeConfig, whisper_config: &WhisperConfig, picture_config: &PictureConfig, translation_config: &TranslationConfig) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;

    // Extract subtitles, chapters, and transcode audio all in parallel
    let input_file = format!("upload/{}", concept_id);
    let input_dir = format!("upload/{}", concept_id);
    let output_dir = format!("upload/{}_processing", concept_id);
    let input_file_sub = input_file.clone();
    let output_dir_sub = output_dir.clone();
    let whisper_config = whisper_config.clone();
    let translation_config = translation_config.clone();
    let input_file_chap = input_file.clone();
    let output_dir_chap = output_dir.clone();
    let (_, _, transcode_result) = tokio::join!(
        task::spawn_blocking(move || {
            extract_subtitles_to_vtt(&input_file_sub, &output_dir_sub, &whisper_config, &translation_config);
        }),
        task::spawn_blocking(move || {
            extract_chapters_to_vtt(&input_file_chap, &output_dir_chap);
        }),
        transcode_audio(
            &input_dir,
            &output_dir,
            audio_config,
            picture_config,
        )
    );
    let transcode_result: Result<(), String> = transcode_result.map_err(|e| format!("{}", e));
    match transcode_result {
        Ok(()) => {
            sqlx::query!(
                "UPDATE media_concepts SET processed = true WHERE id = $1;",
                concept_id
            )
            .execute(&pool)
            .await
            .map_err(|e| format!("Database update error: {}", e))?;
            let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
            Ok(())
        }
        Err(e) => Err(format!("Audio transcode failed: {}", e)),
    }
}

async fn process_document_pdf(concept_id: String, pool: PgPool, pdf_config: &PdfConfig) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;

    let input_file = format!("upload/{}", concept_id);
    let output_dir = format!("upload/{}_processing", concept_id);

    // Generate thumbnails and extract text in parallel
    let input_file_thumb = input_file.clone();
    let output_dir_thumb = output_dir.clone();
    let pdf_config_clone = pdf_config.clone();
    let input_file_text = input_file.clone();
    let output_dir_text = output_dir.clone();

    let (thumb_result, text_result) = tokio::join!(
        task::spawn_blocking(move || {
            generate_pdf_thumbnails(&input_file_thumb, &output_dir_thumb, &pdf_config_clone)
        }),
        task::spawn_blocking(move || {
            extract_pdf_text(&input_file_text, &output_dir_text)
        })
    );

    let thumb_result = thumb_result.map_err(|e| format!("Thumbnail task panicked: {}", e))?;
    let text_result = text_result.map_err(|e| format!("Text extraction task panicked: {}", e))?;

    if let Err(e) = &thumb_result {
        eprintln!("PDF thumbnail generation failed for {}: {}", concept_id, e);
    }
    if let Err(e) = &text_result {
        eprintln!("PDF text extraction failed for {}: {}", concept_id, e);
    }

    // Require at least thumbnails to succeed
    thumb_result?;

    sqlx::query!(
        "UPDATE media_concepts SET processed = true WHERE id = $1;",
        concept_id
    )
    .execute(&pool)
    .await
    .map_err(|e| format!("Database update error: {}", e))?;

    // Move the file into the processing folder and rename it to 'document.pdf'
    let dest_file = format!("{}/document.pdf", output_dir);
    fs::rename(&input_file, &dest_file)
        .map_err(|e| format!("Failed to move source document to processing folder: {}", e))?;

    Ok(())
}

fn generate_pdf_thumbnails(input_file: &str, output_dir: &str, pdf_config: &PdfConfig) -> Result<(), String> {
    // Use pdfium-render to render the first page to a temporary PNG
    let pdfium = Pdfium::default();
    let document = pdfium.load_pdf_from_file(input_file, None)
        .map_err(|e| format!("Failed to load PDF with pdfium: {}", e))?;

    let pages = document.pages();
    if pages.len() == 0 {
        return Err("PDF has no pages".to_string());
    }

    let first_page = pages.get(0)
        .map_err(|e| format!("Failed to get first page: {}", e))?;

    let render_config = PdfRenderConfig::new()
        .set_target_width(pdf_config.render_width as Pixels)
        .set_maximum_height((pdf_config.render_width * 3) as Pixels);

    let bitmap = first_page.render_with_config(&render_config)
        .map_err(|e| format!("Failed to render PDF page: {}", e))?;

    let dynamic_image = bitmap.as_image();

    let rgb_image = dynamic_image.into_rgb8();

    // Save as temporary PNG for ffmpeg to process
    let temp_png = format!("{}/temp_page.png", output_dir);
    rgb_image.save_with_format(&temp_png, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to save temp PNG: {}", e))?;

    let orig_width = rgb_image.width();
    let orig_height = rgb_image.height();

    // Calculate thumbnail dimensions maintaining aspect ratio
    let (thumb_width, thumb_height) = calculate_hd_scale(
        orig_width, orig_height,
        pdf_config.thumbnail_width, pdf_config.thumbnail_height,
    );

    // Generate thumbnail.avif and thumbnail.jpg using ffmpeg (consistent with other pipelines)
    let avif_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -c:v libsvtav1 -svtav1-params avif=1 -crf {} -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 -update 1 '{}/thumbnail.avif'",
        temp_png, pdf_config.thumbnail_crf, thumb_width, thumb_height, output_dir
    );
    let jpg_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v {} '{}/thumbnail.jpg'",
        temp_png, thumb_width, thumb_height, pdf_config.jpg_quality, output_dir
    );

    println!("Executing: {}", avif_cmd);
    let avif_status = Command::new("sh").arg("-c").arg(&avif_cmd).status()
        .map_err(|e| format!("Failed to execute ffmpeg for AVIF: {}", e))?;
    if !avif_status.success() {
        return Err(format!("ffmpeg AVIF thumbnail failed with exit code: {:?}", avif_status.code()));
    }

    println!("Executing: {}", jpg_cmd);
    let jpg_status = Command::new("sh").arg("-c").arg(&jpg_cmd).status()
        .map_err(|e| format!("Failed to execute ffmpeg for JPG: {}", e))?;
    if !jpg_status.success() {
        return Err(format!("ffmpeg JPG thumbnail failed with exit code: {:?}", jpg_status.code()));
    }

    // Clean up temporary PNG
    let _ = fs::remove_file(&temp_png);

    println!("Generated PDF thumbnails for {}", input_file);
    Ok(())
}

fn extract_pdf_text(input_file: &str, output_dir: &str) -> Result<(), String> {
    let mut doc = pdf_oxide::PdfDocument::open(input_file)
        .map_err(|e| format!("Failed to open PDF with pdf_oxide: {}", e))?;

    let page_count = doc.page_count()
        .map_err(|e| format!("Failed to get PDF page count: {}", e))?;
    if page_count == 0 {
        return Err("PDF has no pages for text extraction".to_string());
    }

    let mut all_markdown = String::new();

    for page_num in 0..page_count {
        let page_md = doc.to_markdown(page_num, &Default::default())
            .map_err(|e| format!("Failed to extract markdown from page {}: {}", page_num, e))?;

        if !page_md.trim().is_empty() {
            if !all_markdown.is_empty() {
                all_markdown.push_str("\n\n---\n\n");
            }
            all_markdown.push_str(page_md.trim());
        }
    }

    let output_path = format!("{}/text.md", output_dir);
    fs::write(&output_path, &all_markdown)
        .map_err(|e| format!("Failed to write text.md: {}", e))?;

    println!("Extracted text from {} pages to {}", page_count, output_path);
    Ok(())
}

fn probe_subtitle_streams(input_file: &str) -> Vec<(u32, String, String, String)> {
    // Returns Vec of (stream_index, language, title, codec)
    //
    // Single ffprobe call:
    // ffprobe -v error -select_streams s -show_entries stream=index,codec_name:stream_tags=language,title -of json <input>
    let mut cmd = Command::new("ffprobe");
    cmd.arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("s")
        .arg("-show_entries")
        .arg("stream=index,codec_name:stream_tags=language,title")
        .arg("-of")
        .arg("json")
        .arg(input_file);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    if !output.status.success() {
        return Vec::new();
    }

    let parsed: FfprobeOutput = match serde_json::from_slice(&output.stdout) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    let streams = match parsed.streams {
        Some(s) => s,
        None => return result,
    };

    for s in streams {
        let idx = match s.index {
            Some(i) => i,
            None => continue,
        };
        let codec = s.codec_name.unwrap_or_else(|| "unknown".to_string());
        let (language, title) = match s.tags {
            Some(t) => (
                t.language.unwrap_or_default(),
                t.title.unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };

        result.push((idx, language, title, codec));
    }

    result
}

fn probe_audio_streams(input_file: &str) -> Vec<(u32, String, String, String)> {
    // Returns Vec of (stream_index, language, title, codec)
    let mut cmd = Command::new("ffprobe");
    cmd.arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("a")
        .arg("-show_entries")
        .arg("stream=index,codec_name:stream_tags=language,title")
        .arg("-of")
        .arg("json")
        .arg(input_file);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    if !output.status.success() {
        return Vec::new();
    }

    let parsed: FfprobeOutput = match serde_json::from_slice(&output.stdout) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    let streams = match parsed.streams {
        Some(s) => s,
        None => return result,
    };

    for s in streams {
        let idx = match s.index {
            Some(i) => i,
            None => continue,
        };
        let codec = s.codec_name.unwrap_or_else(|| "unknown".to_string());
        let (language, title) = match s.tags {
            Some(t) => (
                t.language.unwrap_or_default(),
                t.title.unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };

        result.push((idx, language, title, codec));
    }

    result
}

async fn transcode_audio_streams_for_dash(
    input_file: &str,
    output_dir: &str,
    audio_bitrate: u32,
    audio_streams: &[(u32, String, String, String)],
    dash_config: &DashConfig,
) -> Vec<(String, String, String)> {
    // Returns Vec of (file_path, language, title) for successfully transcoded audio streams
    // Transcode all audio streams in parallel
    let mut handles = Vec::new();

    for (audio_idx, (_stream_index, language, title, _codec)) in audio_streams.iter().enumerate() {
        let output_file = format!("{}/audio_stream_{}.webm", output_dir, audio_idx);

        let mut cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -map 0:a:{} -c:a {} -b:a {}k -vbr {} -ac {} -vn",
            input_file, audio_idx, dash_config.audio_codec, audio_bitrate, dash_config.audio_vbr, dash_config.audio_channels
        );

        // Set language metadata if available
        if !language.is_empty() {
            cmd.push_str(&format!(" -metadata:s:a:0 language={}", language));
        }
        if !title.is_empty() {
            cmd.push_str(&format!(" -metadata:s:a:0 title='{}'", title.replace('\'', "'\\''")));
        }

        cmd.push_str(&format!(" -f webm '{}'", output_file));

        let language_owned = language.clone();
        let title_owned = title.clone();
        let output_file_owned = output_file.clone();
        handles.push(task::spawn_blocking(move || {
            println!("Executing: {}", cmd);
            let status = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status();
            (status, audio_idx, output_file_owned, language_owned, title_owned)
        }));
    }

    // Collect results from all parallel audio transcodes
    let mut result = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((status, audio_idx, output_file, language, title)) => match status {
                Ok(s) if s.success() => {
                    println!(
                        "Generated audio stream {}: {} (language: {}, title: {})",
                        audio_idx,
                        output_file,
                        if language.is_empty() { "und" } else { &language },
                        if title.is_empty() { "none" } else { &title }
                    );
                    result.push((output_file, language, title));
                }
                Ok(s) => {
                    eprintln!(
                        "Failed to transcode audio stream {} with exit code: {:?}",
                        audio_idx,
                        s.code()
                    );
                }
                Err(e) => {
                    eprintln!("Failed to execute ffmpeg for audio stream {}: {}", audio_idx, e);
                }
            },
            Err(e) => {
                eprintln!("Audio transcode task panicked: {}", e);
            }
        }
    }

    result
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Normalize a language code to ISO 639-1 (two-letter) format.
/// Handles ISO 639-2/B (e.g., "eng"), ISO 639-2/T (e.g., "ces"), and common full names.
fn normalize_language_code(code: &str) -> Option<String> {
    let code_lower = code.to_lowercase();
    let code_lower = code_lower.trim();

    if code_lower.is_empty() || code_lower == "und" || code_lower == "undetermined" {
        return None;
    }

    // Already ISO 639-1 (2-letter) — accept as-is
    if code_lower.len() == 2 {
        return Some(code_lower.to_string());
    }

    // ISO 639-2/B and 639-2/T (3-letter) to ISO 639-1
    if code_lower.len() == 3 {
        let mapped = match &*code_lower {
            "eng" => "en", "cze" | "ces" => "cs", "ger" | "deu" => "de",
            "fre" | "fra" => "fr", "spa" => "es", "ita" => "it",
            "por" => "pt", "rus" => "ru", "jpn" => "ja", "kor" => "ko",
            "chi" | "zho" => "zh", "ara" => "ar", "hin" => "hi",
            "pol" => "pl", "dut" | "nld" => "nl", "swe" => "sv",
            "dan" => "da", "nor" | "nob" | "nno" => "no", "fin" => "fi",
            "hun" => "hu", "tur" => "tr", "gre" | "ell" => "el",
            "heb" => "he", "tha" => "th", "vie" => "vi", "ind" => "id",
            "may" | "msa" => "ms", "ukr" => "uk", "rum" | "ron" => "ro",
            "bul" => "bg", "hrv" | "scr" => "hr", "slo" | "slk" => "sk",
            "slv" => "sl", "srp" | "scc" => "sr", "lit" => "lt",
            "lav" => "lv", "est" => "et", "cat" => "ca", "glg" => "gl",
            "baq" | "eus" => "eu", "wel" | "cym" => "cy", "gle" => "ga",
            "ice" | "isl" => "is", "mac" | "mkd" => "mk", "alb" | "sqi" => "sq",
            "bos" => "bs", "mlt" => "mt", "ltz" => "lb", "afr" => "af",
            "swa" => "sw", "tgl" => "tl", "ben" => "bn", "tam" => "ta",
            "tel" => "te", "mal" => "ml", "kan" => "kn", "guj" => "gu",
            "mar" => "mr", "nep" => "ne", "sin" => "si", "khm" => "km",
            "lao" => "lo", "bur" | "mya" => "my", "geo" | "kat" => "ka",
            "amh" => "am", "per" | "fas" => "fa", "urd" => "ur",
            "pus" => "ps", "kur" => "ku", "lat" => "la", "epo" => "eo",
            _ => return None,
        };
        return Some(mapped.to_string());
    }

    // Full language names
    match &*code_lower {
        "english" => Some("en".to_string()),
        "czech" => Some("cs".to_string()),
        "german" => Some("de".to_string()),
        "french" => Some("fr".to_string()),
        "spanish" => Some("es".to_string()),
        "italian" => Some("it".to_string()),
        "portuguese" => Some("pt".to_string()),
        "russian" => Some("ru".to_string()),
        "japanese" => Some("ja".to_string()),
        "korean" => Some("ko".to_string()),
        "chinese" => Some("zh".to_string()),
        "arabic" => Some("ar".to_string()),
        "hindi" => Some("hi".to_string()),
        "polish" => Some("pl".to_string()),
        "dutch" => Some("nl".to_string()),
        "swedish" => Some("sv".to_string()),
        "danish" => Some("da".to_string()),
        "norwegian" => Some("no".to_string()),
        "finnish" => Some("fi".to_string()),
        "hungarian" => Some("hu".to_string()),
        "turkish" => Some("tr".to_string()),
        "greek" => Some("el".to_string()),
        "hebrew" => Some("he".to_string()),
        "thai" => Some("th".to_string()),
        "vietnamese" => Some("vi".to_string()),
        "indonesian" => Some("id".to_string()),
        "ukrainian" => Some("uk".to_string()),
        "romanian" => Some("ro".to_string()),
        "bulgarian" => Some("bg".to_string()),
        "croatian" => Some("hr".to_string()),
        "slovak" => Some("sk".to_string()),
        "slovenian" => Some("sl".to_string()),
        "serbian" => Some("sr".to_string()),
        _ => None,
    }
}

/// Detect the language of an audio file using Whisper.cpp verbose_json response.
/// Extracts a short audio sample and sends it to the Whisper API for language detection.
fn detect_language_via_whisper(input_file: &str, output_dir: &str, whisper_config: &WhisperConfig) -> Option<String> {
    let temp_audio = format!("{}/lang_detect_temp.wav", output_dir);

    // Extract first 30 seconds for language detection
    let extract_result = Command::new("ffmpeg")
        .arg("-nostdin")
        .arg("-v").arg("error")
        .arg("-i").arg(input_file)
        .arg("-t").arg("30")
        .arg("-ar").arg("16000")
        .arg("-ac").arg("1")
        .arg("-c:a").arg("pcm_s16le")
        .arg("-y")
        .arg(&temp_audio)
        .output();

    if let Err(e) = extract_result {
        println!("Failed to extract audio for language detection: {}", e);
        return None;
    }

    let form = match multipart::Form::new()
        .text("response_format", "verbose_json".to_string())
        .text("model", whisper_config.model.clone())
        .text("temperature", whisper_config.temperature.to_string())
        .file("file", &temp_audio)
    {
        Ok(f) => f,
        Err(e) => {
            println!("Failed to read temp audio for language detection: {}", e);
            let _ = fs::remove_file(&temp_audio);
            return None;
        }
    };

    let client = match Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to create HTTP client for language detection: {}", e);
            let _ = fs::remove_file(&temp_audio);
            return None;
        }
    };

    let response = client.post(&whisper_config.url).multipart(form).send();
    let _ = fs::remove_file(&temp_audio);

    match response {
        Ok(res) if res.status().is_success() => {
            if let Ok(body) = res.text() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) {
                    return parsed.get("language")
                        .and_then(|v| v.as_str())
                        .map(|s| {
                            // Whisper returns full names like "english"; normalize to ISO 639-1 ("en")
                            normalize_language_code(s).unwrap_or_else(|| s.to_string())
                        });
                }
            }
            None
        }
        Ok(res) => {
            println!("Whisper language detection returned error: {}", res.status());
            None
        }
        Err(e) => {
            println!("Failed to connect to Whisper API for language detection: {}", e);
            None
        }
    }
}

fn extract_subtitles_to_vtt(input_file: &str, output_dir: &str, whisper_config: &WhisperConfig, translation_config: &TranslationConfig) -> Vec<String> {
    let subtitle_streams = probe_subtitle_streams(input_file);
    let translation_enabled = !translation_config.languages.is_empty();

    // FALLBACK LOGIC: If no subtitles exist in the file, use Whisper.cpp
    if subtitle_streams.is_empty() {
        println!("No built-in subtitles found. Falling back to Whisper.cpp on {}...", whisper_config.url);
        return generate_whisper_vtt(input_file, output_dir, whisper_config, translation_config);
    }

    // Create captions directory
    let captions_dir = format!("{}/captions", output_dir);
    fs::create_dir_all(&captions_dir).expect("Failed to create captions directory");

    let mut saved_files: Vec<String> = Vec::new();
    let mut available_subs: Vec<(String, Option<String>)> = Vec::new(); // (filename, iso_lang_code)
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Precompute per-stream output file paths and names
    // Tuple: (stream_idx, final_name, output_file, language, title, iso_code)
    let mut outputs: Vec<(u32, String, String, String, String, Option<String>)> = Vec::new();

    for (stream_idx, language, title, codec) in subtitle_streams {
        // Try to normalize language tag to ISO 639-1 code
        let iso_code = if !language.is_empty() {
            normalize_language_code(&language)
        } else {
            None
        };

        // When translation is enabled, prefer ISO code as filename
        let base_name = if translation_enabled {
            if let Some(ref code) = iso_code {
                code.clone()
            } else if !language.is_empty() {
                sanitize_filename(&language)
            } else if !title.is_empty() {
                sanitize_filename(&title)
            } else {
                format!("subtitle_{}", stream_idx)
            }
        } else {
            if !language.is_empty() {
                sanitize_filename(&language)
            } else if !title.is_empty() {
                sanitize_filename(&title)
            } else {
                format!("subtitle_{}", stream_idx)
            }
        };

        let mut final_name = base_name.clone();
        let mut counter = 1;
        while used_names.contains(&final_name) || final_name.is_empty() {
            final_name = format!("{}_{}", base_name, counter);
            counter += 1;
        }
        used_names.insert(final_name.clone());

        let output_file = format!("{}/{}.vtt", captions_dir, final_name);

        println!(
            "Preparing subtitle stream {} (language: '{}', title: '{}', codec: {}) to VTT as '{}' (iso: {:?})",
            stream_idx,
            if language.is_empty() { "unknown" } else { &language },
            if title.is_empty() { "none" } else { &title },
            codec,
            final_name,
            iso_code
        );

        outputs.push((stream_idx, final_name, output_file, language, title, iso_code));
    }

    // Build a single ffmpeg invocation that extracts all subtitle streams at once.
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin")
        .arg("-v").arg("error")
        .arg("-i").arg(input_file);

    for (stream_idx, _final_name, output_file, _language, _title, _iso) in &outputs {
        cmd.arg("-map").arg(format!("0:{}", stream_idx))
            .arg("-c:s").arg("webvtt")
            .arg(output_file);
    }

    cmd.arg("-y");

    println!("Extracting {} subtitle stream(s) to VTT...", outputs.len());
    let result = cmd.output();

    match result {
        Ok(output) => {
            if output.status.success() {
                for (_stream_idx, final_name, output_file, _language, _title, iso_code) in outputs {
                    match fs::metadata(&output_file) {
                        Ok(metadata) if metadata.len() > 0 => {
                            saved_files.push(final_name.clone());
                            available_subs.push((final_name, iso_code));
                        }
                        _ => { let _ = fs::remove_file(&output_file); }
                    }
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("Failed to extract subtitles to VTT: {}", stderr);
                for (_stream_idx, _final_name, output_file, _language, _title, _iso) in outputs {
                    let _ = fs::remove_file(&output_file);
                }
            }
        }
        Err(e) => {
            println!("Error executing ffmpeg for subtitle extraction: {}", e);
            for (_stream_idx, _final_name, output_file, _language, _title, _iso) in outputs {
                let _ = fs::remove_file(&output_file);
            }
        }
    }

    // Translate missing languages if translation is configured
    if translation_enabled && !available_subs.is_empty() {
        let all_names = ensure_configured_languages(&captions_dir, &available_subs, translation_config);
        create_list_txt(&captions_dir, &all_names);
        all_names
    } else {
        create_list_txt(&captions_dir, &saved_files);
        saved_files
    }
}

/// Probe audio duration from a media file.
/// Returns duration in seconds, with a fallback default if probing fails.
fn probe_audio_duration(input_file: &str) -> f64 {
    let probe_cmd = Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-select_streams").arg("a:0")
        .arg("-show_entries").arg("format=duration")
        .arg("-of").arg("default=noprint_wrappers=1:nokey=1")
        .arg(input_file)
        .output();

    match probe_cmd {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().next()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(3600.0)
        }
        _ => 3600.0
    }
}

/// Represents a detected silence interval in the audio.
#[derive(Debug, Clone)]
struct SilenceInterval {
    start: f64,
    end: f64,
}

impl SilenceInterval {
    fn midpoint(&self) -> f64 {
        (self.start + self.end) / 2.0
    }
}

/// Detect silence intervals in a specific time range of an audio file.
/// Uses `-ss` before `-i` for fast input-level seeking (keyframe seek),
/// then `-t` to limit the analysis duration.
/// Returns silence intervals with timestamps adjusted to absolute file positions.
fn detect_silence_in_range(
    input_file: &str,
    noise_db: f64,
    min_duration: f64,
    seek_start: f64,
    range_duration: f64,
) -> Vec<SilenceInterval> {
    let filter = format!("silencedetect=noise={}dB:d={}", noise_db, min_duration);
    let result = Command::new("ffmpeg")
        .arg("-nostdin")
        .arg("-v").arg("info")
        .arg("-ss").arg(format!("{:.3}", seek_start))
        .arg("-i").arg(input_file)
        .arg("-t").arg(format!("{:.3}", range_duration))
        .arg("-af").arg(&filter)
        .arg("-f").arg("null")
        .arg("-")
        .output();

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            println!(
                "Failed to run FFmpeg silencedetect for range {:.0}s-{:.0}s: {}",
                seek_start, seek_start + range_duration, e
            );
            return Vec::new();
        }
    };

    // When -ss is before -i, ffmpeg resets timestamps to 0 for the seeked segment,
    // so we offset all detected times by seek_start to get absolute positions.
    parse_silence_stderr(&String::from_utf8_lossy(&output.stderr), seek_start)
}

/// Parse silence_start / silence_end lines from ffmpeg stderr.
/// Adds `offset` to all timestamps (used for windowed detection where -ss resets timestamps).
fn parse_silence_stderr(stderr: &str, offset: f64) -> Vec<SilenceInterval> {
    let mut intervals = Vec::new();
    let mut current_start: Option<f64> = None;

    for line in stderr.lines() {
        if line.contains("silence_start:") {
            // Format: [silencedetect @ ...] silence_start: 123.456
            if let Some(val) = line.split("silence_start:").nth(1) {
                if let Ok(s) = val.trim().parse::<f64>() {
                    current_start = Some(s + offset);
                }
            }
        } else if line.contains("silence_end:") {
            // Format: [silencedetect @ ...] silence_end: 125.789 | silence_duration: 2.333
            if let Some(val) = line.split("silence_end:").nth(1) {
                // Take just the number before the pipe
                let end_str = val.split('|').next().unwrap_or("").trim();
                if let Ok(e) = end_str.parse::<f64>() {
                    if let Some(s) = current_start.take() {
                        intervals.push(SilenceInterval { start: s, end: e + offset });
                    }
                }
            }
        }
    }

    intervals
}

/// Detect silence only near expected split boundaries for improved performance.
/// Instead of scanning the entire file (which is very slow for long videos),
/// this runs parallel windowed silence detection around each expected chunk boundary.
///
/// For a 3-hour video with 10-minute targets, this typically reduces wall-clock time
/// from minutes to seconds by:
/// 1. Using `-ss` before `-i` for fast keyframe seeking (no decoding from start)
/// 2. Only analyzing ~6 minutes of audio per split point instead of the full file
/// 3. Running all windows in parallel via threads
fn detect_silence_near_splits(
    input_file: &str,
    noise_db: f64,
    min_duration: f64,
    total_duration: f64,
    target_secs: f64,
    max_secs: f64,
    parallel_limit: u32,
) -> Vec<SilenceInterval> {
    let extra = max_secs - target_secs; // how far past target we might need to search (e.g. 300s)
    let margin = 120.0; // look-back before target to account for accumulated drift

    // Calculate search windows around each expected split boundary
    let mut windows: Vec<(f64, f64)> = Vec::new();
    let mut pos = target_secs;
    while pos < total_duration {
        let win_start = (pos - margin).max(0.0);
        let win_end = (pos + extra + 60.0).min(total_duration); // extra buffer past max
        windows.push((win_start, win_end));
        pos += target_secs;
    }

    if windows.is_empty() {
        return Vec::new();
    }

    // Merge overlapping or nearly-adjacent windows to avoid redundant scanning
    windows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in windows {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 + 30.0 {
                // Windows overlap or are within 30s — merge them
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    let total_scan: f64 = merged.iter().map(|(s, e)| e - s).sum();
    println!(
        "Windowed silence detection: {} window(s) covering {:.0}s of {:.0}s ({:.0}% of file)",
        merged.len(),
        total_scan,
        total_duration,
        (total_scan / total_duration) * 100.0
    );

    // Run silence detection in parallel, limited to parallel_limit concurrent threads
    let batch_size = (parallel_limit.max(1)) as usize;
    println!("Running silence detection with parallel_limit={}", batch_size);
    let input = input_file.to_string();

    let mut all_silences: Vec<SilenceInterval> = Vec::new();
    for batch in merged.chunks(batch_size) {
        let handles: Vec<_> = batch
            .iter()
            .map(|&(win_start, win_end)| {
                let input = input.clone();
                std::thread::spawn(move || {
                    detect_silence_in_range(&input, noise_db, min_duration, win_start, win_end - win_start)
                })
            })
            .collect();

        for handle in handles {
            match handle.join() {
                Ok(mut silences) => all_silences.append(&mut silences),
                Err(_) => println!("Warning: silence detection thread panicked"),
            }
        }
    }

    // Sort by start time
    all_silences.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));

    // Deduplicate overlapping intervals that may result from overlapping windows
    let mut deduped: Vec<SilenceInterval> = Vec::new();
    for interval in all_silences {
        if let Some(last) = deduped.last_mut() {
            if interval.start <= last.end + 0.1 {
                last.end = last.end.max(interval.end);
                continue;
            }
        }
        deduped.push(interval);
    }

    deduped
}

/// Compute split points for audio based on silence intervals.
/// Targets chunks of ~target_secs, extending up to max_secs if no silence is
/// found at the target boundary.
/// Returns a list of split times (in seconds) where the audio should be cut.
fn compute_split_points(duration: f64, silences: &[SilenceInterval], target_secs: f64, max_secs: f64) -> Vec<f64> {
    let mut split_points: Vec<f64> = Vec::new();
    let mut current_pos = 0.0;

    while current_pos + target_secs < duration {
        let target = current_pos + target_secs;
        let max_end = current_pos + max_secs;

        // Find the best silence interval to split at.
        // Prefer silences closest to the target duration but within the max window.
        // Search window: from (target - 60s) to max_end for a silence boundary.
        let search_start = (target - 60.0).max(current_pos + 60.0);
        let search_end = max_end.min(duration);

        let best_silence = silences.iter()
            .filter(|s| s.midpoint() >= search_start && s.midpoint() <= search_end)
            .min_by(|a, b| {
                let dist_a = (a.midpoint() - target).abs();
                let dist_b = (b.midpoint() - target).abs();
                dist_a.partial_cmp(&dist_b).unwrap_or(std::cmp::Ordering::Equal)
            });

        let split_at = if let Some(silence) = best_silence {
            silence.midpoint()
        } else {
            // No silence found in window; force split at max to avoid cutting speech
            // at an arbitrary point, but we have no better option.
            max_end.min(duration)
        };

        // Don't add a split point if it's very close to the end of the audio
        if duration - split_at < 30.0 {
            break;
        }

        split_points.push(split_at);
        current_pos = split_at;
    }

    split_points
}

/// Parse a VTT timestamp (HH:MM:SS.mmm) into total seconds.
fn parse_vtt_timestamp(ts: &str) -> Option<f64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 { return None; }
    let hours: f64 = parts[0].trim().parse().ok()?;
    let minutes: f64 = parts[1].trim().parse().ok()?;
    let seconds: f64 = parts[2].trim().parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Format seconds back into VTT timestamp (HH:MM:SS.mmm).
fn format_vtt_timestamp(total_secs: f64) -> String {
    let total_secs = if total_secs < 0.0 { 0.0 } else { total_secs };
    let hours = (total_secs / 3600.0).floor() as u32;
    let minutes = ((total_secs % 3600.0) / 60.0).floor() as u32;
    let seconds = total_secs % 60.0;
    format!("{:02}:{:02}:{:06.3}", hours, minutes, seconds)
}

/// Offset all timestamps in a VTT string by the given number of seconds.
/// Skips the WEBVTT header and any cue identifiers; only adjusts "HH:MM:SS.mmm --> HH:MM:SS.mmm" lines.
fn offset_vtt(vtt: &str, offset_secs: f64) -> String {
    let mut out = String::with_capacity(vtt.len());
    for line in vtt.lines() {
        if line.contains(" --> ") {
            // Timestamp line: "00:01:23.456 --> 00:01:27.890"
            let parts: Vec<&str> = line.splitn(2, " --> ").collect();
            if parts.len() == 2 {
                if let (Some(start), Some(end)) = (parse_vtt_timestamp(parts[0]), parse_vtt_timestamp(parts[1])) {
                    out.push_str(&format_vtt_timestamp(start + offset_secs));
                    out.push_str(" --> ");
                    out.push_str(&format_vtt_timestamp(end + offset_secs));
                    out.push('\n');
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Send a single audio file to the Whisper.cpp server and return the VTT text.
fn whisper_transcribe_file(audio_path: &str, whisper_config: &WhisperConfig, timeout_secs: u64) -> Option<String> {
    let form = match multipart::Form::new()
        .text("response_format", whisper_config.response_format.clone())
        .text("model", whisper_config.model.clone())
        .text("temperature", whisper_config.temperature.to_string())
        .file("file", audio_path)
    {
        Ok(f) => f,
        Err(e) => {
            println!("Failed to read audio file for upload: {}", e);
            return None;
        }
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap();

    match client.post(&whisper_config.url).multipart(form).send() {
        Ok(res) if res.status().is_success() => res.text().ok(),
        Ok(res) => {
            println!("Whisper API returned an error: {}", res.status());
            None
        }
        Err(e) => {
            println!("Failed to connect to Whisper API: {}", e);
            None
        }
    }
}

/// Helper function to generate subtitles using Whisper.cpp API.
/// Audio is optimized for whisper.cpp (16 kHz, mono, PCM_s16le).
/// Long files are split at silence boundaries with a target of 10 minutes per chunk
/// and a maximum of 15 minutes to avoid cutting through speech.
fn generate_whisper_vtt(input_file: &str, output_dir: &str, whisper_config: &WhisperConfig, translation_config: &TranslationConfig) -> Vec<String> {
    let captions_dir = format!("{}/captions", output_dir);
    fs::create_dir_all(&captions_dir).expect("Failed to create captions directory");

    let translation_enabled = !translation_config.languages.is_empty();

    // Detect language if translation is enabled
    let detected_lang = if translation_enabled {
        println!("Detecting audio language via Whisper...");
        let lang = detect_language_via_whisper(input_file, output_dir, whisper_config);
        if let Some(ref l) = lang {
            println!("Detected audio language: {}", l);
        } else {
            println!("Language detection failed or unavailable.");
        }
        lang
    } else {
        None
    };

    // Use AI_<detected_lang> naming when translation is enabled, otherwise use config output_label
    let output_label = if let Some(ref lang) = detected_lang {
        format!("AI_{}", lang)
    } else {
        whisper_config.output_label.clone()
    };

    let duration = probe_audio_duration(input_file);

    let target_chunk = whisper_config.target_chunk_secs;
    let max_chunk = whisper_config.max_chunk_secs;

    if duration > target_chunk {
        // Long audio: detect silence and split at silence boundaries
        println!(
            "Audio duration {:.0}s exceeds {} min target. Detecting silence for smart splitting...",
            duration, (target_chunk / 60.0) as u32
        );

        let silences = detect_silence_near_splits(
            input_file,
            whisper_config.silence_noise_db,
            whisper_config.silence_min_duration,
            duration,
            target_chunk,
            max_chunk,
            whisper_config.silence_detect_parallel,
        );
        println!("Detected {} silence intervals near split boundaries.", silences.len());

        let split_points = compute_split_points(duration, &silences, target_chunk, max_chunk);

        // Build chunk boundaries: [(start, end), ...]
        let mut boundaries: Vec<(f64, f64)> = Vec::new();
        let mut prev = 0.0;
        for &sp in &split_points {
            boundaries.push((prev, sp));
            prev = sp;
        }
        boundaries.push((prev, duration));

        println!(
            "Splitting into {} chunks for Whisper (16kHz, mono, PCM_s16le)...",
            boundaries.len()
        );
        for (i, (start, end)) in boundaries.iter().enumerate() {
            println!("  Chunk {}: {:.1}s - {:.1}s ({:.1}s)", i + 1, start, end, end - start);
        }

        // Extract each chunk as a WAV file
        let mut chunk_files: Vec<(String, f64)> = Vec::new(); // (path, offset_secs)
        for (i, (start, end)) in boundaries.iter().enumerate() {
            let chunk_duration = end - start;
            let chunk_path = format!("{}/whisper_chunk_{}.wav", captions_dir, i);
            let result = Command::new("ffmpeg")
                .arg("-nostdin")
                .arg("-v").arg("error")
                .arg("-i").arg(input_file)
                .arg("-ss").arg(format!("{:.3}", start))
                .arg("-t").arg(format!("{:.3}", chunk_duration))
                .arg("-ar").arg("16000")
                .arg("-ac").arg("1")
                .arg("-c:a").arg("pcm_s16le")
                .arg("-y")
                .arg(&chunk_path)
                .output();
            match result {
                Ok(output) if output.status.success() => chunk_files.push((chunk_path, *start)),
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    println!("Failed to extract chunk {}: {}", i + 1, stderr);
                }
                Err(e) => println!("Failed to run ffmpeg for chunk {}: {}", i + 1, e),
            }
        }

        if chunk_files.is_empty() {
            println!("Failed to extract any audio chunks for Whisper.");
            return Vec::new();
        }

        // Transcribe each chunk and merge VTT results
        let mut merged_vtt = String::from("WEBVTT\n\n");
        let mut any_success = false;

        for (i, (chunk_path, offset_secs)) in chunk_files.iter().enumerate() {
            let chunk_duration = boundaries[i].1 - boundaries[i].0;
            let chunk_timeout = (chunk_duration * 2.0).ceil() as u64;
            println!(
                "Transcribing chunk {}/{} (offset {:.0}s, duration {:.0}s) via Whisper.cpp at {}...",
                i + 1, chunk_files.len(), offset_secs, chunk_duration, whisper_config.url
            );

            if let Some(vtt_text) = whisper_transcribe_file(chunk_path, whisper_config, chunk_timeout) {
                let body = vtt_text.trim_start_matches("WEBVTT").trim_start();
                if !body.is_empty() {
                    let offset_body = offset_vtt(body, *offset_secs);
                    merged_vtt.push_str(&offset_body);
                    if !merged_vtt.ends_with('\n') {
                        merged_vtt.push('\n');
                    }
                    merged_vtt.push('\n');
                    any_success = true;
                }
            } else {
                println!("Warning: failed to transcribe chunk {}, gap in subtitles.", i + 1);
            }
        }

        // Clean up chunk files
        for (chunk_path, _) in &chunk_files {
            let _ = fs::remove_file(chunk_path);
        }

        let mut saved_files = Vec::new();
        if any_success {
            let final_name = output_label.clone();
            let output_file = format!("{}/{}.vtt", captions_dir, final_name);
            if fs::write(&output_file, &merged_vtt).is_ok() {
                println!("Successfully generated merged VTT via Whisper.cpp as '{}' ({} chunks, silence-based splitting).", final_name, chunk_files.len());
                saved_files.push(final_name);
            }
        }

        // Translate missing languages if translation is configured
        if translation_enabled && !saved_files.is_empty() {
            let available_subs: Vec<(String, Option<String>)> = saved_files
                .iter()
                .map(|name| (name.clone(), detected_lang.clone()))
                .collect();
            let all_names = ensure_configured_languages(&captions_dir, &available_subs, translation_config);
            create_list_txt(&captions_dir, &all_names);
            all_names
        } else {
            create_list_txt(&captions_dir, &saved_files);
            saved_files
        }
    } else {
        // Short audio (under target chunk size): single-file path
        let temp_audio = format!("{}/temp_audio.wav", captions_dir);
        let timeout_secs = (duration * 2.0).ceil() as u64;

        println!("Extracting audio for Whisper (16kHz, mono, PCM_s16le)...");
        let audio_cmd = Command::new("ffmpeg")
            .arg("-nostdin")
            .arg("-v").arg("error")
            .arg("-i").arg(input_file)
            .arg("-ar").arg("16000")
            .arg("-ac").arg("1")
            .arg("-c:a").arg("pcm_s16le")
            .arg("-y")
            .arg(&temp_audio)
            .output();

        if let Err(e) = audio_cmd {
            println!("Failed to extract audio for Whisper: {}", e);
            return Vec::new();
        }

        println!("Sending audio to Whisper.cpp API at {} (timeout: {}s)...", whisper_config.url, timeout_secs);

        let mut saved_files = Vec::new();

        if let Some(vtt_content) = whisper_transcribe_file(&temp_audio, whisper_config, timeout_secs) {
            let final_name = output_label.clone();
            let output_file = format!("{}/{}.vtt", captions_dir, final_name);
            if fs::write(&output_file, &vtt_content).is_ok() {
                println!("Successfully generated VTT via Whisper.cpp as '{}'.", final_name);
                saved_files.push(final_name);
            }
        }

        let _ = fs::remove_file(&temp_audio);

        // Translate missing languages if translation is configured
        if translation_enabled && !saved_files.is_empty() {
            let available_subs: Vec<(String, Option<String>)> = saved_files
                .iter()
                .map(|name| (name.clone(), detected_lang.clone()))
                .collect();
            let all_names = ensure_configured_languages(&captions_dir, &available_subs, translation_config);
            create_list_txt(&captions_dir, &all_names);
            all_names
        } else {
            create_list_txt(&captions_dir, &saved_files);
            saved_files
        }
    }
}

fn create_list_txt(captions_dir: &str, saved_files: &[String]) {
    if !saved_files.is_empty() {
        let list_file_path = format!("{}/list.txt", captions_dir);
        let content = saved_files.join("\n");
        if let Err(e) = fs::write(&list_file_path, content) {
            println!("Failed to create list.txt: {}", e);
        } else {
            println!("Created list.txt with {} subtitle entries.", saved_files.len());
        }
    }
}

// --- Subtitle translation support ---

/// A single VTT cue with timestamp range and text content.
struct VttCue {
    start: String,
    end: String,
    text: String,
}

/// Parse a WebVTT file into individual cues.
fn parse_vtt_cues(vtt_content: &str) -> Vec<VttCue> {
    let mut cues = Vec::new();
    let mut lines = vtt_content.lines().peekable();

    // Skip past WEBVTT header and any metadata until first timestamp line
    while let Some(&line) = lines.peek() {
        if line.contains(" --> ") {
            break;
        }
        lines.next();
    }

    while lines.peek().is_some() {
        let line = match lines.next() {
            Some(l) => l,
            None => break,
        };

        if line.contains(" --> ") {
            let parts: Vec<&str> = line.splitn(2, " --> ").collect();
            if parts.len() == 2 {
                let start = parts[0].trim().to_string();
                let end = parts[1].trim().to_string();
                let mut text_lines = Vec::new();

                // Collect text lines until empty line or end of input
                while let Some(&next_line) = lines.peek() {
                    if next_line.trim().is_empty() {
                        lines.next();
                        break;
                    }
                    text_lines.push(lines.next().unwrap().to_string());
                }

                if !text_lines.is_empty() {
                    cues.push(VttCue {
                        start,
                        end,
                        text: text_lines.join("\n"),
                    });
                }
            }
        }
    }

    cues
}

/// Rebuild a WebVTT file from parsed cues.
fn build_vtt_from_cues(cues: &[VttCue]) -> String {
    let mut vtt = String::from("WEBVTT\n\n");
    for cue in cues {
        vtt.push_str(&cue.start);
        vtt.push_str(" --> ");
        vtt.push_str(&cue.end);
        vtt.push('\n');
        vtt.push_str(&cue.text);
        vtt.push_str("\n\n");
    }
    vtt
}

/// Strips common LLM preamble patterns from translation output.
/// Models sometimes output phrases like "Here are translation options:" or
/// "Zde je několik možností překladu, v závislosti na kontextu:" before the
/// actual translated text.
fn strip_translation_preamble(text: &str) -> String {
    let trimmed = text.trim();

    // If the first line ends with a colon it's an introductory preamble.
    let first_newline = trimmed.find('\n');
    let first_line = match first_newline {
        Some(pos) => &trimmed[..pos],
        None => trimmed,
    };

    if first_line.trim_end().ends_with(':') {
        if let Some(pos) = first_newline {
            let rest = trimmed[pos..].trim();
            if !rest.is_empty() {
                // Strip a leading list marker such as "1. ", "1) ", "- ", "* "
                let without_marker = strip_list_prefix(rest);
                // Take only the first option (up to the next newline)
                let first_option = without_marker.lines().next().unwrap_or("").trim();
                if !first_option.is_empty() {
                    return first_option.to_string();
                }
            }
        }
        // Only the preamble came through with nothing after it — signal failure.
        return String::new();
    }

    trimmed.to_string()
}

fn strip_list_prefix(text: &str) -> &str {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() > 2 {
        // "1. " / "1) " / "2. " etc.
        if bytes[0].is_ascii_digit() && (bytes[1] == b'.' || bytes[1] == b')') && bytes[2] == b' ' {
            return text[3..].trim();
        }
        // "- " / "* "
        if (bytes[0] == b'-' || bytes[0] == b'*') && bytes[1] == b' ' {
            return text[2..].trim();
        }
    }
    text
}

fn translate_text_via_llama(
    client: &Client,
    text: &str,
    target_lang: &str,
    llama_url: &str,
) -> Option<String> {
    let single_line = text.replace('\n', " ");

    let prompt = format!(
        "<start_of_turn>user\nTranslate the following to {}. Output ONLY the translated text, no explanations or alternatives:\n{}<end_of_turn>\n<start_of_turn>model\n",
        target_lang, single_line
    );

    let body = json!({
        "prompt": prompt,
        "n_predict": 1024,
        "temperature": 0.1,
        "stop": ["<end_of_turn>", "<start_of_turn>", "\n\n"],
        "cache_prompt": true
    });

    let url = format!("{}/completion", llama_url.trim_end_matches('/'));

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| {
            eprintln!("llama.cpp request failed: {}", e);
            e
        })
        .ok()?;

    if !response.status().is_success() {
        eprintln!("llama.cpp returned error status: {}", response.status());
        return None;
    }

    let json: serde_json::Value = response.json().ok()?;

    json.get("content")
        .and_then(|v| v.as_str())
        .map(|s| strip_translation_preamble(s))
        .filter(|s| !s.is_empty())
}

fn translate_subtitle_file(
    source_path: &str,
    source_lang: &str,
    target_lang: &str,
    output_path: &str,
    translation_config: &TranslationConfig,
) -> bool {
    let vtt_content = match fs::read_to_string(source_path) {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to read source VTT for translation: {}", e);
            return false;
        }
    };

    let cues = parse_vtt_cues(&vtt_content);
    if cues.is_empty() {
        println!("No cues found in source VTT for translation.");
        return false;
    }

    println!(
        "Translating {} cues from {} to {} via TranslateGemma at {}...",
        cues.len(), source_lang, target_lang, translation_config.llama_url
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(translation_config.timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to create HTTP client for translation: {}", e);
            return false;
        }
    };

    let mut translated_cues = Vec::new();
    let total = cues.len();

    for (i, cue) in cues.iter().enumerate() {
        let translated_text = translate_text_via_llama(
            &client, &cue.text, target_lang, &translation_config.llama_url,
        );

        match translated_text {
            Some(text) if !text.is_empty() => {
                translated_cues.push(VttCue {
                    start: cue.start.clone(),
                    end: cue.end.clone(),
                    text,
                });
            }
            _ => {
                // Keep original text if translation fails
                println!("Warning: failed to translate cue {}/{}, keeping original.", i + 1, total);
                translated_cues.push(VttCue {
                    start: cue.start.clone(),
                    end: cue.end.clone(),
                    text: cue.text.clone(),
                });
            }
        }

        if (i + 1) % 100 == 0 {
            println!("Translation progress: {}/{} cues ({} -> {})", i + 1, total, source_lang, target_lang);
        }
    }

    let output_vtt = build_vtt_from_cues(&translated_cues);
    match fs::write(output_path, &output_vtt) {
        Ok(()) => {
            println!(
                "Successfully translated subtitles {} -> {} ({} cues).",
                source_lang, target_lang, total
            );
            true
        }
        Err(e) => {
            println!("Failed to write translated VTT: {}", e);
            false
        }
    }
}

/// Ensure all configured subtitle languages are available by translating from existing subtitles.
/// Prefers the configured source_language (default: "en") as the translation source.
/// available_subs is a list of (filename_without_ext, optional_iso_lang_code).
fn ensure_configured_languages(
    captions_dir: &str,
    available_subs: &[(String, Option<String>)],
    translation_config: &TranslationConfig,
) -> Vec<String> {
    let mut all_names: Vec<String> = available_subs.iter().map(|(name, _)| name.clone()).collect();

    if translation_config.languages.is_empty() {
        return all_names;
    }

    // Determine which configured languages are already covered
    let covered_langs: std::collections::HashSet<String> = available_subs
        .iter()
        .filter_map(|(_, lang)| lang.clone())
        .collect();

    let missing_langs: Vec<&String> = translation_config
        .languages
        .iter()
        .filter(|lang| !covered_langs.contains(*lang))
        .collect();

    if missing_langs.is_empty() {
        println!("All configured subtitle languages are already available.");
        return all_names;
    }

    println!(
        "Missing subtitle languages: {:?}. Available: {:?}",
        missing_langs,
        available_subs.iter().map(|(n, l)| format!("{}({})", n, l.as_deref().unwrap_or("?"))).collect::<Vec<_>>()
    );

    // Find best source subtitle: prefer configured source_language (default "en")
    let source = available_subs
        .iter()
        .find(|(_, lang)| lang.as_deref() == Some(&translation_config.source_language))
        .or_else(|| available_subs.first());

    let (source_name, source_lang) = match source {
        Some((name, lang)) => (
            name.clone(),
            lang.clone().unwrap_or_else(|| "unknown".to_string()),
        ),
        None => {
            println!("No source subtitle available for translation.");
            return all_names;
        }
    };

    let source_path = format!("{}/{}.vtt", captions_dir, source_name);
    println!(
        "Using '{}' ({}) as translation source.",
        source_name, source_lang
    );

    for target_lang in &missing_langs {
        let output_name = format!("AI_{}", target_lang);
        let output_path = format!("{}/{}.vtt", captions_dir, output_name);

        if translate_subtitle_file(
            &source_path,
            &source_lang,
            target_lang,
            &output_path,
            translation_config,
        ) {
            all_names.push(output_name);
        }
    }

    all_names
}

fn extract_chapters_to_vtt(input_file: &str, output_dir: &str) {
    // Probe chapters using ffprobe JSON output
    let mut cmd = Command::new("ffprobe");
    cmd.arg("-v")
        .arg("error")
        .arg("-show_chapters")
        .arg("-print_format")
        .arg("json")
        .arg(input_file);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            println!("Failed to probe chapters: {}", e);
            return;
        }
    };

    if !output.status.success() {
        return;
    }

    let parsed: FfprobeChaptersOutput = match serde_json::from_slice(&output.stdout) {
        Ok(p) => p,
        Err(_) => return,
    };

    let chapters = match parsed.chapters {
        Some(c) if !c.is_empty() => c,
        _ => return,
    };

    // Build WebVTT content
    let mut vtt_content = String::from("WEBVTT\n\n");

    for chapter in &chapters {
        let start_seconds: f64 = chapter
            .start_time
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let end_seconds: f64 = chapter
            .end_time
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        let title = chapter
            .tags
            .as_ref()
            .and_then(|t| t.title.as_deref())
            .unwrap_or("");

        if title.is_empty() {
            continue;
        }

        let start_formatted = format_timestamp_vtt(start_seconds);
        let end_formatted = format_timestamp_vtt(end_seconds);

        vtt_content.push_str(&format!(
            "{} --> {}\n{}\n\n",
            start_formatted, end_formatted, title
        ));
    }

    // Only write if we had at least one chapter with a title
    if vtt_content.len() > "WEBVTT\n\n".len() {
        let output_path = format!("{}/chapters.vtt", output_dir);
        match fs::write(&output_path, &vtt_content) {
            Ok(_) => println!(
                "Extracted {} chapters to {}",
                chapters.len(),
                output_path
            ),
            Err(e) => println!("Failed to write chapters.vtt: {}", e),
        }
    }
}

async fn transcode_picture(input_file: &str, output_dir: &str, picture_config: &PictureConfig) -> Result<(), ffmpeg_next::Error> {
    // Get image dimensions
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 '{}'",
        input_file
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&probe_cmd)
        .output()
        .map_err(|_| ffmpeg_next::Error::External)?;
    let dimensions = String::from_utf8_lossy(&output.stdout);
    let (orig_width, orig_height) = parse_dimensions(&dimensions);

    // Calculate scaled dimensions for HD thumbnail (closest to target while maintaining aspect ratio)
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height, picture_config.thumbnail_width, picture_config.thumbnail_height);

    // Run all three picture transcodes in parallel
    let transcode_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -c:v libsvtav1 -svtav1-params avif=1 -crf {} -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, picture_config.crf, output_dir
    );
    let thumbnail_cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -c:v libsvtav1 -svtav1-params avif=1 -crf {} -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
            input_file, picture_config.thumbnail_crf, thumb_width, thumb_height, output_dir
        );
    let thumbnail_ogp_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v {} '{}/thumbnail.jpg'",
        input_file, thumb_width, thumb_height, picture_config.jpg_quality, output_dir
    );

    let (r1, r2, r3) = tokio::join!(
        task::spawn_blocking(move || {
            println!("Executing: {}", transcode_cmd);
            Command::new("sh").arg("-c").arg(&transcode_cmd).status()
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_cmd);
            Command::new("sh").arg("-c").arg(&thumbnail_cmd).status()
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_ogp_cmd);
            Command::new("sh").arg("-c").arg(&thumbnail_ogp_cmd).status()
        })
    );

    // Check results
    match r1 {
        Ok(Ok(s)) if s.success() => {}
        _ => {
            eprintln!("Failed to transcode picture to AVIF");
            return Err(ffmpeg_next::Error::External);
        }
    }
    match r2 {
        Ok(Ok(s)) if s.success() => {}
        _ => {
            eprintln!("Failed to transcode picture thumbnail AVIF");
            return Err(ffmpeg_next::Error::External);
        }
    }
    match r3 {
        Ok(Ok(s)) if s.success() => {}
        _ => {
            eprintln!("Failed to transcode picture thumbnail JPG");
            return Err(ffmpeg_next::Error::External);
        }
    }

    Ok(())
}

fn parse_dimensions(dim_output: &str) -> (u32, u32) {
    let cleaned = dim_output.trim();
    let parts: Vec<&str> = cleaned.split('x').collect();
    if parts.len() == 2 {
        let width = parts[0].parse().unwrap_or(1280);
        let height = parts[1].parse().unwrap_or(720);
        return (width, height);
    }
    (1280, 720) // Default fallback
}

fn get_audio_codec(input_file: &str) -> String {
    let codec_cmd = format!(
        "ffprobe -v error -select_streams a:0 -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    let output = Command::new("sh").arg("-c").arg(&codec_cmd).output();

    match output {
        Ok(result) if result.status.success() => {
            let codec = String::from_utf8_lossy(&result.stdout);
            codec.trim().to_lowercase()
        }
        _ => "unknown".to_string(),
    }
}

fn get_audio_bitrate(input_file: &str) -> u32 {
    let bitrate_cmd = format!(
        "ffprobe -v error -select_streams a:0 -show_entries stream=bit_rate -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    let output = Command::new("sh").arg("-c").arg(&bitrate_cmd).output();

    match output {
        Ok(result) if result.status.success() => {
            let bitrate_str = String::from_utf8_lossy(&result.stdout);
            bitrate_str.trim().parse().unwrap_or(128000)
        }
        _ => 128000, // Default to 128 kbps if unknown
    }
}

fn has_video_stream(input_file: &str) -> bool {
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    match Command::new("sh").arg("-c").arg(&probe_cmd).output() {
        Ok(result) if result.status.success() => {
            let output_str = String::from_utf8_lossy(&result.stdout);
            output_str.contains("video")
        }
        _ => false,
    }
}

fn is_cover_art_video(input_file: &str) -> bool {
    // Check if video stream duration is very short compared to audio
    // This typically indicates cover art/album art in a music file
    let duration_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=duration -of default=noprint_wrappers=1:nokey=1 '{}'",
        input_file
    );

    match Command::new("sh").arg("-c").arg(&duration_cmd).output() {
        Ok(result) if result.status.success() => {
            let dur_str = String::from_utf8_lossy(&result.stdout);
            match dur_str.trim().parse::<f64>() {
                Ok(duration) if duration <= 1.0 => true,
                _ => false,
            }
        }
        _ => false,
    }
}

fn count_audio_streams(input_file: &str) -> u32 {
    let count_cmd = format!(
        "ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 '{}' | wc -l",
        input_file
    );

    match Command::new("sh").arg("-c").arg(&count_cmd).output() {
        Ok(result) if result.status.success() => {
            let count_str = String::from_utf8_lossy(&result.stdout);
            count_str.trim().parse().unwrap_or(1)
        }
        _ => 1,
    }
}

fn count_video_streams(input_file: &str) -> u32 {
    let count_cmd = format!(
        "ffprobe -v error -select_streams v -show_entries stream=index -of csv=p=0 '{}' | wc -l",
        input_file
    );

    match Command::new("sh").arg("-c").arg(&count_cmd).output() {
        Ok(result) if result.status.success() => {
            let count_str = String::from_utf8_lossy(&result.stdout);
            count_str.trim().parse().unwrap_or(0)
        }
        _ => 0,
    }
}

fn extract_additional_audio(input_file: &str, output_dir: &str, audio_config: &AudioTranscodeConfig) -> Result<(), ffmpeg_next::Error> {
    // Get total number of audio streams
    let count_cmd = format!(
        "ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 '{}'",
        input_file
    );

    let output = Command::new("sh")
        .arg("-c")
        .arg(&count_cmd)
        .output()
        .map_err(|_| ffmpeg_next::Error::External)?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    let streams: Vec<&str> = output_str.trim().split('\n').collect();

    // Skip the first audio stream (usually the main one) and process additional ones
    for (idx, _) in streams.iter().enumerate().skip(1) {
        let stream_idx = idx; // 0-based index for ffmpeg (stream 1 is index 0)
        let audio_codec = get_audio_codec_for_stream(input_file, stream_idx as u32);

        let bitrate = if audio_config.lossless_codecs.iter().any(|c| c == &audio_codec) {
            &audio_config.lossless_bitrate
        } else {
            &audio_config.lossy_bitrate
        };

        let output_path = format!("{}/audio_{}.{}", output_dir, idx + 1, audio_config.output_format);
        let extract_cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -map 0:a:{} -c:a {} -b:a {} -vbr {} -application {} '{}'",
            input_file, stream_idx, audio_config.codec, bitrate, audio_config.vbr, audio_config.application, output_path
        );

        println!("Executing: {}", extract_cmd);
        let status = Command::new("sh")
            .arg("-c")
            .arg(&extract_cmd)
            .status();
        if let Err(e) = status {
            eprintln!("Warning: Failed to extract audio stream {}: {}", idx + 1, e);
        }
    }

    Ok(())
}

fn get_audio_codec_for_stream(input_file: &str, stream_idx: u32) -> String {
    let codec_cmd = format!(
        "ffprobe -v error -select_streams a:{} -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 '{}'",
        stream_idx, input_file
    );

    let output = Command::new("sh").arg("-c").arg(&codec_cmd).output();

    match output {
        Ok(result) if result.status.success() => {
            let codec = String::from_utf8_lossy(&result.stdout);
            codec.trim().to_lowercase()
        }
        _ => "unknown".to_string(),
    }
}

async fn extract_secondary_video_as_cover(
    input_file: &str,
    output_dir: &str,
    picture_config: &PictureConfig,
) -> Result<(), ffmpeg_next::Error> {
    // Get dimensions of the secondary video stream (usually the cover art)
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:1 -show_entries stream=width,height -of csv=s=x:p=0 '{}'",
        input_file
    );

    let output = Command::new("sh").arg("-c").arg(&probe_cmd).output();

    let (orig_width, orig_height) = match output {
        Ok(result) if result.status.success() => {
            let dimensions = String::from_utf8_lossy(&result.stdout);
            parse_dimensions(&dimensions)
        }
        _ => {
            // Fallback to first video stream
            let probe_cmd = format!(
                "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 '{}'",
                input_file
            );
            let output = Command::new("sh")
                .arg("-c")
                .arg(&probe_cmd)
                .output()
                .map_err(|_| ffmpeg_next::Error::External)?;
            let dimensions = String::from_utf8_lossy(&output.stdout);
            parse_dimensions(&dimensions)
        }
    };

    // Calculate scaled dimensions for HD thumbnail
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height, picture_config.thumbnail_width, picture_config.thumbnail_height);

    // Check if we have multiple video streams
    let video_count = count_video_streams(input_file);
    let stream_selector = if video_count > 1 { "v:1" } else { "v:0" };

    // Run all three cover extractions in parallel
    let cover_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf {} -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, stream_selector, picture_config.cover_crf, output_dir
    );
    let thumbnail_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf {} -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
        input_file, stream_selector, picture_config.cover_thumbnail_crf, thumb_width, thumb_height, output_dir
    );
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v {} '{}/thumbnail.jpg'",
        input_file, stream_selector, thumb_width, thumb_height, picture_config.jpg_quality, output_dir
    );

    let (_, _, _) = tokio::join!(
        task::spawn_blocking(move || {
            println!("Executing: {}", cover_cmd);
            let _ = Command::new("sh").arg("-c").arg(&cover_cmd).status();
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_cmd);
            let _ = Command::new("sh").arg("-c").arg(&thumbnail_cmd).status();
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_jpg_cmd);
            let _ = Command::new("sh").arg("-c").arg(&thumbnail_jpg_cmd).status();
        })
    );

    Ok(())
}

async fn extract_album_cover(input_file: &str, output_dir: &str, picture_config: &PictureConfig) -> Result<(), ffmpeg_next::Error> {
    // Get album cover dimensions
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 '{}'",
        input_file
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&probe_cmd)
        .output()
        .map_err(|_| ffmpeg_next::Error::External)?;
    let dimensions = String::from_utf8_lossy(&output.stdout);
    let (orig_width, orig_height) = parse_dimensions(&dimensions);

    // Calculate scaled dimensions for HD thumbnail
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height, picture_config.thumbnail_width, picture_config.thumbnail_height);

    // Run all three cover extractions in parallel
    let cover_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf {} -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, picture_config.cover_crf, output_dir
    );
    let thumbnail_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf {} -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
        input_file, picture_config.cover_thumbnail_crf, thumb_width, thumb_height, output_dir
    );
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v {} '{}/thumbnail.jpg'",
        input_file, thumb_width, thumb_height, picture_config.jpg_quality, output_dir
    );

    let (_, _, _) = tokio::join!(
        task::spawn_blocking(move || {
            println!("Executing: {}", cover_cmd);
            let _ = Command::new("sh").arg("-c").arg(&cover_cmd).status();
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_cmd);
            let _ = Command::new("sh").arg("-c").arg(&thumbnail_cmd).status();
        }),
        task::spawn_blocking(move || {
            println!("Executing: {}", thumbnail_jpg_cmd);
            let _ = Command::new("sh").arg("-c").arg(&thumbnail_jpg_cmd).status();
        })
    );

    Ok(())
}

async fn transcode_audio(input_file: &str, output_dir: &str, audio_config: &AudioTranscodeConfig, picture_config: &PictureConfig) -> Result<(), ffmpeg_next::Error> {
    // Detect source codec to determine bitrate
    let source_codec = get_audio_codec(input_file);
    println!("Detected audio codec: {}", source_codec);

    // Lossless codecs get higher bitrate
    let bitrate = if audio_config.lossless_codecs.iter().any(|c| c == &source_codec) {
        &audio_config.lossless_bitrate
    } else {
        &audio_config.lossy_bitrate
    };

    println!("Using bitrate: {} for {} codec", bitrate, source_codec);

    // Transcode to configured format with configured codec
    let output_path = format!("{}/audio.{}", output_dir, audio_config.output_format);
    let transcode_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:a:0 -c:a {} -b:a {} -vbr {} -application {} '{}'",
        input_file, audio_config.codec, bitrate, audio_config.vbr, audio_config.application, output_path
    );
    let transcode_cmd_owned = transcode_cmd.clone();
    let status = task::spawn_blocking(move || {
        println!("Executing: {}", transcode_cmd_owned);
        Command::new("sh")
            .arg("-c")
            .arg(&transcode_cmd_owned)
            .status()
    }).await.map_err(|_| ffmpeg_next::Error::External)?.map_err(|_| ffmpeg_next::Error::External)?;
    if !status.success() {
        eprintln!("Failed to transcode audio to Opus");
        return Err(ffmpeg_next::Error::External);
    }

    // Extract additional audio streams and album cover in parallel
    let audio_stream_count = count_audio_streams(input_file);
    let has_video = has_video_stream(input_file);

    let mut handles: Vec<task::JoinHandle<()>> = Vec::new();

    if audio_stream_count > 1 {
        println!("Found {} audio streams, extracting additional streams...", audio_stream_count);
        let input_owned = input_file.to_string();
        let output_owned = output_dir.to_string();
        let audio_config_owned = audio_config.clone();
        handles.push(task::spawn_blocking(move || {
            let _ = extract_additional_audio(&input_owned, &output_owned, &audio_config_owned);
        }));
    }

    if has_video {
        println!("Found album cover in audio file, extracting...");
        let input_owned = input_file.to_string();
        let output_owned = output_dir.to_string();
        let picture_config_owned = picture_config.clone();
        handles.push(tokio::spawn(async move {
            let _ = extract_album_cover(&input_owned, &output_owned, &picture_config_owned).await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

fn calculate_hd_scale(width: u32, height: u32, target_width: u32, target_height: u32) -> (u32, u32) {

    // Calculate aspect ratios
    let aspect_ratio = width as f32 / height as f32;
    let target_aspect = target_width as f32 / target_height as f32;

    let (new_width, new_height) = if aspect_ratio > target_aspect {
        // Image is wider than 16:9, scale by width
        let scaled_height = (target_width as f32 / aspect_ratio) as u32;
        (target_width, scaled_height.min(target_height))
    } else {
        // Image is taller than 16:9, scale by height
        let scaled_width = (target_height as f32 * aspect_ratio) as u32;
        (scaled_width.min(target_width), target_height)
    };

    // Ensure dimensions are even (required for some codecs)
    (new_width / 2 * 2, new_height / 2 * 2)
}

#[derive(Serialize, Deserialize)]
struct ThumbnailCue {
    start_time: f64,
    end_time: f64,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    sprite_idx: u32,
}

#[derive(Clone, Copy)]
enum EncoderType {
    Nvenc,
    Qsv,
    Vaapi,
}

#[derive(Debug, Clone)]
struct HdrInfo {
    is_hdr: bool,
    color_transfer: Option<String>,
    color_primaries: Option<String>,
    color_space: Option<String>,
}

fn detect_hdr(input_file: &str) -> HdrInfo {
    let cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=color_transfer,color_primaries,color_space -of default=noprint_wrappers=1 '{}'",
        input_file
    );

    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output();

    let mut hdr_info = HdrInfo {
        is_hdr: false,
        color_transfer: None,
        color_primaries: None,
        color_space: None,
    };

    match output {
        Ok(result) if result.status.success() => {
            let stdout = String::from_utf8_lossy(&result.stdout);
            for line in stdout.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    let value = value.trim().to_string();
                    match key.trim() {
                        "color_transfer" => {
                            hdr_info.color_transfer = Some(value.clone());
                            // HDR transfers: smpte2084 (PQ), arib-std-b67 (HLG)
                            if value == "smpte2084" || value == "arib-std-b67" {
                                hdr_info.is_hdr = true;
                            }
                        }
                        "color_primaries" => {
                            hdr_info.color_primaries = Some(value.clone());
                            // BT.2020 is commonly used with HDR
                            if value == "bt2020" {
                                hdr_info.is_hdr = true;
                            }
                        }
                        "color_space" => {
                            hdr_info.color_space = Some(value.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    hdr_info
}

fn build_encoder_params(config: &VideoConfig, framerate: f32, hdr_info: &HdrInfo) -> (String, String, String, EncoderType) {
        // Build tonemapping filter if HDR is detected
        let tonemap_filter = if hdr_info.is_hdr {
            println!("HDR detected: transfer={:?}, primaries={:?}, space={:?}",
                hdr_info.color_transfer, hdr_info.color_primaries, hdr_info.color_space);
            // mobius tonemapping with 10-bit output
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=mobius,zscale=t=bt709:m=bt709:r=tv,format=yuv420p10le".to_string()
        } else {
            String::new()
        };

        match config.encoder {
            VideoEncoder::Nvenc => {
                let settings = config.nvenc.as_ref().expect("NVENC settings required");

                // If HDR detected, we need to handle tonemapping
                let hwaccel = if hdr_info.is_hdr {
                    // For HDR, we process in software then upload to CUDA
                    "-hwaccel cuda".to_string()
                } else {
                    "-hwaccel cuda -hwaccel_output_format cuda".to_string()
                };

                let mut params = format!(
                    "-c:v {} -preset {} -tier {} -rc {} -cq {} -qmin {} -qmax {}",
                    settings.codec,
                    settings.preset,
                    settings.tier,
                    settings.rc,
                    settings.cq,
                    settings.cq + 10,
                    settings.cq.saturating_sub(10)
                );

                if let Some(la) = settings.lookahead {
                    params.push_str(&format!(" -lookahead {}", la));
                }
                if settings.temporal_aq.unwrap_or(false) {
                    params.push_str(" -temporal-aq 1");
                }

                (
                    hwaccel,
                    params,
                    tonemap_filter,
                    EncoderType::Nvenc,
                )
            }
            VideoEncoder::Qsv => {
                let settings = config.qsv.as_ref().expect("QSV settings required");

                let hwaccel = if hdr_info.is_hdr {
                    // For HDR, we need software processing first
                    String::new()
                } else {
                    "-hwaccel qsv -hwaccel_output_format qsv -c:v av1_qsv".to_string()
                };

                // Use simpler parameters for Intel Arc compatibility
                let mut params = format!(
                    "-c:v {} -preset:v {}",
                    settings.codec, settings.preset
                );

                if settings.look_ahead_depth > 0 {
                    params.push_str(&format!(" -extbrc:v 1 -look_ahead_depth:v {}", settings.look_ahead_depth));
                }

                // If a quality value is provided, use la_icq rate control on QSV.
                if settings.global_quality > 0 {
                    // global_quality is the QSV quality knob used by la_icq/ICQ-style modes
                    params.push_str(&format!(" -global_quality:v {}", settings.global_quality));
                }

                (
                    hwaccel,
                    params,
                    tonemap_filter,
                    EncoderType::Qsv,
                )
            }
            VideoEncoder::Vaapi => {
                let settings = config.vaapi.as_ref().expect("VAAPI settings required");

                let hwaccel = if hdr_info.is_hdr {
                    // For HDR, we need software processing first
                    String::new()
                } else {
                    "-hwaccel vaapi -vaapi_device /dev/dri/renderD128".to_string()
                };

                let quality = settings.quality;
                let mut params = format!(
                    "-c:v {} -global_quality {} -qp {}",
                    settings.codec, quality, quality
                );

                params.push_str(" -compression_level 7");

                (
                    hwaccel,
                    params,
                    tonemap_filter,
                    EncoderType::Vaapi,
                )
            }
        }
    }



fn format_timestamp_vtt(seconds: f64) -> String {
    let hours = (seconds / 3600.0) as u32;
    let minutes = ((seconds % 3600.0) / 60.0) as u32;
    let secs = (seconds % 60.0) as u32;
    let millis = ((seconds % 1.0) * 1000.0) as u32;
    format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, secs, millis)
}

fn post_process_dash_manifest(
    mpd_path: &str,
    audio_info: &[(String, String, String)], // Vec of (file_path, language, title)
) {
    // Add <Label> and <Role> elements to audio AdaptationSets in the MPD manifest.
    // This enables DASH players to distinguish audio tracks, especially when
    // multiple tracks share the same language (e.g., "English" vs "English - Director's Commentary")
    // or have no metadata at all.
    if audio_info.len() <= 1 && audio_info.iter().all(|(_, _, title)| title.is_empty()) {
        // Single audio stream without title - no need to post-process
        return;
    }

    let mpd_content = match fs::read_to_string(mpd_path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Warning: Could not read MPD for post-processing: {}", e);
            return;
        }
    };

    // Pre-compute labels with disambiguation for duplicates.
    // E.g., three "eng" tracks with no title become: "eng", "eng (2)", "eng (3)"
    // Tracks with unique titles are left as-is.
    let mut raw_labels: Vec<String> = Vec::with_capacity(audio_info.len());
    for (_, language, title) in audio_info {
        let label = if !title.is_empty() {
            title.clone()
        } else if !language.is_empty() {
            language.clone()
        } else {
            format!("Track")
        };
        raw_labels.push(label);
    }

    // Count occurrences of each label and disambiguate duplicates
    let mut labels: Vec<String> = Vec::with_capacity(raw_labels.len());
    let mut seen_count: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    // First pass: count total occurrences of each label
    let mut total_count: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for label in &raw_labels {
        *total_count.entry(label.clone()).or_insert(0) += 1;
    }
    // Second pass: build disambiguated labels
    for label in &raw_labels {
        let count = *total_count.get(label).unwrap_or(&1);
        if count > 1 {
            let seen = seen_count.entry(label.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                labels.push(label.clone());
            } else {
                labels.push(format!("{} ({})", label, seen));
            }
        } else {
            labels.push(label.clone());
        }
    }

    let mut result = String::with_capacity(mpd_content.len() + 512);
    let mut audio_adaptation_idx = 0;

    for line in mpd_content.lines() {
        result.push_str(line);
        result.push('\n');

        // Detect audio AdaptationSet opening tags
        if line.contains("<AdaptationSet") && line.contains("contentType=\"audio\"") {
            if audio_adaptation_idx < audio_info.len() {
                let (_, _, title) = &audio_info[audio_adaptation_idx];
                let label = &labels[audio_adaptation_idx];

                // Detect indentation from the AdaptationSet line
                let indent = &line[..line.len() - line.trim_start().len()];
                let child_indent = format!("{}  ", indent);

                // Add <Label> element
                result.push_str(&format!("{}<Label>{}</Label>\n", child_indent, label));

                // Add <Role> element for commentary tracks
                let title_lower = title.to_lowercase();
                if title_lower.contains("commentary") || title_lower.contains("komentář") {
                    result.push_str(&format!(
                        "{}<Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"commentary\"/>\n",
                        child_indent
                    ));
                } else if audio_info.len() > 1 && audio_adaptation_idx == 0 {
                    // Mark the first audio track as "main" when there are multiple tracks
                    result.push_str(&format!(
                        "{}<Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"main\"/>\n",
                        child_indent
                    ));
                }

                audio_adaptation_idx += 1;
            }
        }
    }

    // Verify all expected audio tracks were found in the MPD
    if audio_adaptation_idx != audio_info.len() {
        eprintln!(
            "WARNING: MPD audio track count mismatch! Expected {} audio AdaptationSets but found {} in MPD. Some audio tracks may be missing.",
            audio_info.len(),
            audio_adaptation_idx
        );
    }

    if let Err(e) = fs::write(mpd_path, result) {
        eprintln!("Warning: Could not write post-processed MPD: {}", e);
    } else {
        println!("Post-processed MPD with {} audio label(s): {:?}", audio_adaptation_idx, labels);
    }
}

async fn transcode_video(
    input_file: &str,
    output_dir: &str,
    config: &VideoConfig,
) -> Result<(), ffmpeg_next::Error> {
    ffmpeg_next::init()?;

    let input_context = format::input(&input_file)?;
    let video_stream = input_context
        .streams()
        .best(media::Type::Video)
        .ok_or(ffmpeg_next::Error::StreamNotFound)?;
    let video_decoder = codec::context::Context::from_parameters(video_stream.parameters())?
        .decoder()
        .video()?;
    let original_width = video_decoder.width();
    let original_height = video_decoder.height();
    let framerate: f32;
    let fr = video_stream.avg_frame_rate();
    let fps = if fr.denominator() == 0 {
        30.0 // fallback to 30fps
    } else {
        fr.numerator() as f32 / fr.denominator() as f32
    };

    if fps > config.fps_cap {
        framerate = config.fps_cap;
    } else {
        framerate = fps;
    }
    let duration = input_context.duration() as f64 / ffmpeg_next::ffi::AV_TIME_BASE as f64; // Video duration in seconds

    let mut audio_bitrate = config.audio_bitrate_base;

    // Calculate aspect ratio once to ensure all resolutions maintain it
    if original_height == 0 || original_width == 0 {
        eprintln!("Invalid video dimensions: {}x{}", original_width, original_height);
        return Err(ffmpeg_next::Error::External);
    }
    let aspect_ratio = original_width as f32 / original_height as f32;

    let mut outputs = Vec::new();
    let mut width = original_width;
    let mut height = original_height;

    // Determine number of quality steps based on resolution
    let num_steps = if (width * height) >= config.threshold_2k_pixels {
        config.max_resolution_steps
    } else {
        config.max_resolution_steps.saturating_sub(1)
    };

    // Apply 2K audio bitrate bonus for DASH audio transcoding
    if num_steps >= config.max_resolution_steps {
        audio_bitrate += config.audio_bitrate_2k_bonus;
    }

    // Audio is transcoded once separately for DASH, not per video quality level
    let dash_audio_bitrate = audio_bitrate;

    let epsilon = 0.01; // Allow for tiny rounding variances

    for i in 0..num_steps.min(config.quality_steps.len() as u32) {
        let step = &config.quality_steps[i as usize];

        let current_ratio = width as f32 / height as f32;
        let ratio_diff = (current_ratio - aspect_ratio).abs();

        if ratio_diff <= epsilon {
            if !outputs.iter().any(|(w, h, _)| *w == width && *h == height) {
                outputs.push((width, height, step.label.clone()));
            }
        } else {
            println!("Skipping {}x{} due to ratio mismatch", width, height);
        }

        let scale_factor = 1.0 / step.scale_divisor as f32;

        let mut new_width = (width as f32 * scale_factor).round() as u32;
        let mut new_height = (new_width as f32 / aspect_ratio).round() as u32;

        new_width = new_width.max(config.min_dimension);
        new_height = new_height.max(config.min_dimension);

        new_width = (new_width / 2) * 2;
        new_height = (new_height / 2) * 2;

        width = new_width;
        height = new_height;
    }

    println!("Generated {} quality outputs: {:?}", outputs.len(), outputs.iter().map(|(_, _, label)| label.clone()).collect::<Vec<_>>());

    let mut webm_files = Vec::new();
    let dash_output_dir = format!("{}/video", output_dir);

    // Detect HDR characteristics
    let hdr_info = detect_hdr(input_file);

    // Build encoder-specific ffmpeg parameters
    let (hwaccel_args, codec_params, tonemap_filter, encoder_type) = build_encoder_params(config, framerate, &hdr_info);

    // Transcode each quality level in parallel (video-only; audio is transcoded once separately for DASH)
    let mut transcode_handles = Vec::new();
    for (w, h, label) in &outputs {
        let output_file = format!("{}/output_{}.webm", output_dir, label);
        webm_files.push(output_file.clone());

        let cmd = match encoder_type {
            EncoderType::Qsv => {
                let hwaccel_args = "-hwaccel qsv -hwaccel_output_format qsv";

                if hdr_info.is_hdr {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' \
                         -vf 'vpp_qsv=w={}:h={}:tonemap=1:format=p010le:out_color_matrix=bt709' \
                         {} -pix_fmt p010le \
                         -an -f webm '{}'",
                        hwaccel_args,
                        input_file,
                        w, h,
                        codec_params,
                        output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' \
                         -vf 'vpp_qsv=w={}:h={}:format=p010le' \
                         {} -pix_fmt p010le \
                         -an -f webm '{}'",
                        hwaccel_args,
                        input_file,
                        w, h,
                        codec_params,
                        output_file
                    )
                }
            }
            EncoderType::Nvenc => {
                if hdr_info.is_hdr {
                    // HDR path: software tonemapping then NVENC encode
                    let filter_chain = if tonemap_filter.is_empty() {
                        format!("scale={}:{}:force_original_aspect_ratio=decrease:finterp=true,format=yuv420p10le", w, h)
                    } else {
                        format!("{},scale={}:{}:force_original_aspect_ratio=decrease:finterp=true", tonemap_filter, w, h)
                    };
                    format!(
                        "ffmpeg -nostdin -y -init_hw_device cuda=cuda0 -filter_hw_device cuda0 -i '{}' -vf '{}' {} -an -f webm '{}'",
                        input_file, filter_chain, codec_params, output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' -vf 'scale_cuda={}:{}:force_original_aspect_ratio=decrease:finterp=true' {} -an -f webm '{}'",
                        hwaccel_args, input_file, w, h, codec_params, output_file
                    )
                }
            }
            EncoderType::Vaapi => {
                if hdr_info.is_hdr {
                    // HDR path: software tonemapping then VAAPI encode
                    let filter_chain = if tonemap_filter.is_empty() {
                        format!("scale={}:{}:force_original_aspect_ratio=decrease,format=p010le", w, h)
                    } else {
                        format!("{},scale={}:{}:force_original_aspect_ratio=decrease,format=p010le", tonemap_filter, w, h)
                    };
                    format!(
                        "ffmpeg -nostdin -y -vaapi_device /dev/dri/renderD128 -i '{}' -vf '{}' {} -an -f webm '{}'",
                        input_file, filter_chain, codec_params, output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' -vf 'scale_vaapi={}:{}:force_original_aspect_ratio=decrease,format=p010le' {} -an -f webm '{}'",
                        hwaccel_args, input_file, w, h, codec_params, output_file
                    )
                }
            }
        };

        let label_owned = label.clone();
        let output_file_owned = output_file.clone();
        transcode_handles.push(task::spawn_blocking(move || {
            println!("Executing: {}", cmd);
            let status = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status();
            (status, label_owned, output_file_owned)
        }));
    }

    // Wait for all quality transcodes to complete in parallel
    for handle in transcode_handles {
        match handle.await {
            Ok((status, label, output_file)) => match status {
                Ok(s) if s.success() => {
                    println!("Generated: {}", output_file);
                }
                Ok(s) => {
                    eprintln!("FFmpeg failed with exit code: {:?} for {}", s.code(), label);
                }
                Err(e) => {
                    eprintln!("Failed to execute ffmpeg for {}: {}", label, e);
                }
            },
            Err(e) => {
                eprintln!("Transcode task panicked: {}", e);
            }
        }
    }

    println!("Creating WebM DASH manifest...");
    webm_files.retain(|file| fs::metadata(file).is_ok());

    if webm_files.is_empty() {
        eprintln!("No WebM files were successfully encoded, cannot create DASH manifest");
        return Err(ffmpeg_next::Error::External);
    }

    fs::create_dir_all(&dash_output_dir).map_err(|e| {
        eprintln!("Failed to create DASH output directory: {}", e);
        ffmpeg_next::Error::External
    })?;

    // Probe and transcode all audio streams for multi-language DASH support
    let audio_streams = probe_audio_streams(input_file);
    println!(
        "Found {} audio stream(s): {:?}",
        audio_streams.len(),
        audio_streams.iter().map(|(_, lang, title, _)| {
            format!("{}({})", if lang.is_empty() { "und" } else { lang }, if title.is_empty() { "none" } else { title })
        }).collect::<Vec<_>>()
    );

    let audio_webm_files = if !audio_streams.is_empty() {
        transcode_audio_streams_for_dash(input_file, output_dir, dash_audio_bitrate, &audio_streams, &config.dash).await
    } else {
        Vec::new()
    };

    // Verify all audio streams were successfully transcoded
    if audio_webm_files.len() != audio_streams.len() {
        eprintln!(
            "WARNING: Audio stream count mismatch! Source has {} audio stream(s) but only {} were successfully transcoded. Missing tracks will not appear in DASH manifest.",
            audio_streams.len(),
            audio_webm_files.len()
        );
    }

    // Build DASH inputs: video files first, then audio files
    let num_video_files = webm_files.len();
    let mut all_inputs: Vec<String> = webm_files
        .iter()
        .map(|file| format!("-i '{}'", file))
        .collect();
    for (audio_file, _, _) in &audio_webm_files {
        all_inputs.push(format!("-i '{}'", audio_file));
    }
    let dash_input_cmds = all_inputs.join(" ");

    // Build maps: video from video files, audio from audio-only files
    let mut maps = String::new();
    for track_num in 0..num_video_files {
        maps.push_str(&format!(" -map {}:v", track_num));
    }
    for (audio_idx, _) in audio_webm_files.iter().enumerate() {
        maps.push_str(&format!(" -map {}:a:0", num_video_files + audio_idx));
    }

    // No fallback needed: video files are video-only (-an), audio is always
    // transcoded separately via transcode_audio_streams_for_dash(). If no audio
    // streams exist in the source, the DASH manifest will simply have no audio.

    // Build adaptation sets: one for video, one per audio language
    // Output stream indices: 0..num_video-1 are video, num_video..num_video+num_audio-1 are audio
    let num_video_outputs = num_video_files;
    let mut adaptation_sets = String::from("id=0,streams=v");

    if audio_webm_files.len() <= 1 {
        // Single audio (or none): simple adaptation set
        if !audio_webm_files.is_empty() {
            adaptation_sets.push_str(&format!(" id=1,streams={}", num_video_outputs));
        }
    } else {
        // Multiple audio streams: separate adaptation set per language
        for (audio_idx, _audio_info) in audio_webm_files.iter().enumerate() {
            let output_stream_idx = num_video_outputs + audio_idx;
            adaptation_sets.push_str(&format!(" id={},streams={}", audio_idx + 1, output_stream_idx));
        }
    }

    // Build language metadata for audio streams
    let mut metadata_args = String::new();
    for (audio_idx, (_, language, _)) in audio_webm_files.iter().enumerate() {
        let output_stream_idx = num_video_outputs + audio_idx;
        if !language.is_empty() {
            metadata_args.push_str(&format!(" -metadata:s:{} language={}", output_stream_idx, language));
            metadata_args.push_str(&format!(" -metadata:s:{} title=\"{}\"", output_stream_idx, language));
        }
    }

    let dash_output_cmd = format!(
        "ffmpeg -nostdin -y -analyzeduration 100M -probesize 100M {} {}{} \
        -c copy -map_metadata -1 \
        -dash_segment_type webm \
        -use_timeline 0 \
        -use_template 1 \
        -frag_duration {} \
        -adaptation_sets '{}' \
        -window_size 0 \
        -extra_window_size 0 \
        -streaming 0 \
        -fflags +genpts \
        -avoid_negative_ts make_zero \
        -index_correction 0 \
        -init_seg_name 'init_$RepresentationID$.webm' \
        -media_seg_name 'chunk_$RepresentationID$_$Number$.webm' \
        '{}/video.mpd'",
        dash_input_cmds, maps, metadata_args, config.dash.segment_duration, adaptation_sets, dash_output_dir
    );

    println!("Executing: {}", dash_output_cmd);
    let dash_status = Command::new("sh")
        .arg("-c")
        .arg(&dash_output_cmd)
        .status()
        .map_err(|e| {
            eprintln!("Failed to execute DASH manifest command: {}", e);
            ffmpeg_next::Error::External
        })?;
    if !dash_status.success() {
        eprintln!("DASH manifest creation failed with exit code: {:?}", dash_status.code());
        return Err(ffmpeg_next::Error::External);
    }

    // Post-process MPD to add <Label> and <Role> elements for audio track selection
    let mpd_path = format!("{}/video.mpd", dash_output_dir);
    post_process_dash_manifest(&mpd_path, &audio_webm_files);

    //OGP video - find quarter_resolution dynamically
    let ogp_source = format!("{}/output_quarter_resolution.webm", output_dir);
    let ogp_dest = format!("{}/video/video.webm", output_dir);

    let ogp_video_result = if fs::metadata(&ogp_source).is_ok() {
        fs::rename(&ogp_source, &ogp_dest)
    } else {
        // Fallback: use the middle quality available
        if !webm_files.is_empty() {
            let middle_idx = webm_files.len() / 2;
            let fallback_source = &webm_files[middle_idx];
            let fallback_result = fs::rename(fallback_source, &ogp_dest);
            webm_files.remove(middle_idx);
            fallback_result
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "No suitable OGP video found"))
        }
    };

    // Remove quarter_resolution from webm_files list if it exists
    if let Some(idx) = webm_files.iter().position(|f| f.ends_with("_quarter_resolution.webm")) {
        webm_files.remove(idx);
    }

    println!("CREATED OGP VIDEO: {:?}", ogp_video_result);

    // Clean up intermediate WebM files
    println!("Remove WebM files...");
    for file in webm_files {
        if let Err(e) = fs::remove_file(&file) {
            eprintln!("Warning: Failed to delete intermediate WebM file {}: {}", file, e);
        }
    }

    // Clean up intermediate audio WebM files
    println!("Remove audio WebM files...");
    for (audio_file, _, _) in &audio_webm_files {
        if let Err(e) = fs::remove_file(audio_file) {
            eprintln!("Warning: Failed to delete intermediate audio WebM file {}: {}", audio_file, e);
        }
    }

    // Generate thumbnails, showcase, and preview sprites all in parallel
    let random_time = if duration > 0.1 {
        rand::rng().random_range(0.0..duration)
    } else {
        0.0
    };
    println!("thumbnail selected time: {:.2} seconds", random_time);

    // Create preview output directory before spawning sprite tasks
    let preview_output_dir = format!("{}/previews", output_dir);
    fs::create_dir_all(&preview_output_dir).map_err(|e| {
        eprintln!("Failed to create preview output directory: {}", e);
        ffmpeg_next::Error::External
    })?;

    let interval_seconds = config.preview_sprites.interval_seconds;
    let thumb_width = config.preview_sprites.thumb_width;
    let thumb_height = config.preview_sprites.thumb_height;
    let max_sprites_per_file = config.preview_sprites.max_sprites_per_file;
    let sprites_across = config.preview_sprites.sprites_across;

    // Calculate number of thumbnails needed (at least 1 for very short videos)
    let num_thumbnails = if duration > 0.0 {
        (duration / interval_seconds).ceil().max(1.0) as u32
    } else {
        1
    };
    let num_sprite_files = ((num_thumbnails as f32) / (max_sprites_per_file as f32)).ceil().max(1.0) as u32;

    println!(
        "Generating {} thumbnail sprites with {} total thumbnails (max {} per file)...",
        num_sprite_files, num_thumbnails, max_sprites_per_file
    );

    // Spawn all post-processing tasks in parallel: JPG thumbnail, AVIF thumbnail, showcase, and all sprite files
    let mut post_handles: Vec<task::JoinHandle<()>> = Vec::new();

    // JPG thumbnail
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -ss {:.2} -i '{}' -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -frames:v 1 -update 1 '{}/thumbnail.jpg'",
        random_time, input_file, config.thumbnail.width, config.thumbnail.height, output_dir
    );
    post_handles.push(task::spawn_blocking(move || {
        println!("Executing: {}", thumbnail_jpg_cmd);
        let _ = Command::new("sh").arg("-c").arg(&thumbnail_jpg_cmd).status();
    }));

    // AVIF thumbnail
    let thumbnail_avif_cmd = format!(
        "ffmpeg -nostdin -y -ss {:.2} -i '{}' -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -frames:v 1 -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -update 1 '{}/thumbnail.avif'",
        random_time, input_file, config.thumbnail.width, config.thumbnail.height, output_dir
    );
    post_handles.push(task::spawn_blocking(move || {
        println!("Executing: {}", thumbnail_avif_cmd);
        let _ = Command::new("sh").arg("-c").arg(&thumbnail_avif_cmd).status();
    }));

    // Animated showcase.avif
    let showcase_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -vf 'scale={}:-2,fps={},format=yuv420p10le' -frames:v {} -c:v libaom-av1 -pix_fmt yuv420p10le -q:v {} -cpu-used {} -row-mt 1 '{}/showcase.avif'",
        input_file, config.showcase.width, config.showcase.fps, config.showcase.max_frames, config.showcase.quality, config.showcase.cpu_used, output_dir
    );
    post_handles.push(task::spawn_blocking(move || {
        println!("Generating showcase.avif...");
        println!("Executing: {}", showcase_cmd);
        let showcase_status = Command::new("sh").arg("-c").arg(&showcase_cmd).status();
        match showcase_status {
            Ok(status) if status.success() => {
                println!("showcase.avif generated successfully");
            }
            Ok(status) => {
                eprintln!("Warning: showcase.avif generation failed with exit code: {:?}", status.code());
            }
            Err(e) => {
                eprintln!("Warning: Failed to execute showcase command: {}", e);
            }
        }
    }));

    // Sprite files with throttled parallelism
    let parallel_limit = config.preview_sprites.parallel_limit.max(1) as usize;
    let semaphore = Arc::new(Semaphore::new(parallel_limit));
    println!("Generating sprite files with parallel_limit={}", parallel_limit);

    let mut sprite_handles: Vec<task::JoinHandle<()>> = Vec::new();
    for sprite_idx in 0..num_sprite_files {
        let start_thumb_idx = sprite_idx * max_sprites_per_file;
        let end_thumb_idx = ((start_thumb_idx + max_sprites_per_file).min(num_thumbnails)) as u32;
        let thumbs_in_this_file = end_thumb_idx - start_thumb_idx;
        let rows_in_this_file =
            ((thumbs_in_this_file as f32) / (sprites_across as f32)).ceil() as u32;

        let start_time = (start_thumb_idx as f64) * interval_seconds;

        let sprite_path = format!("{}/preview_sprite_{}.avif", preview_output_dir, sprite_idx);
        let duration_for_this_file = thumbs_in_this_file as f64 * interval_seconds;

        let tile_filter = format!(
            "fps=1/{:.3},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2,tile={}x{}",
            interval_seconds, thumb_width, thumb_height, thumb_width, thumb_height,
            sprites_across, rows_in_this_file
        );

        let sprite_cmd = format!(
            "ffmpeg -nostdin -y -ss {:.3} -t {:.3} -i '{}' -vf '{}' -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -q:v {} -r 1 -frames:v 1 -update 1 '{}'",
            start_time, duration_for_this_file, input_file, tile_filter, config.preview_sprites.quality, sprite_path
        );

        let permit = Arc::clone(&semaphore);
        sprite_handles.push(task::spawn(async move {
            let _permit = permit.acquire().await.expect("semaphore closed");
            task::spawn_blocking(move || {
                println!("Executing sprite {}: {}", sprite_idx, sprite_cmd);
                let sprite_status = Command::new("sh").arg("-c").arg(&sprite_cmd).status();
                match sprite_status {
                    Ok(status) if status.success() => {
                        println!("Sprite {} generated successfully", sprite_idx);
                    }
                    Ok(status) => {
                        eprintln!("Warning: Sprite {} generation failed with exit code: {:?}", sprite_idx, status.code());
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to execute sprite {} command: {}", sprite_idx, e);
                    }
                }
            }).await.ok();
        }));
    }

    // Wait for all post-processing tasks to complete (thumbnails + showcase)
    for handle in post_handles {
        let _ = handle.await;
    }

    // Wait for all sprite generation tasks to complete
    for handle in sprite_handles {
        let _ = handle.await;
    }

    // Generate WebVTT file with sprite coordinates
    let mut vtt_cues: Vec<ThumbnailCue> = Vec::new();

    for i in 0..num_thumbnails {
        let sprite_file_idx = i / max_sprites_per_file;
        let local_idx = i % max_sprites_per_file;
        let row = local_idx / sprites_across;
        let col = local_idx % sprites_across;
        let x = col * thumb_width;
        let y = row * thumb_height;
        let start_time = (i as f64) * interval_seconds;
        let end_time = ((i + 1) as f64) * interval_seconds;

        vtt_cues.push(ThumbnailCue {
            start_time,
            end_time: end_time.min(duration),
            x,
            y,
            width: thumb_width,
            height: thumb_height,
            sprite_idx: sprite_file_idx,
        });
    }

    // Write WebVTT file
    let vtt_path = format!("{}/previews.vtt", preview_output_dir);
    let mut vtt_content = String::from("WEBVTT\n\n");

    for cue in &vtt_cues {
        let start_formatted = format_timestamp_vtt(cue.start_time);
        let end_formatted = format_timestamp_vtt(cue.end_time);
        let sprite_filename = format!("preview_sprite_{}.avif", cue.sprite_idx);
        vtt_content.push_str(&format!(
            "{} --> {}\n{}#xywh={},{},{},{}\n\n",
            start_formatted, end_formatted, sprite_filename, cue.x, cue.y, cue.width, cue.height
        ));
    }

    if let Err(e) = fs::write(&vtt_path, vtt_content) {
        eprintln!("Warning: Failed to write thumbnails.vtt file: {}", e);
    }
    println!(
        "Generated WebVTT thumbnails file with {} cues across {} sprite files",
        vtt_cues.len(),
        num_sprite_files
    );

    Ok(())
}
