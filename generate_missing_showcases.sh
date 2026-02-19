#!/bin/bash
# generate_missing_showcases.sh
#
# Scans all processed video directories (upload/*_processing/) and generates
# showcase.avif for any that have video/video.webm but are missing showcase.avif.
#
# Uses the same ffmpeg parameters as the Rust processor:
#   - 480px width, aspect ratio preserved
#   - 60 frames at 2fps (30-second animation)
#   - libaom-av1 codec, q:v 40, 10-bit color

UPLOAD_DIR="${1:-upload}"

if [ ! -d "$UPLOAD_DIR" ]; then
    echo "Error: directory '$UPLOAD_DIR' does not exist"
    exit 1
fi

generated=0
skipped=0
failed=0

for dir in "$UPLOAD_DIR"/*_processing; do
    [ -d "$dir" ] || continue

    video_file="$dir/video/video.webm"
    showcase_file="$dir/showcase.avif"

    if [ ! -f "$video_file" ]; then
        continue
    fi

    if [ -f "$showcase_file" ]; then
        skipped=$((skipped + 1))
        continue
    fi

    echo "Generating showcase.avif for $dir ..."
    ffmpeg -y -i "$video_file" \
        -vf 'scale=480:-1:force_original_aspect_ratio=decrease,fps=2,format=yuv420p10le' \
        -frames:v 60 \
        -c:v libaom-av1 \
        -pix_fmt yuv420p10le \
        -q:v 40 \
        -cpu-used 6 \
        -row-mt 1 \
        "$showcase_file"

    if [ $? -eq 0 ] && [ -f "$showcase_file" ]; then
        echo "  OK: $showcase_file"
        generated=$((generated + 1))
    else
        echo "  FAILED: $dir"
        failed=$((failed + 1))
    fi
done

echo ""
echo "Done. Generated: $generated | Already existed: $skipped | Failed: $failed"
