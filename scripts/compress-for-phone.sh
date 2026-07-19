#!/usr/bin/env bash
#
# compress-for-phone.sh
#
# Transcode downloaded Bilibili videos into small, phone-friendly copies for
# offline viewing, laid out as a Jellyfin/Plex/Infuse TV library in iCloud Drive.
#
#   Source: ~/Library/Application Support/com.lanyeeee.bilibili-video-downloader/视频下载
#   Dest:   ~/Library/Mobile Documents/com~apple~CloudDocs/Videos/Library
#
# Output: 360p (long edge capped, aspect preserved) / 15 fps / HEVC CRF 30.
#
# Each source file "<series>/NNNN.<title>.mp4" becomes a TV-library entry:
#   <show>/Season 01/<show> - S01E<NN> - <title>.mp4
# where <show> is the series folder mapped through map_show() below (falling back
# to the folder name), <NN> is the NNNN prefix, and <title> is the rest of the
# filename. TV metadata (show / season / episode / title, iTunes media_type 10)
# is embedded into each newly converted file.
#
# For Jellyfin/Emby it also writes local metadata sidecars (so custom shows that
# aren't in TheMovieDB still group/sort correctly without any online match):
#   - a per-episode "<video>.nfo" (title, plot, aired, director, showtitle)
#   - a per-episode "<video>-thumb.jpg" thumbnail
#   - a per-show "tvshow.nfo" (title, overview, studio) and "poster.jpg"
# Rich fields come from the matching source ".nfo"/".jpg"; episodes whose source
# has since been deleted get a minimal NFO derived from the destination filename.
# Sidecars are written only when missing, so manual edits and reruns are safe.
#
# In Jellyfin: add the destination as a *Shows* library, enable the Nfo metadata
# reader, and disable the online (TheMovieDB/TheTVDB) fetchers for series/episodes.
#
# Usage:
#   ./compress-for-phone.sh [-n COUNT] [SRC_DIR] [DST_DIR]
#
# Options:
#   -n, --limit COUNT   Convert at most COUNT new videos, then stop. Already-
#                       converted files that are skipped do NOT count toward the
#                       limit, so `-n 3` always encodes the next 3 pending videos.
#
# Re-running is safe: files already converted (and non-empty) are skipped, so a
# second run only encodes newly downloaded videos.

set -euo pipefail

LIMIT=0  # 0 means no limit

# Parse the -n/--limit flag; remaining args are the optional SRC and DST paths.
positional=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -n | --limit)
      LIMIT="${2:-}"
      if ! [[ "$LIMIT" =~ ^[0-9]+$ ]]; then
        echo "error: $1 requires a non-negative integer, got: '${2:-}'" >&2
        exit 1
      fi
      shift 2
      ;;
    -h | --help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do positional+=("$1"); shift; done
      ;;
    -*)
      echo "error: unknown option: $1" >&2
      exit 1
      ;;
    *)
      positional+=("$1")
      shift
      ;;
  esac
done

SRC="${positional[0]:-$HOME/Library/Application Support/com.lanyeeee.bilibili-video-downloader/视频下载}"
DST="${positional[1]:-$HOME/Library/Mobile Documents/com~apple~CloudDocs/Videos/Library}"

# Encoding knobs — tweak here if you want a different size/quality tradeoff.
SCALE_LONG_EDGE=640   # width cap
SCALE_SHORT_EDGE=360  # height cap
FPS=15
CRF=30
PRESET=medium
AUDIO_BITRATE=96k

# Every series is treated as a single season.
SEASON="01"

# Map a source series folder name to the show name used in the TV-library layout
# and metadata. Add a `"<folder>") echo "<show>";;` case to simplify/rename a
# series; folders with no case fall through to their own name unchanged.
# (A function + case is used instead of an associative array so the script runs
# on macOS's stock bash 3.2, which has no `declare -A`.)
map_show() {
  case "$1" in
    "【周易精讲】细读百家之祖，万法之源") echo "周易精讲" ;;
    # "【某某】长长的副标题") echo "某某" ;;
    *) echo "$1" ;;
  esac
}

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "error: ffmpeg not found on PATH" >&2
  exit 1
fi

# python3 builds the NFO XML — it escapes CJK/&<> correctly and can read the
# source Kodi .nfo, which is far safer than assembling XML by hand in bash.
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 not found on PATH (needed to write .nfo metadata)" >&2
  exit 1
fi

if [[ ! -d "$SRC" ]]; then
  echo "error: source directory does not exist: $SRC" >&2
  exit 1
fi

