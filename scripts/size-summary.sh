#!/usr/bin/env bash
set -euo pipefail

PROFILE_FLAG="--release"
PROFILE_NAME="release"

for arg in "$@"; do
    case "$arg" in
        --debug|-d|--dev)
            PROFILE_FLAG=""
            PROFILE_NAME="dev"
            ;;
        --release|-r)
            PROFILE_FLAG="--release"
            PROFILE_NAME="release"
            ;;
        --help|-h)
            echo "Usage: $0 [--release | --debug]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Usage: $0 [--release | --debug]" >&2
            exit 1
            ;;
    esac
done

BUILD_ARGS=()
SIZE_ARGS=(-q)
NM_ARGS=(-q)
if [[ -n "$PROFILE_FLAG" ]]; then
    BUILD_ARGS+=("$PROFILE_FLAG")
    SIZE_ARGS+=("$PROFILE_FLAG")
    NM_ARGS+=("$PROFILE_FLAG")
fi

# Run cargo build directly attached to the terminal so progress and colored output stream live.
cargo build ${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}

SIZE_OUT="$(cargo size "${SIZE_ARGS[@]}" -- -A)"
NM_OUT="$(cargo nm "${NM_ARGS[@]}" -- --print-size)"

# Sum `esp_alloc::heap_allocator!` static arena symbols placed in main DRAM (.bss),
# excluding the reclaimed DRAM2 region (0x3fcce400+).
BSS_HEAP_BYTES=0
while read -r addr size_hex _type name; do
    if [[ "$name" == *main_task*4HEAP && "$addr" != 3fcce* ]]; then
        BSS_HEAP_BYTES=$((BSS_HEAP_BYTES + 16#$size_hex))
    fi
done <<< "$NM_OUT"

LC_ALL=C awk -v profile="$PROFILE_NAME" -v bss_heap="$BSS_HEAP_BYTES" '
function fmt_int(n,    s, out) {
    s = sprintf("%d", n)
    out = ""
    while (length(s) > 3) {
        out = "," substr(s, length(s) - 2, 3) out
        s = substr(s, 1, length(s) - 3)
    }
    return s out
}

function fmt_kib_bytes(b) {
    return sprintf("%6.2f KiB (%7s)", b / 1024.0, fmt_int(b))
}

function print_top_row(label, bytes, detail) {
    printf "│ %-24s │ %20s │ %16s │\n", label, fmt_kib_bytes(bytes), detail
}

function print_sub_row(branch, label, bytes, detail) {
    printf "│  %s %-20s │ %20s │ %16s │\n", branch, label, fmt_kib_bytes(bytes), detail
}

$1 == ".text"          { s_text = $2 }
$1 == ".rodata"        { s_rodata = $2 }
$1 == ".rodata.wifi"   { s_rodata_wifi = $2 }
$1 == ".flash.appdesc" { s_appdesc = $2 }
$1 == ".trap"          { s_trap = $2 }
$1 == ".rwtext"        { s_rwtext = $2 }
$1 == ".rwtext.wifi"   { s_rwtext_wifi = $2 }
$1 == ".data"          { s_data = $2 }
$1 == ".data.wifi"     { s_data_wifi = $2 }
$1 == ".bss"           { s_bss = $2 }
$1 == ".stack"         { s_stack = $2 }
$1 == ".dram2_uninit"  { s_dram2 = $2 }

END {
    rodata_total = s_rodata + s_rodata_wifi + s_appdesc
    iram_total   = s_trap + s_rwtext + s_rwtext_wifi
    data_total   = s_data + s_data_wifi
    ram_init     = iram_total + data_total
    flash_total  = s_text + rodata_total + ram_init

    bss_non_heap = s_bss - bss_heap
    if (bss_non_heap < 0) bss_non_heap = 0

    sram_total   = iram_total + data_total + s_bss + s_stack
    heap_total   = bss_heap + s_dram2

    title = sprintf("doorsys-firmware Memory Summary (%s)", profile)
    pad_left = int((68 - length(title)) / 2)
    pad_right = 68 - length(title) - pad_left

    printf "╭────────────────────────────────────────────────────────────────────╮\n"
    printf "│%*s%s%*s│\n", pad_left, "", title, pad_right, ""
    printf "├──────────────────────────┬──────────────────────┬──────────────────┤\n"
    printf "│ %-24s │ %20s │ %16s │\n", "Region / Category", "Used (KiB / Bytes)", "Details"
    printf "├──────────────────────────┼──────────────────────┼──────────────────┤\n"
    print_top_row("Flash (Total)",                 flash_total,  "")
    print_sub_row("├─", "Code (.text)",            s_text,       "IROM")
    print_sub_row("├─", "Read-Only (.rodata)",     rodata_total, "DROM")
    print_sub_row("└─", "RAM Init (.rw/.data)",    ram_init,     "IRAM + DRAM img")
    printf "├──────────────────────────┼──────────────────────┼──────────────────┤\n"
    printf "│ %-24s │ %20s │ %16s │\n", sprintf("SRAM (%6.2f KiB DRAM)", sram_total / 1024.0), "", ""
    print_sub_row("├─", "IRAM (.trap/.rwtext)",    iram_total,   sprintf("%5.1f%% of SRAM", (iram_total * 100.0) / sram_total))
    print_sub_row("├─", "Data (.data)",            data_total,   sprintf("%5.1f%% of SRAM", (data_total * 100.0) / sram_total))
    print_sub_row("├─", "BSS (non-heap)",          bss_non_heap, sprintf("%5.1f%% of SRAM", (bss_non_heap * 100.0) / sram_total))
    print_sub_row("├─", "Heap (in .bss)",          bss_heap,     sprintf("%5.1f%% of SRAM", (bss_heap * 100.0) / sram_total))
    print_sub_row("└─", "Stack (available)",       s_stack,      sprintf("%5.1f%% of SRAM", (s_stack * 100.0) / sram_total))
    printf "├──────────────────────────┼──────────────────────┼──────────────────┤\n"
    print_top_row("Heap (Total esp_alloc)",        heap_total,   "")
    print_sub_row("├─", "Main DRAM (.bss)",        bss_heap,     "")
    print_sub_row("└─", "Reclaimed (.dram2)",      s_dram2,      "Bootloader RAM")
    printf "╰──────────────────────────┴──────────────────────┴──────────────────╯\n"
}
' <<< "$SIZE_OUT"
