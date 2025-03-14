// Copyright 2025 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use derive_more::Debug;
use risc0_binfmt::{MemoryImage2, Page, WordAddr};
use risc0_zkp::core::digest::Digest;

use super::{node_idx, platform::*};

pub const PAGE_WORDS: usize = PAGE_BYTES / WORD_SIZE;

const LOAD_ROOT_CYCLES: u32 = 1;
const RESUME_CYCLES: u32 = 2;
const SUSPEND_CYCLES: u32 = 2;
const STORE_ROOT_CYCLES: u32 = 1;

const POSEIDON_PAGING: u32 = 1;
const POSEIDON_LOAD_IN: u32 = 2;
const POSEIDON_DO_OUT: u32 = 1;
const POSEIDON_EXTERNAL: u32 = 8;
const POSEIDON_INTERNAL: u32 = 1;
const POSEIDON_ENTRY: u32 = 1;
pub(crate) const POSEIDON_BLOCK_WORDS: u32 = 8;
pub(crate) const POSEIDON_PAGE_ROUNDS: u32 = PAGE_WORDS as u32 / POSEIDON_BLOCK_WORDS;

const PAGE_CYCLES: u32 = POSEIDON_PAGING + 10 * POSEIDON_PAGE_ROUNDS + POSEIDON_DO_OUT;

const NODE_CYCLES: u32 =
    POSEIDON_PAGING + POSEIDON_LOAD_IN + POSEIDON_EXTERNAL + POSEIDON_INTERNAL + POSEIDON_DO_OUT;

pub(crate) const RESERVED_PAGING_CYCLES: u32 = LOAD_ROOT_CYCLES
    + POSEIDON_ENTRY
    + POSEIDON_PAGING
    + RESUME_CYCLES
    + SUSPEND_CYCLES
    + POSEIDON_ENTRY
    + POSEIDON_PAGING
    + STORE_ROOT_CYCLES;

