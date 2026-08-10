//! HID report descriptors: what a device says its reports mean.
//!
//! The boot protocol is a fixed layout two device classes agree to speak, and
//! a driver that only understands it understands exactly those two. Everything
//! else -- a tablet, a mouse with a hi-res wheel, anything with more than three
//! buttons -- describes itself in a report descriptor instead, and is
//! uninterpretable without reading one.
//!
//! This parses the item stream (HID 1.11 §6.2.2) far enough to locate the
//! fields a pointing device reports, and carries the one bit that cannot be
//! guessed from a byte layout: whether an axis is absolute or relative. That
//! flag is the whole difference between a mouse and a tablet.

/// Where one field sits in a report, and how to read it.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// Offset from the start of the report body, in bits.
    pub offset_bits: u16,
    /// Width of one instance, in bits.
    pub size_bits: u8,
    /// How many instances follow each other (buttons come in runs).
    pub count: u8,
    pub logical_min: i32,
    pub logical_max: i32,
    /// Set when the value is a displacement rather than a position.
    pub relative: bool,
}

impl Field {
    /// Read instance `index`, sign-extended when the field's range says it is
    /// signed. Returns `None` if the report is too short to hold it.
    pub fn read(&self, report: &[u8], index: u8) -> Option<i32> {
        let offset = self.offset_bits as usize + self.size_bits as usize * index as usize;
        let size = self.size_bits as usize;
        if size == 0 || size > 32 || offset + size > report.len() * 8 {
            return None;
        }

        let mut value: u32 = 0;
        for bit in 0..size {
            let at = offset + bit;
            let set = report[at / 8] >> (at % 8) & 1;
            value |= (set as u32) << bit;
        }

        // A negative logical minimum is how a descriptor says "signed"; there
        // is no separate flag for it.
        if self.logical_min < 0 && size < 32 && value & (1 << (size - 1)) != 0 {
            return Some((value | (!0u32 << size)) as i32);
        }
        Some(value as i32)
    }

    /// Scale an absolute reading onto `0..span`, which is how a tablet's
    /// logical range becomes a screen coordinate.
    pub fn scale(&self, value: i32, span: i32) -> i32 {
        let range = self.logical_max as i64 - self.logical_min as i64;
        if range <= 0 || span <= 1 {
            return 0;
        }
        let offset = (value as i64 - self.logical_min as i64).clamp(0, range);
        (offset * (span as i64 - 1) / range) as i32
    }
}

/// The fields a pointing device reports, located in its own report descriptor.
#[derive(Debug, Clone, Copy)]
pub struct PointerReport {
    /// Report ID this describes, or 0 when the device uses no report IDs. A
    /// device that uses them prefixes every report with the id byte.
    pub report_id: u8,
    /// The button run, one bit per button.
    pub buttons: Option<Field>,
    pub x: Option<Field>,
    pub y: Option<Field>,
    pub wheel: Option<Field>,
}

impl PointerReport {
    /// Whether the axes carry positions rather than displacements.
    pub fn absolute(&self) -> bool {
        self.x.is_some_and(|f| !f.relative)
    }

    /// Strip the report-id prefix, and reject a report belonging to a
    /// different id. A device with no report ids has no prefix to strip.
    pub fn body<'a>(&self, report: &'a [u8]) -> Option<&'a [u8]> {
        if self.report_id == 0 {
            return Some(report);
        }
        match report.split_first() {
            Some((&id, rest)) if id == self.report_id => Some(rest),
            _ => None,
        }
    }

    /// Buttons as a bitmap, low bit first, as the rest of the kernel wants it.
    pub fn buttons_of(&self, body: &[u8]) -> u8 {
        let Some(field) = self.buttons else {
            return 0;
        };
        let mut mask = 0u8;
        for index in 0..field.count.min(8) {
            if field.read(body, index).unwrap_or(0) != 0 {
                mask |= 1 << index;
            }
        }
        mask
    }
}

