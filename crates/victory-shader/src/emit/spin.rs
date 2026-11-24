//! Loops that only wait for memory to change (spin waits).
//!
//! A spin wait reads the same address again and again until another
//! workgroup, queue or the command processor writes it: decoupled
//! look-back prefix sums, persistent-thread queues, locks. On the console
//! the write comes within microseconds. On the host it may never come: the
//! writer's view can be another host resource than the reader's, null
//! (reads zeros) or not mirrored yet, and the host compiler may hoist a
//! load out of a loop that writes nothing. Such a loop then runs to the
//! iteration limit in every lane, 2^20 uncached reads: about a second per
//! wave. These loops get a much smaller limit ([`super::spin_limit`]).
//!
//! A loop counts as one only when nothing in it can have an effect the
//! limit would cut short: no stores, exports, LDS writes, barriers or
//! messages; every memory read and atomic at an address the loop does not
//! change; and no loop-carried value made by arithmetic (a counter means
//! the loop does work per iteration). What it carries are lane masks,
//! compare results and what it read.

use std::collections::{HashMap, HashSet};

use crate::gfx10::{Fields, Operand};
use crate::ir::{EXEC, Program, Reg, Value};

/// Headers (IR block indices) of the loops that only wait for memory.
pub(super) fn spin_loops(p: &Program) -> HashSet<usize> {
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut rpo = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        rpo[b] = n;
    }
    let mut found = HashSet::new();
    for &h in &order {
        let latches: Vec<usize> = p.blocks[h]
            .preds
            .iter()
            .copied()
            .filter(|&q| rpo[q] != usize::MAX && rpo[h] <= rpo[q])
            .collect();
        if latches.is_empty() {
            continue;
        }
        if Loop::new(p, h, &latches, &rpo).waits_only() {
            found.insert(h);
        }
    }
    found
}

#[derive(Clone, Copy)]
enum Def {
    Phi(usize, usize),
    Inst(usize, usize),
}

/// What an instruction does to memory, for the spin test.
#[derive(PartialEq)]
enum Access {
    /// Reads (or an atomic): fine at an address the loop keeps.
    Read,
    /// Writes or synchronises: the loop does work.
    Effect,
    /// Registers only.
    None,
}

struct Loop<'a> {
    p: &'a Program,
    header: usize,
    body: HashSet<usize>,
    defs: HashMap<Value, Def>,
    /// Phis that merge one value with themselves, by that value.
    alias: HashMap<Value, Value>,
}

