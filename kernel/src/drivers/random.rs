use alloc::{sync::Arc, vec::Vec};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering},
};

use raw_cpuid::CpuId;
use spin::{Mutex, Once};

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    log, timer,
};

static DEVICE_INSTANCE: Once<Arc<RandomDevice>> = Once::new();
static FALLBACK_SEED: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    DEVICE_INSTANCE.call_once(|| {
        let device = Arc::new(RandomDevice::new());
        if let Err(err) = register_device_str("/random", device.clone()) {
            log!("failed to register /dev/random: {err:?}");
        }
        device
    });
}

pub fn fill_bytes(buffer: &mut [u8]) {
    if let Some(device) = DEVICE_INSTANCE.get() {
        device.fill_bytes(buffer);
    } else {
        RandomDevice::new().fill_bytes(buffer);
    }
}

struct RandomDevice {
    source: RandomSource,
}

impl RandomDevice {
    fn new() -> Self {
        let source = RandomSource::detect();
        Self { source }
    }

    fn next_u64(&self) -> u64 {
        self.source.next_u64()
    }

    fn fill_bytes(&self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(core::mem::size_of::<u64>()) {
            let value = self.next_u64();
            let bytes = value.to_ne_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&bytes[..len]);
        }
    }
}

impl DevFsDevice for RandomDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        let mut output = Vec::with_capacity(count);
        let mut remaining = count;

        while remaining > 0 {
            let value = self.next_u64();
            let bytes = value.to_ne_bytes();
            let take = remaining.min(bytes.len());
            output.extend_from_slice(&bytes[..take]);
            remaining -= take;
        }

        Ok(output)
    }
}

enum RandomSource {
    Rdrand { fallback: Mutex<SplitMix64> },
    Software(Mutex<SplitMix64>),
}

impl RandomSource {
    fn detect() -> Self {
        if Self::rdrand_available() {
            log!("RDRAND detected; exposing hardware random source");
            Self::Rdrand {
                fallback: Mutex::new(SplitMix64::new(seed_entropy())),
            }
        } else {
            log!("RDRAND unavailable; falling back to software RNG");
            Self::Software(Mutex::new(SplitMix64::new(seed_entropy())))
        }
    }

    fn next_u64(&self) -> u64 {
        match self {
            Self::Rdrand { fallback } => {
                if let Some(v) = rdrand64() {
                    v
                } else {
                    fallback.lock().next_u64()
                }
            }
            Self::Software(state) => state.lock().next_u64(),
        }
    }

    fn rdrand_available() -> bool {
        CpuId::new()
            .get_feature_info()
            .map(|features| features.has_rdrand())
            .unwrap_or(false)
    }
}

fn rdrand64() -> Option<u64> {
    let mut value = 0u64;
    for _ in 0..16 {
        let status = unsafe { core::arch::x86_64::_rdrand64_step(&mut value) };
        if status == 1 {
            return Some(value);
        }
        spin_loop();
    }
    None
}

fn seed_entropy() -> u64 {
    let mut seed = FALLBACK_SEED.load(Ordering::Relaxed);

    if seed == 0 {
        let mut mix = unsafe { core::arch::x86_64::_rdtsc() };
        mix ^= timer::uptime_us();

        if let Some(timer) = crate::drivers::hpet::driver::get_hpet_timer() {
            mix ^= timer.get_counter();
        }

        mix ^= (&mix as *const u64) as u64;

        if mix == 0 {
            mix = 0x9E37_79B9_7F4A_7C15;
        }

        FALLBACK_SEED.store(mix, Ordering::Relaxed);
        seed = mix;
    }

    seed
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}
