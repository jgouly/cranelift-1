//! This module defines arm64-specific machine instruction types.

#![allow(non_snake_case)]
#![allow(unused_imports)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]

use crate::binemit::{CodeOffset, CodeSink, ConstantPoolSink, NullConstantPoolSink};
use crate::ir::constant::{ConstantData, ConstantOffset};
use crate::ir::types::{B1, B128, B16, B32, B64, B8, F32, F64, I128, I16, I32, I64, I8};
use crate::ir::{FuncRef, GlobalValue, Type};
use crate::machinst::*;

use regalloc::Map as RegallocMap;
use regalloc::{InstRegUses, Set};
use regalloc::{
    RealReg, RealRegUniverse, Reg, RegClass, RegClassInfo, SpillSlot, VirtualReg, Writable,
    NUM_REG_CLASSES,
};

use alloc::vec::Vec;
use smallvec::SmallVec;
use std::mem;
use std::string::{String, ToString};

pub mod regs;
pub use self::regs::*;
pub mod imms;
pub use self::imms::*;
pub mod args;
pub use self::args::*;
pub mod emit;
pub use self::emit::*;

//=============================================================================
// Instructions (top level): definition

/// An ALU operation. This can be paired with several instruction formats
/// below (see `Inst`) in any combination.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ALUOp {
    Add32,
    Add64,
    Sub32,
    Sub64,
    Orr32,
    Orr64,
    And32,
    And64,
    AddS32,
    AddS64,
    SubS32,
    SubS64,
    MAdd32, // multiply-add
    MAdd64,
}

/// Instruction formats.
#[derive(Clone, Debug)]
pub enum Inst {
    /// A no-op of zero size.
    Nop,

    /// A no-op that is one instruction large.
    Nop4,

    /// An ALU operation with two register sources and a register destination.
    AluRRR {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
    },
    /// An ALU operation with three register sources and a register destination.
    AluRRRR {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
        ra: Reg,
    },
    /// An ALU operation with a register source and an immediate-12 source, and a register
    /// destination.
    AluRRImm12 {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        imm12: Imm12,
    },
    /// An ALU operation with a register source and an immediate-logic source, and a register destination.
    AluRRImmLogic {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        imml: ImmLogic,
    },
    /// An ALU operation with a register source and an immediate-shiftamt source, and a register destination.
    AluRRImmShift {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        immshift: ImmShift,
    },
    /// An ALU operation with two register sources, one of which can be shifted, and a register
    /// destination.
    AluRRRShift {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
        shiftop: ShiftOpAndAmt,
    },
    /// An ALU operation with two register sources, one of which can be {zero,sign}-extended and
    /// shifted, and a register destination.
    AluRRRExtend {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
        extendop: ExtendOp,
    },
    /// An unsigned (zero-extending) 8-bit load.
    ULoad8 { rd: Writable<Reg>, mem: MemArg },
    /// A signed (sign-extending) 8-bit load.
    SLoad8 { rd: Writable<Reg>, mem: MemArg },
    /// An unsigned (zero-extending) 16-bit load.
    ULoad16 { rd: Writable<Reg>, mem: MemArg },
    /// A signed (sign-extending) 16-bit load.
    SLoad16 { rd: Writable<Reg>, mem: MemArg },
    /// An unsigned (zero-extending) 32-bit load.
    ULoad32 { rd: Writable<Reg>, mem: MemArg },
    /// A signed (sign-extending) 32-bit load.
    SLoad32 { rd: Writable<Reg>, mem: MemArg },
    /// A 64-bit load.
    ULoad64 { rd: Writable<Reg>, mem: MemArg },

    /// An 8-bit store.
    Store8 { rd: Reg, mem: MemArg },
    /// A 16-bit store.
    Store16 { rd: Reg, mem: MemArg },
    /// A 32-bit store.
    Store32 { rd: Reg, mem: MemArg },
    /// A 64-bit store.
    Store64 { rd: Reg, mem: MemArg },

    /// A store of a pair of registers.
    StoreP64 { rt: Reg, rt2: Reg, mem: PairMemArg },
    /// A load of a pair of registers.
    LoadP64 {
        rt: Writable<Reg>,
        rt2: Writable<Reg>,
        mem: PairMemArg,
    },

