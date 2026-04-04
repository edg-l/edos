//! Standalone test harness for the kernel intrusive list module.
//!
//! This is a copy of `kernel/src/util/intrusive_list.rs` adapted so that
//! `cargo test` works under a normal (std-capable) toolchain. When changing
//! the kernel module, keep this copy in sync.

use core::marker::PhantomData;
use core::ptr;

// ---------------------------------------------------------------------------
// Link node
// ---------------------------------------------------------------------------

pub struct Link {
    next: *mut Link,
    prev: *mut Link,
    linked: bool,
}

unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Link {
    pub const fn new() -> Self {
        Self {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            linked: false,
        }
    }

    #[inline]
    pub fn is_linked(&self) -> bool {
        self.linked
    }
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Linked trait
// ---------------------------------------------------------------------------

pub unsafe trait Linked {
    unsafe fn links(ptr: *const Self) -> *mut Link;
    unsafe fn from_link(link: *mut Link) -> *mut Self;
}

// ---------------------------------------------------------------------------
// IntrusiveList
// ---------------------------------------------------------------------------

pub struct IntrusiveList<T: Linked> {
    head: *mut Link,
    tail: *mut Link,
    len: usize,
    _marker: PhantomData<*mut T>,
}

unsafe impl<T: Linked + Send> Send for IntrusiveList<T> {}
unsafe impl<T: Linked + Sync> Sync for IntrusiveList<T> {}

impl<T: Linked> IntrusiveList<T> {
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub unsafe fn push_back(&mut self, ptr: *mut T) {
        debug_assert!(!ptr.is_null());
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            !unsafe { (*link).linked },
            "push_back: node is already linked"
        );

        unsafe {
            (*link).prev = self.tail;
            (*link).next = ptr::null_mut();
            (*link).linked = true;
        }

        if self.tail.is_null() {
            self.head = link;
        } else {
            unsafe { (*self.tail).next = link };
        }
        self.tail = link;
        self.len += 1;
    }

    pub unsafe fn push_front(&mut self, ptr: *mut T) {
        debug_assert!(!ptr.is_null());
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            !unsafe { (*link).linked },
            "push_front: node is already linked"
        );

        unsafe {
            (*link).next = self.head;
            (*link).prev = ptr::null_mut();
            (*link).linked = true;
        }

        if self.head.is_null() {
            self.tail = link;
        } else {
            unsafe { (*self.head).prev = link };
        }
        self.head = link;
        self.len += 1;
    }

    pub fn pop_front(&mut self) -> Option<*mut T> {
        if self.head.is_null() {
            return None;
        }
        let link = self.head;
        unsafe { self.unlink(link) };
        Some(unsafe { T::from_link(link) })
    }

    pub fn pop_back(&mut self) -> Option<*mut T> {
        if self.tail.is_null() {
            return None;
        }
        let link = self.tail;
        unsafe { self.unlink(link) };
        Some(unsafe { T::from_link(link) })
    }

    pub unsafe fn remove(&mut self, ptr: *mut T) {
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            unsafe { (*link).linked },
            "remove: node is not in a list"
        );
        unsafe { self.unlink(link) };
    }

    unsafe fn unlink(&mut self, link: *mut Link) {
        unsafe {
            let prev = (*link).prev;
            let next = (*link).next;

            if prev.is_null() {
                self.head = next;
            } else {
                (*prev).next = next;
            }

            if next.is_null() {
                self.tail = prev;
            } else {
                (*next).prev = prev;
            }

            (*link).next = ptr::null_mut();
            (*link).prev = ptr::null_mut();
            (*link).linked = false;
        }
        self.len -= 1;
    }

    pub fn front(&self) -> Option<*mut T> {
        if self.head.is_null() {
            None
        } else {
            Some(unsafe { T::from_link(self.head) })
        }
    }

    pub fn back(&self) -> Option<*mut T> {
        if self.tail.is_null() {
            None
        } else {
            Some(unsafe { T::from_link(self.tail) })
        }
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            current: self.head,
            remaining: self.len,
            _marker: PhantomData,
        }
    }

    pub fn drain(&mut self) -> Drain<'_, T> {
        Drain { list: self }
    }
}

impl<T: Linked> Default for IntrusiveList<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Iterators
// ---------------------------------------------------------------------------

pub struct Iter<'a, T: Linked> {
    current: *mut Link,
    remaining: usize,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: Linked> Iterator for Iter<'a, T> {
    type Item = *mut T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }
        let link = self.current;
        self.current = unsafe { (*link).next };
        self.remaining -= 1;
        Some(unsafe { T::from_link(link) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, T: Linked> ExactSizeIterator for Iter<'a, T> {}

pub struct Drain<'a, T: Linked> {
    list: &'a mut IntrusiveList<T>,
}

impl<'a, T: Linked> Iterator for Drain<'a, T> {
    type Item = *mut T;

    fn next(&mut self) -> Option<Self::Item> {
        self.list.pop_front()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.list.len, Some(self.list.len))
    }
}

