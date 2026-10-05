use heapless::spsc::Queue;
use spin::Mutex;

static SCANCODE_QUEUE: Mutex<Queue<u8, 128>> = Mutex::new(Queue::new());

pub fn push_scancode(scancode: u8) {
    let mut q = SCANCODE_QUEUE.lock();
    // Full queue: silently drop — no panic in IRQ context
    let _ = q.enqueue(scancode);
}

pub fn pop_scancode() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| SCANCODE_QUEUE.lock().dequeue())
}