    /// A MOV instruction. These are encoded as ORR's (AluRRR form) but we
    /// keep them separate at the `Inst` level for better pretty-printing
    /// and faster `is_move()` logic.
    Mov { rd: Writable<Reg>, rm: Reg },

    /// A MOVZ with a 16-bit immediate.
    MovZ {
        rd: Writable<Reg>,
        imm: MoveWideConst,
    },

    /// A MOVN with a 16-bit immediate.
    MovN {
        rd: Writable<Reg>,
        imm: MoveWideConst,
    },

    /// A machine call instruction.
    Call { dest: FuncRef },
    /// A machine indirect-call instruction.
    CallInd { rn: Reg },

    // ---- branches (exactly one must appear at end of BB) ----
    /// A machine return instruction.
    Ret {},
    /// An unconditional branch.
    Jump { dest: BranchTarget },

    /// A conditional branch.
    CondBr {
        taken: BranchTarget,
        not_taken: BranchTarget,
        kind: CondBrKind,
    },

    /// Lowered conditional branch: contains the original instruction, and a
    /// flag indicating whether to invert the taken-condition or not. Only one
    /// BranchTarget is retained, and the other is implicitly the next
    /// instruction, given the final basic-block layout.
    CondBrLowered {
        target: BranchTarget,
        inverted: bool,
        kind: CondBrKind,
    },

    /// As for `CondBrLowered`, but represents a condbr/uncond-br sequence (two
    /// actual machine instructions). Needed when the final block layout implies
    /// that both arms of a conditional branch are not the fallthrough block.
    CondBrLoweredCompound {
        taken: BranchTarget,
        not_taken: BranchTarget,
        kind: CondBrKind,
    },
}

impl Inst {
    /// Create a move instruction.
    pub fn mov(to_reg: Writable<Reg>, from_reg: Reg) -> Inst {
        Inst::Mov {
            rd: to_reg,
            rm: from_reg,
        }
    }
}

//=============================================================================
// Instructions: get_regs

fn memarg_regs(memarg: &MemArg, used: &mut Set<Reg>, modified: &mut Set<Writable<Reg>>) {
    match memarg {
        &MemArg::Base(reg) | &MemArg::BaseSImm9(reg, ..) | &MemArg::BaseUImm12Scaled(reg, ..) => {
            used.insert(reg);
        }
        &MemArg::BasePlusReg(r1, r2) | &MemArg::BasePlusRegScaled(r1, r2, ..) => {
            used.insert(r1);
            used.insert(r2);
        }
        &MemArg::Label(..) => {}
        &MemArg::PreIndexed(reg, ..) | &MemArg::PostIndexed(reg, ..) => {
            modified.insert(reg);
        }
        &MemArg::StackOffset(..) => {
            used.insert(fp_reg());
        }
    }
}

fn pairmemarg_regs(
    pairmemarg: &PairMemArg,
    used: &mut Set<Reg>,
    modified: &mut Set<Writable<Reg>>,
) {
    match pairmemarg {
        &PairMemArg::SignedOffset(reg, ..) => {
            used.insert(reg);
        }
        &PairMemArg::PreIndexed(reg, ..) | &PairMemArg::PostIndexed(reg, ..) => {
            modified.insert(reg);
        }
    }
}