# Write "<video>.nfo" for one episode. Rich fields are pulled from the source
# Kodi .nfo when the 6th arg points at an existing file; otherwise minimal.
#   args: out_nfo  title  season  episode  showtitle  src_nfo(may be "")
gen_episode_nfo() {
  python3 - "$@" <<'PY'
import sys, xml.etree.ElementTree as ET
out, title, season, episode, showtitle = sys.argv[1:6]
src = sys.argv[6] if len(sys.argv) > 6 else ""
plot = aired = director = studio = ""
if src:
    try:
        r = ET.parse(src).getroot()
        def g(tag):
            e = r.find(tag)
            return e.text or "" if e is not None else ""
        plot, aired, director, studio = g("plot"), g("premiered"), g("director"), g("studio")
    except Exception:
        pass
root = ET.Element("episodedetails")
ET.SubElement(root, "title").text = title
ET.SubElement(root, "showtitle").text = showtitle
ET.SubElement(root, "season").text = season
ET.SubElement(root, "episode").text = episode
for tag, val in (("plot", plot), ("aired", aired), ("studio", studio), ("director", director)):
    if val:
        ET.SubElement(root, tag).text = val
ET.indent(root)
ET.ElementTree(root).write(out, encoding="utf-8", xml_declaration=True)
PY
}

# Write "tvshow.nfo" for one show. Overview comes from the source .nfo's
# <set><overview> when the 3rd arg points at an existing file.
#   args: out_nfo  show_title  src_nfo(may be "")
gen_show_nfo() {
  python3 - "$@" <<'PY'
import sys, xml.etree.ElementTree as ET
out, title = sys.argv[1:3]
src = sys.argv[3] if len(sys.argv) > 3 else ""
plot = ""
if src:
    try:
        ov = ET.parse(src).getroot().find("set/overview")
        plot = ov.text or "" if ov is not None else ""
    except Exception:
        pass
root = ET.Element("tvshow")
ET.SubElement(root, "title").text = title
if plot:
    ET.SubElement(root, "plot").text = plot
ET.SubElement(root, "studio").text = "Bilibili"
ET.indent(root)
ET.ElementTree(root).write(out, encoding="utf-8", xml_declaration=True)
PY
}

mkdir -p "$DST"

# Encode into a local temp dir (NOT the destination). This keeps failed/partial
# encodes out of the destination entirely, and — since the destination is iCloud
# Drive — avoids iCloud uploading a half-written file during every long encode.
# The finished file is moved into the destination only on success.
workdir="$(mktemp -d "${TMPDIR:-/tmp}/compress-for-phone.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

# Collect video files (case-insensitive extensions), NUL-delimited to survive
# spaces and non-ASCII characters in the CJK filenames.
videos=()
while IFS= read -r -d '' f; do
  videos+=("$f")
done < <(find "$SRC" -type f \( \
  -iname '*.mp4' -o -iname '*.mkv' -o -iname '*.flv' -o \
  -iname '*.mov' -o -iname '*.m4v' \) -print0 | sort -z)

total="${#videos[@]}"
if [[ "$total" -eq 0 ]]; then
  echo "No video files found under: $SRC"
  exit 0
fi

echo "Found $total video(s)."
echo "  Source: $SRC"
echo "  Dest:   $DST"
[[ "$LIMIT" -gt 0 ]] && echo "  Limit:  convert up to $LIMIT new video(s)"
echo

converted=0
skipped=0
failed=0
meta=0
i=0

