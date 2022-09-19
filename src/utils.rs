use std::time::Duration;
use quanta::Clock;
use crate::bench::Count;

pub fn black_box<T>(dummy: T) -> T {
    unsafe { std::ptr::read_volatile(&dummy) }
}

pub fn delay_cycles(num_iterations: usize) {
    static VALUE: usize = 0;
    for _ in 0..num_iterations {
        // unsafe { std::arch::asm!("nop"); } might not work on all platforms
        black_box(&VALUE);
    }
}

// Returns the duration of doing `num_iterations` of clock.raw()
pub fn clock_read_overhead(clock: &Clock, num_iterations: Count) -> Duration {
    let start = clock.raw();
    for _ in 0..(num_iterations-1) {
        black_box(clock.raw());
    }
    let end = clock.raw();
    clock.delta(start, end)
}
