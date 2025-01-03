use crate::relaxation::RelocationModifier;

#[derive(Debug, Clone, Copy)]
pub enum RelaxationKind {
    /// Transforms a mov instruction that would have loaded an address to not use the GOT. The
    /// transformation will look like `mov *x(%rip), reg` -> `lea x(%rip), reg`.
    MovIndirectToLea,

    /// Transforms a mov instruction that would have loaded an absolute value to not use the GOT.
    /// The transformation will look like `mov *x(%rip), reg` ->  `mov x, reg`.
    MovIndirectToAbsolute,

    /// Transforms a mov instruction that would have loaded an absolute value to not use the GOT.
    /// The transformation will look like `mov *x(%rip), reg` ->  `mov x, reg`.
    RexMovIndirectToAbsolute,

    // Transforms an indirect sub to an absolute sub.
    RexSubIndirectToAbsolute,

    // Transforms an indirect cmp to an absolute cmp.
    RexCmpIndirectToAbsolute,

    /// Transform a call instruction like `call *x(%rip)` -> `call x(%rip)`.
    CallIndirectToRelative,

    /// Leave the instruction alone. Used when we only want to change the kind of relocation used.
    NoOp,

    /// Transform general dynamic (GD) into local exec.
    TlsGdToLocalExec,

    /// As above, but for the large-model form of the instruction.
    TlsGdToLocalExecLarge,

    /// Transform local dynamic (LD) into local exec.
    TlsLdToLocalExec,

    /// Transform general dynamic (GD) into initial exec
    TlsGdToInitialExec,
}

impl RelaxationKind {
    pub fn apply(
        self,
        section_bytes: &mut [u8],
        offset_in_section: &mut u64,
        addend: &mut u64,
        next_modifier: &mut RelocationModifier,
    ) {
        let offset = *offset_in_section as usize;
        match self {
            RelaxationKind::MovIndirectToLea => {
                // Since the value is an address, we transform a PC-relative mov into a PC-relative
                // lea.
                section_bytes[offset - 2] = 0x8d;
            }
            RelaxationKind::MovIndirectToAbsolute => {
                // Turn a PC-relative mov into an absolute mov.
                section_bytes[offset - 2] = 0xc7;
                let mod_rm = &mut section_bytes[offset - 1];
                *mod_rm = (*mod_rm >> 3) & 0x7 | 0xc0;
                *addend = 0;
            }
            RelaxationKind::RexMovIndirectToAbsolute => {
                // Turn a PC-relative mov into an absolute mov.
                let rex = section_bytes[offset - 3];
                section_bytes[offset - 3] = (rex & !4) | ((rex & 4) >> 2);
                section_bytes[offset - 2] = 0xc7;
                let mod_rm = &mut section_bytes[offset - 1];
                *mod_rm = (*mod_rm >> 3) & 0x7 | 0xc0;
                *addend = 0;
            }
            RelaxationKind::RexSubIndirectToAbsolute => {
                // Turn a PC-relative sub into an absolute sub.
                let rex = section_bytes[offset - 3];
                section_bytes[offset - 3] = (rex & !4) | ((rex & 4) >> 2);
                section_bytes[offset - 2] = 0x81;
                let mod_rm = &mut section_bytes[offset - 1];
                *mod_rm = (*mod_rm >> 3) & 0x7 | 0xe8;
                *addend = 0;
            }
            RelaxationKind::RexCmpIndirectToAbsolute => {
                // Turn a PC-relative cmp into an absolute cmp.
                let rex = section_bytes[offset - 3];
                section_bytes[offset - 3] = (rex & !4) | ((rex & 4) >> 2);
                section_bytes[offset - 2] = 0x81;
                let mod_rm = &mut section_bytes[offset - 1];
                *mod_rm = (*mod_rm >> 3) & 0x7 | 0xf8;
                *addend = 0;
            }
            RelaxationKind::CallIndirectToRelative => {
                section_bytes[offset - 2..offset].copy_from_slice(&[0x67, 0xe8]);
            }
            RelaxationKind::TlsGdToLocalExec => {
                section_bytes[offset - 4..offset + 8].copy_from_slice(&[
                    0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, // mov %fs:0,%rax
                    0x48, 0x8d, 0x80, // lea {offset}(%rax),%rax
                ]);
                *offset_in_section += 8;
                *addend = 0;
                *next_modifier = RelocationModifier::SkipNextRelocation;
            }
            RelaxationKind::TlsGdToLocalExecLarge => {
                section_bytes[offset - 3..offset + 19].copy_from_slice(&[
                    0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, // mov %fs:0,%rax
                    0x48, 0x8d, 0x80, 0, 0, 0, 0, // lea {offset}(%rax),%rax
                    0x66, 0x0f, 0x1f, 0x44, 0, 0, // nopw (%rax,%rax)
                ]);
                *offset_in_section += 9;
                *addend = 0;
                *next_modifier = RelocationModifier::SkipNextRelocation;
            }
            RelaxationKind::TlsGdToInitialExec => {
                section_bytes[offset - 4..offset + 8]
                    .copy_from_slice(&[0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x03, 0x05]);
                *offset_in_section += 8;
                *addend = -12_i64 as u64;
                *next_modifier = RelocationModifier::SkipNextRelocation;
            }
            RelaxationKind::TlsLdToLocalExec => {
                // Transforms to: `mov %fs:0x0,%rax` with some amount of padding depending on
                // whether the subsequent instruction is 64 bit (first) or 32 bit (second).
                if section_bytes.get(offset + 4..offset + 6) == Some(&[0x48, 0xb8]) {
                    section_bytes[offset - 3..offset + 19].copy_from_slice(&[
                        // nopw (%rax,%rax)
                        0x66, 0x66, 0x66, 0x66, 0x2e, 0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0,
                        // mov %fs:0,%rax
                        0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0,
                    ]);
                    *offset_in_section += 15;
                } else {
                    section_bytes[offset - 3..offset + 9].copy_from_slice(&[
                        0x66, 0x66, 0x66, 0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0,
                    ]);
                    *offset_in_section += 5;
                }
                *next_modifier = RelocationModifier::SkipNextRelocation;
            }
            RelaxationKind::NoOp => {}
        }
    }
}