impl<'a> Loop<'a> {
    fn new(p: &'a Program, header: usize, latches: &[usize], rpo: &[usize]) -> Self {
        let mut body = HashSet::from([header]);
        let mut work = latches.to_vec();
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(p.blocks[b].preds.iter().filter(|&&q| rpo[q] != usize::MAX));
            }
        }
        let mut defs = HashMap::new();
        for &b in &body {
            for (i, phi) in p.blocks[b].phis.iter().enumerate() {
                defs.insert(phi.value, Def::Phi(b, i));
            }
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    defs.insert(v, Def::Inst(b, i));
                }
            }
        }
        let mut l = Loop {
            p,
            header,
            body,
            defs,
            alias: HashMap::new(),
        };
        // Registers the loop does not write still get phis at its header:
        // each merges one value with itself.
        loop {
            let mut changed = false;
            for &b in &l.body {
                for phi in &p.blocks[b].phis {
                    if l.alias.contains_key(&phi.value) {
                        continue;
                    }
                    let mut other = None;
                    let trivial = phi.args.iter().all(|&a| {
                        let a = l.resolve(a);
                        a == phi.value || *other.get_or_insert(a) == a
                    });
                    if trivial && let Some(o) = other {
                        l.alias.insert(phi.value, o);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        l
    }

    fn resolve(&self, mut v: Value) -> Value {
        // Aliases form no cycle: each points at a value made earlier.
        for _ in 0..64 {
            match self.alias.get(&v) {
                Some(&a) => v = a,
                None => break,
            }
        }
        v
    }

    fn waits_only(&self) -> bool {
        let mut reads = 0;
        let mut memo = HashMap::new();
        for &b in &self.body {
            for inst in &self.p.blocks[b].insts {
                match access(inst) {
                    Access::Effect => return false,
                    Access::None => {}
                    Access::Read => {
                        reads += 1;
                        let kept = address(inst)
                            .into_iter()
                            .filter_map(|r| inst.read(r))
                            .all(|v| self.invariant(v, &mut memo));
                        if !kept {
                            return false;
                        }
                    }
                }
            }
        }
        if reads == 0 {
            return false;
        }
        // What goes round the loop: no counter.
        let mut seen = HashSet::new();
        self.p.blocks[self.header]
            .phis
            .iter()
            .filter(|phi| !self.alias.contains_key(&phi.value))
            .all(|phi| self.carried_ok(phi.value, &mut seen))
    }

    /// Whether `v` is the same in every iteration.
    fn invariant(&self, v: Value, memo: &mut HashMap<Value, bool>) -> bool {
        let v = self.resolve(v);
        let Some(&def) = self.defs.get(&v) else {
            return true;
        };
        if let Some(&known) = memo.get(&v) {
            return known;
        }
        let r = match def {
            Def::Phi(..) => false,
            Def::Inst(b, i) => {
                let inst = &self.p.blocks[b].insts[i];
                let valu = matches!(inst.fields, Fields::Valu(_));
                let name = inst.name();
                let lane_wise = valu
                    && !name.starts_with("v_readfirstlane")
                    && !name.starts_with("v_readlane")
                    && !name.starts_with("v_cmp");
                matches!(inst.fields, Fields::Sop { .. } | Fields::Valu(_))
                    && inst
                        .reads
                        .iter()
                        .filter(|(r, _)| !(lane_wise && is_exec(*r)))
                        .all(|&(_, x)| self.invariant(x, memo))
            }
        };
        memo.insert(v, r);
        r
    }

    /// Whether loop-carried `v` holds what the loop read, lane masks or
    /// compare results, and nothing computed by arithmetic.
    fn carried_ok(&self, v: Value, seen: &mut HashSet<Value>) -> bool {
        let v = self.resolve(v);
        if !seen.insert(v) {
            return true;
        }
        match self.defs.get(&v) {
            None => true,
            Some(&Def::Phi(b, i)) => self.p.blocks[b].phis[i]
                .args
                .iter()
                .all(|&a| self.carried_ok(a, seen)),
            Some(&Def::Inst(b, i)) => {
                let inst = &self.p.blocks[b].insts[i];
                let name = inst.name();
                if access(inst) == Access::Read || mask_op(name) {
                    return true;
                }
                if copy_op(name) {
                    let valu = matches!(inst.fields, Fields::Valu(_));
                    return inst
                        .reads
                        .iter()
                        .filter(|(r, _)| !(valu && is_exec(*r)))
                        .all(|&(_, x)| self.carried_ok(x, seen));
                }
                false
            }
        }
    }
}

fn is_exec(r: Reg) -> bool {
    matches!(r, Reg::S(c) if c == EXEC || c == EXEC + 1)
}

/// Lane-mask bookkeeping and compares: what every divergent loop carries.
fn mask_op(name: &str) -> bool {
    [
        "s_and_", "s_or_", "s_xor_", "s_andn2_", "s_orn2_", "s_nand_", "s_nor_", "s_xnor_",
        "s_not_", "s_cmp_", "s_cmpk_", "s_bitcmp", "v_cmp_", "v_cmpx_",
    ]
    .iter()
    .any(|p| name.starts_with(p))
}

/// Moves and selects: carry what they copy.
fn copy_op(name: &str) -> bool {
    name.starts_with("s_mov_")
        || name.starts_with("s_cmov_")
        || name.starts_with("s_cselect_")
        || name.starts_with("v_mov_b32")
        || name.starts_with("v_cndmask_b32")
        || name.starts_with("v_readfirstlane_b32")
        || name.starts_with("v_readlane_b32")
}

fn access(inst: &crate::ir::Inst) -> Access {
    let name = inst.name();
    match &inst.fields {
        Fields::Exp { .. } => Access::Effect,
        Fields::Sopp { .. } => {
            if name == "s_barrier" || name.starts_with("s_sendmsg") || name.starts_with("s_trap") {
                Access::Effect
            } else {
                Access::None
            }
        }
        Fields::Sop { .. } | Fields::Valu(_) | Fields::Vintrp { .. } => Access::None,
        Fields::Smem { data, .. } => {
            if name.contains("store") || name.contains("atomic") {
                Access::Effect
            } else if data.is_some() {
                Access::Read
            } else {
                Access::None
            }
        }
        Fields::Buffer { lds, .. } => {
            if *lds || name.contains("store") {
                Access::Effect
            } else if name.contains("load") || name.contains("atomic") {
                Access::Read
            } else {
                // Cache invalidations.
                Access::None
            }
        }
        Fields::Image { .. } => {
            if name.contains("store") {
                Access::Effect
            } else {
                Access::Read
            }
        }
        Fields::Ds { gds, .. } => {
            if *gds {
                Access::Effect
            } else if name.starts_with("ds_read") {
                Access::Read
            } else if name.starts_with("ds_swizzle") || name.contains("permute") {
                Access::None
            } else {
                Access::Effect
            }
        }
        Fields::Flat { .. } => {
            if name.starts_with("global_load_") {
                Access::Read
            } else {
                Access::Effect
            }
        }
    }
}

/// The registers that make up a memory access's address.
fn address(inst: &crate::ir::Inst) -> Vec<Reg> {
    let mut regs = Vec::new();
    let mut add = |o: &Operand| match *o {
        Operand::Scalar { code, dwords } => {
            regs.extend((0..dwords).map(|k| Reg::S(code.wrapping_add(k))));
        }
        Operand::Vector { reg, dwords } => {
            regs.extend((0..dwords).map(|k| Reg::V(reg.wrapping_add(k))));
        }
        _ => {}
    };
    match &inst.fields {
        Fields::Smem { base, soffset, .. } => {
            add(base);
            add(soffset);
        }
        Fields::Buffer {
            addr,
            rsrc,
            soffset,
            offen,
            idxen,
            ..
        } => {
            add(rsrc);
            add(soffset);
            if *offen || *idxen {
                add(addr);
            }
        }
        Fields::Image {
            addr,
            rsrc,
            sampler,
            ..
        } => {
            add(rsrc);
            if let Some(s) = sampler {
                add(s);
            }
            addr.iter().for_each(&mut add);
        }
        Fields::Ds { addr: Some(a), .. } => add(a),
        Fields::Flat { addr, saddr, .. } => {
            if let Some(a) = addr {
                add(a);
            }
            if let Some(s) = saddr {
                add(s);
            }
        }
        _ => {}
    }
    regs
}

#[cfg(test)]
mod tests {
    use super::spin_loops;
    use crate::ir;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn spins(words: &[u32]) -> Vec<usize> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = ir::build(&code, false, 256).unwrap();
        let mut h: Vec<usize> = spin_loops(&p).into_iter().collect();
        h.sort_unstable();
        h
    }

    #[test]
    fn a_loop_rereading_one_flag_is_a_spin_wait() {
        // Each lane waits until its flag (at v0 in the buffer at s[8:11])
        // is no longer 0.
        let words = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 4: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF9, // s_cbranch_execnz -> 4
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(spins(&words).len(), 1);
    }

    #[test]
    fn a_loop_that_stores_or_counts_is_not() {
        // The same with a store of the flag back: work per iteration.
        let stores = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 4: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0xE070_2000,
            0x8002_0100, // buffer_store_dword v1, v0, s[8:11], 0 idxen
            0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF7, // s_cbranch_execnz -> 4
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(spins(&stores), Vec::<usize>::new());
        // A loop walking the buffer: the address moves.
        let walks = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 4: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0x4A00_0081, // v_add_nc_u32 v0, 1, v0
            0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF8, // s_cbranch_execnz -> 4
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(spins(&walks), Vec::<usize>::new());
        // Rereading one address while counting: a loop with work to do.
        let counts = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0xE030_6000,
            0x8002_0100, // 4: buffer_load_dword v1, v0, s[8:11], 0 idxen glc
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0x4A04_0481, // v_add_nc_u32 v2, 1, v2
            0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFF8, // s_cbranch_execnz -> 4
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(spins(&counts), Vec::<usize>::new());
    }

    #[test]
    fn a_loop_without_memory_reads_is_not() {
        // The divergent counting loop of the structure tests (wave32).
        let words = [
            0xBE82_0380, // s_mov_b32 s2, 0
            0x4A00_0081, // 4: v_add_nc_u32 v0, 1, v0
            0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
            0x8802_026A, // s_or_b32 s2, vcc_lo, s2
            0x8A7E_027E, // s_andn2_b32 exec_lo, exec_lo, s2
            0xBF89_FFFB, // s_cbranch_execnz -> 4
            0x887E_027E, // s_or_b32 exec_lo, exec_lo, s2
            S_ENDPGM,
        ];
        assert_eq!(spins(&words), Vec::<usize>::new());
    }
}
