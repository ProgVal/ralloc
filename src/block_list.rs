//! The memory bookkeeping module.
//!
//! Blocks are the main unit for the memory bookkeeping. A block is a simple construct with a
//! `Unique` pointer and a size. `BlockEntry` contains an additional field, which marks if a block
//! is free or not. The block list is simply a continuous list of block entries kept in the
//! bookkeeper.
//!
//! The mechanism is outlined below:
//!
//! Allocate.
//! =========
//!
//! We start with our initial segment.
//!
//!        Address space
//!       I---------------------------------I
//!     B
//!     l
//!     k
//!     s
//!
//! We then split it at the aligner, which is used for making sure that the pointer is aligned properly.
//!
//!        Address space
//!       I------I
//!     B   ^    I--------------------------I
//!     l  al
//!     k
//!     s
//!
//! We then use the remaining block, but leave the excessive space.
//!
//!        Address space
//!       I------I
//!     B                           I--------I
//!     l        \_________________/
//!     k        our allocated block.
//!     s
//!
//! The pointer to the marked area is then returned.
//!
//! Deallocate
//! ==========
//!
//!        Address space
//!       I------I
//!     B                                  I--------I
//!     l        \_________________/
//!     k     the used block we want to deallocate.
//!     s
//!
//! We start by inserting the block, while keeping the list sorted. See `insertion` for details.
//!
//!
//!        Address space
//!       I------I
//!     B        I-----------------I
//!     l                                  I--------I
//!     k
//!     s
//!
//! Now the merging phase starts. We first observe that the first and the second block shares the
//! end and the start respectively, in other words, we can merge these by adding the size together:
//!
//!        Address space
//!       I------------------------I
//!     B                                  I--------I
//!     l
//!     k
//!     s
//!
//! Insertion
//! =========
//!
//! We want to insert the block denoted by the tildes into our list. Perform a binary search to
//! find where insertion is appropriate.
//!
//!        Address space
//!       I------I
//!     B < here                      I--------I
//!     l                                              I------------I
//!     k
//!     s                                                             I---I
//!                  I~~~~~~~~~~I
//!
//! If the entry is not empty, we check if the block can be merged to the left (i.e., the previous
//! block). If not, check if it is possible to the right. If both of these fails, we keep pushing
//! the blocks to the right to the next entry until a empty entry is reached:
//!
//!        Address space
//!       I------I
//!     B < here                      I--------I <~ this one cannot move down, due to being blocked.
//!     l
//!     k                                              I------------I <~ thus we have moved this one down.
//!     s                                                             I---I
//!                  I~~~~~~~~~~I
//!
//! Repeating yields:
//!
//!        Address space
//!       I------I
//!     B < here
//!     l                             I--------I <~ this one cannot move down, due to being blocked.
//!     k                                              I------------I <~ thus we have moved this one down.
//!     s                                                             I---I
//!                  I~~~~~~~~~~I
//!
//! Now an empty space is left out, meaning that we can insert the block:
//!
//!        Address space
//!       I------I
//!     B            I----------I
//!     l                             I--------I
//!     k                                              I------------I
//!     s                                                             I---I
//!
//! The insertion is now completed.
//!
//! Reallocation.
//! =============
//!
//! We will first try to perform an in-place reallocation, and if that fails, we will use memmove.
//!
//!        Address space
//!       I------I
//!     B \~~~~~~~~~~~~~~~~~~~~~/
//!     l     needed
//!     k
//!     s
//!
//! We simply find the block next to our initial block. If this block is free and have sufficient
//! size, we will simply merge it into our initial block. If these conditions are not met, we have
//! to deallocate our list, and then allocate a new one, after which we use memmove to copy the
//! data over to the newly allocated list.
//!
//! Guarantees made.
//! ================
//!
//! 1. The list is always sorted.
//! 2. No two free blocks overlap.
//! 3. No two free blocks are adjacent.

use block::{BlockEntry, Block};
use sys;

use std::mem::align_of;
use std::{ops, ptr, slice, cmp};
use std::ptr::Unique;

use extra::option::OptionalExt;

/// The memory bookkeeper.
///
/// This is the main primitive in ralloc. Its job is to keep track of the free blocks in a
/// structured manner, such that allocation, reallocation, and deallocation are all efficient.
/// Parituclarly, it keeps a list of free blocks, commonly called the "block list". This list is
/// kept. Entries in the block list can be "empty", meaning that you can overwrite the entry
/// without breaking consistency.
pub struct Bookkeeper {
    /// The capacity of the block list.
    cap: usize,
    /// The length of the block list.
    len: usize,
    /// The pointer to the first element in the block list.
    ptr: Unique<BlockEntry>,
}

/// Calculate the aligner.
///
/// The aligner is what we add to a pointer to align it to a given value.
fn aligner(ptr: *mut u8, align: usize) -> usize {
    align - ptr as usize % align
}