impl<'a, T: Linked> ExactSizeIterator for Drain<'a, T> {}

impl<'a, T: Linked> Drop for Drain<'a, T> {
    fn drop(&mut self) {
        while self.list.pop_front().is_some() {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct Node {
        value: u64,
        link: Link,
    }

    impl Node {
        fn new(value: u64) -> Self {
            Self {
                value,
                link: Link::new(),
            }
        }
    }

    unsafe impl Linked for Node {
        unsafe fn links(ptr: *const Self) -> *mut Link {
            unsafe { core::ptr::addr_of_mut!((*ptr.cast_mut()).link) }
        }
        unsafe fn from_link(link: *mut Link) -> *mut Self {
            unsafe {
                (link as *mut u8)
                    .sub(core::mem::offset_of!(Node, link))
                    .cast::<Self>()
            }
        }
    }

    fn collect_values(list: &IntrusiveList<Node>) -> Vec<u64> {
        list.iter().map(|p| unsafe { (*p).value }).collect()
    }

    #[test]
    fn empty_list() {
        let list = IntrusiveList::<Node>::new();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.front().is_none());
        assert!(list.back().is_none());
    }

    #[test]
    fn pop_empty() {
        let mut list = IntrusiveList::<Node>::new();
        assert!(list.pop_front().is_none());
        assert!(list.pop_back().is_none());
    }

    #[test]
    fn push_back_pop_front_fifo() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
        }