// Usage pages and usages this cares about (HID Usage Tables 1.12).
const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const USAGE_POINTER: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_WHEEL: u16 = 0x38;

/// Item types, from the two bits above the size in an item's prefix.
const TYPE_MAIN: u8 = 0;
const TYPE_GLOBAL: u8 = 1;
const TYPE_LOCAL: u8 = 2;

/// How many usages one Main item may name before the rest are ignored. A
/// pointing device names a handful; the bound exists so a malformed descriptor
/// cannot make the parser allocate.
const MAX_USAGES: usize = 32;

/// Global item state, which persists across Main items until changed.
#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    logical_min: i32,
    logical_max: i32,
    report_size: u8,
    report_count: u8,
    report_id: u8,
}

/// Find the pointer fields in a report descriptor, if it describes one.
///
/// Returns `None` for a descriptor that reports no X/Y pair, which is how a
/// keyboard, a consumer-control page or an unparseable descriptor is refused:
/// binding on "it told us where a pointer is" rather than on an interface
/// protocol code is the point of reading the descriptor at all.
pub fn parse_pointer(descriptor: &[u8]) -> Option<PointerReport> {
    let mut globals = Globals::default();
    let mut usages: [u16; MAX_USAGES] = [0; MAX_USAGES];
    let mut usage_count = 0usize;
    let mut usage_min: Option<u16> = None;

    // Bit offset within the report body, per report id. A descriptor that uses
    // ids restarts the offset for each one; one that does not has a single
    // running offset.
    let mut offset_bits: u16 = 0;
    let mut offset_id: u8 = 0;

    let mut found = PointerReport {
        report_id: 0,
        buttons: None,
        x: None,
        y: None,
        wheel: None,
    };
    // Only the collection that declares itself a pointer or a mouse is
    // interesting: a keyboard descriptor can carry an X/Y pair in a vendor
    // collection, and taking it would make the keyboard the pointer.
    let mut in_pointer_collection = false;
    let mut depth: i32 = 0;
    let mut pointer_depth: i32 = -1;

    let mut at = 0usize;
    while at < descriptor.len() {
        let prefix = descriptor[at];
        at += 1;

        // A long item carries its own size and no tag this cares about.
        if prefix == 0xFE {
            let size = *descriptor.get(at)? as usize;
            at = at.checked_add(2 + size)?;
            continue;
        }

        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        if at + size > descriptor.len() {
            break;
        }
        let mut data: u32 = 0;
        for (i, byte) in descriptor[at..at + size].iter().enumerate() {
            data |= (*byte as u32) << (8 * i);
        }
        at += size;

        let item_type = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;

        match (item_type, tag) {
            (TYPE_GLOBAL, 0) => globals.usage_page = data as u16,
            (TYPE_GLOBAL, 1) => globals.logical_min = signed(data, size),
            (TYPE_GLOBAL, 2) => globals.logical_max = signed(data, size),
            (TYPE_GLOBAL, 7) => globals.report_size = data as u8,
            (TYPE_GLOBAL, 8) => {
                globals.report_id = data as u8;
                if globals.report_id != offset_id {
                    offset_id = globals.report_id;
                    offset_bits = 0;
                }
            }
            (TYPE_GLOBAL, 9) => globals.report_count = data as u8,

            (TYPE_LOCAL, 0) => {
                if usage_count < MAX_USAGES {
                    usages[usage_count] = data as u16;
                    usage_count += 1;
                }
            }
            (TYPE_LOCAL, 1) => usage_min = Some(data as u16),

            // Collection: a pointer or mouse usage opens the region worth
            // reading. The usage was named by the Local items just before it.
            (TYPE_MAIN, 10) => {
                depth += 1;
                let names_pointer = usages[..usage_count]
                    .iter()
                    .any(|u| *u == USAGE_POINTER || *u == USAGE_MOUSE);
                if globals.usage_page == PAGE_GENERIC_DESKTOP
                    && names_pointer
                    && !in_pointer_collection
                {
                    in_pointer_collection = true;
                    pointer_depth = depth;
                }
                usage_count = 0;
                usage_min = None;
            }
            (TYPE_MAIN, 12) => {
                if in_pointer_collection && depth == pointer_depth {
                    in_pointer_collection = false;
                }
                depth -= 1;
                usage_count = 0;
                usage_min = None;
            }

            // Input: the fields themselves.
            (TYPE_MAIN, 8) => {
                let width = globals.report_size as u16 * globals.report_count as u16;
                let is_constant = data & 1 != 0;
                let is_variable = data & 2 != 0;
                let relative = data & 4 != 0;

                if !is_constant && is_variable && in_pointer_collection {
                    let field = |count: u8| Field {
                        offset_bits,
                        size_bits: globals.report_size,
                        count,
                        logical_min: globals.logical_min,
                        logical_max: globals.logical_max,
                        relative,
                    };

                    if globals.usage_page == PAGE_BUTTON && found.buttons.is_none() {
                        found.buttons = Some(field(globals.report_count));
                        found.report_id = globals.report_id;
                    } else if globals.usage_page == PAGE_GENERIC_DESKTOP {
                        // Each instance in the run takes the next named usage,
                        // which is how X and Y arrive as one Input item.
                        for index in 0..globals.report_count as usize {
                            let usage = usages
                                .get(index)
                                .copied()
                                .filter(|_| index < usage_count)
                                .or_else(|| usage_min.map(|min| min + index as u16))
                                .unwrap_or(0);
                            let mut one = field(1);
                            one.offset_bits =
                                offset_bits + globals.report_size as u16 * index as u16;
                            match usage {
                                USAGE_X if found.x.is_none() => {
                                    found.x = Some(one);
                                    found.report_id = globals.report_id;
                                }
                                USAGE_Y if found.y.is_none() => found.y = Some(one),
                                USAGE_WHEEL if found.wheel.is_none() => found.wheel = Some(one),
                                _ => {}
                            }
                        }
                    }
                }

                offset_bits = offset_bits.saturating_add(width);
                usage_count = 0;
                usage_min = None;
            }

            // Output and Feature items occupy no space in an input report, but
            // they do clear the local state.
            (TYPE_MAIN, _) => {
                usage_count = 0;
                usage_min = None;
            }
            _ => {}
        }
    }

    (found.x.is_some() && found.y.is_some()).then_some(found)
}

