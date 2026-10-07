fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["doctor"] {
        eprintln!("usage: epochsnap doctor");
        std::process::exit(2);
    }
    match epochsnap::probe_userfaultfd() {
        Ok(caps) => println!(
            "userspace-only synchronous WP available\npage_size={} features={:#x} ioctls={:#x} range_ioctls={:#x}\nRegistration checked; actual fault validation requires linux_uffd tests.",
            caps.page_size, caps.features, caps.ioctls, caps.range_ioctls
        ),
        Err(error) => {
            eprintln!(
                "EpochSnap unavailable: {error}\nCheck kernel support and seccomp/LSM policy; no privileged fallback is used."
            );
            std::process::exit(1);
        }
    }
}
