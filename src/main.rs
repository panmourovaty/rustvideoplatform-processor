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
}

#[tokio::main]
async fn main() {
    let config: Config = serde_json::from_str(&fs::read_to_string("config.json").unwrap()).unwrap();

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.dbconnection)
        .await
        .unwrap();

    process(pool).await;
}

async fn process(pool: PgPool) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));

    loop {
        interval.tick().await;
        let unprocessed_concepts =
            sqlx::query!("SELECT id,type FROM media_concepts WHERE processed = false;")
                .fetch_all(&pool)
                .await
                .expect("Database error");

        for concept in unprocessed_concepts {
            if concept.r#type == "video".to_owned() {
                println!("processing concept: {}", concept.id);
                process_video(concept.id, pool.clone()).await;
            }
        }
    }
}

async fn process_video(concept_id: String, pool: PgPool) {
    fs::create_dir_all(format!("upload/{}_processing", &concept_id))
        .expect("Failed to create concept processing result directory");
    let transcode_result = transcode_video(
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
        fs::remove_file(format!("upload/{}", concept_id).as_str());
    }
}

#[derive(Serialize, Deserialize)]
struct ConceptPreview {
    startTime: u128,
    endTime: u128,
    text: String,
}

fn transcode_video(input_file: &str, output_dir: &str) -> Result<(), ffmpeg_next::Error> {
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
    if video_stream.avg_frame_rate().0 as f32 > 60.0 {
        framerate = 60.0;
    } else {
        framerate = video_stream.avg_frame_rate().0 as f32;
    }
    let duration = input_context.duration() as f64 / ffmpeg_next::ffi::AV_TIME_BASE as f64; // Video duration in seconds

    let base_bitrate_per_pixel = 4; // 33 Mbps for 4k
    let base_max_bitrate_per_pixel = 5; // 41 Mbps for 4k
    let mut audio_bitrate = 300; // 300 kbit

    let mut outputs = Vec::new();
    let mut width = original_width;
    let mut height = original_height;
    let quality_steps;
    if (width * height) >= 3686400 {
        //2k video
        quality_steps = 4;
        audio_bitrate += 100;
    } else {
        quality_steps = 3;
    }

    for i in 0..quality_steps {
        let label = match i {
            0 => "original",
            1 => "half_resolution",
            2 => "quarter_resolution",
            3 => "eighth_resolution",
            _ => unreachable!(),
        };

        let bitrate = (width * height) * base_bitrate_per_pixel;
        let max_bitrate = (width * height) * base_max_bitrate_per_pixel;
        let min_bitrate = 10_000;

        outputs.push((
            width,
            height,
            label,
            bitrate,
            max_bitrate,
            min_bitrate,
            audio_bitrate,
        ));

        width /= 2;
        height /= 2;
        audio_bitrate /= 2;
    }

    let mut webm_files = Vec::new();
    let dash_output_dir = format!("{}/video", output_dir);

    let mut cmd = format!(
        "ffmpeg -y -hwaccel vaapi -vaapi_device /dev/dri/renderD128 -i {} ",
        input_file
    );
    for (w, h, label, bitrate, max_bitrate, min_bitrate, audio_bitrate) in outputs {
        let output_file = format!("{}/output_{}.webm", output_dir, label);
        webm_files.push(output_file.clone());
        cmd.push_str(format!(" -vf 'scale_vaapi={}:{},fps={},format=nv12,hwupload' -c:v av1_vaapi -b:v {} -maxrate {} -minrate {} -c:a libopus -b:a {}k -f webm {} ",
        w, h, framerate, bitrate, max_bitrate, min_bitrate, audio_bitrate, output_file).as_str());
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
    for track_num in 0..quality_steps {
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
    let random_time = rand::thread_rng().gen_range(0.0..duration);
    println!("thumbnail selected time: {:.2} seconds", random_time);

    let thumbnail_cmd = format!(
        "ffmpeg -y -ss {:.2} -i {} -vf 'scale_vaapi=1920:1080' -frames:v 1 {}/thumbnail.jpg -frames:v 1 {}/thumbnail.avif",
        random_time, input_file, output_dir, output_dir
    );
    println!("Executing: {}", thumbnail_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(thumbnail_cmd)
        .status()
        .expect("Failed to generate thumbnails");

    // generovat previews
    let preview_output_dir = format!("{}/previews", output_dir);
    fs::create_dir_all(&preview_output_dir).expect("Failed to create preview output directory");
    let mut preview_list: Vec<ConceptPreview> = Vec::new();
    let mut preview_time: u128 = 0;
    let mut preview_id: u128 = 1;
    loop {
        let new_preview: ConceptPreview = ConceptPreview {
            startTime: preview_time,
            endTime: preview_time + 10,
            text: format!("previews/preview{}.avif", preview_id),
        };
        preview_list.push(new_preview);
        preview_time += 10;
        preview_id += 1;

        if preview_time > duration as u128 {
            break;
        }
    }
    fs::write(
        format!("{}/previews.json", preview_output_dir),
        serde_json::to_string(&preview_list).unwrap(),
    )
    .expect("Unable to write file");

    let preview_cmd = format!(
        "ffmpeg -hwaccel vaapi -hwaccel_output_format vaapi -vaapi_device /dev/dri/renderD128 -i {} -vf \"fps=1/10,scale_vaapi=320:180,format=nv12\" -vsync vfr -q:v 10 -f image2 -c:v av1_vaapi \"{}/preview%d.avif\"",
        input_file, preview_output_dir
    );
    println!("Executing: {}", preview_cmd);
    Command::new("sh")
        .arg("-c")
        .arg(preview_cmd)
        .status()
        .expect("Failed to generate previews");

    Ok(())
}
