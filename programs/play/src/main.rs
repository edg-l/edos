//! play - play WAV audio files through /dev/dsp

use std::env;
use std::fs::File;
use std::io::Read;
use std::process;

use edos_lib::io as eio;
use edos_lib::process as eproc;

const AUDIO_IOCTL_SET_FORMAT: u64 = 1;
const AUDIO_IOCTL_DRAIN: u64 = 3;

struct WavInfo {
    sample_rate: u32,
    bits_per_sample: u16,
    channels: u16,
    data_offset: usize,
    data_size: u32,
}

fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn parse_wav_header(data: &[u8]) -> Result<WavInfo, &'static str> {
    if data.len() < 44 {
        return Err("file too small for WAV header");
    }

    // RIFF header
    if &data[0..4] != b"RIFF" {
        return Err("not a RIFF file");
    }
    if &data[8..12] != b"WAVE" {
        return Err("not a WAVE file");
    }

    // Find "fmt " chunk
    let mut pos = 12;
    let mut fmt_found = false;
    let mut audio_format: u16 = 0;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = read_u32_le(data, pos + 4) as usize;

        if chunk_id == b"fmt " {
            if chunk_size < 16 || pos + 8 + chunk_size > data.len() {
                return Err("invalid fmt chunk");
            }
            let fmt = pos + 8;
            audio_format = read_u16_le(data, fmt);
            channels = read_u16_le(data, fmt + 2);
            sample_rate = read_u32_le(data, fmt + 4);
            // bytes 8..11 = byte rate (skip)
            // bytes 12..13 = block align (skip)
            bits_per_sample = read_u16_le(data, fmt + 14);
            fmt_found = true;
        }

        if chunk_id == b"data" {
            if !fmt_found {
                return Err("data chunk before fmt chunk");
            }
            if audio_format != 1 {
                return Err("not PCM format (only uncompressed PCM supported)");
            }
            return Ok(WavInfo {
                sample_rate,
                bits_per_sample,
                channels,
                data_offset: pos + 8,
                data_size: chunk_size as u32,
            });
        }

        // Advance to next chunk (chunks are 2-byte aligned)
        pos += 8 + ((chunk_size + 1) & !1);
    }

    Err("no data chunk found")
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: play <file.wav>");
        process::exit(1);
    }

    let path = &args[1];

    // Read entire WAV file
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("play: {}: {}", path, e);
            process::exit(1);
        }
    };

    let mut data = Vec::new();
    if let Err(e) = file.read_to_end(&mut data) {
        eprintln!("play: read error: {}", e);
        process::exit(1);
    }

    // Parse WAV header
    let wav = match parse_wav_header(&data) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("play: {}: {}", path, e);
            process::exit(1);
        }
    };

    let duration_secs = wav.data_size as f64
        / (wav.sample_rate as f64 * wav.channels as f64 * (wav.bits_per_sample as f64 / 8.0));

    println!(
        "Playing: {} ({}Hz, {}-bit, {}ch, {:.1}s)",
        path, wav.sample_rate, wav.bits_per_sample, wav.channels, duration_secs
    );

    // Open /dev/dsp
    let dsp_fd = eio::open("/dev/dsp", 0);
    if dsp_fd < 0 {
        eprintln!("play: cannot open /dev/dsp");
        process::exit(1);
    }
    let dsp_fd = dsp_fd as u64;

    // Set format: pack sample_rate | (bits << 16) | (channels << 24)
    let format_arg = wav.sample_rate as u64
        | ((wav.bits_per_sample as u64) << 16)
        | ((wav.channels as u64) << 24);
    let ret = eio::ioctl(dsp_fd, AUDIO_IOCTL_SET_FORMAT, format_arg);
    if ret < 0 {
        eprintln!(
            "play: unsupported format ({}Hz/{}bit/{}ch)",
            wav.sample_rate, wav.bits_per_sample, wav.channels
        );
        process::exit(1);
    }

    // Write PCM data to /dev/dsp
    let pcm_end = (wav.data_offset + wav.data_size as usize).min(data.len());
    let pcm_data = &data[wav.data_offset..pcm_end];
    let mut offset = 0;
    while offset < pcm_data.len() {
        let chunk = &pcm_data[offset..];
        let Ok(written) = eproc::write(dsp_fd, chunk) else {
            eprintln!("play: write error");
            process::exit(1);
        };
        if written == 0 {
            // Ring buffer full, yield and retry
            std::thread::yield_now();
            continue;
        }
        offset += written;
    }

    // Wait for playback to complete
    eio::ioctl(dsp_fd, AUDIO_IOCTL_DRAIN, 0);

    println!("Done.");
}