        assert_eq!(list.len(), 3);
        assert_eq!(collect_values(&list), vec![1, 2, 3]);

        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 1);
        assert!(!a.link.is_linked());

        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 2);

        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 3);

        assert!(list.is_empty());
        assert!(list.pop_front().is_none());
    }

    #[test]
    fn push_front_pop_front_lifo() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_front(&mut a);
            list.push_front(&mut b);
            list.push_front(&mut c);
        }

        assert_eq!(collect_values(&list), vec![3, 2, 1]);

        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 3);
        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 2);
        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 1);

        assert!(list.is_empty());
    }

    #[test]
    fn push_back_pop_back() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
        }

        let p = list.pop_back().unwrap();
        assert_eq!(unsafe { (*p).value }, 2);
        let p = list.pop_back().unwrap();
        assert_eq!(unsafe { (*p).value }, 1);
        assert!(list.is_empty());
    }

    #[test]
    fn front_back_peek() {
        let mut a = Node::new(10);
        let mut b = Node::new(20);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
        }

        assert_eq!(unsafe { (*list.front().unwrap()).value }, 10);
        assert_eq!(unsafe { (*list.back().unwrap()).value }, 20);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn remove_middle() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
            list.remove(&mut b);
        }

        assert_eq!(list.len(), 2);
        assert!(!b.link.is_linked());
        assert_eq!(collect_values(&list), vec![1, 3]);
    }

    #[test]
    fn remove_head() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
            list.remove(&mut a);
        }

        assert_eq!(list.len(), 2);
        assert_eq!(collect_values(&list), vec![2, 3]);
    }

    #[test]
    fn remove_tail() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
            list.remove(&mut c);
        }

        assert_eq!(list.len(), 2);
        assert_eq!(collect_values(&list), vec![1, 2]);
    }

    #[test]
    fn remove_only_element() {
        let mut a = Node::new(42);
        let mut list = IntrusiveList::<Node>::new();

        unsafe {
            list.push_back(&mut a);
            list.remove(&mut a);
        }

        assert!(list.is_empty());
        assert!(list.front().is_none());
        assert!(list.back().is_none());
    }

    #[test]
    fn is_linked_tracks_state() {
        let mut a = Node::new(1);
        assert!(!a.link.is_linked());

        let mut list = IntrusiveList::<Node>::new();
        unsafe { list.push_back(&mut a) };
        assert!(a.link.is_linked());

        list.pop_front();
        assert!(!a.link.is_linked());
    }

    #[test]
    fn reinsert_after_removal() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
        }

        list.pop_front(); // removes a
        assert!(!a.link.is_linked());

        unsafe { list.push_back(&mut a) };
        assert_eq!(collect_values(&list), vec![2, 1]);
    }

    #[test]
    fn single_element_push_pop_all_directions() {
        let mut a = Node::new(1);
        let mut list = IntrusiveList::<Node>::new();

        // push_back, pop_front
        unsafe { list.push_back(&mut a) };
        assert_eq!(unsafe { (*list.pop_front().unwrap()).value }, 1);
        assert!(list.is_empty());

        // push_back, pop_back
        unsafe { list.push_back(&mut a) };
        assert_eq!(unsafe { (*list.pop_back().unwrap()).value }, 1);
        assert!(list.is_empty());

        // push_front, pop_front
        unsafe { list.push_front(&mut a) };
        assert_eq!(unsafe { (*list.pop_front().unwrap()).value }, 1);
        assert!(list.is_empty());

        // push_front, pop_back
        unsafe { list.push_front(&mut a) };
        assert_eq!(unsafe { (*list.pop_back().unwrap()).value }, 1);
        assert!(list.is_empty());
    }

    #[test]
    fn drain_empties_list() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
        }

        let values: Vec<u64> = list.drain().map(|p| unsafe { (*p).value }).collect();
        assert_eq!(values, vec![1, 2, 3]);
        assert!(list.is_empty());
        assert!(!a.link.is_linked());
        assert!(!b.link.is_linked());
        assert!(!c.link.is_linked());
    }

    #[test]
    fn drain_partial_drop_unlinks_remaining() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
        }

        {
            let mut drain = list.drain();
            let p = drain.next().unwrap();
            assert_eq!(unsafe { (*p).value }, 1);
            // drop(drain) runs here — should unlink b and c.
        }

        assert!(list.is_empty());
        assert!(!a.link.is_linked());
        assert!(!b.link.is_linked());
        assert!(!c.link.is_linked());
    }

    #[test]
    fn iter_size_hint() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
        }

        let mut it = list.iter();
        assert_eq!(it.len(), 2);
        it.next();
        assert_eq!(it.len(), 1);
        it.next();
        assert_eq!(it.len(), 0);
    }

    #[test]
    fn interleaved_push_pop() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);
        let mut d = Node::new(4);

        let mut list = IntrusiveList::<Node>::new();

        unsafe { list.push_back(&mut a) };
        unsafe { list.push_back(&mut b) };

        let p = list.pop_front().unwrap();
        assert_eq!(unsafe { (*p).value }, 1);

        unsafe { list.push_back(&mut c) };
        unsafe { list.push_front(&mut d) };

        // Expected order: d(4), b(2), c(3)
        assert_eq!(collect_values(&list), vec![4, 2, 3]);
    }

    #[test]
    fn many_elements() {
        const N: usize = 1000;
        let mut nodes: Vec<Node> = (0..N as u64).map(Node::new).collect();
        let mut list = IntrusiveList::<Node>::new();

        for node in nodes.iter_mut() {
            unsafe { list.push_back(node) };
        }

        assert_eq!(list.len(), N);

        for i in 0..N {
            let p = list.pop_front().unwrap();
            assert_eq!(unsafe { (*p).value }, i as u64);
        }

        assert!(list.is_empty());
    }

    #[test]
    fn remove_all_individually() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.push_back(&mut c);
        }

        // Remove in non-sequential order: middle, head, tail
        unsafe { list.remove(&mut b) };
        assert_eq!(collect_values(&list), vec![1, 3]);
        unsafe { list.remove(&mut a) };
        assert_eq!(collect_values(&list), vec![3]);
        unsafe { list.remove(&mut c) };
        assert!(list.is_empty());
    }

    #[test]
    fn push_pop_push_maintains_integrity() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);
        let mut c = Node::new(3);

        let mut list = IntrusiveList::<Node>::new();

        // Fill and empty completely, then refill.
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
        }
        list.pop_front();
        list.pop_front();
        assert!(list.is_empty());

        unsafe {
            list.push_back(&mut c);
            list.push_back(&mut a);
            list.push_back(&mut b);
        }
        assert_eq!(collect_values(&list), vec![3, 1, 2]);
    }

    #[test]
    fn two_element_remove_head_then_tail() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.remove(&mut a);
        }
        assert_eq!(collect_values(&list), vec![2]);
        assert_eq!(unsafe { (*list.front().unwrap()).value }, 2);
        assert_eq!(unsafe { (*list.back().unwrap()).value }, 2);

        unsafe { list.remove(&mut b) };
        assert!(list.is_empty());
    }

    #[test]
    fn two_element_remove_tail_then_head() {
        let mut a = Node::new(1);
        let mut b = Node::new(2);

        let mut list = IntrusiveList::<Node>::new();
        unsafe {
            list.push_back(&mut a);
            list.push_back(&mut b);
            list.remove(&mut b);
        }
        assert_eq!(collect_values(&list), vec![1]);

        unsafe { list.remove(&mut a) };
        assert!(list.is_empty());
    }
}
