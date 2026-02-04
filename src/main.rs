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

#[derive(Deserialize, Clone)]
struct Config {
    dbconnection: String,
    video: VideoConfig,
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
    lookahead: Option<u32>,
    #[serde(default)]
    look_ahead_depth: Option<u32>,
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
        "ffprobe -v error -select_streams v:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 {}",
        input_file
    );

    // Check for audio stream
    let audio_probe_cmd = format!(
        "ffprobe -v error -select_streams a:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -select_streams v:0 -show_entries stream=nb_frames -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate,avg_frame_rate -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -select_streams v:0 -show_entries stream=nb_frames -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate,avg_frame_rate -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
            "ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
            sqlx::query!("SELECT id,type FROM media_concepts WHERE processed = false;")
                .fetch_all(&pool)
                .await
                .expect("Database error");

        for concept in unprocessed_concepts {
            let input_file = format!("upload/{}", concept.id);
            let detected_type = detect_file_type(&input_file);

            // Override database type if detection yields different result
            let actual_type = if let Some(dt) = detected_type {
                dt
            } else {
                concept.r#type.clone()
            };

            if actual_type == "video".to_owned() {
                println!("processing concept: {} as video", concept.id);
                process_video(concept.id, pool.clone(), video_config.clone()).await;
            } else if actual_type == "picture".to_owned() {
                println!("processing concept: {} as picture", concept.id);
                process_picture(concept.id, pool.clone()).await;
            } else if actual_type == "audio".to_owned() {
                println!("processing concept: {} as audio", concept.id);
                process_audio(concept.id, pool.clone()).await;
            }
        }
    }
}

async fn process_video(concept_id: String, pool: PgPool, video_config: VideoConfig) {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .expect("Failed to create concept processing result directory");
    let transcode_result = transcode_video(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
        &video_config,
    );
    if transcode_result.is_ok() {
        sqlx::query!(
            "UPDATE media_concepts SET processed = true WHERE id = $1;",
            concept_id
        )
        .execute(&pool)
        .await
        .expect("Database error");
        let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
    }
}

async fn process_picture(concept_id: String, pool: PgPool) {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .expect("Failed to create concept processing result directory");
    let transcode_result = transcode_picture(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
    );
    if transcode_result.is_ok() {
        sqlx::query!(
            "UPDATE media_concepts SET processed = true WHERE id = $1;",
            concept_id
        )
        .execute(&pool)
        .await
        .expect("Database error");
        let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
    }
}

async fn process_audio(concept_id: String, pool: PgPool) {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .expect("Failed to create concept processing result directory");
    let transcode_result = transcode_audio(
        format!("upload/{}", concept_id).as_str(),
        format!("upload/{}_processing", concept_id).as_str(),
    );
    if transcode_result.is_ok() {
        sqlx::query!(
            "UPDATE media_concepts SET processed = true WHERE id = $1;",
            concept_id
        )
        .execute(&pool)
        .await
        .expect("Database error");
        let _ = fs::remove_file(format!("upload/{}", concept_id).as_str());
    }
}

fn transcode_picture(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
    // Get image dimensions
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 {}",
        input_file
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&probe_cmd)
        .output()
        .expect("Failed to probe image dimensions");
    let dimensions = String::from_utf8_lossy(&output.stdout);
    let (orig_width, orig_height) = parse_dimensions(&dimensions);

    // Calculate scaled dimensions for HD thumbnail (closest to 1920x1080 while maintaining aspect ratio)
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height);

    // Full resolution AVIF
    let transcode_cmd = format!(
        "ffmpeg -i {} -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 {}/picture.avif",
        input_file, output_dir
    );
    println!("Executing: {}", transcode_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(transcode_cmd)
        .status()
        .expect("Failed to transcode picture");

    // HD thumbnail AVIF with proper aspect ratio
    let thumbnail_cmd = format!(
            "ffmpeg -i {} -c:v libsvtav1 -svtav1-params avif=1 -crf 30 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 {}/thumbnail.avif",
            input_file, thumb_width, thumb_height, output_dir
        );
    println!("Executing: {}", thumbnail_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(thumbnail_cmd)
        .status()
        .expect("Failed to transcode picture thumbnail");

    // HD thumbnail JPG for older devices
    let thumbnail_ogp_cmd = format!(
        "ffmpeg -i {} -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 {}/thumbnail.jpg",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_ogp_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(thumbnail_ogp_cmd)
        .status()
        .expect("Failed to transcode picture thumbnail for OGP");

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
        "ffprobe -v error -select_streams a:0 -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 {}",
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
        "ffprobe -v error -select_streams a:0 -show_entries stream=bit_rate -of default=noprint_wrappers=1:nokey=1 {}",
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
        "ffprobe -v error -select_streams v:0 -show_entries stream=codec_type -of default=noprint_wrappers=1:nokey=1 {}",
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
        "ffprobe -v error -select_streams v:0 -show_entries stream=duration -of default=noprint_wrappers=1:nokey=1 {}",
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
        "ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 {} | wc -l",
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
        "ffprobe -v error -select_streams v -show_entries stream=index -of csv=p=0 {} | wc -l",
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
        "ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 {}",
        input_file
    );

    let output = Command::new("sh")
        .arg("-c")
        .arg(&count_cmd)
        .output()
        .expect("Failed to count audio streams");

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
            "ffmpeg -i {} -map 0:a:{} -c:a libopus -b:a {} -vbr on -application audio {} -y",
            input_file, stream_idx, bitrate, output_path
        );

        println!("Executing: {}", extract_cmd);
        Command::new("sh")
            .arg("-c")
            .arg(&extract_cmd)
            .status()
            .expect(&format!("Failed to extract audio stream {}", idx + 1));
    }

    Ok(())
}

