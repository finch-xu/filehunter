#!/usr/bin/env bash
# bench/bench_modes.sh — FileHunter search-mode benchmark
# Dependencies: curl, dd, awk, xargs, python3 (all pre-installed on macOS/Linux)
# Compatible with bash 3.2+ (macOS default)
set -euo pipefail

###############################################################################
# Configuration
###############################################################################
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$ROOT_DIR/target/release/filehunter"
BENCH_DIR="/tmp/filehunter-bench"
RESULTS_FILE="$ROOT_DIR/bench/results.md"
REPEAT=30          # single-request iterations
CONCURRENCY=50     # parallel requests
CURL_TIMEOUT=10    # per-request timeout (seconds)

###############################################################################
# Cleanup
###############################################################################
cleanup() {
    # stop any sampler still running
    [ -f "$BENCH_DIR/sampler_pid" ] && kill "$(cat "$BENCH_DIR/sampler_pid")" 2>/dev/null || true
    for pidfile in "$BENCH_DIR"/pid_*; do
        [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
    done
    rm -rf "$BENCH_DIR"
}
trap cleanup EXIT

###############################################################################
# Build binary (if needed)
###############################################################################
if [ ! -x "$BINARY" ]; then
    echo ">> Building release binary ..."
    cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
fi

###############################################################################
# Prepare test environment
###############################################################################
echo ">> Preparing test files ..."
rm -rf "$BENCH_DIR"
mkdir -p "$BENCH_DIR/path1" "$BENCH_DIR/path2" "$BENCH_DIR/path3"

# Generate 1 KB file
dd if=/dev/urandom of="$BENCH_DIR/path1/bench_1k.txt" bs=1024 count=1 2>/dev/null
cp "$BENCH_DIR/path1/bench_1k.txt" "$BENCH_DIR/path2/bench_1k.txt"
cp "$BENCH_DIR/path1/bench_1k.txt" "$BENCH_DIR/path3/bench_1k.txt"

# Generate 1 MB file
dd if=/dev/urandom of="$BENCH_DIR/path1/bench_1m.bin" bs=1048576 count=1 2>/dev/null
cp "$BENCH_DIR/path1/bench_1m.bin" "$BENCH_DIR/path2/bench_1m.bin"
cp "$BENCH_DIR/path1/bench_1m.bin" "$BENCH_DIR/path3/bench_1m.bin"

# Set different mtime so latest_modified mode is meaningful
# path1 = oldest, path2 = middle, path3 = newest
touch -t 202501010000.00 "$BENCH_DIR/path1/bench_1k.txt" "$BENCH_DIR/path1/bench_1m.bin"
touch -t 202506010000.00 "$BENCH_DIR/path2/bench_1k.txt" "$BENCH_DIR/path2/bench_1m.bin"
touch -t 202512010000.00 "$BENCH_DIR/path3/bench_1k.txt" "$BENCH_DIR/path3/bench_1m.bin"

###############################################################################
# Helper: generate config TOML for a given mode & port
###############################################################################
gen_config() {
    local mode=$1 port=$2
    cat > "$BENCH_DIR/config_${mode}.toml" <<EOF
[server]
bind = "127.0.0.1:${port}"

[search]
mode = "${mode}"

[[search.paths]]
root = "${BENCH_DIR}/path1"

[[search.paths]]
root = "${BENCH_DIR}/path2"

[[search.paths]]
root = "${BENCH_DIR}/path3"
EOF
}

###############################################################################
# Helper: wait until port is ready
###############################################################################
wait_for_port() {
    local port=$1
    local i=0
    while ! curl -s -o /dev/null "http://127.0.0.1:${port}/bench_1k.txt" 2>/dev/null; do
        sleep 0.2
        i=$((i + 1))
        if [ $i -ge 50 ]; then
            echo "ERROR: server on port $port did not become ready" >&2
            exit 1
        fi
    done
}

###############################################################################
# Helper: high-resolution timestamp in nanoseconds (portable)
###############################################################################
now_ns() {
    python3 -c 'import time; print(int(time.time()*1e9))'
}

###############################################################################
# Helper: single-request latency (average of $REPEAT runs, returns ms)
###############################################################################
measure_latency() {
    local port=$1 file=$2
    local total=0
    local i=0
    while [ $i -lt $REPEAT ]; do
        t=$(curl -s -o /dev/null -w '%{time_total}' \
            --max-time "$CURL_TIMEOUT" \
            "http://127.0.0.1:${port}/${file}")
        total=$(awk "BEGIN{printf \"%.6f\", $total + $t}")
        i=$((i + 1))
    done
    awk "BEGIN{printf \"%.2f\", ($total / $REPEAT) * 1000}"
}

###############################################################################
# Helper: concurrent total time (wall clock for $CONCURRENCY parallel curls)
###############################################################################
measure_concurrent() {
    local port=$1 file=$2
    local start end
    start=$(now_ns)
    seq "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
        curl -s -o /dev/null --max-time "$CURL_TIMEOUT" \
        "http://127.0.0.1:${port}/${file}"
    end=$(now_ns)
    python3 -c "print('%.2f' % (($end - $start) / 1e6))"
}

###############################################################################
# Helper: get RSS of a process in KB
###############################################################################
get_rss_kb() {
    local pid=$1
    ps -o rss= -p "$pid" 2>/dev/null | awk '{print $1}'
}

###############################################################################
# Helper: start background sampler — samples RSS (KB) and CPU (%) every 50ms
#   Writes peak values to $BENCH_DIR/sample_{rss,cpu}
###############################################################################
start_sampler() {
    local pid=$1
    local rss_file="$BENCH_DIR/sample_rss"
    local cpu_file="$BENCH_DIR/sample_cpu"
    echo "0" > "$rss_file"
    echo "0.0" > "$cpu_file"

    (
        while true; do
            line=$(ps -o rss=,%cpu= -p "$pid" 2>/dev/null) || break
            rss=$(echo "$line" | awk '{print $1}')
            cpu=$(echo "$line" | awk '{print $2}')
            prev_rss=$(cat "$rss_file")
            prev_cpu=$(cat "$cpu_file")
            # update peak RSS
            if [ "$rss" -gt "$prev_rss" ] 2>/dev/null; then
                echo "$rss" > "$rss_file"
            fi
            # update peak CPU
            is_higher=$(awk "BEGIN{print ($cpu > $prev_cpu) ? 1 : 0}")
            if [ "$is_higher" -eq 1 ]; then
                echo "$cpu" > "$cpu_file"
            fi
            sleep 0.05
        done
    ) &
    echo $! > "$BENCH_DIR/sampler_pid"
}

stop_sampler() {
    if [ -f "$BENCH_DIR/sampler_pid" ]; then
        kill "$(cat "$BENCH_DIR/sampler_pid")" 2>/dev/null || true
        wait "$(cat "$BENCH_DIR/sampler_pid")" 2>/dev/null || true
        rm -f "$BENCH_DIR/sampler_pid"
    fi
}

###############################################################################
# Helper: format RSS from KB to human-readable MB
###############################################################################
format_rss() {
    local kb=$1
    awk "BEGIN{printf \"%.2f\", $kb / 1024}"
}

###############################################################################
# Run benchmarks — store results in plain files (bash 3.2 compatible)
###############################################################################
for mode in sequential concurrent latest_modified; do
    PORT=$((10000 + RANDOM % 50000))
    gen_config "$mode" "$PORT"

    echo ">> Benchmarking mode: $mode (port $PORT) ..."

    "$BINARY" -c "$BENCH_DIR/config_${mode}.toml" &
    SERVER_PID=$!
    echo "$SERVER_PID" > "$BENCH_DIR/pid_${mode}"
    wait_for_port "$PORT"

    # warm up
    curl -s -o /dev/null "http://127.0.0.1:${PORT}/bench_1k.txt"
    curl -s -o /dev/null "http://127.0.0.1:${PORT}/bench_1m.bin"
    sleep 0.2

    # --- Idle memory (after warm-up, before load) ---
    idle_rss=$(get_rss_kb "$SERVER_PID")
    echo "$idle_rss" > "$BENCH_DIR/res_${mode}_idle_rss"

    # --- Latency tests ---
    measure_latency "$PORT" "bench_1k.txt"  > "$BENCH_DIR/res_${mode}_lat_1k"
    measure_latency "$PORT" "bench_1m.bin"  > "$BENCH_DIR/res_${mode}_lat_1m"

    # --- Concurrent tests with resource sampling ---
    start_sampler "$SERVER_PID"
    measure_concurrent "$PORT" "bench_1k.txt" > "$BENCH_DIR/res_${mode}_con_1k"
    measure_concurrent "$PORT" "bench_1m.bin" > "$BENCH_DIR/res_${mode}_con_1m"
    stop_sampler

    # collect peak values
    cat "$BENCH_DIR/sample_rss" > "$BENCH_DIR/res_${mode}_peak_rss"
    cat "$BENCH_DIR/sample_cpu" > "$BENCH_DIR/res_${mode}_peak_cpu"

    kill "$SERVER_PID" 2>/dev/null || true
    rm -f "$BENCH_DIR/pid_${mode}"
    echo "   done."
done

###############################################################################
# Collect results from files
###############################################################################
seq_lat_1k=$(cat "$BENCH_DIR/res_sequential_lat_1k")
seq_lat_1m=$(cat "$BENCH_DIR/res_sequential_lat_1m")
seq_con_1k=$(cat "$BENCH_DIR/res_sequential_con_1k")
seq_con_1m=$(cat "$BENCH_DIR/res_sequential_con_1m")
seq_idle_rss=$(cat "$BENCH_DIR/res_sequential_idle_rss")
seq_peak_rss=$(cat "$BENCH_DIR/res_sequential_peak_rss")
seq_peak_cpu=$(cat "$BENCH_DIR/res_sequential_peak_cpu")

con_lat_1k=$(cat "$BENCH_DIR/res_concurrent_lat_1k")
con_lat_1m=$(cat "$BENCH_DIR/res_concurrent_lat_1m")
con_con_1k=$(cat "$BENCH_DIR/res_concurrent_con_1k")
con_con_1m=$(cat "$BENCH_DIR/res_concurrent_con_1m")
con_idle_rss=$(cat "$BENCH_DIR/res_concurrent_idle_rss")
con_peak_rss=$(cat "$BENCH_DIR/res_concurrent_peak_rss")
con_peak_cpu=$(cat "$BENCH_DIR/res_concurrent_peak_cpu")

lm_lat_1k=$(cat "$BENCH_DIR/res_latest_modified_lat_1k")
lm_lat_1m=$(cat "$BENCH_DIR/res_latest_modified_lat_1m")
lm_con_1k=$(cat "$BENCH_DIR/res_latest_modified_con_1k")
lm_con_1m=$(cat "$BENCH_DIR/res_latest_modified_con_1m")
lm_idle_rss=$(cat "$BENCH_DIR/res_latest_modified_idle_rss")
lm_peak_rss=$(cat "$BENCH_DIR/res_latest_modified_peak_rss")
lm_peak_cpu=$(cat "$BENCH_DIR/res_latest_modified_peak_cpu")

# Format RSS to MB
seq_idle_mb=$(format_rss "$seq_idle_rss")
seq_peak_mb=$(format_rss "$seq_peak_rss")
con_idle_mb=$(format_rss "$con_idle_rss")
con_peak_mb=$(format_rss "$con_peak_rss")
lm_idle_mb=$(format_rss "$lm_idle_rss")
lm_peak_mb=$(format_rss "$lm_peak_rss")

###############################################################################
# Output results
###############################################################################
TABLE="| Metric | \`sequential\` | \`concurrent\` | \`latest_modified\` |
|---|---|---|---|
| Single-request latency (1 KB) | ${seq_lat_1k} ms | ${con_lat_1k} ms | ${lm_lat_1k} ms |
| Single-request latency (1 MB) | ${seq_lat_1m} ms | ${con_lat_1m} ms | ${lm_lat_1m} ms |
| 50 concurrent total time (1 KB) | ${seq_con_1k} ms | ${con_con_1k} ms | ${lm_con_1k} ms |
| 50 concurrent total time (1 MB) | ${seq_con_1m} ms | ${con_con_1m} ms | ${lm_con_1m} ms |
| Idle memory (RSS) | ${seq_idle_mb} MB | ${con_idle_mb} MB | ${lm_idle_mb} MB |
| Peak memory (RSS) | ${seq_peak_mb} MB | ${con_peak_mb} MB | ${lm_peak_mb} MB |
| Peak CPU usage | ${seq_peak_cpu}% | ${con_peak_cpu}% | ${lm_peak_cpu}% |"

echo ""
echo "===== Benchmark Results ====="
echo ""
echo "$TABLE"
echo ""

cat > "$RESULTS_FILE" <<EOF
# FileHunter Search Mode Benchmark

- **Paths**: 3 search paths (local SSD)
- **Build**: release (LTO + strip)
- **Single-request iterations**: ${REPEAT}
- **Concurrent requests**: ${CONCURRENCY}

$TABLE

> Generated by \`bash bench/bench_modes.sh\`
EOF

echo ">> Results saved to $RESULTS_FILE"
