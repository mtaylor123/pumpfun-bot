use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub struct AppFlags {
    pub stop_buys: Arc<AtomicBool>,   // monitor -> invest
    pub invest_done: Arc<AtomicBool>, // invest -> monitor
}

impl AppFlags {
    pub fn new() -> Self {
        Self {
            stop_buys: Arc::new(AtomicBool::new(false)),
            invest_done: Arc::new(AtomicBool::new(false)),
        }
    }

    // Optional convenience helpers (nice to have)
    pub fn request_stop(&self) {
        self.stop_buys.store(true, Ordering::SeqCst);
    }

    pub fn is_stop_requested(&self) -> bool {
        self.stop_buys.load(Ordering::SeqCst)
    }

    pub fn mark_invest_done(&self) {
        self.invest_done.store(true, Ordering::SeqCst);
    }

    pub fn is_invest_done(&self) -> bool {
        self.invest_done.load(Ordering::SeqCst)
    }

    pub fn reset_for_new_run(&self) {
        self.stop_buys.store(false, Ordering::SeqCst);
        self.invest_done.store(false, Ordering::SeqCst);
    }
}