//! Library part of the ferrofs service: the [`engine::Engine`] that owns the volume
//! indexes and answers IPC requests. The binary wraps it in a Windows service; the
//! `fake_service` example serves it over the pipe for UI tests without admin rights.

pub mod engine;