fn arm64_get_regs(inst: &Inst) -> InstRegUses {
    let mut iru = InstRegUses::new();

    match inst {
        &Inst::AluRRR { rd, rn, rm, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
            iru.used.insert(rm);
        }
        &Inst::AluRRRR { rd, rn, rm, ra, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
            iru.used.insert(rm);
            iru.used.insert(ra);
        }
        &Inst::AluRRImm12 { rd, rn, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
        }
        &Inst::AluRRImmLogic { rd, rn, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
        }
        &Inst::AluRRImmShift { rd, rn, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
        }
        &Inst::AluRRRShift { rd, rn, rm, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
            iru.used.insert(rm);
        }
        &Inst::AluRRRExtend { rd, rn, rm, .. } => {
            iru.defined.insert(rd);
            iru.used.insert(rn);
            iru.used.insert(rm);
        }
        &Inst::ULoad8 { rd, ref mem, .. }
        | &Inst::SLoad8 { rd, ref mem, .. }
        | &Inst::ULoad16 { rd, ref mem, .. }
        | &Inst::SLoad16 { rd, ref mem, .. }
        | &Inst::ULoad32 { rd, ref mem, .. }
        | &Inst::SLoad32 { rd, ref mem, .. }
        | &Inst::ULoad64 { rd, ref mem, .. } => {
            iru.defined.insert(rd);
            memarg_regs(mem, &mut iru.used, &mut iru.modified);
        }
        &Inst::Store8 { rd, ref mem, .. }
        | &Inst::Store16 { rd, ref mem, .. }
        | &Inst::Store32 { rd, ref mem, .. }
        | &Inst::Store64 { rd, ref mem, .. } => {
            iru.used.insert(rd);
            memarg_regs(mem, &mut iru.used, &mut iru.modified);
        }
        &Inst::StoreP64 {
            rt, rt2, ref mem, ..
        } => {
            iru.used.insert(rt);
            iru.used.insert(rt2);
            pairmemarg_regs(mem, &mut iru.used, &mut iru.modified);
        }
        &Inst::LoadP64 {
            rt, rt2, ref mem, ..
        } => {
            iru.defined.insert(rt);
            iru.defined.insert(rt2);
            pairmemarg_regs(mem, &mut iru.used, &mut iru.modified);
        }
        &Inst::Mov { rd, rm } => {
            iru.defined.insert(rd);
            iru.used.insert(rm);
        }
        &Inst::MovZ { rd, .. } | &Inst::MovN { rd, .. } => {
            iru.defined.insert(rd);
        }
        &Inst::Jump { .. } | &Inst::Call { .. } | &Inst::Ret { .. } => {}
        &Inst::CallInd { rn, .. } => {
            iru.used.insert(rn);
        }
        &Inst::CondBr { ref kind, .. }
        | &Inst::CondBrLowered { ref kind, .. }
        | &Inst::CondBrLoweredCompound { ref kind, .. } => match kind {
            CondBrKind::Zero(rt) | CondBrKind::NotZero(rt) => {
                iru.used.insert(*rt);
            }
            CondBrKind::Cond(_) => {}
        },
        &Inst::Nop | Inst::Nop4 => {}
    }

    // Enforce the invariant that if a register is in the 'modify' set, it
    // should not be in 'defined' or 'used'.
    iru.defined.remove(&iru.modified);
    iru.used.remove(&Set::from_vec(
        iru.modified.iter().map(|r| r.to_reg()).collect(),
    ));

    iru
}

//=============================================================================
// Instructions: map_regs