fn get_audio_codec_for_stream(input_file: &str, stream_idx: u32) -> String {
    let codec_cmd = format!(
        "ffprobe -v error -select_streams a:{} -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 {}",
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
        "ffprobe -v error -select_streams v:1 -show_entries stream=width,height -of csv=s=x:p=0 {}",
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
                "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 {}",
                input_file
            );
            let output = Command::new("sh")
                .arg("-c")
                .arg(&probe_cmd)
                .output()
                .expect("Failed to probe video dimensions");
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
        "ffmpeg -i {} -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 {}/picture.avif -y",
        input_file, stream_selector, output_dir
    );
    println!("Executing: {}", cover_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(&cover_cmd)
        .status()
        .expect("Failed to extract cover art");

    // Create thumbnail AVIF
    let thumbnail_cmd = format!(
        "ffmpeg -i {} -map 0:{} -c:v libsvtav1 -svtav1-params avif=1 -crf 30 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 {}/thumbnail.avif -y",
        input_file, stream_selector, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_cmd)
        .status()
        .expect("Failed to create cover thumbnail");

    // Create thumbnail JPG for older devices
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -i {} -map 0:{} -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 {}/thumbnail.jpg -y",
        input_file, stream_selector, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_jpg_cmd)
        .status()
        .expect("Failed to create cover thumbnail JPG");

    Ok(())
}

