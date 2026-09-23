//! Fixed, source-free checks of the filter inherited by the bootstrap.
//!
//! Filter mode and Docker's recipe do not establish actual syscall behavior.
//! The selected runtime/image is trusted against deliberate probe-specific
//! deception (DECISIONS 533); this is not arbitrary BPF equivalence checking.
//! A tiny generated ELF uses all three x86 syscall ABIs without a compiler,
//! host helper, writable executable mount, or project-local unsafe Rust.

use std::fmt::Write as _;

/// Only the forwarder may initiate the private connection. Its other process
/// and Fast Open restrictions are the same as the workload's.
#[derive(Clone, Copy)]
pub(super) enum Role {
    Workload,
    Forwarder,
}

pub(super) fn command(role: Role) -> String {
    let mut hex = String::new();
    for byte in executable(role) {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    // Both supported engine images supply Perl. The fixed ELF is execution
    // machinery, never a declaration or a caller-supplied program. memfd
    // avoids a writable executable mount or a persisted helper. Seal the file
    // against writes/growth/shrinkage before executing it. CLOEXEC closes
    // the descriptor when the kernel starts the ELF. A missing interpreter,
    // unavailable memfd execution, short write, signal or wrong result refuses
    // the gate. The engine inherits the waiter's filter, not a probe's report.
    format!(
        "/usr/bin/perl -e 'use strict; use warnings; my $name=\"pbps-seccomp-probe-v1\"; my $fd=syscall(319,$name,3); die \"probe memfd\" if $fd<0; open(my $out, \">&=$fd\") or die \"probe open\"; my $bytes=pack(\"H*\",$ARGV[0]); my $n=syswrite($out,$bytes); die \"probe write\" unless defined($n) && $n==length($bytes); die \"probe seal\" unless syscall(72,$fd,1033,15)==0; exec(\"/proc/self/fd/$fd\"); die \"probe exec\";' '{hex}'"
    )
}

#[derive(Clone, Copy)]
enum Abi {
    Native,
    I386,
    X32,
}

/// Each denied call has harmless invalid operands if the filter is absent.
/// EPERM is observed before operand validation under the qualified recipe.
/// In particular, the send calls carry MSG_FASTOPEN in their actual flags
/// argument; testing connect alone missed that separate entry point.
fn calls() -> [(u32, u32, u32, [u32; 4]); 11] {
    let bad = u32::MAX;
    [
        (42, 362, 42, [bad, 0, 0, 0]),             // connect
        (101, 26, 521, [bad, 0, 0, 0]),            // ptrace
        (310, 347, 539, [bad, 0, 0, 0]),           // process_vm_readv
        (311, 348, 540, [bad, 0, 0, 0]),           // process_vm_writev
        (438, 438, 438, [bad, 0, 0, 0]),           // pidfd_getfd
        (272, 310, 272, [0, 0, 0, 0]),             // unshare (no flags)
        (425, 425, 425, [0, 0, 0, 0]),             // io_uring_setup (zero entries)
        (44, 369, 44, [bad, 0, 0, 0x2000_0000]),   // sendto + MSG_FASTOPEN
        (46, 370, 518, [bad, 0, 0x2000_0000, 0]),  // sendmsg + MSG_FASTOPEN
        (307, 345, 538, [bad, 0, 0, 0x2000_0000]), // sendmmsg + MSG_FASTOPEN
        (41, 359, 41, [40, 1, 0, 0]),              // socket(AF_VSOCK, SOCK_STREAM, 0)
    ]
}

fn executable(role: Role) -> Vec<u8> {
    let mut code = Code(Vec::new());
    // No inherited register may turn a deliberately invalid call into an
    // operation on a real object. r8/r9 are the fifth/sixth native arguments.
    code.0.extend([0x45, 0x31, 0xc0, 0x45, 0x31, 0xc9]);
    let mut case = 1;
    for abi in [Abi::Native, Abi::I386, Abi::X32] {
        for (native, i386, x32, args) in calls() {
            if native == 42 && matches!(role, Role::Forwarder) {
                continue;
            }
            let number = match abi {
                Abi::Native => native,
                Abi::I386 => i386,
                Abi::X32 => 0x4000_0000 | x32,
            };
            code.call(abi, number, args);
            code.require(-1, case); // Linux -EPERM, not a library errno.
            case += 1;
        }
    }
    if matches!(role, Role::Workload) {
        // i386 also exposes connect through socketcall(SYS_CONNECT, NULL).
        code.call(Abi::I386, 102, [3, 0, 0, 0]);
        code.require(-1, case);
    }
    code.exit(0);
    elf(&code.0)
}

struct Code(Vec<u8>);

impl Code {
    fn immediate(&mut self, opcode: &[u8], value: u32) {
        self.0.extend(opcode);
        self.0.extend(value.to_le_bytes());
    }

    fn call(&mut self, abi: Abi, number: u32, args: [u32; 4]) {
        self.immediate(&[0xb8], number); // mov eax, syscall number
        match abi {
            Abi::Native | Abi::X32 => {
                self.immediate(&[0xbf], args[0]); // edi
                self.immediate(&[0xbe], args[1]); // esi
                self.immediate(&[0xba], args[2]); // edx
                self.immediate(&[0x41, 0xba], args[3]); // r10d
                self.0.extend([0x45, 0x31, 0xc0, 0x45, 0x31, 0xc9]);
                self.0.extend([0x0f, 0x05]); // syscall
            }
            Abi::I386 => {
                self.immediate(&[0xbb], args[0]); // ebx
                self.immediate(&[0xb9], args[1]); // ecx
                self.immediate(&[0xba], args[2]); // edx
                self.immediate(&[0xbe], args[3]); // esi
                self.0.extend([0x31, 0xff, 0x31, 0xed]); // edi/ebp = 0
                self.0.extend([0xcd, 0x80]); // int 0x80, independent of ELF class
            }
        }
    }

    fn require(&mut self, expected: i32, case: u32) {
        self.0.push(0x3d); // cmp eax, expected
        self.0.extend(expected.to_le_bytes());
        self.0.extend([0x74, 12]); // je past the failure exit sequence
        self.exit(case);
    }

    fn exit(&mut self, code: u32) {
        self.immediate(&[0xbf], code); // edi = exit status
        self.immediate(&[0xb8], 60); // native exit
        self.0.extend([0x0f, 0x05]);
    }
}

fn elf(code: &[u8]) -> Vec<u8> {
    // One read/execute PT_LOAD segment. There is no interpreter, dynamic
    // loader, data relocation, executable stack request, or writable segment.
    let base = 0x0040_0000_u64;
    let header = 64 + 56;
    let length = (header + code.len()) as u64;
    let mut bytes = vec![0_u8; header];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes()); // ET_EXEC
    bytes[18..20].copy_from_slice(&62_u16.to_le_bytes()); // EM_X86_64
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&(base + header as u64).to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes()); // PT_LOAD
    bytes[68..72].copy_from_slice(&5_u32.to_le_bytes()); // PF_R | PF_X
    bytes[80..88].copy_from_slice(&base.to_le_bytes());
    bytes[88..96].copy_from_slice(&base.to_le_bytes());
    bytes[96..104].copy_from_slice(&length.to_le_bytes());
    bytes[104..112].copy_from_slice(&length.to_le_bytes());
    bytes[112..120].copy_from_slice(&4096_u64.to_le_bytes());
    bytes.extend(code);
    bytes
}
