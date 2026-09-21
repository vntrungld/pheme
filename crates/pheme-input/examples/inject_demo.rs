//! Moves the pointer in a small square and types "pheme" after 3 seconds. Linux/Windows.
use std::thread::sleep;
use std::time::Duration;

use pheme_proto::KeyCode;

fn main() {
    let mut inj = pheme_input::detect_inject().expect("inject backend");
    println!("screens: {:?}", inj.screens());
    println!("focus a text editor; starting in 3 s");
    sleep(Duration::from_secs(3));
    for (dx, dy) in [(100, 0), (0, 100), (-100, 0), (0, -100)] {
        inj.mouse_move_rel(dx, dy).unwrap();
        sleep(Duration::from_millis(200));
    }
    for code in [0x13u16, 0x0B, 0x08, 0x10, 0x08] {
        inj.key(KeyCode(code), true).unwrap();
        inj.key(KeyCode(code), false).unwrap();
        sleep(Duration::from_millis(50));
    }
    inj.wheel(0, -240).unwrap();
}
