/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Containers for the `Cx` state that changes while Rust code is using it.
//!
//! Any call into QuickJS or the embedder can run finalizers, natives or hooks that reach the same
//! `Cx` through `current()` and push to its pool or realm stack. `Cx` is therefore only reached
//! through shared references, and these containers hand out copies of their elements: no
//! reference into them outlives a method call, and their methods never call out.

use std::cell::UnsafeCell;
use std::collections::VecDeque;

pub(crate) struct Stack<T: Copy>(UnsafeCell<Vec<T>>);

impl<T: Copy> Default for Stack<T> {
    fn default() -> Self {
        Stack(UnsafeCell::new(Vec::new()))
    }
}

impl<T: Copy> Stack<T> {
    fn with<R>(&self, f: impl FnOnce(&mut Vec<T>) -> R) -> R {
        // SAFETY: `Stack` is not `Sync`, and the borrow ends before `with` returns: `f` is one of
        // the methods below, which call nothing that could reach this stack.
        f(unsafe { &mut *self.0.get() })
    }

    pub fn push(&self, v: T) {
        self.with(|s| s.push(v))
    }

    pub fn pop(&self) -> Option<T> {
        self.with(|s| s.pop())
    }

    pub fn last(&self) -> Option<T> {
        self.with(|s| s.last().copied())
    }

    pub fn len(&self) -> usize {
        self.with(|s| s.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.with(|s| s.clone())
    }
}

pub(crate) struct Queue<T: Copy>(UnsafeCell<VecDeque<T>>);

impl<T: Copy> Default for Queue<T> {
    fn default() -> Self {
        Queue(UnsafeCell::new(VecDeque::new()))
    }
}

impl<T: Copy> Queue<T> {
    fn with<R>(&self, f: impl FnOnce(&mut VecDeque<T>) -> R) -> R {
        // SAFETY: as for `Stack::with`.
        f(unsafe { &mut *self.0.get() })
    }

    pub fn push_back(&self, v: T) {
        self.with(|q| q.push_back(v))
    }

    pub fn push_front(&self, v: T) {
        self.with(|q| q.push_front(v))
    }

    pub fn pop_front(&self) -> Option<T> {
        self.with(|q| q.pop_front())
    }

    pub fn front(&self) -> Option<T> {
        self.with(|q| q.front().copied())
    }

    pub fn len(&self) -> usize {
        self.with(|q| q.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn take(&self) -> VecDeque<T> {
        self.with(std::mem::take)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::Stack;

    thread_local!(static STACK: Cell<*const Stack<u32>> = const { Cell::new(std::ptr::null()) });

    /// A "finalizer" that reaches the stack the way finalizers reach `Cx`: through a thread-local
    /// pointer, while the caller holds a shared reference.
    fn free(v: u32) {
        if v >= 100 {
            let stack = unsafe { &*STACK.with(Cell::get) };
            stack.push(v - 100);
        }
    }

    // Run under Miri: `cargo +nightly miri test -p mozjs --lib reentrant`.
    #[test]
    fn pushes_during_a_drain_are_drained() {
        let stack = Stack::default();
        STACK.with(|s| s.set(&stack));
        for v in [1, 101, 102, 203] {
            stack.push(v);
        }
        let mut freed = vec![];
        while let Some(v) = stack.pop() {
            free(v);
            freed.push(v);
        }
        STACK.with(|s| s.set(std::ptr::null()));
        freed.sort();
        assert_eq!(freed, [1, 1, 2, 3, 101, 102, 103, 203]);
    }
}
