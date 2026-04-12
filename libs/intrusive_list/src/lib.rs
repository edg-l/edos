//! Intrusive doubly-linked list.
//!
//! Zero-allocation linked list where link nodes are embedded directly in the
//! container type. Designed for use in the scheduler and other kernel subsystems
//! where heap allocation is forbidden (e.g. inside interrupt-disabled sections).
//!
//! # Design
//!
//! Each element that participates in a list embeds a [`Link`] field. The
//! [`IntrusiveList`] operates on raw pointers and never allocates. All
//! operations (push, pop, remove) are O(1).
//!
//! # Safety contract
//!
//! - A node must not be in more than one list at a time (per `Link` field).
//! - Nodes must not be moved or dropped while linked.
//! - The caller must ensure nodes remain valid for the lifetime of the list.
//! - All list mutations require `&mut self`, so external synchronization
//!   (e.g. a spinlock) is the caller's responsibility.

#![no_std]

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// x86-64 canonical-address check. Upper 17 bits must be all-0 or all-1
/// for a 48-bit virtual address. Null (0) is canonical. Used to detect
/// pointer corruption early in debug builds — zero cost in release.
#[inline]
fn is_canonical(addr: u64) -> bool {
    let high = addr >> 47;
    high == 0 || high == 0x1_FFFF
}

// ---------------------------------------------------------------------------
// Link node
// ---------------------------------------------------------------------------

/// An intrusive list link node. Embed this in your container struct.
///
/// ```ignore
/// struct MyStruct {
///     data: u64,
///     link: Link,
/// }
/// ```
pub struct Link {
    next: *mut Link,
    prev: *mut Link,
    linked: AtomicBool,
}

