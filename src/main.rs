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
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::process::Command;
use std::time::Duration;
use reqwest::blocking::{Client, multipart};

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
    whisper_url: Option<String>,
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
}

#[tokio::main]
async fn main() {
    let config: Config = serde_json::from_str(&fs::read_to_string("config.json").unwrap()).unwrap();

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.dbconnection)
        .await
        .unwrap();

    process(pool, config.video).await;
}

fn detect_file_type(input_file: &str) -> Option<String> {
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

async fn process(pool: PgPool, video_config: VideoConfig) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));

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
                process_video(concept.id.clone(), pool.clone(), video_config.clone())
                    .await
                    .map_err(|e| format!("video processing failed: {}", e))
            } else if actual_type == "picture" {
                println!("processing concept: {} as picture", concept.id);
                process_picture(concept.id.clone(), pool.clone())
                    .await
                    .map_err(|e| format!("picture processing failed: {}", e))
            } else if actual_type == "audio" {
                println!("processing concept: {} as audio", concept.id);
                process_audio(concept.id.clone(), pool.clone())
                    .await
                    .map_err(|e| format!("audio processing failed: {}", e))
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

async fn process_video(concept_id: String, pool: PgPool, video_config: VideoConfig) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;

    // Extract subtitles before transcoding
    let input_file = format!("upload/{}", concept_id);
    let output_dir = format!("upload/{}_processing", concept_id);
    extract_subtitles_to_vtt(&input_file, &output_dir);

    // Extract chapters if present
    extract_chapters_to_vtt(&input_file, &output_dir);

    let transcode_result = transcode_video(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
        &video_config,
    );
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

async fn process_picture(concept_id: String, pool: PgPool) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;
    let transcode_result = transcode_picture(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
    );
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

async fn process_audio(concept_id: String, pool: PgPool) -> Result<(), String> {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .map_err(|e| format!("Failed to create processing directory: {}", e))?;

    // Extract subtitles before transcoding (some audio formats can contain lyrics/subtitles)
    let input_file = format!("upload/{}", concept_id);
    let output_dir = format!("upload/{}_processing", concept_id);
    extract_subtitles_to_vtt(&input_file, &output_dir);

    // Extract chapters if present (audiobooks, podcasts, etc.)
    extract_chapters_to_vtt(&input_file, &output_dir);

    let transcode_result = transcode_audio(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
    );
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

fn transcode_audio_streams_for_dash(
    input_file: &str,
    output_dir: &str,
    audio_bitrate: u32,
    audio_streams: &[(u32, String, String, String)],
) -> Vec<(String, String, String)> {
    // Returns Vec of (file_path, language, title) for successfully transcoded audio streams
    let mut result = Vec::new();

    for (audio_idx, (_stream_index, language, title, _codec)) in audio_streams.iter().enumerate() {
        let output_file = format!("{}/audio_stream_{}.webm", output_dir, audio_idx);

        let mut cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -map 0:a:{} -c:a libopus -b:a {}k -vbr constrained -ac 2 -vn",
            input_file, audio_idx, audio_bitrate
        );

        // Set language metadata if available
        if !language.is_empty() {
            cmd.push_str(&format!(" -metadata:s:a:0 language={}", language));
        }
        if !title.is_empty() {
            cmd.push_str(&format!(" -metadata:s:a:0 title='{}'", title.replace('\'', "'\\''")));
        }

        cmd.push_str(&format!(" -f webm '{}'", output_file));

        println!("Executing: {}", cmd);
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status();

        match status {
            Ok(s) if s.success() => {
                println!(
                    "Generated audio stream {}: {} (language: {}, title: {})",
                    audio_idx,
                    output_file,
                    if language.is_empty() { "und" } else { language },
                    if title.is_empty() { "none" } else { title }
                );
                result.push((output_file, language.clone(), title.clone()));
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
        }
    }

    result
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn extract_subtitles_to_vtt(input_file: &str, output_dir: &str) -> Vec<String> {
    let subtitle_streams = probe_subtitle_streams(input_file); // Assuming this is defined elsewhere

    // FALLBACK LOGIC: If no subtitles exist in the file, use Whisper.cpp
    if subtitle_streams.is_empty() {
        println!("No built-in subtitles found. Falling back to Whisper.cpp on http://whisper:8080...");
        return generate_whisper_vtt(input_file, output_dir);
    }

    // Create captions directory
    let captions_dir = format!("{}/captions", output_dir);
    fs::create_dir_all(&captions_dir).expect("Failed to create captions directory");

    let mut saved_files: Vec<String> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Precompute per-stream output file paths and names
    let mut outputs: Vec<(u32, String, String, String, String)> = Vec::new();

    for (stream_idx, language, title, codec) in subtitle_streams {
        let base_name = if !language.is_empty() {
            sanitize_filename(&language)
        } else if !title.is_empty() {
            sanitize_filename(&title)
        } else {
            format!("subtitle_{}", stream_idx)
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
            "Preparing subtitle stream {} (language: '{}', title: '{}', codec: {}) to VTT as '{}'",
            stream_idx,
            if language.is_empty() { "unknown" } else { &language },
            if title.is_empty() { "none" } else { &title },
            codec,
            final_name
        );

        outputs.push((stream_idx, final_name, output_file, language, title));
    }

    // Build a single ffmpeg invocation that extracts all subtitle streams at once.
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin")
        .arg("-v").arg("error")
        .arg("-i").arg(input_file);

    for (stream_idx, _final_name, output_file, _language, _title) in &outputs {
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
                for (_stream_idx, final_name, output_file, _language, _title) in outputs {
                    match fs::metadata(&output_file) {
                        Ok(metadata) if metadata.len() > 0 => {
                            saved_files.push(final_name);
                        }
                        _ => { let _ = fs::remove_file(&output_file); }
                    }
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("Failed to extract subtitles to VTT: {}", stderr);
                for (_stream_idx, _final_name, output_file, _language, _title) in outputs {
                    let _ = fs::remove_file(&output_file);
                }
            }
        }
        Err(e) => {
            println!("Error executing ffmpeg for subtitle extraction: {}", e);
            for (_stream_idx, _final_name, output_file, _language, _title) in outputs {
                let _ = fs::remove_file(&output_file);
            }
        }
    }

    create_list_txt(&captions_dir, &saved_files);
    saved_files
}

/// Helper function to generate subtitles using Whisper.cpp API
fn generate_whisper_vtt(input_file: &str, output_dir: &str) -> Vec<String> {
    let captions_dir = format!("{}/captions", output_dir);
    fs::create_dir_all(&captions_dir).expect("Failed to create captions directory");

    let temp_audio = format!("{}/temp_audio.wav", captions_dir);

    // 1. Extract audio to 16kHz mono WAV (required by Whisper)
    println!("Extracting audio for Whisper...");
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

    // 2. Send the audio to the Whisper.cpp server
    println!("Sending audio to Whisper.cpp API...");

    let form = match multipart::Form::new()
        .text("response_format", "vtt")
        .text("model", "whisper-1")
        .file("file", &temp_audio)
    {
        Ok(f) => f,
        Err(e) => {
            println!("Failed to read audio file for upload: {}", e);
            let _ = fs::remove_file(&temp_audio);
            return Vec::new();
        }
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(3600)) // 1 hour
        .build()
        .unwrap();

    let response = client.post("http://whisper:8080/inference")
        .multipart(form)
        .send();

    let mut saved_files = Vec::new();

    match response {
        Ok(res) if res.status().is_success() => {
            if let Ok(vtt_content) = res.text() {
                let final_name = "AI_transcription".to_string();
                let output_file = format!("{}/{}.vtt", captions_dir, final_name);

                if fs::write(&output_file, vtt_content).is_ok() {
                    println!("Successfully generated VTT via Whisper.cpp.");
                    saved_files.push(final_name);
                }
            }
        }
        Ok(res) => println!("Whisper API returned an error: {}", res.status()),
        Err(e) => println!("Failed to connect to Whisper API: {}", e),
    }

    // 3. Clean up the temporary WAV file and create list.txt
    let _ = fs::remove_file(&temp_audio);
    create_list_txt(&captions_dir, &saved_files);

    saved_files
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

fn transcode_picture(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
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

    // Calculate scaled dimensions for HD thumbnail (closest to 1920x1080 while maintaining aspect ratio)
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height);

    // Full resolution AVIF
    let transcode_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, output_dir
    );
    println!("Executing: {}", transcode_cmd);
    let status = Command::new("sh")
        .arg("-c")
        .arg(transcode_cmd)
        .status()
        .map_err(|_| ffmpeg_next::Error::External)?;
    if !status.success() {
        eprintln!("Failed to transcode picture to AVIF");
        return Err(ffmpeg_next::Error::External);
    }

    // HD thumbnail AVIF with proper aspect ratio
    let thumbnail_cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -c:v libsvtav1 -svtav1-params avif=1 -crf 28 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
            input_file, thumb_width, thumb_height, output_dir
        );
    println!("Executing: {}", thumbnail_cmd);
    let status = Command::new("sh")
        .arg("-c")
        .arg(thumbnail_cmd)
        .status()
        .map_err(|_| ffmpeg_next::Error::External)?;
    if !status.success() {
        eprintln!("Failed to transcode picture thumbnail AVIF");
        return Err(ffmpeg_next::Error::External);
    }

    // HD thumbnail JPG for older devices
    let thumbnail_ogp_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 '{}/thumbnail.jpg'",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_ogp_cmd);
    let status = Command::new("sh")
        .arg("-c")
        .arg(thumbnail_ogp_cmd)
        .status()
        .map_err(|_| ffmpeg_next::Error::External)?;
    if !status.success() {
        eprintln!("Failed to transcode picture thumbnail JPG");
        return Err(ffmpeg_next::Error::External);
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

fn extract_additional_audio(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
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

        let bitrate = if audio_codec == "flac" || audio_codec == "wav" || audio_codec == "pcm_s16le"
        {
            "300k"
        } else {
            "256k"
        };

        let output_path = format!("{}/audio_{}.ogg", output_dir, idx + 1);
        let extract_cmd = format!(
            "ffmpeg -nostdin -y -i '{}' -map 0:a:{} -c:a libopus -b:a {} -vbr on -application audio '{}'",
            input_file, stream_idx, bitrate, output_path
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

fn extract_secondary_video_as_cover(
    input_file: &str,
    output_dir: &str,
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
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height);

    // Check if we have multiple video streams
    let video_count = count_video_streams(input_file);
    let stream_selector = if video_count > 1 { "v:1" } else { "v:0" };

    // Extract full resolution cover
    let cover_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, stream_selector, output_dir
    );
    println!("Executing: {}", cover_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&cover_cmd)
        .status();

    // Create thumbnail AVIF
    let thumbnail_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf 30 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
        input_file, stream_selector, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_cmd)
        .status();

    // Create thumbnail JPG for older devices
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:{} -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 '{}/thumbnail.jpg'",
        input_file, stream_selector, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_jpg_cmd)
        .status();

    Ok(())
}

