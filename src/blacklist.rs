use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Global trader blacklist
static mut BLACKLIST: Option<Arc<Mutex<HashSet<String>>>> = None;

// Initialize blacklist
pub fn init_blacklist(interval: u64) {
    let blacklist = Arc::new(Mutex::new(HashSet::new()));
    unsafe {
        BLACKLIST = Some(blacklist.clone());
    }

    // Spawn thread to refresh blacklist.txt
    thread::spawn(move || {
        loop {
            if let Ok(file) = File::open("blacklist.txt") {
                let reader = BufReader::new(file);
                let mut new_blacklist = HashSet::new();
                
                for line in reader.lines() {
                    if let Ok(trader) = line {
                        new_blacklist.insert(trader.trim().to_string());
                    }
                }
                
                if let Ok(mut blacklist) = blacklist.lock() {
                    *blacklist = new_blacklist;
                }
            }
            
            thread::sleep(Duration::from_secs(interval));
        }
    });
}

// Check if trader is in blacklist
pub fn is_trader_in_blacklist(trader_id: &str) -> bool {
    unsafe {
        if let Some(blacklist) = &BLACKLIST {
            if let Ok(blacklist) = blacklist.lock() {
                return blacklist.contains(trader_id);
            }
        }
    }
    false
}