// Link is Send: the raw pointers it contains reference nodes that the caller
// guarantees are valid and externally synchronised.
unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Link {
    /// Create a new, unlinked node.
    pub const fn new() -> Self {
        Self {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            linked: AtomicBool::new(false),
        }
    }

    /// Returns `true` if this node is currently part of a list.
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::Relaxed)
    }
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Link {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Link")
            .field("linked", &self.linked.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Linked trait
// ---------------------------------------------------------------------------

/// Trait for container types that embed a [`Link`].
///
/// # Safety
///
/// - [`links`](Linked::links) must return a pointer to a `Link` at a **fixed
///   offset** within `Self`.
/// - [`from_link`](Linked::from_link) must correctly recover a `*mut Self`
///   from the `Link` pointer using the same offset.
/// - The `Link` must be part of the same allocation as the container.
pub unsafe trait Linked {
    /// Get a raw pointer to the embedded `Link` from a container pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid, aligned instance of `Self`.
    unsafe fn links(ptr: *const Self) -> *mut Link;

    /// Recover a container pointer from a `Link` pointer.
    ///
    /// # Safety
    ///
    /// `link` must point to a `Link` that is embedded in an instance of `Self`.
    unsafe fn from_link(link: *mut Link) -> *mut Self;
}

// ---------------------------------------------------------------------------
// IntrusiveList
// ---------------------------------------------------------------------------

/// A doubly-linked intrusive list.
///
/// All push / pop / remove operations are **O(1)** and **allocation-free**.
///
/// The list does **not** own the nodes — the caller is responsible for keeping
/// them alive and properly synchronised.
pub struct IntrusiveList<T: Linked> {
    head: *mut Link,
    tail: *mut Link,
    len: usize,
    _marker: PhantomData<*mut T>,
}

unsafe impl<T: Linked + Send> Send for IntrusiveList<T> {}
unsafe impl<T: Linked + Sync> Sync for IntrusiveList<T> {}

impl<T: Linked> IntrusiveList<T> {
    /// Create a new, empty list.
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
            _marker: PhantomData,
        }
    }

    /// Number of elements currently in the list.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the list contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // -- insertion -----------------------------------------------------------

    /// Push an element to the **back** (tail) of the list.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a valid, non-null pointer to a `T`.
    /// - The node's `Link` must **not** already be in any list.
    /// - The node must not be moved or dropped while it remains linked.
    pub unsafe fn push_back(&mut self, ptr: *mut T) {
        debug_assert!(!ptr.is_null());
        debug_assert!(
            is_canonical(ptr as u64),
            "push_back: non-canonical ptr {:p}",
            ptr
        );
        debug_assert!(
            is_canonical(self.head as u64),
            "push_back: non-canonical head {:p}",
            self.head
        );
        debug_assert!(
            is_canonical(self.tail as u64),
            "push_back: non-canonical tail {:p}",
            self.tail
        );
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            !unsafe { (*link).linked.load(Ordering::Relaxed) },
            "push_back: node is already linked"
        );

        unsafe {
            (*link).prev = self.tail;
            (*link).next = ptr::null_mut();
            (*link).linked.store(true, Ordering::Relaxed);
        }

        if self.tail.is_null() {
            self.head = link;
        } else {
            unsafe { (*self.tail).next = link };
        }
        self.tail = link;
        self.len += 1;
    }

    /// Push an element to the **front** (head) of the list.
    ///
    /// # Safety
    ///
    /// Same requirements as [`push_back`](Self::push_back).
    pub unsafe fn push_front(&mut self, ptr: *mut T) {
        debug_assert!(!ptr.is_null());
        debug_assert!(
            is_canonical(ptr as u64),
            "push_front: non-canonical ptr {:p}",
            ptr
        );
        debug_assert!(
            is_canonical(self.head as u64),
            "push_front: non-canonical head {:p}",
            self.head
        );
        debug_assert!(
            is_canonical(self.tail as u64),
            "push_front: non-canonical tail {:p}",
            self.tail
        );
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            !unsafe { (*link).linked.load(Ordering::Relaxed) },
            "push_front: node is already linked"
        );

        unsafe {
            (*link).next = self.head;
            (*link).prev = ptr::null_mut();
            (*link).linked.store(true, Ordering::Relaxed);
        }

        if self.head.is_null() {
            self.tail = link;
        } else {
            unsafe { (*self.head).prev = link };
        }
        self.head = link;
        self.len += 1;
    }

    // -- removal -------------------------------------------------------------

    /// Pop the element at the **front** (head).
    ///
    /// Returns `None` if the list is empty.
    pub fn pop_front(&mut self) -> Option<*mut T> {
        if self.head.is_null() {
            return None;
        }
        let link = self.head;
        unsafe { self.unlink(link) };
        Some(unsafe { T::from_link(link) })
    }

    /// Pop the element at the **back** (tail).
    ///
    /// Returns `None` if the list is empty.
    pub fn pop_back(&mut self) -> Option<*mut T> {
        if self.tail.is_null() {
            return None;
        }
        let link = self.tail;
        unsafe { self.unlink(link) };
        Some(unsafe { T::from_link(link) })
    }

    /// Remove a specific element from the list in O(1).
    ///
    /// # Safety
    ///
    /// - `ptr` must point to a `T` whose `Link` is currently in **this** list.
    pub unsafe fn remove(&mut self, ptr: *mut T) {
        let link = unsafe { T::links(ptr) };
        debug_assert!(
            unsafe { (*link).linked.load(Ordering::Relaxed) },
            "remove: node is not in a list"
        );
        unsafe { self.unlink(link) };
    }

    /// Internal: unlink a node from the list, updating head/tail/len.
    ///
    /// # Safety
    ///
    /// `link` must be a valid, linked node in this list.
    unsafe fn unlink(&mut self, link: *mut Link) {
        debug_assert!(
            is_canonical(link as u64),
            "unlink: non-canonical link {:p}",
            link
        );
        unsafe {
            let prev = (*link).prev;
            let next = (*link).next;

            debug_assert!(
                is_canonical(prev as u64),
                "unlink: non-canonical prev {:p}",
                prev
            );
            debug_assert!(
                is_canonical(next as u64),
                "unlink: non-canonical next {:p}",
                next
            );

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
            (*link).linked.store(false, Ordering::Relaxed);
        }
        self.len -= 1;
    }

    // -- access --------------------------------------------------------------

    /// Peek at the front element without removing it.
    pub fn front(&self) -> Option<*mut T> {
        if self.head.is_null() {
            None
        } else {
            Some(unsafe { T::from_link(self.head) })
        }
    }

    /// Peek at the back element without removing it.
    pub fn back(&self) -> Option<*mut T> {
        if self.tail.is_null() {
            None
        } else {
            Some(unsafe { T::from_link(self.tail) })
        }
    }

    // -- iteration -----------------------------------------------------------

    /// Iterate over elements front-to-back.
    ///
    /// # Safety
    ///
    /// The list must not be mutated during iteration. Nodes must remain valid.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            current: self.head,
            remaining: self.len,
            _marker: PhantomData,
        }
    }

    /// Drain all elements front-to-back, unlinking each node.
    ///
    /// The returned iterator yields `*mut T` and unlinks every node it visits.
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