fn extract_album_cover(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
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
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height);

    // Extract full resolution album cover
    let cover_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 '{}/picture.avif'",
        input_file, output_dir
    );
    println!("Executing: {}", cover_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&cover_cmd)
        .status();

    // Create thumbnail AVIF
    let thumbnail_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf 30 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 '{}/thumbnail.avif'",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_cmd)
        .status();

    // Create thumbnail JPG for older devices
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:v:0 -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 '{}/thumbnail.jpg'",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_jpg_cmd)
        .status();

    Ok(())
}

fn transcode_audio(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
    // Detect source codec to determine bitrate
    let source_codec = get_audio_codec(input_file);
    println!("Detected audio codec: {}", source_codec);

    // FLAC and WAV get higher bitrate (300 kb/s), others use 256 kb/s
    let bitrate = if source_codec == "flac" || source_codec == "wav" || source_codec == "pcm_s16le"
    {
        "300k"
    } else {
        "256k"
    };

    println!("Using bitrate: {} for {} codec", bitrate, source_codec);

    // Transcode to OGG with Opus
    let output_path = format!("{}/audio.ogg", output_dir);
    let transcode_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -map 0:a:0 -c:a libopus -b:a {} -vbr on -application audio '{}'",
        input_file, bitrate, output_path
    );
    println!("Executing: {}", transcode_cmd);
    let status = Command::new("sh")
        .arg("-c")
        .arg(&transcode_cmd)
        .status()
        .map_err(|_| ffmpeg_next::Error::External)?;
    if !status.success() {
        eprintln!("Failed to transcode audio to Opus");
        return Err(ffmpeg_next::Error::External);
    }

    // Extract additional audio streams if present
    let audio_stream_count = count_audio_streams(input_file);
    if audio_stream_count > 1 {
        println!("Found {} audio streams, extracting additional streams...", audio_stream_count);
        let _ = extract_additional_audio(input_file, output_dir);
    }

    // Check if audio file has embedded album cover (video stream)
    if has_video_stream(input_file) {
        println!("Found album cover in audio file, extracting...");
        let _ = extract_album_cover(input_file, output_dir);
    }

    Ok(())
}