const INVALID_IDX: u32 = u32::MAX;
const NUM_PAGES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub(crate) enum PageState {
    Unloaded,
    Loaded,
    Dirty,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PageTraceEvent {
    PageIn { cycles: u32 },
    PageOut { cycles: u32 },
}

#[derive(Debug)]
pub(crate) struct PagedMemory {
    pub image: MemoryImage2,
    #[debug(skip)]
    page_table: Vec<u32>,
    #[debug(skip)]
    page_cache: Vec<Page>,
    #[debug("{page_states:#x?}")]
    pub(crate) page_states: BTreeMap<u32, PageState>,
    pub cycles: u32,
    user_registers: [u32; REG_MAX],
    machine_registers: [u32; REG_MAX],
    tracing_enabled: bool,
    trace_events: Vec<PageTraceEvent>,
}

impl PagedMemory {
    pub(crate) fn new(mut image: MemoryImage2, tracing_enabled: bool) -> Self {
        let mut machine_registers = [0; REG_MAX];
        let mut user_registers = [0; REG_MAX];
        let page_idx = MACHINE_REGS_ADDR.waddr().page_idx();
        let page = image.get_page(page_idx).unwrap();
        for idx in 0..REG_MAX {
            machine_registers[idx] = page.load(MACHINE_REGS_ADDR.waddr() + idx);
            user_registers[idx] = page.load(USER_REGS_ADDR.waddr() + idx);
        }

        Self {
            image,
            page_table: vec![INVALID_IDX; NUM_PAGES],
            page_cache: Vec::new(),
            page_states: BTreeMap::new(),
            cycles: RESERVED_PAGING_CYCLES,
            user_registers,
            machine_registers,
            tracing_enabled,
            trace_events: vec![],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.page_table.fill(INVALID_IDX);
        self.page_cache.clear();
        self.page_states.clear();
        self.cycles = RESERVED_PAGING_CYCLES;
    }

    fn try_load_register(&self, addr: WordAddr) -> Option<u32> {
        if addr >= USER_REGS_ADDR.waddr() && addr < USER_REGS_ADDR.waddr() + REG_MAX {
            let reg_idx = addr - USER_REGS_ADDR.waddr();
            Some(self.user_registers[reg_idx.0 as usize])
        } else if addr >= MACHINE_REGS_ADDR.waddr() && addr < MACHINE_REGS_ADDR.waddr() + REG_MAX {
            let reg_idx = addr - MACHINE_REGS_ADDR.waddr();
            Some(self.machine_registers[reg_idx.0 as usize])
        } else {
            None
        }
    }

    fn try_store_register(&mut self, addr: WordAddr, word: u32) -> bool {
        if addr >= USER_REGS_ADDR.waddr() && addr < USER_REGS_ADDR.waddr() + REG_MAX {
            let reg_idx = addr - USER_REGS_ADDR.waddr();
            self.user_registers[reg_idx.0 as usize] = word;
            true
        } else if addr >= MACHINE_REGS_ADDR.waddr() && addr < MACHINE_REGS_ADDR.waddr() + REG_MAX {
            let reg_idx = addr - MACHINE_REGS_ADDR.waddr();
            self.machine_registers[reg_idx.0 as usize] = word;
            true
        } else {
            false
        }
    }

    fn peek_ram(&mut self, addr: WordAddr) -> Result<u32> {
        let page_idx = addr.page_idx();
        let cache_idx = self.page_table[page_idx as usize];
        if cache_idx == INVALID_IDX {
            // Unloaded, peek into image
            Ok(self.image.get_page(page_idx)?.load(addr))
        } else {
            // Loaded, get from cache
            Ok(self.page_cache[cache_idx as usize].load(addr))
        }
    }

    pub(crate) fn peek(&mut self, addr: WordAddr) -> Result<u32> {
        if addr >= MEMORY_END_ADDR {
            bail!("Invalid peek address: {addr:?}");
        }

        match self.try_load_register(addr) {
            Some(word) => Ok(word),
            None => self.peek_ram(addr),
        }
    }

    pub(crate) fn peek_page(&mut self, page_idx: u32) -> Result<Vec<u8>> {
        let cache_idx = self.page_table[page_idx as usize];
        if cache_idx == INVALID_IDX {
            // Unloaded, peek into image
            Ok(self.image.get_page(page_idx)?.0.clone())
        } else {
            // Loaded, get from cache
            Ok(self.page_cache[cache_idx as usize].0.clone())
        }
    }

    fn load_ram(&mut self, addr: WordAddr) -> Result<u32> {
        let page_idx = addr.page_idx();
        let node_idx = node_idx(page_idx);
        // tracing::trace!("load: {addr:?}, page: {page_idx:#08x}, node: {node_idx:#08x}");
        let mut cache_idx = self.page_table[page_idx as usize];
        if cache_idx == INVALID_IDX {
            self.load_page(page_idx)?;
            self.page_states.insert(node_idx, PageState::Loaded);
            cache_idx = self.page_table[page_idx as usize];
        }
        Ok(self.page_cache[cache_idx as usize].load(addr))
    }

    pub(crate) fn load(&mut self, addr: WordAddr) -> Result<u32> {
        if addr >= MEMORY_END_ADDR {
            bail!("Invalid load address: {addr:?}");
        }

        match self.try_load_register(addr) {
            Some(word) => Ok(word),
            None => self.load_ram(addr),
        }
    }

    fn store_ram(&mut self, addr: WordAddr, word: u32) -> Result<()> {
        // tracing::trace!("store: {addr:?}, page: {page_idx:#08x}, word: {word:#010x}");
        let page_idx = addr.page_idx();
        let page = self.page_for_writing(page_idx)?;
        page.store(addr, word);
        Ok(())
    }

    pub(crate) fn store(&mut self, addr: WordAddr, word: u32) -> Result<()> {
        if addr >= MEMORY_END_ADDR {
            bail!("Invalid store address: {addr:?}");
        }

        if self.try_store_register(addr, word) {
            Ok(())
        } else {
            self.store_ram(addr, word)
        }
    }

    fn page_for_writing(&mut self, page_idx: u32) -> Result<&mut Page> {
        let node_idx = node_idx(page_idx);
        let state = if let Some(state) = self.page_states.get(&node_idx) {
            *state
        } else {
            self.load_page(page_idx)?;
            PageState::Loaded
        };
        if state == PageState::Loaded {
            self.cycles += PAGE_CYCLES;
            self.trace_page_out(PAGE_CYCLES);
            self.fixup_costs(node_idx, PageState::Dirty);
            self.page_states.insert(node_idx, PageState::Dirty);
        }
        let cache_idx = self.page_table[page_idx as usize] as usize;
        Ok(self.page_cache.get_mut(cache_idx).unwrap())
    }

    fn write_registers(&mut self) {
        // Copy register values first to avoid borrow conflicts
        let user_registers = self.user_registers;
        let machine_registers = self.machine_registers;
        // This works because we can assume that user and machine register files
        // live in the same page.
        let page_idx = MACHINE_REGS_ADDR.waddr().page_idx();
        let page = self.page_for_writing(page_idx).unwrap();
        for idx in 0..REG_MAX {
            page.store(MACHINE_REGS_ADDR.waddr() + idx, machine_registers[idx]);
            page.store(USER_REGS_ADDR.waddr() + idx, user_registers[idx]);
        }
    }

    pub(crate) fn commit(&mut self) -> Result<(Digest, MemoryImage2, Digest)> {
        // tracing::trace!("commit: {self:#?}");

        self.write_registers();

        let pre_state = self.image.image_id();
        let mut image = MemoryImage2::default();

        // Gather the original pages
        for (&node_idx, &page_state) in self.page_states.iter() {
            if node_idx < MEMORY_PAGES as u32 {
                continue;
            }
            let page_idx = page_idx(node_idx);
            tracing::trace!("commit: {page_idx:#08x}, state: {page_state:?}");

            // Copy original state of all pages accessed in this segment.
            image.set_page(page_idx, self.image.get_page(page_idx)?);

            // Update dirty pages into the image that accumulates over a session.
            if page_state == PageState::Dirty {
                let cache_idx = self.page_table[page_idx as usize] as usize;
                let page = &self.page_cache[cache_idx];
                self.image.set_page(page_idx, page.clone());
            }
        }

        // Add minimal needed 'uncles'
        for &node_idx in self.page_states.keys() {
            // If this is a leaf, break
            if node_idx >= MEMORY_PAGES as u32 {
                break;
            }

            let lhs_idx = node_idx * 2;
            let rhs_idx = node_idx * 2 + 1;

            // Otherwise, add whichever child digest (if any) is not loaded
            if !self.page_states.contains_key(&lhs_idx) {
                image.set_digest(lhs_idx, *self.image.get_digest(lhs_idx)?);
            }
            if !self.page_states.contains_key(&rhs_idx) {
                image.set_digest(rhs_idx, *self.image.get_digest(rhs_idx)?);
            }
        }

        let post_state = self.image.image_id();

        Ok((pre_state, image, post_state))
    }

    fn load_page(&mut self, page_idx: u32) -> Result<()> {
        tracing::trace!("load_page: {page_idx:#08x}");
        let page = self.image.get_page(page_idx)?;
        self.page_table[page_idx as usize] = self.page_cache.len() as u32;
        self.page_cache.push(page);
        self.cycles += PAGE_CYCLES;
        self.trace_page_in(PAGE_CYCLES);
        self.fixup_costs(node_idx(page_idx), PageState::Loaded);
        Ok(())
    }

    fn fixup_costs(&mut self, mut node_idx: u32, goal: PageState) {
        tracing::trace!("fixup: {node_idx:#010x}: {goal:?}");
        while node_idx != 0 {
            let state = *self
                .page_states
                .get(&node_idx)
                .unwrap_or(&PageState::Unloaded);
            if goal > state {
                if node_idx < MEMORY_PAGES as u32 {
                    if state == PageState::Unloaded {
                        // tracing::trace!("fixup: {state:?}: {node_idx:#010x}");
                        self.cycles += NODE_CYCLES;
                        self.trace_page_in(NODE_CYCLES);
                    }
                    if goal == PageState::Dirty {
                        // tracing::trace!("fixup: {goal:?}: {node_idx:#010x}");
                        self.cycles += NODE_CYCLES;
                        self.trace_page_out(NODE_CYCLES);
                    }
                }
                self.page_states.insert(node_idx, goal);
            }
            node_idx /= 2;
        }
    }

    pub(crate) fn trace_events(&self) -> &[PageTraceEvent] {
        &self.trace_events
    }

    pub(crate) fn clear_trace_events(&mut self) {
        self.trace_events.clear();
    }

    fn trace_page_in(&mut self, cycles: u32) {
        if self.tracing_enabled {
            self.trace_events.push(PageTraceEvent::PageIn { cycles });
        }
    }

    fn trace_page_out(&mut self, cycles: u32) {
        if self.tracing_enabled {
            self.trace_events.push(PageTraceEvent::PageOut { cycles });
        }
    }
}

pub(crate) fn page_idx(node_idx: u32) -> u32 {
    node_idx - MEMORY_PAGES as u32
}