/// Front-to-back iterator. Does **not** remove elements.
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

/// Draining iterator — pops every element from the front.
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
        // Ensure all remaining nodes are unlinked even if the caller
        // doesn't exhaust the iterator.
        while self.list.pop_front().is_some() {}
    }
}

// ---------------------------------------------------------------------------
// Helper macro
// ---------------------------------------------------------------------------

/// Implement [`Linked`] for a type given the name of its `Link` field.
///
/// ```ignore
/// use intrusive_list::impl_linked;
///
/// struct Task {
///     id: u64,
///     sched_link: Link,
/// }
///
/// impl_linked!(Task, sched_link);
/// ```
#[macro_export]
macro_rules! impl_linked {
    ($type:ty, $field:ident) => {
        unsafe impl $crate::Linked for $type {
            #[inline]
            unsafe fn links(ptr: *const Self) -> *mut $crate::Link {
                unsafe { ::core::ptr::addr_of_mut!((*ptr.cast_mut()).$field) }
            }

            #[inline]
            unsafe fn from_link(link: *mut $crate::Link) -> *mut Self {
                unsafe {
                    (link as *mut u8)
                        .sub(::core::mem::offset_of!($type, $field))
                        .cast::<Self>()
                }
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

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

    impl_linked!(Node, link);

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

        list.pop_front();
        assert!(!a.link.is_linked());

        unsafe { list.push_back(&mut a) };
        assert_eq!(collect_values(&list), vec![2, 1]);
    }

    #[test]
    fn single_element_push_pop_all_directions() {
        let mut a = Node::new(1);
        let mut list = IntrusiveList::<Node>::new();

        unsafe { list.push_back(&mut a) };
        assert_eq!(unsafe { (*list.pop_front().unwrap()).value }, 1);
        assert!(list.is_empty());

        unsafe { list.push_back(&mut a) };
        assert_eq!(unsafe { (*list.pop_back().unwrap()).value }, 1);
        assert!(list.is_empty());

        unsafe { list.push_front(&mut a) };
        assert_eq!(unsafe { (*list.pop_front().unwrap()).value }, 1);
        assert!(list.is_empty());

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

    struct DualNode {
        value: u64,
        link_a: Link,
        link_b: Link,
    }

    impl DualNode {
        fn new(value: u64) -> Self {
            Self {
                value,
                link_a: Link::new(),
                link_b: Link::new(),
            }
        }
    }

    impl_linked!(DualNode, link_a);

    #[test]
    fn dual_link_independence() {
        let mut node = DualNode::new(99);

        let mut list_a = IntrusiveList::<DualNode>::new();
        unsafe { list_a.push_back(&mut node) };

        assert!(node.link_a.is_linked());
        assert!(!node.link_b.is_linked());

        list_a.pop_front();
        assert!(!node.link_a.is_linked());
    }
}