/// Canonicalize a BRK request.
///
/// Syscalls can be expensive, which is why we would rather accquire more memory than necessary,
/// than having many syscalls acquiring memory stubs. Memory stubs are small blocks of memory,
/// which are essentially useless until merge with another block.
///
/// To avoid many syscalls and accumulating memory stubs, we BRK a little more memory than
/// necessary. This function calculate the memory to be BRK'd based on the necessary memory.
fn canonicalize_brk(size: usize) -> usize {
    const BRK_MULTIPLIER: usize = 1;
    const BRK_MIN: usize = 200;
    const BRK_MIN_EXTRA: usize = 500;

    cmp::max(BRK_MIN, size + cmp::min(BRK_MULTIPLIER * size, BRK_MIN_EXTRA))
}

impl Bookkeeper {
    /// Allocate a chunk of memory.
    ///
    /// This function takes a size and an alignment. From these a fitting block is found, to which
    /// a pointer is returned. The pointer returned has the following guarantees:
    ///
    /// 1. It is aligned to `align`: In particular, `align` divides the address.
    /// 2. The chunk can be safely read and written, up to `size`. Reading or writing out of this
    ///    bound is undefined behavior.
    /// 3. It is a valid, unique, non-null pointer, until `free` is called again.
    pub fn alloc(&mut self, size: usize, align: usize) -> Unique<u8> {
        let mut ins = None;

        // We run right-to-left, since new blocks tend to get added to the right.
        for (n, i) in self.iter_mut().enumerate().rev() {
            let aligner = aligner(*i.ptr, align);

            if i.size - aligner >= size {
                // Set the excessive space as free.
                ins = Some((n, Block {
                    size: i.size - aligner - size,
                    ptr: unsafe { Unique::new((*i.ptr as usize + aligner + size) as *mut _) },
                }));

                // Leave the stub behind.
                if aligner == 0 {
                    i.free = false;
                } else {
                    i.size = aligner;
                }
            }
        }

        if let Some((n, b)) = ins {
            let res = unsafe {
                Unique::new((*b.ptr as usize - size) as *mut _)
            };

            if b.size != 0 {
                self.insert(n, b.into());
            }

            // Check consistency.
            self.check();

            res
        } else {
            // No fitting block found. Allocate a new block.
            self.alloc_fresh(size, align)
        }
    }

    /// Push to the block list.
    ///
    /// This will append a block entry to the end of the block list. Make sure that this entry has
    /// a value higher than any of the elements in the list, to keep it sorted.
    fn push(&mut self, block: BlockEntry) {
        let len = self.len;
        self.reserve(len + 1);

        unsafe {
            ptr::write((&mut *self.last_mut().unchecked_unwrap() as *mut _).offset(1), block);
        }

        // Check consistency.
        self.check();
    }

    /// Find a block's index through binary search.
    ///
    /// If it fails, the value will be where the block could be inserted to keep the list sorted.
    fn search(&mut self, block: &Block) -> Result<usize, usize> {
        self.binary_search_by(|x| (**x).cmp(block))
    }

    /// Allocate _fresh_ space.
    ///
    /// "Fresh" means that the space is allocated through a BRK call to the kernel.
    fn alloc_fresh(&mut self, size: usize, align: usize) -> Unique<u8> {
        // Calculate the canonical size (extra space is allocated to limit the number of system calls).
        let can_size = canonicalize_brk(size);
        // Get the previous segment end.
        let seg_end = sys::segment_end().unwrap_or_else(|x| x.handle());
        // Calculate the aligner.
        let aligner = aligner(seg_end, align);
        // Use SYSBRK to allocate extra data segment.
        let ptr = sys::inc_brk(can_size + aligner).unwrap_or_else(|x| x.handle());

        let alignment_block = Block {
            size: aligner,
            ptr: ptr,
        };
        let res = Block {
            ptr: alignment_block.end(),
            size: size,
        };

        // Add it to the list. This will not change the order, since the pointer is higher than all
        // the previous blocks.
        self.push(alignment_block.into());

        // Add the extra space allocated.
        self.push(Block {
            size: can_size - size,
            ptr: res.end(),
        }.into());

        // Check consistency.
        self.check();

        res.ptr
    }

    /// Reallocate inplace.
    ///
    /// This will try to reallocate a buffer inplace, meaning that the buffers length is merely
    /// extended, and not copied to a new buffer.
    ///
    /// Returns `Err(())` if the buffer extension couldn't be done, `Err(())` otherwise.
    ///
    /// The following guarantees are made:
    ///
    /// 1. If this function returns `Ok(())`, it is valid to read and write within the bound of the
    ///    new size.
    /// 2. No changes are made to the allocated buffer itself.
    /// 3. On failure, the state of the allocator is left unmodified.
    fn realloc_inplace(&mut self, ind: usize, old_size: usize, size: usize) -> Result<(), ()> {
        if ind == self.len - 1 { return Err(()) }

        let additional = old_size - size;

        if old_size + self[ind + 1].size >= size {
            // Leave the excessive space.
            self[ind + 1].ptr = unsafe {
                Unique::new((*self[ind + 1].ptr as usize + additional) as *mut _)
            };
            self[ind + 1].size -= additional;

            // Set the excessive block as free if it is empty.
            if self[ind + 1].size == 0 {
                self[ind + 1].free = false;
            }

            // Check consistency.
            self.check();

            Ok(())
        } else {
            Err(())
        }
    }

