//! HDA codec discovery and configuration.

use super::HdaController;
use crate::log;
use alloc::string::String;

// HDA verb helpers (12-bit verb ID + 8-bit payload format)
const GET_PARAM: u32 = 0xF00_00; // Get Parameter (verb 0xF00, param in low 8 bits)
const SET_POWER_STATE: u32 = 0x705_00; // Set Power State
const SET_STREAM_FORMAT: u32 = 0x200_00; // Set Converter Format (verb 0x200)
const SET_CHANNEL_STREAM: u32 = 0x706_00; // Set Converter Stream/Channel ID
const SET_PIN_WIDGET_CONTROL: u32 = 0x707_00;
const SET_EAPD: u32 = 0x70C_00;
const SET_AMP_GAIN_MUTE: u32 = 0x300_00; // 4-bit verb 0x3, 16-bit payload

// Parameter IDs (for GET_PARAM)
const PARAM_VENDOR_ID: u32 = 0x00;
const PARAM_NODE_COUNT: u32 = 0x04;
const PARAM_FUNC_GROUP_TYPE: u32 = 0x05;
const PARAM_AUDIO_WIDGET_CAP: u32 = 0x09;
const PARAM_PIN_CAP: u32 = 0x0C;
const PARAM_OUT_AMP_CAP: u32 = 0x12;

// Widget types (bits [23:20] of Audio Widget Capabilities)
const WIDGET_AUD_OUT: u8 = 0x0; // Audio Output (DAC)
const WIDGET_PIN: u8 = 0x4; // Pin Complex

// Pin widget control bits
const PIN_OUT_EN: u8 = 0x40;

/// Information about the discovered codec topology needed for playback.
pub struct CodecInfo {
    pub cad: u8,     // Codec address (usually 0)
    pub afg_nid: u8, // Audio Function Group node ID
    pub dac_nid: u8, // DAC (Audio Output) node ID
    pub pin_nid: u8, // Output pin node ID
}

/// Discover codecs, find the DAC and output pin, configure for playback.
pub fn discover_and_configure(ctrl: &mut HdaController) -> Result<CodecInfo, String> {
    // Use codec address 0 (first codec)
    let cad: u8 = 0;

    // Get vendor ID for logging
    let vendor = ctrl.codec_command(cad, 0, GET_PARAM | PARAM_VENDOR_ID)?;
    log!("hda: codec vendor={:#010x}", vendor);

    // Get subordinate node count from root node 0
    let node_count = ctrl.codec_command(cad, 0, GET_PARAM | PARAM_NODE_COUNT)?;
    let start_nid = ((node_count >> 16) & 0xFF) as u8;
    let num_nodes = (node_count & 0xFF) as u8;
    log!("hda: root nodes: start={}, count={}", start_nid, num_nodes);

    // Find Audio Function Group
    let mut afg_nid: Option<u8> = None;
    for nid in start_nid..start_nid + num_nodes {
        let ftype = ctrl.codec_command(cad, nid, GET_PARAM | PARAM_FUNC_GROUP_TYPE)?;
        if (ftype & 0xFF) == 0x01 {
            // Audio Function Group
            afg_nid = Some(nid);
            log!("hda: AFG at nid {}", nid);
            break;
        }
    }
    let afg_nid = afg_nid.ok_or_else(|| String::from("HDA: no Audio Function Group found"))?;

    // Power on the AFG (D0 = full power)
    ctrl.codec_command(cad, afg_nid, SET_POWER_STATE | 0x00)?;

    // Get widget node range under AFG
    let widget_count = ctrl.codec_command(cad, afg_nid, GET_PARAM | PARAM_NODE_COUNT)?;
    let wid_start = ((widget_count >> 16) & 0xFF) as u8;
    let wid_num = (widget_count & 0xFF) as u8;
    log!("hda: AFG widgets: start={}, count={}", wid_start, wid_num);

    // Walk widgets to find DAC and output pin
    let mut dac_nid: Option<u8> = None;
    let mut pin_nid: Option<u8> = None;

    for nid in wid_start..wid_start + wid_num {
        let wcaps = ctrl.codec_command(cad, nid, GET_PARAM | PARAM_AUDIO_WIDGET_CAP)?;
        let wtype = ((wcaps >> 20) & 0xF) as u8;

        match wtype {
            WIDGET_AUD_OUT => {
                if dac_nid.is_none() {
                    dac_nid = Some(nid);
                    log!("hda: DAC at nid {}", nid);
                }
            }
            WIDGET_PIN => {
                // Check pin capabilities for output
                let pincap = ctrl.codec_command(cad, nid, GET_PARAM | PARAM_PIN_CAP)?;
                let has_output = (pincap & (1 << 4)) != 0; // Output capable
                if has_output && pin_nid.is_none() {
                    pin_nid = Some(nid);
                    log!("hda: output pin at nid {}", nid);
                }
            }
            _ => {}
        }
    }

    let dac_nid = dac_nid.ok_or_else(|| String::from("HDA: no DAC widget found"))?;
    let pin_nid = pin_nid.ok_or_else(|| String::from("HDA: no output pin found"))?;

    // Configure DAC: set stream tag=1, channel=0
    ctrl.codec_command(cad, dac_nid, SET_CHANNEL_STREAM | (1 << 4) | 0)?;

    // Set DAC format: 48kHz, 16-bit, stereo
    // Format word: base=48k (bit14=0), mult=1x, div=1x, bits=16 (bits[6:4]=001), channels=2 (bits[3:0]=1)
    let format_word: u32 = 0x0011; // 48kHz, 16-bit, stereo
    ctrl.codec_command(cad, dac_nid, SET_STREAM_FORMAT | format_word)?;

    // Unmute DAC output amp: output, left+right, max gain
    // SetAmplifierGainMute is a 4-bit verb (0x3) with 16-bit payload
    // Payload: [15]=output, [13]=left, [12]=right, [7]=!mute, [6:0]=gain
    let amp_payload: u32 = 0xB07F; // output | left | right | gain=0x7F | unmute
    ctrl.codec_command(cad, dac_nid, SET_AMP_GAIN_MUTE | amp_payload)?;

    // Enable output pin
    ctrl.codec_command(cad, pin_nid, SET_PIN_WIDGET_CONTROL | PIN_OUT_EN as u32)?;

    // Check if pin has EAPD and enable it
    let pincap = ctrl.codec_command(cad, pin_nid, GET_PARAM | PARAM_PIN_CAP)?;
    if (pincap & (1 << 16)) != 0 {
        // EAPD capable - enable it
        ctrl.codec_command(cad, pin_nid, SET_EAPD | 0x02)?; // bit 1 = EAPD
        log!("hda: EAPD enabled on pin nid {}", pin_nid);
    }

    // Unmute pin output amp too (if it has one)
    let pin_wcaps = ctrl.codec_command(cad, pin_nid, GET_PARAM | PARAM_AUDIO_WIDGET_CAP)?;
    if (pin_wcaps & (1 << 2)) != 0 {
        // Has output amp
        ctrl.codec_command(cad, pin_nid, SET_AMP_GAIN_MUTE | amp_payload)?;
    }

    Ok(CodecInfo {
        cad,
        afg_nid,
        dac_nid,
        pin_nid,
    })
}