fn arm64_map_regs(
    inst: &mut Inst,
    pre_map: &RegallocMap<VirtualReg, RealReg>,
    post_map: &RegallocMap<VirtualReg, RealReg>,
) {
    fn map(m: &RegallocMap<VirtualReg, RealReg>, r: Reg) -> Reg {
        if r.is_virtual() {
            m.get(&r.to_virtual_reg()).cloned().unwrap().to_reg()
        } else {
            r
        }
    }

    fn map_wr(m: &RegallocMap<VirtualReg, RealReg>, r: Writable<Reg>) -> Writable<Reg> {
        Writable::from_reg(map(m, r.to_reg()))
    }

    fn map_mem(u: &RegallocMap<VirtualReg, RealReg>, mem: &MemArg) -> MemArg {
        // N.B.: we take only the pre-map here, but this is OK because the
        // only addressing modes that update registers (pre/post-increment on
        // ARM64) both read and write registers, so they are "mods" rather
        // than "defs", so must be the same in both the pre- and post-map.
        match mem {
            &MemArg::Base(reg) => MemArg::Base(map(u, reg)),
            &MemArg::BaseSImm9(reg, simm9) => MemArg::BaseSImm9(map(u, reg), simm9),
            &MemArg::BaseUImm12Scaled(reg, uimm12) => MemArg::BaseUImm12Scaled(map(u, reg), uimm12),
            &MemArg::BasePlusReg(r1, r2) => MemArg::BasePlusReg(map(u, r1), map(u, r2)),
            &MemArg::BasePlusRegScaled(r1, r2, ty) => {
                MemArg::BasePlusRegScaled(map(u, r1), map(u, r2), ty)
            }
            &MemArg::Label(ref l) => MemArg::Label(l.clone()),
            &MemArg::PreIndexed(r, simm9) => MemArg::PreIndexed(map_wr(u, r), simm9),
            &MemArg::PostIndexed(r, simm9) => MemArg::PostIndexed(map_wr(u, r), simm9),
            &MemArg::StackOffset(off) => MemArg::StackOffset(off),
        }
    }

    fn map_pairmem(u: &RegallocMap<VirtualReg, RealReg>, mem: &PairMemArg) -> PairMemArg {
        match mem {
            &PairMemArg::SignedOffset(reg, simm7) => PairMemArg::SignedOffset(map(u, reg), simm7),
            &PairMemArg::PreIndexed(reg, simm7) => PairMemArg::PreIndexed(map_wr(u, reg), simm7),
            &PairMemArg::PostIndexed(reg, simm7) => PairMemArg::PostIndexed(map_wr(u, reg), simm7),
        }
    }

    fn map_br(u: &RegallocMap<VirtualReg, RealReg>, br: &CondBrKind) -> CondBrKind {
        match br {
            &CondBrKind::Zero(reg) => CondBrKind::Zero(map(u, reg)),
            &CondBrKind::NotZero(reg) => CondBrKind::NotZero(map(u, reg)),
            &CondBrKind::Cond(c) => CondBrKind::Cond(c),
        }
    }

    let u = pre_map; // For brevity below.
    let d = post_map;

    let newval = match inst {
        &mut Inst::AluRRR { alu_op, rd, rn, rm } => Inst::AluRRR {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            rm: map(u, rm),
        },
        &mut Inst::AluRRRR {
            alu_op,
            rd,
            rn,
            rm,
            ra,
        } => Inst::AluRRRR {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            rm: map(u, rm),
            ra: map(u, ra),
        },
        &mut Inst::AluRRImm12 {
            alu_op,
            rd,
            rn,
            ref imm12,
        } => Inst::AluRRImm12 {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            imm12: imm12.clone(),
        },
        &mut Inst::AluRRImmLogic {
            alu_op,
            rd,
            rn,
            ref imml,
        } => Inst::AluRRImmLogic {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            imml: imml.clone(),
        },
        &mut Inst::AluRRImmShift {
            alu_op,
            rd,
            rn,
            ref immshift,
        } => Inst::AluRRImmShift {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            immshift: immshift.clone(),
        },
        &mut Inst::AluRRRShift {
            alu_op,
            rd,
            rn,
            rm,
            ref shiftop,
        } => Inst::AluRRRShift {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            rm: map(u, rm),
            shiftop: shiftop.clone(),
        },
        &mut Inst::AluRRRExtend {
            alu_op,
            rd,
            rn,
            rm,
            ref extendop,
        } => Inst::AluRRRExtend {
            alu_op,
            rd: map_wr(d, rd),
            rn: map(u, rn),
            rm: map(u, rm),
            extendop: extendop.clone(),
        },
        &mut Inst::ULoad8 { rd, ref mem } => Inst::ULoad8 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::SLoad8 { rd, ref mem } => Inst::SLoad8 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::ULoad16 { rd, ref mem } => Inst::ULoad16 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::SLoad16 { rd, ref mem } => Inst::SLoad16 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::ULoad32 { rd, ref mem } => Inst::ULoad32 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::SLoad32 { rd, ref mem } => Inst::SLoad32 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::ULoad64 { rd, ref mem } => Inst::ULoad64 {
            rd: map_wr(d, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::Store8 { rd, ref mem } => Inst::Store8 {
            rd: map(u, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::Store16 { rd, ref mem } => Inst::Store16 {
            rd: map(u, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::Store32 { rd, ref mem } => Inst::Store32 {
            rd: map(u, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::Store64 { rd, ref mem } => Inst::Store64 {
            rd: map(u, rd),
            mem: map_mem(u, mem),
        },
        &mut Inst::StoreP64 { rt, rt2, ref mem } => Inst::StoreP64 {
            rt: map(u, rt),
            rt2: map(u, rt2),
            mem: map_pairmem(u, mem),
        },
        &mut Inst::LoadP64 { rt, rt2, ref mem } => Inst::LoadP64 {
            rt: map_wr(d, rt),
            rt2: map_wr(d, rt2),
            mem: map_pairmem(u, mem),
        },
        &mut Inst::Mov { rd, rm } => Inst::Mov {
            rd: map_wr(d, rd),
            rm: map(u, rm),
        },
        &mut Inst::MovZ { rd, ref imm } => Inst::MovZ {
            rd: map_wr(d, rd),
            imm: imm.clone(),
        },
        &mut Inst::MovN { rd, ref imm } => Inst::MovN {
            rd: map_wr(d, rd),
            imm: imm.clone(),
        },
        &mut Inst::Jump { dest } => Inst::Jump { dest },
        &mut Inst::Call { dest } => Inst::Call { dest },
        &mut Inst::Ret {} => Inst::Ret {},
        &mut Inst::CallInd { rn } => Inst::CallInd { rn: map(u, rn) },
        &mut Inst::CondBr {
            taken,
            not_taken,
            kind,
        } => Inst::CondBr {
            taken,
            not_taken,
            kind: map_br(u, &kind),
        },
        &mut Inst::CondBrLowered {
            target,
            inverted,
            kind,
        } => Inst::CondBrLowered {
            target,
            inverted,
            kind: map_br(u, &kind),
        },
        &mut Inst::CondBrLoweredCompound {
            taken,
            not_taken,
            kind,
        } => Inst::CondBrLoweredCompound {
            taken,
            not_taken,
            kind: map_br(u, &kind),
        },
        &mut Inst::Nop => Inst::Nop,
        &mut Inst::Nop4 => Inst::Nop4,
    };
    *inst = newval;
}

//=============================================================================
// Instructions: misc functions and external interface

impl MachInst for Inst {
    fn get_regs(&self) -> InstRegUses {
        arm64_get_regs(self)
    }

    fn map_regs(
        &mut self,
        pre_map: &RegallocMap<VirtualReg, RealReg>,
        post_map: &RegallocMap<VirtualReg, RealReg>,
    ) {
        arm64_map_regs(self, pre_map, post_map);
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            &Inst::Mov { rd, rm } => Some((rd, rm)),
            _ => None,
        }
    }

    fn is_term(&self) -> MachTerminator {
        match self {
            &Inst::Ret {} => MachTerminator::Ret,
            &Inst::Jump { dest } => MachTerminator::Uncond(dest.as_block_index().unwrap()),
            &Inst::CondBr {
                taken, not_taken, ..
            } => MachTerminator::Cond(
                taken.as_block_index().unwrap(),
                not_taken.as_block_index().unwrap(),
            ),
            &Inst::CondBrLowered { .. } | &Inst::CondBrLoweredCompound { .. } => {
                panic!("is_term() called after lowering branches");
            }
            _ => MachTerminator::None,
        }
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg) -> Inst {
        Inst::mov(to_reg, from_reg)
    }

    fn gen_nop(preferred_size: usize) -> Inst {
        // We can't give a NOP (or any insn) < 4 bytes.
        assert!(preferred_size >= 4);
        Inst::Nop4
    }

    fn maybe_direct_reload(&self, _reg: VirtualReg, _slot: SpillSlot) -> Option<Inst> {
        None
    }

    fn rc_for_type(ty: Type) -> RegClass {
        match ty {
            I8 | I16 | I32 | I64 | B1 | B8 | B16 | B32 | B64 => RegClass::I64,
            F32 | F64 => RegClass::V128,
            I128 | B128 => RegClass::V128,
            _ => panic!("Unexpected SSA-value type!"),
        }
    }

    fn gen_jump(blockindex: BlockIndex) -> Inst {
        Inst::Jump {
            dest: BranchTarget::Block(blockindex),
        }
    }

    fn with_block_rewrites(&mut self, block_target_map: &[BlockIndex]) {
        match self {
            &mut Inst::Jump { ref mut dest } => {
                dest.map(block_target_map);
            }
            &mut Inst::CondBr {
                ref mut taken,
                ref mut not_taken,
                ..
            } => {
                taken.map(block_target_map);
                not_taken.map(block_target_map);
            }
            &mut Inst::CondBrLowered { .. } | &mut Inst::CondBrLoweredCompound { .. } => {
                panic!("with_block_rewrites called after branch lowering!");
            }
            _ => {}
        }
    }

    fn with_fallthrough_block(&mut self, fallthrough: Option<BlockIndex>) {
        match self {
            &mut Inst::CondBr {
                taken,
                not_taken,
                kind,
            } => {
                if taken.as_block_index() == fallthrough {
                    *self = Inst::CondBrLowered {
                        target: not_taken,
                        inverted: true,
                        kind,
                    };
                } else if not_taken.as_block_index() == fallthrough {
                    *self = Inst::CondBrLowered {
                        target: taken,
                        inverted: false,
                        kind,
                    };
                } else {
                    // We need a compound sequence (condbr / uncond-br).
                    *self = Inst::CondBrLoweredCompound {
                        taken,
                        not_taken,
                        kind,
                    };
                }
            }
            &mut Inst::Jump { dest } => {
                if dest.as_block_index() == fallthrough {
                    *self = Inst::Nop;
                }
            }
            _ => {}
        }
    }

    fn with_block_offsets(&mut self, my_offset: CodeOffset, targets: &[CodeOffset]) {
        match self {
            &mut Inst::CondBrLowered { ref mut target, .. } => {
                target.lower(targets, my_offset);
            }
            &mut Inst::CondBrLoweredCompound {
                ref mut taken,
                ref mut not_taken,
                ..
            } => {
                taken.lower(targets, my_offset);
                not_taken.lower(targets, my_offset);
            }
            &mut Inst::Jump { ref mut dest } => {
                dest.lower(targets, my_offset);
            }
            _ => {}
        }
    }

    fn reg_universe() -> RealRegUniverse {
        create_reg_universe()
    }
}

//=============================================================================
// Pretty-printing of instructions.

fn mem_finalize_for_show<CPS: ConstantPoolSink>(
    mem: &MemArg,
    mb_rru: Option<&RealRegUniverse>,
    consts: &mut CPS,
) -> (String, MemArg) {
    let (mem_insts, mem) = mem_finalize(0, mem, consts);
    let mut mem_str = mem_insts
        .into_iter()
        .map(|inst| inst.show_rru(mb_rru))
        .collect::<Vec<_>>()
        .join(" ; ");
    if !mem_str.is_empty() {
        mem_str += " ; ";
    }

    (mem_str, mem)
}

impl ShowWithRRU for Inst {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        let mut nullcps = NullConstantPoolSink {};
        self.show_rru_with_constsink(mb_rru, &mut nullcps)
    }
}

impl Inst {
    /// Show the instruction, also providing constants to a constant sink.
    pub fn show_rru_with_constsink<CPS: ConstantPoolSink>(
        &self,
        mb_rru: Option<&RealRegUniverse>,
        consts: &mut CPS,
    ) -> String {
        fn op_is32(alu_op: ALUOp) -> (&'static str, bool) {
            match alu_op {
                ALUOp::Add32 => ("add", true),
                ALUOp::Add64 => ("add", false),
                ALUOp::Sub32 => ("sub", true),
                ALUOp::Sub64 => ("sub", false),
                ALUOp::Orr32 => ("orr", true),
                ALUOp::Orr64 => ("orr", false),
                ALUOp::And32 => ("and", true),
                ALUOp::And64 => ("and", false),
                ALUOp::AddS32 => ("adds", true),
                ALUOp::AddS64 => ("adds", false),
                ALUOp::SubS32 => ("subs", true),
                ALUOp::SubS64 => ("subs", false),
                ALUOp::MAdd32 => ("madd", true),
                ALUOp::MAdd64 => ("madd", false),
            }
        }

        match self {
            &Inst::Nop => "".to_string(),
            &Inst::Nop4 => "nop".to_string(),
            &Inst::AluRRR { alu_op, rd, rn, rm } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let rm = show_ireg_sized(rm, mb_rru, is32);
                format!("{} {}, {}, {}", op, rd, rn, rm)
            }
            &Inst::AluRRRR {
                alu_op,
                rd,
                rn,
                rm,
                ra,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let rm = show_ireg_sized(rm, mb_rru, is32);
                let ra = show_ireg_sized(ra, mb_rru, is32);
                format!("{} {}, {}, {}, {}", op, rd, rn, rm, ra)
            }
            &Inst::AluRRImm12 {
                alu_op,
                rd,
                rn,
                ref imm12,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);

                if imm12.bits == 0 && alu_op == ALUOp::Add64 {
                    // special-case MOV (used for moving into SP).
                    format!("mov {}, {}", rd, rn)
                } else {
                    let imm12 = imm12.show_rru(mb_rru);
                    format!("{} {}, {}, {}", op, rd, rn, imm12)
                }
            }
            &Inst::AluRRImmLogic {
                alu_op,
                rd,
                rn,
                ref imml,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let imml = imml.show_rru(mb_rru);
                format!("{} {}, {}, {}", op, rd, rn, imml)
            }
            &Inst::AluRRImmShift {
                alu_op,
                rd,
                rn,
                ref immshift,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let immshift = immshift.show_rru(mb_rru);
                format!("{} {}, {}, {}", op, rd, rn, immshift)
            }
            &Inst::AluRRRShift {
                alu_op,
                rd,
                rn,
                rm,
                ref shiftop,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let rm = show_ireg_sized(rm, mb_rru, is32);
                let shiftop = shiftop.show_rru(mb_rru);
                format!("{} {}, {}, {}, {}", op, rd, rn, rm, shiftop)
            }
            &Inst::AluRRRExtend {
                alu_op,
                rd,
                rn,
                rm,
                ref extendop,
            } => {
                let (op, is32) = op_is32(alu_op);
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let rn = show_ireg_sized(rn, mb_rru, is32);
                let rm = show_ireg_sized(rm, mb_rru, is32);
                let extendop = extendop.show_rru(mb_rru);
                format!("{} {}, {}, {}, {}", op, rd, rn, rm, extendop)
            }
            &Inst::ULoad8 { rd, ref mem }
            | &Inst::SLoad8 { rd, ref mem }
            | &Inst::ULoad16 { rd, ref mem }
            | &Inst::SLoad16 { rd, ref mem }
            | &Inst::ULoad32 { rd, ref mem }
            | &Inst::SLoad32 { rd, ref mem }
            | &Inst::ULoad64 { rd, ref mem } => {
                let (mem_str, mem) = mem_finalize_for_show(mem, mb_rru, consts);

                let is_unscaled_base = match &mem {
                    &MemArg::Base(..) | &MemArg::BaseSImm9(..) => true,
                    _ => false,
                };
                let (op, is32) = match (self, is_unscaled_base) {
                    (&Inst::ULoad8 { .. }, false) => ("ldrb", true),
                    (&Inst::ULoad8 { .. }, true) => ("ldurb", true),
                    (&Inst::SLoad8 { .. }, false) => ("ldrsb", false),
                    (&Inst::SLoad8 { .. }, true) => ("ldursb", false),
                    (&Inst::ULoad16 { .. }, false) => ("ldrh", true),
                    (&Inst::ULoad16 { .. }, true) => ("ldurh", true),
                    (&Inst::SLoad16 { .. }, false) => ("ldrsh", false),
                    (&Inst::SLoad16 { .. }, true) => ("ldursh", false),
                    (&Inst::ULoad32 { .. }, false) => ("ldr", true),
                    (&Inst::ULoad32 { .. }, true) => ("ldur", true),
                    (&Inst::SLoad32 { .. }, false) => ("ldrsw", false),
                    (&Inst::SLoad32 { .. }, true) => ("ldursw", false),
                    (&Inst::ULoad64 { .. }, false) => ("ldr", false),
                    (&Inst::ULoad64 { .. }, true) => ("ldur", false),
                    _ => unreachable!(),
                };
                let rd = show_ireg_sized(rd.to_reg(), mb_rru, is32);
                let mem = mem.show_rru(mb_rru);
                format!("{}{} {}, {}", mem_str, op, rd, mem)
            }
            &Inst::Store8 { rd, ref mem }
            | &Inst::Store16 { rd, ref mem }
            | &Inst::Store32 { rd, ref mem }
            | &Inst::Store64 { rd, ref mem } => {
                let (mem_str, mem) = mem_finalize_for_show(mem, mb_rru, consts);

                let is_unscaled_base = match &mem {
                    &MemArg::Base(..) | &MemArg::BaseSImm9(..) => true,
                    _ => false,
                };
                let (op, is32) = match (self, is_unscaled_base) {
                    (&Inst::Store8 { .. }, false) => ("strb", true),
                    (&Inst::Store8 { .. }, true) => ("sturb", true),
                    (&Inst::Store16 { .. }, false) => ("strh", true),
                    (&Inst::Store16 { .. }, true) => ("sturh", true),
                    (&Inst::Store32 { .. }, false) => ("str", true),
                    (&Inst::Store32 { .. }, true) => ("stur", true),
                    (&Inst::Store64 { .. }, false) => ("str", false),
                    (&Inst::Store64 { .. }, true) => ("stur", false),
                    _ => unreachable!(),
                };
                let rd = show_ireg_sized(rd, mb_rru, is32);
                let mem = mem.show_rru(mb_rru);
                format!("{}{} {}, {}", mem_str, op, rd, mem)
            }
            &Inst::StoreP64 { rt, rt2, ref mem } => {
                let rt = rt.show_rru(mb_rru);
                let rt2 = rt2.show_rru(mb_rru);
                let mem = mem.show_rru_sized(mb_rru, /* size = */ 8);
                format!("stp {}, {}, {}", rt, rt2, mem)
            }
            &Inst::LoadP64 { rt, rt2, ref mem } => {
                let rt = rt.to_reg().show_rru(mb_rru);
                let rt2 = rt2.to_reg().show_rru(mb_rru);
                let mem = mem.show_rru_sized(mb_rru, /* size = */ 8);
                format!("ldp {}, {}, {}", rt, rt2, mem)
            }
            &Inst::Mov { rd, rm } => {
                let rd = rd.to_reg().show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("mov {}, {}", rd, rm)
            }
            &Inst::MovZ { rd, ref imm } => {
                let rd = rd.to_reg().show_rru(mb_rru);
                let imm = imm.show_rru(mb_rru);
                format!("movz {}, {}", rd, imm)
            }
            &Inst::MovN { rd, ref imm } => {
                let rd = rd.to_reg().show_rru(mb_rru);
                let imm = imm.show_rru(mb_rru);
                format!("movn {}, {}", rd, imm)
            }
            &Inst::Call { dest: _ } => {
                let dest = "!!".to_string(); // TODO
                format!("bl {}", dest)
            }
            &Inst::CallInd { rn } => {
                let rn = rn.show_rru(mb_rru);
                format!("bl {}", rn)
            }
            &Inst::Ret {} => "ret".to_string(),
            &Inst::Jump { ref dest } => {
                let dest = dest.show_rru(mb_rru);
                format!("b {}", dest)
            }
            &Inst::CondBr {
                ref taken,
                ref not_taken,
                ref kind,
            } => {
                let taken = taken.show_rru(mb_rru);
                let not_taken = not_taken.show_rru(mb_rru);
                match kind {
                    &CondBrKind::Zero(reg) => {
                        let reg = reg.show_rru(mb_rru);
                        format!("cbz {}, {} ; b {}", reg, taken, not_taken)
                    }
                    &CondBrKind::NotZero(reg) => {
                        let reg = reg.show_rru(mb_rru);
                        format!("cbnz {}, {} ; b {}", reg, taken, not_taken)
                    }
                    &CondBrKind::Cond(c) => {
                        let c = c.show_rru(mb_rru);
                        format!("b.{} {} ; b {}", c, taken, not_taken)
                    }
                }
            }
            &Inst::CondBrLowered {
                ref target,
                inverted,
                ref kind,
            } => {
                let target = target.show_rru(mb_rru);
                let kind = if inverted {
                    kind.invert()
                } else {
                    kind.clone()
                };
                match &kind {
                    &CondBrKind::Zero(reg) => {
                        let reg = reg.show_rru(mb_rru);
                        format!("cbz {}, {}", reg, target)
                    }
                    &CondBrKind::NotZero(reg) => {
                        let reg = reg.show_rru(mb_rru);
                        format!("cbnz {}, {}", reg, target)
                    }
                    &CondBrKind::Cond(c) => {
                        let c = c.show_rru(mb_rru);
                        format!("b.{} {}", c, target)
                    }
                }
            }
            &Inst::CondBrLoweredCompound {
                ref taken,
                ref not_taken,
                ref kind,
            } => {
                let first = Inst::CondBrLowered {
                    target: taken.clone(),
                    inverted: false,
                    kind: kind.clone(),
                };
                let second = Inst::Jump {
                    dest: not_taken.clone(),
                };
                first.show_rru(mb_rru) + " ; " + &second.show_rru(mb_rru)
            }
        }
    }
}