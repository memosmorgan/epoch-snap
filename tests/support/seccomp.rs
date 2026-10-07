use std::os::unix::process::CommandExt;
use std::process::Command;

pub(crate) fn deny_userfaultfd(command: &mut Command) {
    // Deny only userfaultfd in this child. The four-instruction BPF program
    // permits everything else; no parent/global policy or sysctl is changed.
    let mut filters = [
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: libc::SYS_userfaultfd as u32,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x0005_0000 | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x7fff_0000,
        },
    ];
    // SAFETY: pre_exec performs only Linux syscalls, no allocation/locks/logging.
    // The captured fixed stack buffer stays live through the filter-install call.
    unsafe {
        command.pre_exec(move || {
            let program = libc::sock_fprog {
                len: 4,
                filter: filters.as_mut_ptr(),
            };
            if libc::syscall(libc::SYS_prctl, libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(
                    libc::SYS_prctl,
                    libc::PR_SET_SECCOMP,
                    2,
                    &program as *const _,
                    0,
                    0,
                ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
