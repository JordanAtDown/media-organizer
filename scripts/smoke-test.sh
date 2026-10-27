#!/usr/bin/env bash
# End-to-end check of a built image: usage `scripts/smoke-test.sh IMAGE`.
# Set DOCKER_PLATFORM=linux/arm64 to force a platform.
#
# A date taken from a file *name* needs no valid metadata, only an allowed
# extension, so a handful of printf-made files exercise the whole date chain.
set -euo pipefail

IMAGE="${1:?usage: $0 IMAGE}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLATFORM=()
[[ -n "${DOCKER_PLATFORM:-}" ]] && PLATFORM=(--platform "$DOCKER_PLATFORM")

WORK="$(mktemp -d)"
NAME="mo-smoke-$$"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "ok:   $*"; }
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

IN="$WORK/in"; OUT="$WORK/out"; STATE="$WORK/state"
mkdir -p "$IN/@eaDir" "$OUT" "$STATE"

# Smallest JPEG the EXIF writer accepts (SOI + EOI).
printf '\377\330\377\331' > "$IN/IMG-20250626-WA0002.jpg"
printf '\377\330\377\331' > "$IN/20220630_211409.jpg"
printf 'mp4 stub'         > "$IN/VID-20250408-WA0022.mp4"
printf 'no date anywhere' > "$IN/sans_date.jpg"
TZ=UTC touch -d '2021-03-04 10:00:00' "$IN/sans_date.jpg"
cp "$ROOT/tests/fixtures/sample_iphone.mov" "$IN/clip.mov"
printf 'thumbnail'        > "$IN/@eaDir/SYNOPHOTO_FILM_M.mp4"

cat > "$WORK/config.toml" <<TOML
stability_secs = 0
poll_interval_secs = 30
[permissions]
file_mode = "0666"
dir_mode  = "0777"
[health]
startup_grace_secs = 30
# No debounce here: the batch must be announced by the very scan that organized it.
[notify]
dir = "/var/lib/media-organizer/events"
quiet_secs = 0
max_wait_secs = 0
[[folders]]
input   = "in"        # → /inputs/in
library = "photos"    # → /libraries/photos
TOML

run() {  # run <extra docker args...> -- <app args...>
    docker run --rm "${PLATFORM[@]}" \
        -e PUID="$(id -u)" -e PGID="$(id -g)" -e TZ=UTC \
        -v "$WORK/config.toml:/etc/media-organizer/config.toml:ro" \
        -v "$IN:/inputs/in" -v "$OUT:/libraries/photos" \
        -v "$STATE:/var/lib/media-organizer" \
        "$IMAGE" "$@"
}

# Each folder is mounted on its own, like in a real compose file: the library and the
# input are different mounts, so nothing can be hard-linked between them and the
# files really are copied across — the case this smoke test exists to prove.

# 1 ── dry-run writes nothing ──────────────────────────────────────────────────
run --dry-run --once --health-bind "" >/dev/null 2>&1
[[ -z "$(ls -A "$OUT")" ]]    || fail "dry-run wrote into output"
[[ -z "$(ls -A "$STATE")" ]]  || fail "dry-run wrote state"
[[ -f "$IN/IMG-20250626-WA0002.jpg" ]] || fail "dry-run moved a source"
ok "dry-run writes nothing (no batch event either)"

# 2 ── a real pass ─────────────────────────────────────────────────────────────
LOG="$(run --once --health-bind "" 2>&1)"
expect() { [[ -f "$OUT/$1" ]] || fail "missing $1 (have: $(cd "$OUT" && find . -type f | sort | tr '\n' ' '))"; ok "placed $1"; }
expect 2025/06/2025-06-26_120000_IMG-20250626-WA0002.jpg
expect 2022/06/2022-06-30_211409_20220630_211409.jpg
expect 2025/04/2025-04-08_120000_VID-20250408-WA0022.mp4
expect 2026/02/2026-02-16_123705_clip.mov
expect 2021/03/2021-03-04_100000_sans_date.jpg

grep -q 'date_source.*mtime' <<<"$LOG" || fail "mtime-dated file was not logged as such"
ok "the mtime fallback is logged"

[[ -f "$IN/@eaDir/SYNOPHOTO_FILM_M.mp4" ]] || fail "@eaDir was not pruned"
[[ ! -e "$OUT/@eaDir" ]] || fail "@eaDir content was organized"
ok "@eaDir pruned"

[[ "$(stat -c %a "$OUT/2025/06/2025-06-26_120000_IMG-20250626-WA0002.jpg")" == 666 ]] || fail "file mode"
[[ "$(stat -c %a "$OUT/2025")"    == 777 ]] || fail "year directory mode"
[[ "$(stat -c %a "$OUT/2025/06")" == 777 ]] || fail "month directory mode"
ok "modes 666 / 777 on the file and every directory"

