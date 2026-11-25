//! Prints the register and command lists (`rhythm::regdump::dump`), or with `--digest` the
//! short form the golden test compares (`rhythm::regdump::digest`). Opens no device.
fn main() {
    if std::env::args().any(|a| a == "--digest") {
        print!("{}", rhythm::regdump::digest());
    } else {
        print!("{}", rhythm::regdump::dump());
    }
}
