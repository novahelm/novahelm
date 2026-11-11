//! hidapi must only ever be used through `novagear_drivers::hid`, whose permanent thread is
//! the one that first initialises it (see that module for the crash this prevents).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Structure: no source file outside `hid.rs` constructs a `HidApi`.
#[test]
fn only_the_hid_module_constructs_hidapi() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    // `HidApi::new` also covers `new_without_enumerate`; `ffi::hid_` is hidapi's raw C calls,
    // which would start the manager without going through the hid thread.
    let needles = [["HidApi", "::new"].concat(), ["ffi", "::hid_"].concat()];
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    let me = Path::new(file!()).file_name().unwrap();
    for f in files {
        if f.ends_with("drivers/src/hid.rs") || f.file_name() == Some(me) {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for n in &needles {
            assert!(
                !text.contains(n.as_str()),
                "{} constructs hidapi directly",
                f.display()
            );
        }
    }
}

/// Behaviour: listings from many short-lived threads all run on one permanent thread, and
/// hidapi is constructed once however many threads ask.
#[test]
fn short_lived_threads_share_one_permanent_hid_thread() {
    let owner = novagear_drivers::hid::owner_thread_id().unwrap();
    let ids: HashSet<_> = (0..6)
        .map(|_| {
            // A thread that exits right after its listing, like an idle-reaped pool thread.
            let id = std::thread::spawn(|| {
                novagear_drivers::hid::with(|api| {
                    let _ = api.device_list().count();
                    std::thread::current().id()
                })
                .unwrap()
            })
            .join()
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            id
        })
        .collect();
    assert_eq!(ids, HashSet::from([owner]));
    assert_ne!(owner, std::thread::current().id());
    assert_eq!(novagear_drivers::hid::constructed(), 1);
}

#[test]
fn a_panicking_job_leaves_the_hid_thread_usable() {
    let r = novagear_drivers::hid::with(|_| -> () { panic!("boom") });
    assert!(r.is_err());
    assert!(novagear_drivers::hid::with(|_| 7).is_ok());
}

#[test]
fn calling_with_from_the_hid_thread_is_an_error_not_a_deadlock() {
    let inner = novagear_drivers::hid::with(|_| novagear_drivers::hid::with(|_| 1).is_err());
    assert!(inner.unwrap());
}