grep -aq '2025:06:26 12:00:00' "$OUT/2025/06/2025-06-26_120000_IMG-20250626-WA0002.jpg" \
    || fail "DateTimeOriginal was not injected"
ok "EXIF date injected from the file name"

[[ -z "$(ls -A "$STATE/work/in")" ]] || fail "work area not empty after a clean scan"
[[ -z "$(find "$OUT" -name '*.mo-part')" ]] || fail "temporary files left in the library"
[[ ! -e "$OUT/.media-organizer" ]] || fail "the library was polluted with a work directory"
[[ -z "$(find "$IN" -maxdepth 1 -type f)" ]] || fail "sources left behind: $(ls "$IN")"
ok "work area empty, nothing left in the library, sources consumed"

# The batch was announced: one atomic event file, readable by another uid, no temporary.
EVENT="$STATE/events/in.json"
[[ -f "$EVENT" ]] || fail "no batch event was written (have: $(cd "$STATE" && find . -type f | sort | tr '\n' ' '))"
grep -q '"folder":"in","seq":1,' "$EVENT"    || fail "unexpected event: $(cat "$EVENT")"
grep -q '"committed":5,"deleted_sources":0,' "$EVENT" || fail "unexpected counts: $(cat "$EVENT")"
grep -q '"library_dirs":\["2021/03","2022/06","2025/04","2025/06","2026/02"\]' "$EVENT" \
    || fail "unexpected library_dirs: $(cat "$EVENT")"
grep -q '"input_dirs":\["\."\]' "$EVENT" || fail "unexpected input_dirs: $(cat "$EVENT")"
[[ "$(stat -c %a "$EVENT")" == 644 ]] || fail "event mode $(stat -c %a "$EVENT")"
[[ -z "$(find "$STATE/events" -mindepth 2 -type f)" ]] || fail "a temporary was left in events/.tmp"
ok "one batch event: seq 1, 5 files, relative directories, mode 644"

# 3 ── a copy of an organized file is a duplicate: left alone, remembered ─────
cp "$OUT/2025/06/2025-06-26_120000_IMG-20250626-WA0002.jpg" "$IN/IMG-20250626-WA0002.jpg"
run --once --health-bind "" >/dev/null 2>&1
[[ -f "$IN/IMG-20250626-WA0002.jpg" ]] || fail "a duplicate was deleted"
[[ -z "$(find "$OUT" -name '*_1.jpg')" ]] || fail "a duplicate got a _1 suffix"
grep -q 'IMG-20250626-WA0002.jpg' "$STATE/duplicates.json" || fail "duplicate not remembered"
ok "duplicate left in place, remembered, no _1"
grep -q '"seq":1,' "$EVENT" || fail "a kept duplicate was announced: $(cat "$EVENT")"
ok "a batch that changed nothing announces nothing"

# 4 ── daemon mode: healthz, then a prompt and clean stop ─────────────────────
docker run -d --name "$NAME" "${PLATFORM[@]}" \
    -e PUID="$(id -u)" -e PGID="$(id -g)" -e TZ=UTC -p 127.0.0.1:0:8080 \
    -v "$WORK/config.toml:/etc/media-organizer/config.toml:ro" \
    -v "$IN:/inputs/in" -v "$OUT:/libraries/photos" \
    -v "$STATE:/var/lib/media-organizer" "$IMAGE" >/dev/null
for _ in $(seq 1 30); do
    docker exec "$NAME" media-organizer healthcheck >/dev/null 2>&1 && break
    sleep 1
done
docker exec "$NAME" media-organizer healthcheck >/dev/null 2>&1 || fail "healthcheck never passed"
PORT="$(docker port "$NAME" 8080/tcp | head -1 | sed 's/.*://')"
BODY="$(curl -fsS "http://127.0.0.1:$PORT/healthz")"
grep -q '"status": "ok"' <<<"$BODY" || fail "healthz is not ok: $BODY"
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/livez")" == 200 ]] || fail "livez"
ok "healthz ok, livez 200"
grep -q '"notify"' <<<"$BODY" || fail "healthz has no notify block: $BODY"
grep -q '"seq": 1' <<<"$BODY"  || fail "healthz does not report the last announced batch: $BODY"
ok "healthz restores the last announced batch after a restart"

START="$(date +%s.%N)"
docker stop -t 10 "$NAME" >/dev/null
ELAPSED="$(echo "$(date +%s.%N) - $START" | bc)"
CODE="$(docker inspect -f '{{.State.ExitCode}}' "$NAME")"
[[ "$CODE" == 0 ]] || fail "exit code $CODE on stop (143 means SIGTERM was not handled)"
awk "BEGIN{exit !($ELAPSED <= 3)}" || fail "stop took ${ELAPSED}s"
ok "docker stop in ${ELAPSED}s, exit code 0"

echo "smoke test passed"
