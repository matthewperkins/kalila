//! Driver for the Open Ephys acquisition board (ECP5 gateware, FT600 USB 3, ONI protocol),
//! plus a real-time simulator implementing the same [`Board`] trait.

pub mod oni;
pub mod oni_board;
pub mod parser;
pub mod registers;
pub mod sim_board;
pub mod types;

pub use oni_board::OniBoard;
pub use sim_board::SimBoard;
pub use types::*;