fn extract_album_cover(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
    // Get album cover dimensions
    let probe_cmd = format!(
        "ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 {}",
        input_file
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&probe_cmd)
        .output()
        .expect("Failed to probe album cover dimensions");
    let dimensions = String::from_utf8_lossy(&output.stdout);
    let (orig_width, orig_height) = parse_dimensions(&dimensions);

    // Calculate scaled dimensions for HD thumbnail
    let (thumb_width, thumb_height) = calculate_hd_scale(orig_width, orig_height);

    // Extract full resolution album cover
    let cover_cmd = format!(
        "ffmpeg -i {} -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf 26 -b:v 0 -frames:v 1 -f image2 {}/picture.avif -y",
        input_file, output_dir
    );
    println!("Executing: {}", cover_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(cover_cmd)
        .status()
        .expect("Failed to extract album cover");

    // Create thumbnail AVIF
    let thumbnail_cmd = format!(
        "ffmpeg -i {} -map 0:v:0 -c:v libsvtav1 -svtav1-params avif=1 -crf 30 -vf 'scale={}:{}:force_original_aspect_ratio=decrease,format=yuv420p10le' -b:v 0 -frames:v 1 -f image2 {}/thumbnail.avif -y",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(thumbnail_cmd)
        .status()
        .expect("Failed to create album cover thumbnail");

    // Create thumbnail JPG for older devices
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -i {} -map 0:v:0 -vf 'scale={}:{}:force_original_aspect_ratio=decrease' -q:v 25 {}/thumbnail.jpg -y",
        input_file, thumb_width, thumb_height, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(thumbnail_jpg_cmd)
        .status()
        .expect("Failed to create album cover thumbnail JPG");

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
        "ffmpeg -i {} -c:a libopus -b:a {} -vbr on -application audio {} -y",
        input_file, bitrate, output_path
    );
    println!("Executing: {}", transcode_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(transcode_cmd)
        .status()
        .expect("Failed to transcode audio");

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

fn build_encoder_params(config: &VideoConfig, framerate: f32) -> (String, VideoCodecParams) {
    match config.encoder {
        VideoEncoder::Nvenc => {
            let settings = config.nvenc.as_ref().expect("NVENC settings required");
            let hwaccel = "-hwaccel cuda -hwaccel_output_format cuda".to_string();

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
                VideoCodecParams {
                    codec: settings.codec.clone(),
                    params,
                    framerate,
                },
            )
        }
        VideoEncoder::Qsv => {
            let settings = config.qsv.as_ref().expect("QSV settings required");
            let hwaccel = "-hwaccel qsv -hwaccel_output_format qsv".to_string();

            let mut params = format!(
                "-c:v {} -preset {} -global_quality {} -extbrc 1",
                settings.codec, settings.preset, settings.global_quality
            );

            if let Some(la) = settings.lookahead {
                params.push_str(&format!(
                    " -look_ahead {}",
                    if la > 0 { "on" } else { "off" }
                ));
                if let Some(depth) = settings.look_ahead_depth {
                    params.push_str(&format!(" -look_ahead_depth {}", depth));
                }
            }

            (
                hwaccel,
                VideoCodecParams {
                    codec: settings.codec.clone(),
                    params,
                    framerate,
                },
            )
        }
        VideoEncoder::Vaapi => {
            let settings = config.vaapi.as_ref().expect("VAAPI settings required");
            let hwaccel = "-hwaccel vaapi -vaapi_device /dev/dri/renderD128".to_string();

            let quality = settings.quality;
            let mut params = format!(
                "-c:v {} -global_quality {} -qp {}",
                settings.codec, quality, quality
            );

            params.push_str(" -compression_level 7");

            (
                hwaccel,
                VideoCodecParams {
                    codec: settings.codec.clone(),
                    params,
                    framerate,
                },
            )
        }
    }
}

#[derive(Clone)]
struct VideoCodecParams {
    codec: String,
    params: String,
    framerate: f32,
}

fn format_video_params(codec_params: &VideoCodecParams, width: u32, height: u32) -> String {
    format!(
        "{} -vf 'scale={}:{}:force_original_aspect_ratio=decrease,fps={}'",
        codec_params.params, width, height, codec_params.framerate
    )
}

fn format_timestamp_vtt(seconds: f64) -> String {
    let hours = (seconds / 3600.0) as u32;
    let minutes = ((seconds % 3600.0) / 60.0) as u32;
    let secs = (seconds % 60.0) as u32;
    let millis = ((seconds % 1.0) * 1000.0) as u32;
    format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, secs, millis)
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
    let aspect_ratio = original_width as f32 / original_height as f32;

    let mut outputs = Vec::new();
    let mut width = original_width;
    let mut height = original_height;

    // Determine number of quality steps based on resolution
    let num_steps = if (width * height) >= config.threshold_2k_pixels {
        config.max_resolution_steps
    } else {
        config.max_resolution_steps - 1
    };

    for i in 0..num_steps.min(config.quality_steps.len() as u32) {
        let step = &config.quality_steps[i as usize];

        if num_steps >= config.max_resolution_steps {
            audio_bitrate += config.audio_bitrate_2k_bonus;
        }

        outputs.push((width, height, step.label.clone(), audio_bitrate));

        // Calculate next resolution by dividing by scale_divisor
        let scale_factor = 1.0 / step.scale_divisor as f32;
        width =
            ((width as f32 * scale_factor / aspect_ratio).round() * aspect_ratio).round() as u32;
        height = (width as f32 / aspect_ratio).round() as u32;
        // Ensure dimensions are even (required for many codecs)
        width = width.max(config.min_dimension);
        height = height.max(config.min_dimension);
        width = (width / 2) * 2;
        height = (height / 2) * 2;

        audio_bitrate = (audio_bitrate / step.audio_bitrate_divisor).max(64);
    }

    let mut webm_files = Vec::new();
    let dash_output_dir = format!("{}/video", output_dir);

    // Build encoder-specific ffmpeg command
    let (hwaccel_args, video_codec_params) = build_encoder_params(config, framerate);

    let mut cmd = format!("ffmpeg -y {} -i {} ", hwaccel_args, input_file);
    for (w, h, label, audio_bitrate) in &outputs {
        let output_file = format!("{}/output_{}.webm", output_dir, label);
        webm_files.push(output_file.clone());
        let video_params = format_video_params(&video_codec_params, *w, *h);
        cmd.push_str(&format!(" -vf '{},unsharp=3:3:1.0:3:3:0.0,format=p010le,hwupload' {} -c:a libopus -b:a {}k -f webm {} ",
                config.filters, video_params, audio_bitrate, output_file));
    }

    println!("Executing: {}", cmd);
    Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .expect("Failed to execute ffmpeg command");

    println!("Creating WebM DASH manifest...");
    webm_files.retain(|file| fs::metadata(file).is_ok());

    fs::create_dir_all(&dash_output_dir).expect("Failed to create DASH output directory");

    let dash_input_cmds = webm_files
        .iter()
        .map(|file| format!("-i {}", file))
        .collect::<Vec<String>>()
        .join(" ");

    let mut maps: String = String::new();
    for track_num in 0..webm_files.len() {
        maps.push_str(format!(" -map {}:v -map {}:a ", track_num, track_num).as_str())
    }

    let dash_output_cmd = format!(
            "ffmpeg -y {} {} -c copy -f dash -dash_segment_type \"webm\" -use_timeline 1 -use_template 1 -adaptation_sets 'id=0,streams=v id=1,streams=a' -init_seg_name 'init_$RepresentationID$.webm' -media_seg_name 'chunk_$RepresentationID$_$Number$.webm' {}/video.mpd",
            dash_input_cmds, maps, dash_output_dir
        );

    println!("Executing: {}", dash_output_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(dash_output_cmd)
        .status()
        .expect("Failed to create WebM DASH stream");

    //OGP video
    let ogp_video_result = fs::rename(
        format!("{}/output_quarter_resolution.webm", output_dir),
        format!("{}/video/video.webm", output_dir),
    );
    webm_files.remove(2);
    println!("CREATED OGP VIDEO: {:?}", ogp_video_result);

    // smazat mezividea
    println!("Remove WebM files...");
    for file in webm_files {
        fs::remove_file(file).expect("Failed to delete WebM file");
    }

    // generovat thumbnails
    let random_time = rand::rng().random_range(0.0..duration);
    println!("thumbnail selected time: {:.2} seconds", random_time);

    // Generate JPG thumbnail
    let thumbnail_jpg_cmd = format!(
        "ffmpeg -y -ss {:.2} -i {} -vf 'scale=1920:1080' -frames:v 1 -update 1 {}/thumbnail.jpg",
        random_time, input_file, output_dir
    );
    println!("Executing: {}", thumbnail_jpg_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(&thumbnail_jpg_cmd)
        .status()
        .expect("Failed to generate jpg thumbnail");

    // Generate AVIF thumbnail
    let thumbnail_avif_cmd = format!(
        "ffmpeg -y -ss {:.2} -i {} -vf 'scale=1920:1080' -frames:v 1 -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -update 1 {}/thumbnail.avif",
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
        "ffmpeg -y -i {} -vf 'scale=480:-1:force_original_aspect_ratio=decrease,fps=2,format=yuv420p10le' -frames:v 60 -c:v libaom-av1 -pix_fmt yuv420p10le -q:v 40 -cpu-used 6 -row-mt 1 {}/showcase.avif",
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
    fs::create_dir_all(&preview_output_dir).expect("Failed to create preview output directory");

    let interval_seconds = 10.0; // 10 second intervals for smoother seeking
    let thumb_width = 160;
    let thumb_height = 90;
    let max_sprites_per_file = 100;
    let sprites_across = 10; // 10 thumbnails per row in the sprite

    // Calculate number of thumbnails needed
    let num_thumbnails = (duration / interval_seconds).ceil() as u32;
    let num_sprite_files = ((num_thumbnails as f32) / (max_sprites_per_file as f32)).ceil() as u32;

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
            "ffmpeg -y -ss {:.3} -t {:.3} -i {} -vf '{}' -c:v libsvtav1 -svtav1-params avif=1 -pix_fmt yuv420p10le -q:v 60 -r 1 -frames:v 1 -update 1 {}",
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

    fs::write(&vtt_path, vtt_content).expect("Failed to write thumbnails.vtt file");
    println!(
        "Generated WebVTT thumbnails file with {} cues across {} sprite files",
        vtt_cues.len(),
        num_sprite_files
    );

    Ok(())
}
