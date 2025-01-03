//! Contains x86_64-specific code to perform various relocation relaxation optimisations. These are
//! supposed to be optional for the linker to do, but it turns out that libc in some cases won't
//! work unless they're performed. e.g. it uses GOT relocations in _start, which cannot work in a
//! static-PIE binary because dynamic relocations haven't yet been applied to the GOT yet.

use crate::arch::Arch;
use crate::args::OutputKind;
use crate::elf::DynamicRelocationKind;
use crate::elf::RelocationKindInfo;
use crate::elf::RelocationSize;
use crate::elf::PLT_ENTRY_SIZE;
use crate::resolution::ValueFlags;
use anyhow::anyhow;
use anyhow::Result;
use linker_utils::elf::shf;
use linker_utils::elf::x86_64_rel_type_to_string;
use linker_utils::elf::SectionFlags;
use linker_utils::relaxation::RelocationModifier;
use linker_utils::x86_64::RelaxationKind;

pub(crate) struct X86_64;

const PLT_ENTRY_TEMPLATE: &[u8] = &[
    0xf3, 0x0f, 0x1e, 0xfa, // endbr64
    0xf2, 0xff, 0x25, 0x0, 0x0, 0x0, 0x0, // bnd jmp *{relative GOT address}(%rip)
    0x0f, 0x1f, 0x44, 0x0, 0x0, // nopl   0x0(%rax,%rax,1)
];

const _ASSERTS: () = {
    assert!(PLT_ENTRY_TEMPLATE.len() as u64 == PLT_ENTRY_SIZE);
};

impl crate::arch::Arch for X86_64 {
    type Relaxation = Relaxation;

    fn elf_header_arch_magic() -> u16 {
        object::elf::EM_X86_64
    }

    fn relocation_from_raw(r_type: u32) -> Result<RelocationKindInfo> {
        let (kind, size) =
            linker_utils::x86_64::relocation_kind_and_size(r_type).ok_or_else(|| {
                anyhow!(
                    "Unsupported relocation type {}",
                    Self::rel_type_to_string(r_type)
                )
            })?;
        Ok(RelocationKindInfo {
            kind,
            size: RelocationSize::ByteSize(size),
            mask: None,
        })
    }

    fn get_dynamic_relocation_type(relocation: DynamicRelocationKind) -> u32 {
        match relocation {
            DynamicRelocationKind::Copy => object::elf::R_X86_64_COPY,
            DynamicRelocationKind::Irelative => object::elf::R_X86_64_IRELATIVE,
            DynamicRelocationKind::DtpMod => object::elf::R_X86_64_DTPMOD64,
            DynamicRelocationKind::DtpOff => object::elf::R_X86_64_DTPOFF64,
            DynamicRelocationKind::TpOff => object::elf::R_X86_64_TPOFF64,
            DynamicRelocationKind::Relative => object::elf::R_X86_64_RELATIVE,
            DynamicRelocationKind::DynamicSymbol => object::elf::R_X86_64_GLOB_DAT,
        }
    }

    fn write_plt_entry(
        plt_entry: &mut [u8],
        got_address: u64,
        plt_address: u64,
    ) -> crate::error::Result {
        plt_entry.copy_from_slice(PLT_ENTRY_TEMPLATE);
        let offset: i32 = ((got_address.wrapping_sub(plt_address + 0xb)) as i64)
            .try_into()
            .map_err(|_| anyhow!("PLT is more than 2GiB away from GOT"))?;
        plt_entry[7..11].copy_from_slice(&offset.to_le_bytes());
        Ok(())
    }