    /// Reallocate memory.
    ///
    /// If necessary it will allocate a new buffer and deallocate the old one.
    ///
    /// The following guarantees are made:
    ///
    /// 1. The returned pointer is valid and aligned to `align`.
    /// 2. The returned pointer points to a buffer containing the same data byte-for-byte as the
    ///    original buffer.
    /// 3. Reading and writing up to the bound, `new_size`, is valid.
    pub fn realloc(&mut self, block: Block, new_size: usize, align: usize) -> Unique<u8> {
        let ind = self.find(&block);

        if self.realloc_inplace(ind, block.size, new_size).is_ok() {
            block.ptr
        } else {
            // Reallocation cannot be done inplace.

            // Allocate a new block with the same size.
            let ptr = self.alloc(new_size, align);

            // Copy the old data to the new location.
            unsafe { ptr::copy(*block.ptr, *ptr, block.size); }

            // Free the old block.
            self.free(block);

            // Check consistency.
            self.check();

            ptr
        }
    }

    /// Reserve space for the block list.
    ///
    /// This will extend the capacity to a number greater than or equals to `needed`, potentially
    /// reallocating the block list.
    fn reserve(&mut self, needed: usize) {
        if needed > self.cap {
            // Reallocate the block list.
            self.ptr = unsafe {
                let block = Block {
                    ptr: Unique::new(*self.ptr as *mut _),
                    size: self.cap,
                };

                Unique::new(*self.realloc(block, needed * 2, align_of::<BlockEntry>()) as *mut _)
            };
            // Update the capacity.
            self.cap = needed * 2;

            // Check consistency.
            self.check();
        }
    }

    /// Perform a binary search to find the appropriate place where the block can be insert or is
    /// located.
    fn find(&mut self, block: &Block) -> usize {
        match self.search(block) {
            Ok(x) => x,
            Err(x) => x,
        }
    }

    /// Free a memory block.
    ///
    /// After this have been called, no guarantees are made about the passed pointer. If it want
    /// to, it could begin shooting laser beams.
    pub fn free(&mut self, block: Block) {
        let ind = self.find(&block);

        // Try to merge left.
        if ind != 0 && self[ind - 1].left_to(&block) {
            self[ind - 1].size += block.size;
        // Try to merge right.
        } else if ind < self.len - 1 && self[ind].left_to(&block) {
            self[ind].size += block.size;
        } else {
            self.insert(ind, block.into());
        }

        // Check consistency.
        self.check();
    }

    /// Insert a block entry at some index.
    ///
    /// If the space is non-empty, the elements will be pushed filling out the empty gaps to the
    /// right. If all places to the right is occupied, it will reserve additional space to the
    /// block list.
    fn insert(&mut self, ind: usize, block: BlockEntry) {
        let len = self.len;

        // Find the next gap, where a used block were.
        let n = self.iter()
            .skip(ind)
            .enumerate()
            .filter(|&(_, x)| x.free)
            .next().map(|x| x.0)
            .unwrap_or_else(|| {
                // No gap was found, so we need to reserve space for new elements.
                self.reserve(len + 1);
                ind
            });

        // Memmove the blocks to close in that gap.
        unsafe {
            ptr::copy(self[ind..].as_ptr(), self[ind + 1..].as_mut_ptr(), self.len - n);
        }

        // Place the block left to the moved line.
        self[ind] = block;
        self.len += 1;

        // Check consistency.
        self.check();
    }

    /// No-op in release mode.
    #[cfg(not(debug_assertions))]
    fn check(&self) {}

    /// Perform consistency checks.
    ///
    /// This will check for the following conditions:
    ///
    /// 1. The list is sorted.
    /// 2. No entries are not overlapping.
    /// 3. The length does not exceed the capacity.
    #[cfg(debug_assertions)]
    fn check(&self) {
        // Check if sorted.
        let mut prev = *self[0].ptr;
        for (n, i) in self.iter().enumerate().skip(1) {
            assert!(*i.ptr > prev, "The block list is not sorted at index, {}.", n);
            prev = *i.ptr;
        }
        // Check if overlapping.
        let mut prev = *self[0].ptr;
        for (n, i) in self.iter().enumerate().skip(1) {
            assert!(!i.free || *i.ptr > prev, "Two blocks are overlapping/adjacent at index, {}.", n);
            prev = *i.end();
        }

        // Check that the length is lower than or equals to the capacity.
        assert!(self.len <= self.cap, "The capacity does not cover the length.")
    }
}

impl ops::Deref for Bookkeeper {
    type Target = [BlockEntry];

    fn deref(&self) -> &[BlockEntry] {
        unsafe {
            slice::from_raw_parts(*self.ptr as *const _, self.len)
        }
    }
}
impl ops::DerefMut for Bookkeeper {
    fn deref_mut(&mut self) -> &mut [BlockEntry] {
        unsafe {
            slice::from_raw_parts_mut(*self.ptr, self.len)
        }
    }
}
