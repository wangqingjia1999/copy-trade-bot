use std::sync::{Arc, Mutex};

pub struct SolPrice {
    price: f64,
}

impl SolPrice {
    pub fn get_price(&self) -> f64 {
        self.price
    }
}

pub type SolPriceHandle = Arc<Mutex<SolPrice>>;

pub fn get_singleton() -> SolPriceHandle {
    static mut INSTANCE: Option<SolPriceHandle> = None;
    
    unsafe {
        if INSTANCE.is_none() {
            INSTANCE = Some(Arc::new(Mutex::new(SolPrice {
                price: 0.0,
            })));
        }
        INSTANCE.as_ref().unwrap().clone()
    }
}

pub fn set_sol_price(price: f64) {
    let instance = get_singleton();
    let mut lock = instance.lock().unwrap();
    lock.price = price;
}

pub fn get_sol_price() -> f64 {
    let instance = get_singleton();
    let lock = instance.lock().unwrap();
    lock.get_price()
}