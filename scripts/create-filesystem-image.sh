#!/usr/bin/env bash
set -euo pipefail

# Mirrors the SATA disk image generation logic from the top-level Makefile.
# Intended for release consumers to rebuild the filesystem disk image locally.

DISK_UUID="12345678-1234-5678-9abc-123456789abc"
PARTITION_UUID="87654321-4321-8765-cba9-987654321fed"
DEFAULT_IMAGE_SIZE="1G"
DEFAULT_VOLUME_LABEL="EDOS"
DEFAULT_ARCHIVE_NAME="filesystem.tar.gz"

TMPDIR=""
KEEP_TMP=false

cleanup() {
    if [[ "$KEEP_TMP" == false && -n "$TMPDIR" ]]; then
        rm -rf "$TMPDIR"
    fi
}
trap cleanup EXIT

print_usage() {
    cat <<USAGE
Usage: $(basename "$0") [options]

Options:
  -o, --output PATH         Output disk image path (default: sata-disk.img)
  -f, --filesystem-archive  Filesystem tarball to extract (default: filesystem.tar.gz
                             next to this script)
  -d, --filesystem-dir DIR  Existing filesystem directory to use instead of an archive
  -s, --size SIZE           Disk size passed to qemu-img (default: ${DEFAULT_IMAGE_SIZE})
      --keep-workdir        Keep the extracted filesystem directory after completion
  -h, --help                Show this help message and exit

Required tools: qemu-img, sgdisk, mformat, mcopy
USAGE
}

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: missing required tool '$1'" >&2
        exit 1
    fi
}

ensure_base_layout() {
    local dir="$1"
    mkdir -p "$dir"
    mkdir -p "$dir"/{bin,dev,home,lib,var,mnt,opt,root,sys,tmp}
}

main() {
    local output_path="sata-disk.img"
    local fs_archive=""
    local fs_dir=""
    local disk_size="${DEFAULT_IMAGE_SIZE}"

    local script_dir
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -o|--output)
                output_path="$2"
                shift 2
                ;;
            -f|--filesystem-archive)
                fs_archive="$2"
                shift 2
                ;;
            -d|--filesystem-dir)
                fs_dir="$2"
                shift 2
                ;;
            -s|--size)
                disk_size="$2"
                shift 2
                ;;
            --keep-workdir)
                KEEP_TMP=true
                shift
                ;;
            -h|--help)
                print_usage
                exit 0
                ;;
            *)
                echo "error: unknown option '$1'" >&2
                print_usage >&2
                exit 1
                ;;
        esac
    done

    require_tool qemu-img
    require_tool sgdisk
    require_tool mformat
    require_tool mcopy

    local workdir=""

    if [[ -n "$fs_dir" ]]; then
        if [[ ! -d "$fs_dir" ]]; then
            echo "error: filesystem directory '$fs_dir' does not exist" >&2
            exit 1
        fi
        workdir=$(cd "$fs_dir" && pwd)
        KEEP_TMP=true
    else
        if [[ -z "$fs_archive" ]]; then
            fs_archive="${script_dir}/${DEFAULT_ARCHIVE_NAME}"
        fi
        if [[ -f "$fs_archive" ]]; then
            TMPDIR=$(mktemp -d)
            echo "Extracting filesystem archive to '$TMPDIR'"
            tar -xf "$fs_archive" -C "$TMPDIR"
        else
            echo "Filesystem archive '$fs_archive' not found, creating empty layout" >&2
            TMPDIR=$(mktemp -d)
            ensure_base_layout "$TMPDIR/filesystem"
        fi

        if [[ -d "$TMPDIR/filesystem" ]]; then
            workdir="$TMPDIR/filesystem"
        else
            workdir="$TMPDIR"
        fi
    fi

    ensure_base_layout "$workdir"

    echo "Creating disk image '$output_path' with size $disk_size"
    qemu-img create -f raw "$output_path" "$disk_size"

    echo "Partitioning image"
    sgdisk "$output_path" -U "$DISK_UUID" -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" --partition-guid=1:"$PARTITION_UUID"

    echo "Formatting filesystem"
    mformat -F -i "${output_path}@@1M" -v "$DEFAULT_VOLUME_LABEL"

    shopt -s nullglob dotglob
    local entries=("$workdir"/*)
    shopt -u dotglob

    if (( ${#entries[@]} > 0 )); then
        echo "Populating filesystem from '$workdir'"
        mcopy -s -i "${output_path}@@1M" "${entries[@]}" ::/
    else
        echo "warning: filesystem directory '$workdir' is empty" >&2
    fi

    shopt -u nullglob

    echo "Done. Created disk image at '$output_path'"
}

main "$@"