for src in "${videos[@]}"; do
  i=$((i + 1))

  rel="${src#"$SRC"/}"
  work="$workdir/out.mp4"     # local temp target for this encode

  printf '[%d/%d] %s\n' "$i" "$total" "$rel"

  # Derive the TV-library naming from "<series>/…/NNNN.<title>.<ext>".
  series="${rel%%/*}"                 # first path component = series folder
  base="$(basename "$rel")"
  base="${base%.*}"                   # strip extension
  if [[ ! "$base" =~ ^([0-9]+)\.(.+)$ ]]; then
    echo "        → skip (no NNNN. episode prefix)"
    skipped=$((skipped + 1))
    continue
  fi
  epnum="${BASH_REMATCH[1]}"
  title="${BASH_REMATCH[2]}"
  show="$(map_show "$series")"
  ep=$((10#$epnum))                   # integer episode number, leading zeros stripped
  eppad="$(printf '%02d' "$ep")"
  out="$DST/$show/Season $SEASON/$show - S${SEASON}E${eppad} - $title.mp4"

  if [[ -s "$out" ]]; then
    echo "        → skip (already converted)"
    skipped=$((skipped + 1))
  elif [[ "$LIMIT" -gt 0 && "$converted" -ge "$LIMIT" ]]; then
    # Encode limit reached: don't encode more. There's no output file to attach
    # metadata to, so move on (already-converted episodes further down the list
    # still get their metadata via the skip branch above).
    echo "        → skip encode (limit of $LIMIT reached)"
    continue
  else
    rm -f "$work"
    if ffmpeg -nostdin -hide_banner -loglevel error -stats -y \
      -i "$src" \
      -vf "scale=w=${SCALE_LONG_EDGE}:h=${SCALE_SHORT_EDGE}:force_original_aspect_ratio=decrease,fps=${FPS}" \
      -c:v libx265 -crf "$CRF" -preset "$PRESET" -tag:v hvc1 \
      -c:a aac -b:a "$AUDIO_BITRATE" \
      -metadata title="$title" \
      -metadata show="$show" \
      -metadata episode_id="$title" \
      -metadata episode_sort="$ep" \
      -metadata season_number="$((10#$SEASON))" \
      -metadata media_type=10 \
      -movflags +faststart \
      "$work"; then
      # Only now touch the destination: copy the finished file into place under a
      # temp name (cross-device copy into iCloud), then atomically rename. This way
      # the destination never holds a partial file that the skip check above would
      # later mistake for a completed conversion.
      mkdir -p "$(dirname "$out")"
      part="$out.part"
      rm -f "$part"
      mv -f "$work" "$part"
      mv -f "$part" "$out"
      echo "        → done: $out"
      converted=$((converted + 1))
    else
      rm -f "$work"
      echo "        → FAILED: $src" >&2
      failed=$((failed + 1))
      continue
    fi
  fi

  # Write metadata + artwork for the episode now on disk (new or already there).
  # Everything is write-if-missing, and rich fields come from the source sidecars.
  src_noext="${src%.*}"
  src_nfo=""; [[ -f "$src_noext.nfo" ]] && src_nfo="$src_noext.nfo"
  nfo="${out%.mp4}.nfo"
  thumb="${out%.mp4}-thumb.jpg"
  show_dir="$DST/$show"
  if [[ ! -f "$nfo" ]]; then
    gen_episode_nfo "$nfo" "$title" "$((10#$SEASON))" "$ep" "$show" "$src_nfo"
    meta=$((meta + 1))
  fi
  if [[ ! -f "$thumb" && -f "$src_noext.jpg" ]]; then
    cp "$src_noext.jpg" "$thumb"
    meta=$((meta + 1))
  fi
  if [[ ! -f "$show_dir/tvshow.nfo" ]]; then
    gen_show_nfo "$show_dir/tvshow.nfo" "$show" "$src_nfo"
    meta=$((meta + 1))
  fi
  if [[ ! -f "$show_dir/poster.jpg" && -f "$SRC/$series/poster.jpg" ]]; then
    cp "$SRC/$series/poster.jpg" "$show_dir/poster.jpg"
    meta=$((meta + 1))
  fi
done

# Backfill pass: destination episodes whose source has been deleted never appear
# in the loop above, so give any .mp4 that still lacks an .nfo a minimal one built
# from its "<show> - S01E<NN> - <title>" filename, and make sure every show that
# owns a video has a tvshow.nfo. Show dirs are derived from the videos themselves
# (not by listing top-level dirs) so stray/hidden folders are never touched.
while IFS= read -r -d '' mp4; do
  nfo="${mp4%.mp4}.nfo"
  if [[ ! -f "$nfo" ]]; then
    bn="$(basename "$mp4")"; bn="${bn%.mp4}"
    if [[ "$bn" =~ ^(.+)\ -\ S0*([0-9]+)E0*([0-9]+)\ -\ (.+)$ ]]; then
      gen_episode_nfo "$nfo" "${BASH_REMATCH[4]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[1]}" ""
      meta=$((meta + 1))
    else
      echo "backfill: skip (unparseable name): $bn" >&2
    fi
  fi
  rel="${mp4#"$DST"/}"
  showdir="$DST/${rel%%/*}"                    # first path component under DST
  if [[ ! -f "$showdir/tvshow.nfo" ]]; then
    gen_show_nfo "$showdir/tvshow.nfo" "$(basename "$showdir")" ""
    meta=$((meta + 1))
  fi
done < <(find "$DST" -type f -iname '*.mp4' -not -path '*/.*' -print0 | sort -z)

echo
echo "Summary: $converted converted, $skipped skipped, $failed failed, $meta metadata file(s) written (of $total)."
[[ "$failed" -eq 0 ]]
