//! Constant buffer reads shifted by a row the host keeps per view
//! (`Options::cbv_row_shift`, contract 3): a synthetic GFX10 program that reads
//! a constant buffer from a table, lowered with the option off and on, and
//! validated by `dxil.dll` when present.
//!
//! shortcut: root CBVs (a descriptor the program builds around a root
//! address, shifted by the `USER_CBV_SHIFT` user-data word) are not covered
//! here; the host's check mode (`VICTORY_CBV_SHIFT_CHECK`, contract 3) covers
//! both kinds on the card.

use super::shader::SLOT_CBV_SHIFTS;
use super::{lower, validator};
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};
use crate::{io, ir, resource, structure};

const NULL: u8 = 125;

/// SMEM `op s[sdata..], s[sbase..], offset`.
const fn smem(op: u32, sdata: u8, sbase: u8, offset: u32) -> [u32; 2] {
    [0x3D << 26 | op << 18 | (sdata as u32) << 6 | (sbase as u32 >> 1), (NULL as u32) << 25 | (offset & 0x1F_FFFF)]
}

/// A one-thread wave32 program: the table address in s4, a constant buffer
/// of kind `kind` at table entry 0, a raw UAV
/// at entry 1. It reads four dwords of row 2 of the constant buffer and
/// stores them to the UAV.
fn program(kind: ResourceType) -> NativeProgram {
    let mut words = vec![0xBE85_0380]; // s_mov_b32 s5, 0
    words.extend(smem(0x02, 8, 4, 0)); // s_load_dwordx4 s[8:11], s[4:5], 0
    words.extend(smem(0x02, 12, 4, 32)); // s_load_dwordx4 s[12:15], s[4:5], 32
    words.extend(smem(0x0A, 16, 8, 32)); // s_buffer_load_dwordx4 s[16:19], s[8:11], 32
    for k in 0..4 {
        words.push(0x7E00_0210 + k); // v_mov_b32 v0, s(16 + k)
        words.extend([0xE070_0000 | (4 * k), 0x8003_0000]); // buffer_store_dword v0, off, s[12:15], 0 offset:4k
    }
    words.push(0xBF81_0000); // s_endpgm
    let param = |root, sgprs, user_data, slot| RootParam { root, table_offset_dwords: 0, sgprs, base_register: 0, user_data, slot };
    let view = |kind, root, table_offset_dwords, user_data, slot| UsedResource {
        kind,
        root,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data,
        slot,
    };
    let layout = vec![param(RootType::Constants, 4, 0, 0), param(RootType::Table, 1, 4, 1)];
    let used = vec![view(kind, RootType::Table, 0, 4, 1), view(ResourceType::Uav, RootType::Table, 8, 4, 1)];
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout,
        used,
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [1, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered with the shift off or on, validated when `dxil.dll` is
/// there.
fn lowered(native: &NativeProgram, shift: bool) -> lower::Lowered {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    let l = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
        cbv_row_shift: shift,
    })
    .expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut l.dxil.clone()).expect("DXIL validation failed");
    }
    l
}

#[test]
fn the_shift_changes_nothing_without_a_constant_buffer() {
    let native = program(ResourceType::Srv);
    let (off, on) = (lowered(&native, false), lowered(&native, true));
    assert_eq!(off.dxil, on.dxil);
    assert!(!on.root_srvs.contains(&SLOT_CBV_SHIFTS));
}

#[test]
fn a_table_constant_buffer_reads_its_shift_from_the_side_table() {
    let native = program(ResourceType::Cbv);
    let (off, on) = (lowered(&native, false), lowered(&native, true));
    assert!(!off.root_srvs.contains(&SLOT_CBV_SHIFTS), "off: no side table");
    assert!(on.root_srvs.contains(&SLOT_CBV_SHIFTS), "on: the side table of shifts is read");
    assert_ne!(off.dxil, on.dxil, "on: the row is shifted");
}