/// Sign-extend an item's data from the width it was encoded in.
fn signed(data: u32, size: usize) -> i32 {
    match size {
        1 => data as u8 as i8 as i32,
        2 => data as u16 as i16 as i32,
        _ => data as i32,
    }
}

#[cfg(feature = "sched-test")]
pub mod tests {
    use super::*;

    /// QEMU's `usb-tablet` report descriptor (hw/usb/dev-hid.c), which is the
    /// device this parser exists for.
    const TABLET: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xa1, 0x01, // Collection (Application)
        0x09, 0x01, //   Usage (Pointer)
        0xa1, 0x00, //   Collection (Physical)
        0x05, 0x09, //     Usage Page (Button)
        0x19, 0x01, //     Usage Minimum (1)
        0x29, 0x03, //     Usage Maximum (3)
        0x15, 0x00, //     Logical Minimum (0)
        0x25, 0x01, //     Logical Maximum (1)
        0x95, 0x03, //     Report Count (3)
        0x75, 0x01, //     Report Size (1)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x95, 0x01, //     Report Count (1)
        0x75, 0x05, //     Report Size (5)
        0x81, 0x01, //     Input (Constant)
        0x05, 0x01, //     Usage Page (Generic Desktop)
        0x09, 0x30, //     Usage (X)
        0x09, 0x31, //     Usage (Y)
        0x15, 0x00, //     Logical Minimum (0)
        0x26, 0xff, 0x7f, //  Logical Maximum (32767)
        0x35, 0x00, //     Physical Minimum (0)
        0x46, 0xff, 0x7f, //  Physical Maximum (32767)
        0x75, 0x10, //     Report Size (16)
        0x95, 0x02, //     Report Count (2)
        0x81, 0x02, //     Input (Data, Variable, Absolute)
        0x05, 0x01, //     Usage Page (Generic Desktop)
        0x09, 0x38, //     Usage (Wheel)
        0x15, 0x81, //     Logical Minimum (-127)
        0x25, 0x7f, //     Logical Maximum (127)
        0x35, 0x00, //     Physical Minimum
        0x45, 0x00, //     Physical Maximum
        0x75, 0x08, //     Report Size (8)
        0x95, 0x01, //     Report Count (1)
        0x81, 0x06, //     Input (Data, Variable, Relative)
        0xc0, //   End Collection
        0xc0, // End Collection
    ];

    /// QEMU's `usb-mouse` report descriptor: the same shape with relative
    /// 8-bit axes, which is what proves the absolute flag is being read rather
    /// than assumed.
    const MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75,
        0x08, 0x95, 0x03, 0x81, 0x06, 0xc0, 0xc0,
    ];

    /// Assert the parser reads both QEMU pointing devices correctly, and
    /// refuses what is not a pointer. Panics on the first disagreement, which
    /// is how every test in this kernel reports a failure.
    pub fn check() {
        let tablet = parse_pointer(TABLET).expect("tablet descriptor did not parse");
        assert!(tablet.absolute(), "tablet axes read as relative");
        let (x, y, wheel, buttons) = (
            tablet.x.unwrap(),
            tablet.y.unwrap(),
            tablet.wheel.unwrap(),
            tablet.buttons.unwrap(),
        );
        assert!(
            x.logical_max == 32767 && x.size_bits == 16,
            "tablet X range {}..{} at {} bits",
            x.logical_min,
            x.logical_max,
            x.size_bits
        );
        assert!(
            buttons.offset_bits == 0
                && x.offset_bits == 8
                && y.offset_bits == 24
                && wheel.offset_bits == 40,
            "tablet field offsets: buttons {} x {} y {} wheel {}",
            buttons.offset_bits,
            x.offset_bits,
            y.offset_bits,
            wheel.offset_bits
        );

        // Middle button, x = 0x1234, y = 0x5678, wheel = -1.
        let body: [u8; 6] = [0x04, 0x34, 0x12, 0x78, 0x56, 0xFF];
        assert!(
            tablet.buttons_of(&body) == 0b100,
            "tablet buttons decoded wrong"
        );
        assert!(
            x.read(&body, 0) == Some(0x1234)
                && y.read(&body, 0) == Some(0x5678)
                && wheel.read(&body, 0) == Some(-1),
            "tablet report decoded wrong"
        );
        assert!(
            x.scale(0, 1920) == 0 && x.scale(32767, 1920) == 1919 && x.scale(16383, 1920) == 959,
            "tablet scaling wrong"
        );

        let mouse = parse_pointer(MOUSE).expect("mouse descriptor did not parse");
        assert!(!mouse.absolute(), "mouse axes read as absolute");
        // Two left and three down, right button held.
        let body: [u8; 4] = [0x02, 0xFE, 0x03, 0x00];
        assert!(
            mouse.buttons_of(&body) == 0b010
                && mouse.x.unwrap().read(&body, 0) == Some(-2)
                && mouse.y.unwrap().read(&body, 0) == Some(3),
            "mouse report decoded wrong"
        );

        // A keyboard names no pointer and must not be taken for one, and a
        // descriptor cut off mid-item is refused rather than half-read.
        let keyboard: &[u8] = &[
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
            0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0,
        ];
        assert!(
            parse_pointer(keyboard).is_none(),
            "a keyboard parsed as a pointer"
        );
        assert!(
            parse_pointer(&TABLET[..7]).is_none(),
            "a truncated descriptor parsed"
        );
    }
}