    fn rel_type_to_string(r_type: u32) -> std::borrow::Cow<'static, str> {
        x86_64_rel_type_to_string(r_type)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Relaxation {
    kind: RelaxationKind,
    rel_info: RelocationKindInfo,
}

impl crate::arch::Relaxation for Relaxation {
    fn new(
        relocation_kind: u32,
        section_bytes: &[u8],
        offset_in_section: u64,
        value_flags: ValueFlags,
        output_kind: OutputKind,
        section_flags: SectionFlags,
    ) -> Option<Self> {
        // TODO: Consider removing Option. There are a few callers though, so need to see how this
        // looks.
        #[allow(clippy::unnecessary_wraps)]
        fn create(kind: RelaxationKind, new_r_type: u32) -> Option<Relaxation> {
            // This only fails for relocation types that we don't support and if we relax to a type
            // we don't support, then that's a bug.
            let rel_info = X86_64::relocation_from_raw(new_r_type).unwrap();
            Some(Relaxation { kind, rel_info })
        }

        let is_known_address = value_flags.contains(ValueFlags::ADDRESS);
        let is_absolute = value_flags.contains(ValueFlags::ABSOLUTE)
            && !value_flags.contains(ValueFlags::DYNAMIC);
        let non_relocatable = !output_kind.is_relocatable();
        let is_absolute_address = is_known_address && non_relocatable;
        let can_bypass_got = value_flags.contains(ValueFlags::CAN_BYPASS_GOT);

        // IFuncs cannot be referenced directly. The always need to go via the GOT. So if we've got
        // say a PLT32 relocation, we don't want to relax it even if we're in a static executable.
        // Furthermore, if we encounter a relocation like PC32 to an ifunc, then we need to change
        // it so that it goes via the GOT. This is kind of the opposite of relaxation.
        if value_flags.contains(ValueFlags::IFUNC) {
            return match relocation_kind {
                object::elf::R_X86_64_PC32 => {
                    return create(RelaxationKind::NoOp, object::elf::R_X86_64_PLT32);
                }
                _ => None,
            };
        }

        // All relaxations below only apply to executable code, so we shouldn't attempt them if a
        // relocation is in a non-executable section.
        if !section_flags.contains(shf::EXECINSTR) {
            return None;
        }

        let offset = offset_in_section as usize;
        // TODO: Try fetching the symbol kind lazily. For most relocation, we don't need it, but
        // because fetching it contains potential error paths, the optimiser probably can't optimise
        // away fetching it.
        match relocation_kind {
            object::elf::R_X86_64_REX_GOTPCRELX => {
                if offset < 3 {
                    return None;
                }
                let b1 = section_bytes[offset - 2];
                let rex = section_bytes[offset - 3];
                if rex != 0x48 && rex != 0x4c {
                    return None;
                }
                if is_absolute || is_absolute_address {
                    match b1 {
                        0x8b => {
                            return create(
                                RelaxationKind::RexMovIndirectToAbsolute,
                                object::elf::R_X86_64_32,
                            );
                        }
                        0x2b => {
                            return create(
                                RelaxationKind::RexSubIndirectToAbsolute,
                                object::elf::R_X86_64_32,
                            );
                        }
                        0x3b => {
                            return create(
                                RelaxationKind::RexCmpIndirectToAbsolute,
                                object::elf::R_X86_64_32,
                            );
                        }
                        _ => return None,
                    }
                } else if can_bypass_got {
                    match b1 {
                        0x8b => {
                            return create(
                                RelaxationKind::MovIndirectToLea,
                                object::elf::R_X86_64_PC32,
                            );
                        }
                        _ => return None,
                    }
                }
            }
            object::elf::R_X86_64_GOTPCRELX => {
                if is_absolute || is_absolute_address {
                    match section_bytes.get(offset - 2)? {
                        0x8b => {
                            return create(
                                RelaxationKind::MovIndirectToAbsolute,
                                object::elf::R_X86_64_32,
                            );
                        }
                        _ => {}
                    }
                }
                if can_bypass_got {
                    match section_bytes.get(offset - 2..offset)? {
                        [0xff, 0x15] => {
                            return create(
                                RelaxationKind::CallIndirectToRelative,
                                object::elf::R_X86_64_PC32,
                            )
                        }
                        _ => return None,
                    }
                }
                return None;
            }
            object::elf::R_X86_64_GOTPCREL if can_bypass_got && offset >= 2 => {
                match section_bytes.get(offset - 2)? {
                    0x8b => {
                        return create(
                            RelaxationKind::MovIndirectToLea,
                            object::elf::R_X86_64_PC32,
                        );
                    }
                    _ => {}
                }
                return None;
            }
            object::elf::R_X86_64_GOTTPOFF if can_bypass_got => {
                match section_bytes.get(offset - 3..offset - 1)? {
                    [0x48 | 0x4c, 0x8b] => {
                        return create(
                            RelaxationKind::RexMovIndirectToAbsolute,
                            object::elf::R_X86_64_TPOFF32,
                        )
                    }
                    _ => {}
                }
            }
            object::elf::R_X86_64_PLT32 if can_bypass_got => {
                return create(RelaxationKind::NoOp, object::elf::R_X86_64_PC32);
            }
            object::elf::R_X86_64_PLTOFF64 if can_bypass_got => {
                return create(RelaxationKind::NoOp, object::elf::R_X86_64_GOTOFF64);
            }
            object::elf::R_X86_64_TLSGD if can_bypass_got && output_kind.is_executable() => {
                let kind = match TlsGdForm::identify(section_bytes, offset)? {
                    TlsGdForm::Regular => RelaxationKind::TlsGdToLocalExec,
                    TlsGdForm::Large => RelaxationKind::TlsGdToLocalExecLarge,
                };
                return create(kind, object::elf::R_X86_64_TPOFF32);
            }
            object::elf::R_X86_64_TLSGD if output_kind.is_executable() => {
                let kind = match TlsGdForm::identify(section_bytes, offset)? {
                    TlsGdForm::Regular => RelaxationKind::TlsGdToInitialExec,
                    TlsGdForm::Large => {
                        // TODO
                        return None;
                    }
                };
                return create(kind, object::elf::R_X86_64_GOTTPOFF);
            }
            object::elf::R_X86_64_TLSLD if output_kind.is_executable() => {
                if section_bytes.get(offset - 3..offset)? == [0x48, 0x8d, 0x3d] {
                    return create(RelaxationKind::TlsLdToLocalExec, object::elf::R_X86_64_NONE);
                }
            }
            _ => return None,
        };
        None
    }