fn calculate_hd_scale(width: u32, height: u32) -> (u32, u32) {
    // HD resolution is 1920x1080
    let target_width: u32 = 1280;
    let target_height: u32 = 720;

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

fn transcode_video(
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

    // Apply 2K audio bitrate bonus once (not per quality step)
    if num_steps >= config.max_resolution_steps {
        audio_bitrate += config.audio_bitrate_2k_bonus;
    }

    // Save the best audio bitrate for separate DASH audio transcoding
    let dash_audio_bitrate = audio_bitrate;

    let epsilon = 0.01; // Allow for tiny rounding variances

    for i in 0..num_steps.min(config.quality_steps.len() as u32) {
        let step = &config.quality_steps[i as usize];

        let current_ratio = width as f32 / height as f32;
        let ratio_diff = (current_ratio - aspect_ratio).abs();

        if ratio_diff <= epsilon {
            if !outputs.iter().any(|(w, h, _, _)| *w == width && *h == height) {
                outputs.push((width, height, step.label.clone(), audio_bitrate));
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

        audio_bitrate = (audio_bitrate / step.audio_bitrate_divisor).max(64);
    }

    println!("Generated {} quality outputs: {:?}", outputs.len(), outputs.iter().map(|(_, _, label, _)| label.clone()).collect::<Vec<_>>());

    let mut webm_files = Vec::new();
    let dash_output_dir = format!("{}/video", output_dir);

    // Detect HDR characteristics
    let hdr_info = detect_hdr(input_file);

    // Build encoder-specific ffmpeg parameters
    let (hwaccel_args, codec_params, tonemap_filter, encoder_type) = build_encoder_params(config, framerate, &hdr_info);

    // Transcode each quality level separately (more reliable with hardware encoders)
    for (w, h, label, audio_bitrate) in &outputs {
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
                         -c:a libopus -b:a {}k -vbr constrained -ac 2 \
                         -f webm '{}'",
                        hwaccel_args,
                        input_file,
                        w, h,
                        codec_params,
                        audio_bitrate,
                        output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' \
                         -vf 'vpp_qsv=w={}:h={}:format=p010le' \
                         {} -pix_fmt p010le \
                         -c:a libopus -b:a {}k -vbr constrained -ac 2 \
                         -f webm '{}'",
                        hwaccel_args,
                        input_file,
                        w, h,
                        codec_params,
                        audio_bitrate,
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
                        "ffmpeg -nostdin -y -init_hw_device cuda=cuda0 -filter_hw_device cuda0 -i '{}' -vf '{}' {} -c:a libopus -b:a {}k -vbr constrained -ac 2 -f webm '{}'",
                        input_file, filter_chain, codec_params, audio_bitrate, output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' -vf 'scale_cuda={}:{}:force_original_aspect_ratio=decrease:finterp=true' {} -c:a libopus -b:a {}k -vbr constrained -ac 2 -f webm '{}'",
                        hwaccel_args, input_file, w, h, codec_params, audio_bitrate, output_file
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
                        "ffmpeg -nostdin -y -vaapi_device /dev/dri/renderD128 -i '{}' -vf '{}' {} -c:a libopus -b:a {}k -vbr constrained -ac 2 -f webm '{}'",
                        input_file, filter_chain, codec_params, audio_bitrate, output_file
                    )
                } else {
                    format!(
                        "ffmpeg -nostdin -y {} -i '{}' -vf 'scale_vaapi={}:{}:force_original_aspect_ratio=decrease,format=p010le' {} -c:a libopus -b:a {}k -vbr constrained -ac 2 -f webm '{}'",
                        hwaccel_args, input_file, w, h, codec_params, audio_bitrate, output_file
                    )
                }
            }
        };

        println!("Executing: {}", cmd);
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status();

        match status {
            Ok(status) if status.success() => {
                println!("Generated: {}", output_file);
            }
            Ok(status) => {
                eprintln!("FFmpeg failed with exit code: {:?} for {}", status.code(), label);
            }
            Err(e) => {
                eprintln!("Failed to execute ffmpeg for {}: {}", label, e);
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
        transcode_audio_streams_for_dash(input_file, output_dir, dash_audio_bitrate, &audio_streams)
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

    // Fallback: if no separate audio files, map audio from first video file
    if audio_webm_files.is_empty() {
        maps.push_str(" -map 0:a?");
    }

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
        "ffmpeg -nostdin -y -analyzeduration 10M -probesize 10M {} {}{} \
        -c copy -map_metadata -1 -f dash -dash_segment_type webm \
        -use_timeline 1 -use_template 1 -min_seg_duration 10500 \
        -adaptation_sets '{}' \
        -init_seg_name 'init_$RepresentationID$.webm' \
        -media_seg_name 'chunk_$RepresentationID$_$Number$.webm' \
        '{}/video.mpd'",
        dash_input_cmds, maps, metadata_args, adaptation_sets, dash_output_dir
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

    // Generate thumbnails
    let random_time = if duration > 0.1 {
        rand::rng().random_range(0.0..duration)
    } else {
        0.0
    };
    println!("thumbnail selected time: {:.2} seconds", random_time);

    // Generate JPG thumbnail (maintain aspect ratio)
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -nostdin -y -ss {:.2} -i '{}' -vf 'scale=1920:1080:force_original_aspect_ratio=decrease' -frames:v 1 -update 1 '{}/thumbnail.jpg'",
        random_time, input_file, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_jpg_cmd)
        .status();

    // Generate AVIF thumbnail (maintain aspect ratio)
    let thumbnail_avif_cmd = format!(
        "ffmpeg -nostdin -y -ss {:.2} -i '{}' -vf 'scale=1920:1080:force_original_aspect_ratio=decrease' -frames:v 1 -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -update 1 '{}/thumbnail.avif'",
        random_time, input_file, output_dir
    );
    println!("Executing: {}", thumbnail_avif_cmd);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_avif_cmd)
        .status();

    // Generate animated showcase.avif
    println!("Generating showcase.avif...");
    let showcase_cmd = format!(
        "ffmpeg -nostdin -y -i '{}' -vf 'scale=480:-2,fps=2,format=yuv420p10le' -frames:v 60 -c:v libaom-av1 -pix_fmt yuv420p10le -q:v 40 -cpu-used 2 -row-mt 1 '{}/showcase.avif'",
        input_file, output_dir
    );
    println!("Executing: {}", showcase_cmd);
    let showcase_status = Command::new("sh").arg("-c").arg(&showcase_cmd).status();

    match showcase_status {
        Ok(status) if status.success() => {
            println!(" showcase.avif generated successfully");
        }
        Ok(status) => {
            eprintln!(
                "Warning: showcase.avif generation failed with exit code: {:?}",
                status.code()
            );
        }
        Err(e) => {
            eprintln!("Warning: Failed to execute showcase command: {}", e);
        }
    }

    // Generate thumbnail sprites for vidstack.io (max 100 sprites per file)
    let preview_output_dir = format!("{}/previews", output_dir);
    fs::create_dir_all(&preview_output_dir).map_err(|e| {
        eprintln!("Failed to create preview output directory: {}", e);
        ffmpeg_next::Error::External
    })?;

    let interval_seconds = 5.0; // 10 second intervals for smoother seeking
    let thumb_width = 640;
    let thumb_height = 360;
    let max_sprites_per_file = 100;
    let sprites_across = 10; // 10 thumbnails per row in the sprite

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

    // Generate each sprite file
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
            "ffmpeg -nostdin -y -ss {:.3} -t {:.3} -i '{}' -vf '{}' -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -q:v 36 -r 1 -frames:v 1 -update 1 '{}'",
            start_time, duration_for_this_file, input_file, tile_filter, sprite_path
        );

        println!("Executing sprite {}: {}", sprite_idx, sprite_cmd);
        let sprite_status = Command::new("sh").arg("-c").arg(&sprite_cmd).status();

        match sprite_status {
            Ok(status) if status.success() => {
                println!("Sprite {} generated successfully", sprite_idx);
            }
            Ok(status) => {
                eprintln!(
                    "Warning: Sprite {} generation failed with exit code: {:?}",
                    sprite_idx,
                    status.code()
                );
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to execute sprite {} command: {}",
                    sprite_idx, e
                );
            }
        }
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
