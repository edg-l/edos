use x86_64::instructions::port::Port;

const RTC_SECONDS: u16 = 0x00;
const RTC_MINUTES: u16 = 0x02;
const RTC_HOURS: u16 = 0x04;
const RTC_DAY: u16 = 0x07;
const RTC_MONTH: u16 = 0x08;
const RTC_YEAR: u16 = 0x09;
const RTC_STATUS_A: u16 = 0x0A;
const RTC_STATUS_B: u16 = 0x0B;

#[derive(Debug, Clone, Copy)]
pub struct RtcDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Read current date/time from RTC
pub fn read_rtc() -> RtcDateTime {
    unsafe {
        // Wait for update to complete
        while is_updating() {
            core::hint::spin_loop();
        }

        let second = read_rtc_register(RTC_SECONDS);
        let minute = read_rtc_register(RTC_MINUTES);
        let hour = read_rtc_register(RTC_HOURS);
        let day = read_rtc_register(RTC_DAY);
        let month = read_rtc_register(RTC_MONTH);
        let year = read_rtc_register(RTC_YEAR);

        let status_b = read_rtc_register(RTC_STATUS_B);
        let binary_mode = (status_b & 0x04) != 0;

        RtcDateTime {
            year: if binary_mode {
                year as u16 + 2000
            } else {
                bcd_to_binary(year) as u16 + 2000
            },
            month: if binary_mode {
                month
            } else {
                bcd_to_binary(month)
            },
            day: if binary_mode { day } else { bcd_to_binary(day) },
            hour: if binary_mode {
                hour
            } else {
                bcd_to_binary(hour)
            },
            minute: if binary_mode {
                minute
            } else {
                bcd_to_binary(minute)
            },
            second: if binary_mode {
                second
            } else {
                bcd_to_binary(second)
            },
        }
    }
}

unsafe fn read_rtc_register(reg: u16) -> u8 {
    unsafe {
        Port::new(0x70).write(reg as u8);
        Port::new(0x71).read()
    }
}

unsafe fn is_updating() -> bool {
    (unsafe { read_rtc_register(RTC_STATUS_A) } & 0x80) != 0
}

fn bcd_to_binary(bcd: u8) -> u8 {
    (bcd & 0x0F) + ((bcd >> 4) * 10)
}
