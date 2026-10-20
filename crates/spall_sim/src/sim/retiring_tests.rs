use super::Retiring;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Flag(Arc<AtomicBool>);
impl Drop for Flag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_retired_value_is_dropped_off_the_calling_thread_and_finish_waits_for_it() {
    let dropped = Arc::new(AtomicBool::new(false));
    let caller = std::thread::current().id();
    struct Where(Arc<AtomicBool>, std::thread::ThreadId, Arc<AtomicBool>);
    impl Drop for Where {
        fn drop(&mut self) {
            self.2
                .store(std::thread::current().id() != self.1, Ordering::SeqCst);
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let off_thread = Arc::new(AtomicBool::new(false));
    let mut retiring = Retiring::default();
    retiring.retire(Where(Arc::clone(&dropped), caller, Arc::clone(&off_thread)));
    retiring.finish();
    assert!(dropped.load(Ordering::SeqCst), "finish waits for the drop");
    assert!(
        off_thread.load(Ordering::SeqCst),
        "dropped on another thread"
    );
    // A second value retires cleanly after the first finished.
    let second = Arc::new(AtomicBool::new(false));
    retiring.retire(Flag(Arc::clone(&second)));
    retiring.finish();
    assert!(second.load(Ordering::SeqCst));
}

#[test]
#[should_panic(expected = "drop failed")]
fn a_panic_while_dropping_is_re_raised_on_the_owner() {
    struct Boom;
    impl Drop for Boom {
        fn drop(&mut self) {
            panic!("drop failed");
        }
    }
    let mut retiring = Retiring::default();
    retiring.retire(Boom);
    retiring.finish();
}
