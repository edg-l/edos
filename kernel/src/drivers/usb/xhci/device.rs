#![expect(unused)]

use alloc::vec::Vec;

use crate::drivers::dma::DmaBuffer;

use super::rings::TransferRing;

// ---------------------------------------------------------------------------
// Standard USB descriptor structures
// ---------------------------------------------------------------------------

/// USB Device Descriptor (18 bytes).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub bcd_usb: u16,
    pub b_device_class: u8,
    pub b_device_sub_class: u8,
    pub b_device_protocol: u8,
    pub b_max_packet_size0: u8,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial_number: u8,
    pub b_num_configurations: u8,
}

/// USB Configuration Descriptor (9 bytes; followed by interface/endpoint descriptors).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub w_total_length: u16,
    pub b_num_interfaces: u8,
    pub b_configuration_value: u8,
    pub i_configuration: u8,
    pub bm_attributes: u8,
    pub b_max_power: u8,
}

/// USB Interface Descriptor (9 bytes).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InterfaceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_interface_number: u8,
    pub b_alternate_setting: u8,
    pub b_num_endpoints: u8,
    pub b_interface_class: u8,
    pub b_interface_sub_class: u8,
    pub b_interface_protocol: u8,
    pub i_interface: u8,
}

/// USB Endpoint Descriptor (7 bytes).
///
/// `b_endpoint_address`: bit 7 = direction (1 = IN), bits [3:0] = endpoint number.
/// `bm_attributes`: bits [1:0] = transfer type (0=control, 1=iso, 2=bulk, 3=interrupt).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_endpoint_address: u8,
    pub bm_attributes: u8,
    pub w_max_packet_size: u16,
    pub b_interval: u8,
}

/// USB Setup Packet (8 bytes, used for control transfers).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SetupPacket {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
}

// ---------------------------------------------------------------------------
// Descriptor type constants
// ---------------------------------------------------------------------------

pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIGURATION: u8 = 2;
pub const DESC_INTERFACE: u8 = 4;
pub const DESC_ENDPOINT: u8 = 5;
pub const DESC_HID: u8 = 0x21;

// ---------------------------------------------------------------------------
// USB class codes
// ---------------------------------------------------------------------------

pub const USB_CLASS_HID: u8 = 0x03;
pub const USB_CLASS_MASS_STORAGE: u8 = 0x08;

// ---------------------------------------------------------------------------
// HID interface protocol codes
// ---------------------------------------------------------------------------

pub const HID_PROTOCOL_KEYBOARD: u8 = 1;
pub const HID_PROTOCOL_MOUSE: u8 = 2;

// ---------------------------------------------------------------------------
// USB speed
// ---------------------------------------------------------------------------

/// USB bus speed, decoded from the xHCI port speed field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbSpeed {
    Full,  // 12 Mbps  (USB 1.1)
    Low,   // 1.5 Mbps (USB 1.0)
    High,  // 480 Mbps (USB 2.0)
    Super, // 5 Gbps   (USB 3.0)
}

impl UsbSpeed {
    /// Default maximum packet size for EP0 based on speed.
    pub fn default_max_packet_size(&self) -> u16 {
        match self {
            UsbSpeed::Low => 8,
            UsbSpeed::Full => 8, // Can be 8/16/32/64; start conservative.
            UsbSpeed::High => 64,
            UsbSpeed::Super => 512,
        }
    }

    /// Convert xHCI port speed field value to `UsbSpeed`.
    pub fn from_port_speed(speed: u8) -> Option<Self> {
        match speed {
            1 => Some(UsbSpeed::Full),
            2 => Some(UsbSpeed::Low),
            3 => Some(UsbSpeed::High),
            4 => Some(UsbSpeed::Super),
            _ => None,
        }
    }

    /// xHCI Slot Context speed encoding.
    pub fn to_slot_speed(&self) -> u32 {
        match self {
            UsbSpeed::Full => 1,
            UsbSpeed::Low => 2,
            UsbSpeed::High => 3,
            UsbSpeed::Super => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// UsbDevice
// ---------------------------------------------------------------------------

/// An enumerated USB device with its associated xHCI resources.
pub struct UsbDevice {
    pub slot_id: u8,
    pub speed: UsbSpeed,
    pub port_id: u8,
    pub ep0_ring: TransferRing,
    pub input_ctx: DmaBuffer,
    pub device_descriptor: Option<DeviceDescriptor>,
    /// Raw configuration descriptor blob (Configuration + Interface + Endpoint descriptors).
    pub config_data: Option<Vec<u8>>,
}
