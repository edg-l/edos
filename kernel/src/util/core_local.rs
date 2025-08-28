// https://github.com/kwzhao/x2apic-rs/issues/21

use core::cell::{Ref, RefCell, RefMut};

use alloc::{boxed::Box, vec::Vec};
use spin::Once;

use crate::acpi::number_of_cores;

pub struct CoreLocal<T> {
    inner: Once<Box<[Once<RefCell<T>>]>>,
    init: fn() -> T,
}

impl<T> CoreLocal<T> {
    pub const fn new(init: fn() -> T) -> Self {
        Self {
            inner: Once::new(),
            init,
        }
    }

    fn ensure_initialized(&self) -> &[Once<RefCell<T>>] {
        self.inner.call_once(|| {
            let cores = number_of_cores();
            let mut cores_values = Vec::with_capacity(cores);
            for _ in 0..cores {
                cores_values.push(Once::new());
            }
            cores_values.into_boxed_slice()
        })
    }

    pub fn read(&self) -> Ref<'_, T> {
        let core_id = current_core_id();
        self.ensure_initialized()[core_id]
            .call_once(|| RefCell::new((self.init)()))
            .borrow()
    }

    pub fn write(&self) -> RefMut<'_, T> {
        let core_id = current_core_id();
        self.ensure_initialized()[core_id]
            .call_once(|| RefCell::new((self.init)()))
            .borrow_mut()
    }
}

unsafe impl<T> Sync for CoreLocal<T> {}

pub fn current_core_id() -> usize {
    0
}
