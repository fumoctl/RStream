//! No picture path on this OS: mpv still plays the sound.
use crate::{Host, Path, Player};
use std::sync::Arc;

pub fn candidates() -> &'static [Path] { &[] }
pub fn open(_path: Path, _host: &Host, _player: &Arc<dyn Player>) -> Result<(), String> { Err("no picture path on this OS".into()) }
pub fn close(_path: Path) {}
pub fn layout(_player: &dyn Player, _hdr_output: bool) {}