    fn apply(
        &self,
        section_bytes: &mut [u8],
        offset_in_section: &mut u64,
        addend: &mut u64,
        next_modifier: &mut RelocationModifier,
    ) {
        self.kind
            .apply(section_bytes, offset_in_section, addend, next_modifier);
    }

    fn rel_info(&self) -> crate::elf::RelocationKindInfo {
        self.rel_info
    }

    fn debug_kind(&self) -> impl std::fmt::Debug {
        &self.kind
    }
}

enum TlsGdForm {
    Regular,
    Large,
}

impl TlsGdForm {
    fn identify(bytes: &[u8], offset: usize) -> Option<Self> {
        if bytes.get(offset - 4..offset) == Some(&[0x66, 0x48, 0x8d, 0x3d])
            && bytes.get(offset + 4..offset + 8) == Some(&[0x66, 0x66, 0x48, 0xe8])
        {
            Some(Self::Regular)
        } else if bytes.get(offset - 3..offset) == Some(&[0x48, 0x8d, 0x3d])
            && bytes.get(offset + 4..offset + 6) == Some(&[0x48, 0xb8])
            && bytes.get(offset + 14..offset + 19) == Some(&[0x48, 0x01, 0xd8, 0xff, 0xd0])
        {
            Some(Self::Large)
        } else {
            None
        }
    }
}

#[test]
fn test_relaxation() {
    use crate::arch::Relaxation as _;
    use crate::args::RelocationModel;

    #[track_caller]
    fn check(relocation_kind: u32, bytes_in: &[u8], address: &[u8], absolute: &[u8]) {
        let mut out = bytes_in.to_owned();
        let mut offset = bytes_in.len() as u64;
        let mut modifier = RelocationModifier::Normal;
        if let Some(r) = Relaxation::new(
            relocation_kind,
            bytes_in,
            offset,
            ValueFlags::ADDRESS,
            OutputKind::StaticExecutable(RelocationModel::Relocatable),
            shf::EXECINSTR,
        ) {
            r.apply(&mut out, &mut offset, &mut 0, &mut modifier);

            assert_eq!(
                out, address,
                "resolved: Expected {address:x?}, got {out:x?}"
            );
        }
        if let Some(r) = Relaxation::new(
            relocation_kind,
            bytes_in,
            offset,
            ValueFlags::ABSOLUTE,
            OutputKind::StaticExecutable(RelocationModel::Relocatable),
            shf::EXECINSTR,
        ) {
            out.copy_from_slice(bytes_in);
            r.apply(&mut out, &mut offset, &mut 0, &mut modifier);
            assert_eq!(
                out, absolute,
                "unresolved: Expected {absolute:x?}, got {out:x?}"
            );
        }
    }

    check(
        object::elf::R_X86_64_REX_GOTPCRELX,
        &[0x48, 0x8b, 0xae],
        &[0x48, 0x8d, 0xae],
        &[0x48, 0xc7, 0xc5],
    );
}
